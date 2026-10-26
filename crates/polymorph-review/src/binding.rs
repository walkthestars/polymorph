//! Bus + LLM binding.
//!
//! F1 proved the loop over injected traits; this module binds those traits to
//! the delegate bus (fresh child per call = clean context structurally) and
//! the D1-managed LLM path (role chains, cooldowns, key injection).
//!
//! Rules:
//! - Text runners are dumb: system pack + `ctx.brief` → one LLM turn → done.
//!   Per-round briefs are rendered by the Bus adapters AT SPAWN, so the
//!   content is always current and reviewers never see history.
//! - Reviewers stay tool-free by construction (clean context). The FIXER
//!   wields tools only behind the operator's `--tools` flag
//!   ([`AgenticFixRunner`]); text-only fixing stays the default. The one
//!   exception is opt-in: `--reviewer-tools` swaps in the read-only
//!   [`crate:review_tools:ToolReviewTurnRunner`].
//! - Workdir is caller-chosen (`InPlace` default; CLI `--repo` is `InPlace`
//!   on one drive-scoped worktree, `DriveWorktree`; per-child `Worktree`
//!   specs serve bench/delegate); `keep_worktree` stays false outside the
//!   bench merge mode (merge-back = F2b/G, untouched).

use async_trait::async_trait;
use polymorph_core::Role;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::{parse_verdict, Executor, ExecutorInput, Reviewer, ReviewerError, ReviewerInput};
use polymorph_core::Verdict;

/// Where a binding turn's model comes from.
#[derive(Debug, Clone)]
pub enum LlmTarget {
    /// Production: D1-managed role chain (fallback, cooldowns, injection).
    /// `state` is shared across the invocation so cooldowns persist round to
    /// round. `FallbackState` is `!Clone`, hence the mutex.
    Managed {
        router: polymorph_llm::RoleRouter,
        providers: Vec<polymorph_core::config::ProviderEntry>,
        state: Arc<tokio::sync::Mutex<polymorph_llm::FallbackState>>,
        role: Role,
        scope: Option<String>,
    },
    /// Test seam (precedent): fixed addressable target for wiremock.
    /// Downstream mapping is identical; only model resolution differs.
    Fixed(genai::ModelSpec),
}

/// Resolve the turn target: a recorded parent model wins (142 directive —
/// children run their parent's model); unknown (first turn) falls back to
/// the configured target verbatim (today's behavior, byte for byte).
fn resolve_target(pinned: &std::sync::Mutex<Option<String>>, configured: &LlmTarget) -> LlmTarget {
    match pinned.lock().ok().and_then(|pin| pin.clone()) {
        Some(name) => LlmTarget::Fixed(genai::ModelSpec::from(name.as_str())),
        None => configured.clone(),
    }
}

/// Record the serving model for the next turn. Poisoned mutex keeps the
/// old pin (fail-soft — a poisoned lock must never fail a turn).
fn record_pin(pinned: &std::sync::Mutex<Option<String>>, served: Option<String>) {
    if let (Some(model), Ok(mut pin)) = (served, pinned.lock()) {
        *pin = Some(model);
    }
}

/// One LLM turn with a fixed system prompt. Shared by both runners.
/// Returns text + serving model name (`None` on Fixed — already pinned,
/// nothing to learn).
async fn single_turn(
    client: &polymorph_llm::LlmClient,
    configured: &LlmTarget,
    target: &LlmTarget,
    system: &str,
    brief: &str,
    cancel: &tokio_util::sync::CancellationToken,
) -> anyhow::Result<(String, Option<String>)> {
    let mut sink = String::new();
    match target {
        LlmTarget::Managed {
            router,
            providers,
            state,
            role,
            scope,
        } => {
            let mut state = state.lock().await;
            let (outcome, model) = client
                .stream_chat_managed(
                    router,
                    providers,
                    &mut state,
                    *role,
                    scope.as_deref(),
                    system,
                    brief,
                    cancel,
                    &mut sink,
                )
                .await?;
            Ok((outcome.text, Some(model)))
        }
        LlmTarget::Fixed(spec) => {
            if let LlmTarget::Managed {
                providers, state, ..
            } = configured
            {
                let model_name = match spec {
                    genai::ModelSpec::Name(ref n) => Some(n.as_str()),
                    genai::ModelSpec::Target(ref t) => Some(t.model.model_name.as_str()),
                    genai::ModelSpec::Iden(ref i) => Some(i.model_name.as_str()),
                };
                if let Some(m) = model_name {
                    let mut state = state.lock().await;
                    let outcome = client
                        .stream_chat_single(
                            providers, &mut state, m, system, brief, cancel, &mut sink,
                        )
                        .await?;
                    return Ok((outcome.text, Some(m.to_string())));
                }
            }
            let outcome = client
                .stream_chat_typed_spec(spec.clone(), system, brief, cancel, &mut sink)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            Ok((outcome.text, None))
        }
    }
}

/// Review turn: system = review pack, prompt = the spawned brief.
/// Always `done`; `data.verdict_text` carries the RAW text (even prose) so
/// the loop — never the runner — judges validity.
pub struct ReviewTurnRunner {
    pub client: polymorph_llm::LlmClient,
    pub target: LlmTarget,
    pub system: String,
    /// Parent-model pin (142 directive): serving model recorded per turn;
    /// next turn resolves `Fixed` to it. `Default` (None) = role chain
    /// (today's behavior). New laterals set it the same way (126 precedent).
    pub pinned: std::sync::Mutex<Option<String>>,
}

#[async_trait]
impl polymorph_delegate::TurnRunner for ReviewTurnRunner {
    async fn run_turn(
        &self,
        ctx: polymorph_delegate::TurnContext,
    ) -> anyhow::Result<polymorph_delegate::TurnOutput> {
        let target = resolve_target(&self.pinned, &self.target);
        let (text, served) = single_turn(
            &self.client,
            &self.target,
            &target,
            &self.system,
            &ctx.brief,
            &ctx.cancel,
        )
        .await?;
        record_pin(&self.pinned, served);
        Ok(polymorph_delegate::TurnOutput {
            text: text.clone(),
            summary: "review turn complete".to_string(),
            tool_calls: 0,
            done: true,
            data: Some(serde_json::json!({ "verdict_text": text })),
        })
    }
}

/// Fix turn: system = act pack, prompt = the spawned brief.
/// Always `done`; `data.artifact` carries the rewritten artifact text.
pub struct FixTurnRunner {
    pub client: polymorph_llm::LlmClient,
    pub target: LlmTarget,
    pub system: String,
    /// Parent-model pin (see `ReviewTurnRunner.pinned`).
    pub pinned: std::sync::Mutex<Option<String>>,
}

#[async_trait]
impl polymorph_delegate::TurnRunner for FixTurnRunner {
    async fn run_turn(
        &self,
        ctx: polymorph_delegate::TurnContext,
    ) -> anyhow::Result<polymorph_delegate::TurnOutput> {
        let target = resolve_target(&self.pinned, &self.target);
        let (text, served) = single_turn(
            &self.client,
            &self.target,
            &target,
            &self.system,
            &ctx.brief,
            &ctx.cancel,
        )
        .await?;
        record_pin(&self.pinned, served);
        Ok(polymorph_delegate::TurnOutput {
            text: text.clone(),
            summary: "fix turn complete".to_string(),
            tool_calls: 0,
            done: true,
            data: Some(serde_json::json!({ "artifact": text })),
        })
    }
}

fn render_review_brief(input: &ReviewerInput, artifact_path: Option<&Path>) -> String {
    let mode = match input.mode {
        crate::LoopMode::Plan => "plan",
        crate::LoopMode::Act => "act",
    };
    let mut out = format!(
        "MODE:\n{mode}\n\nGOAL:\n{}\n\nACCEPTANCE CRITERIA:\n{}\n\nARTIFACT UNDER REVIEW:\n{}",
        input.goal, input.criteria, input.artifact
    );
    if let Some(p) = artifact_path {
        out.push_str(&format!("\n\nARTIFACT PATH:\n{}", p.display()));
    }
    // Verdict discipline: a finding that cannot name its
    // evidence is a hypothesis, not a verdict. The reviewer sees ONLY the
    // artifact text above — any file it cites must have its content IN
    // that text. Findings without a file, or naming files outside the
    // artifact, are dropped, never emitted.
    out.push_str(
        "\n\nVERDICT DISCIPLINE:\nEvery finding MUST cite the accused file and line (`line` is a 1-based integer, or null) and quote the accused code in its issue; the quoted content must appear in ARTIFACT UNDER REVIEW above. A finding that cites no file, cites a file whose content is not in the artifact text, or describes a type's fields or signatures without having that type's definition in evidence, is a hypothesis — drop it, do not emit it. When prior-round context is absent, say what was checked, not what was assumed.",
    );
    // A multi-file artifact set — every listed file is the
    // artifact. Only for set text, so single-file briefs stay identical.
    if crate::is_artifact_set(&input.artifact) {
        out.push_str(&format!(
            " Every file under {} is the artifact: findings on any of them are in scope — cite the listed path and the shown line number. A file listed there without content (ABSENT, BINARY, TOO LARGE, UNREADABLE, SKIPPED, OMITTED) has no lines in evidence.",
            crate::ARTIFACT_SET_HEADER
        ));
    }
    // Attached peripheral files are artifact text too. Only
    // when attached, so briefs without them stay byte-identical.
    if split_peripheral(&input.artifact).1 {
        out.push_str(&format!(
            " Files under {PERIPHERAL_HEADER} are part of ARTIFACT UNDER REVIEW: findings on them are in scope — cite the listed path and the shown line number. A file listed there without content (DELETED, BINARY, TOO LARGE, UNREADABLE, SKIPPED, OMITTED) has no lines in evidence."
        ));
    }
    out
}

/// CREATE-job signal for the reviewer brief: when the artifact
/// path names a file that cannot be read AND no current artifact text was
/// supplied (the CLI maps a missing file to empty for CREATE jobs), tell
/// the reviewer to judge goal/criteria fit instead of emitting file-missing
/// findings. Returns `None` in every other case so the brief stays
/// byte-identical: file exists, content present, or an unresolvable
/// workdir (a moved path must never prime a false CREATE narrative).
fn create_job_note(
    workdir: &polymorph_delegate::WorkdirSpec,
    path: &Path,
    artifact: &str,
) -> Option<String> {
    // Attached peripheral files are not artifact content.
    let (artifact, _) = split_peripheral(artifact);
    if !artifact.trim().is_empty() {
        return None;
    }
    if artifact_from_file(workdir, path).is_some() {
        return None;
    }
    Some(format!(
        "\n\nARTIFACT FILE STATUS:\n{CREATE_JOB_STATUS} Judge goal/criteria fit against the empty artifact; do NOT emit findings merely claiming the file is missing. If the file should be created, emit exactly ONE finding that names the target file, states the required contents from the goal and criteria (that file citation satisfies VERDICT DISCIPLINE), and proposes the creation — a failing verdict with zero findings is schema-invalid and wastes the round.\n"
    ))
}

/// The CREATE-job fact both the reviewer note and the harness seed state.
const CREATE_JOB_STATUS: &str =
    "The artifact path does not exist on disk and no current content was provided (CREATE job or deleted path).";

/// Harness-authored round-1 finding for a CREATE job: exactly
/// when [`create_job_note`] fires, the finding that note prescribes the
/// reviewer to emit — ONE finding that names the target file, states the
/// required contents (the goal and criteria, which the fix brief carries
/// verbatim) and proposes the creation. Fed to
/// [`crate::run_loop_seeded`], it replaces the round-1 reviewer call whose
/// only legal answer was this finding. `None` whenever the
/// note is (file exists, even empty; content given), so those drives
/// stay review-first (ruling). Always a
/// [`crate::SeedKind::Create`] seed.
pub fn create_job_seed(
    workdir: &polymorph_delegate::WorkdirSpec,
    path: &Path,
    artifact: &str,
) -> Option<crate::RoundSeed> {
    create_job_note(workdir, path, artifact)?;
    let target = path.display();
    let findings = vec![polymorph_core::Finding {
        severity: polymorph_core::Severity::P1,
        file: target.to_string(),
        line: None,
        issue: format!(
            "Harness-synthesized CREATE finding (no reviewer call): {CREATE_JOB_STATUS} {target} must be created with the required contents stated in GOAL and ACCEPTANCE CRITERIA."
        ),
        proposed_fix: format!(
            "Create {target} (and any other files GOAL names) with the full contents GOAL and ACCEPTANCE CRITERIA require, then run the verification they name."
        ),
    }];
    Some(crate::RoundSeed {
        kind: crate::SeedKind::Create,
        findings,
    })
}

/// Classify an edit-call failure for the bench taxonomy:
/// downcast to the typed [`HashlineError`](polymorph_tools::hashline::HashlineError)
/// preserved by `.context()` at the call site — TYPED matching, never
/// string-sniffing (spirit). Stale tags (the prime suspect:
/// path/tag-spelling drift between read-mint and edit) separate from
/// malformed patches (parse class) and all other apply rejections.
/// Non-hashline failures fall through to plain `"error"`.
pub fn classify_edit_error(e: &anyhow::Error) -> &'static str {
    use polymorph_tools::hashline::HashlineError;
    let Some(root) = e.downcast_ref::<HashlineError>() else {
        return "error";
    };
    match root {
        HashlineError::StaleTag { .. } => "edit-stale",
        HashlineError::Parse { .. }
        | HashlineError::EmptyBody { .. }
        | HashlineError::Unsupported(_)
        | HashlineError::MissingRegister(_) => "edit-parse",
        _ => "edit-apply",
    }
}

/// Max chars of a recorded command summary: one line, bounded,
/// so a heredoc or generated script never floods the ledger or the brief.
const MAX_CMD_SUMMARY_CHARS: usize = 160;

/// Process-running tools whose exit code the ledger records.
fn is_command_tool(name: &str) -> bool {
    matches!(name, "bash" | "exec")
}

/// Single-line, char-bounded command summary: whitespace runs and control
/// characters collapse to one space, overflow is cut on a char boundary
/// (never a byte index) and marked `...`.
fn summarize_command(cmd: &str) -> String {
    let mut flat = String::with_capacity(cmd.len().min(MAX_CMD_SUMMARY_CHARS * 4));
    let mut pending_space = false;
    for c in cmd.chars() {
        if c.is_whitespace() || c.is_control() {
            pending_space = !flat.is_empty();
            continue;
        }
        if pending_space {
            flat.push(' ');
            pending_space = false;
        }
        flat.push(c);
    }
    if flat.chars().count() > MAX_CMD_SUMMARY_CHARS {
        let mut cut: String = flat.chars().take(MAX_CMD_SUMMARY_CHARS - 3).collect();
        cut.push_str("...");
        cut
    } else {
        flat
    }
}

/// Zero-test heuristic: a test command whose filter
/// matched nothing exits 0 in under a second and looked identical to a pass.
/// Returns the matched marker. Label-only — never a verdict, and the
/// fixer's fed tool text is unchanged (only the reviewer's evidence block
/// shows it); cargo's `running 0 tests` counts only when NO harness in the
/// output ran tests (empty doc-test binaries print it on every green run).
fn zero_test_marker(output: &str) -> Option<&'static str> {
    zero_test_marker_lines(output.lines())
}

/// [`zero_test_marker`] over lines (a spilled stream is scanned
/// from its file, never loaded whole). Same answer: a marker anywhere wins
/// (in `MARKERS` order), else any nonzero `running N tests` clears a zero.
fn zero_test_marker_lines<S: AsRef<str>>(
    lines: impl IntoIterator<Item = S>,
) -> Option<&'static str> {
    const MARKERS: [&str; 3] = [
        "No tests found for given includes",
        "no tests ran",
        "collected 0 items",
    ];
    let mut found = [false; MARKERS.len()];
    let mut saw_zero = false;
    let mut saw_nonzero = false;
    for line in lines {
        let line = line.as_ref();
        for (hit, m) in found.iter_mut().zip(MARKERS) {
            *hit |= line.contains(m);
        }
        let Some(rest) = line.trim().strip_prefix("running ") else {
            continue;
        };
        let Some(n) = rest
            .strip_suffix(" tests")
            .or_else(|| rest.strip_suffix(" test"))
        else {
            continue;
        };
        match n.parse::<u64>() {
            Ok(0) => saw_zero = true,
            Ok(_) => saw_nonzero = true,
            Err(_) => {}
        }
    }
    if let Some(i) = found.iter().position(|&hit| hit) {
        return Some(MARKERS[i]);
    }
    (saw_zero && !saw_nonzero).then_some("running 0 tests")
}

/// Zero-test marker of a command result whose output spilled:
/// the digest omits lines, so the full streams are scanned instead — the
/// spill file line by line, an inline stream as is. `None` when nothing
/// spilled (the caller's digest-free scan stands) or a file is unreadable
/// (the digest scan stands too).
fn spilled_zero_test_marker(
    result: &serde_json::Value,
    workdir: &Path,
) -> Option<Option<&'static str>> {
    use std::io::BufRead as _;
    let spilled = ["stdout_spill", "stderr_spill"]
        .iter()
        .any(|k| result.get(*k).is_some());
    if !spilled {
        return None;
    }
    let mut lines: Vec<Box<dyn Iterator<Item = String>>> = Vec::new();
    for stream in ["stdout", "stderr"] {
        match result
            .get(format!("{stream}_spill"))
            .and_then(|s| s.get("path"))
        {
            Some(path) => {
                let path = workdir.join(path.as_str()?);
                let file = std::fs::File::open(path).ok()?;
                lines.push(Box::new(
                    std::io::BufReader::new(file)
                        .split(b'\n')
                        .map_while(Result::ok)
                        .map(|l| String::from_utf8_lossy(&l).into_owned()),
                ));
            }
            None => {
                let text = result.get(stream).and_then(|s| s.as_str()).unwrap_or("");
                let owned: Vec<String> = text.lines().map(str::to_string).collect();
                lines.push(Box::new(owned.into_iter()));
            }
        }
    }
    Some(zero_test_marker_lines(lines.into_iter().flatten()))
}

/// Structured ledger fields for one tool call, merged into its
/// `tool_outcomes` entry. Command tools record `cmd` (summary) always, plus
/// `exit_code` / `zero_tests` when the process ran; successful mutations
/// record `paths` (write target, or the edit report's non-noop files —
/// reusable as the turn's mutated-file set). An edit that renamed a file
/// (`MV`) lists the source then the destination in `paths` and records the
/// pair in `renamed: [{from, to}]`. Other tools stay `{tool, outcome}`
/// only, so pre-#6 consumers and ledgers read unchanged. (Files a command
/// changed are merged in by the runner: [`crate::command_changes`].)
fn ledger_extras(
    name: &str,
    args: &serde_json::Value,
    result: Option<&serde_json::Value>,
) -> serde_json::Map<String, serde_json::Value> {
    use serde_json::Value;
    let mut extras = serde_json::Map::new();
    if is_command_tool(name) {
        let cmd = args
            .get("cmd")
            .or_else(|| args.get("command"))
            .and_then(Value::as_str)
            .unwrap_or("");
        extras.insert("cmd".into(), Value::String(summarize_command(cmd)));
        if let Some(v) = result {
            if let Some(code) = v.get("exit_code").and_then(Value::as_i64) {
                extras.insert("exit_code".into(), Value::from(code));
            }
            let mut output = String::new();
            for stream in ["stdout", "stderr"] {
                if let Some(s) = v.get(stream).and_then(Value::as_str) {
                    output.push_str(s);
                    output.push('\n');
                }
            }
            if let Some(marker) = zero_test_marker(&output) {
                extras.insert("zero_tests".into(), Value::String(marker.into()));
            }
        }
        return extras;
    }
    let Some(v) = result else {
        return extras;
    };
    let mut renamed: Vec<Value> = Vec::new();
    let paths: Vec<Value> = match name {
        "write" => args
            .get("path")
            .and_then(Value::as_str)
            .map(|p| vec![Value::String(p.to_string())])
            .unwrap_or_default(),
        "edit" => {
            // `MV` destinations by section path (the report names the
            // source only; its action reads `Renamed { to: .. }`).
            let moves: Vec<(String, String)> = args
                .get("patch")
                .and_then(Value::as_str)
                .and_then(|p| polymorph_tools::hashline::parse(p).ok())
                .map(|patch| {
                    patch
                        .sections
                        .into_iter()
                        .filter_map(|sec| {
                            sec.ops.into_iter().find_map(|op| match op {
                                polymorph_tools::hashline::Op::MoveFile { dest } => {
                                    Some((sec.path.clone(), dest))
                                }
                                _ => None,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            let mut paths = Vec::new();
            for f in v
                .get("files")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let action = f.get("action").and_then(Value::as_str).unwrap_or("");
                let Some(path) = f.get("path").and_then(Value::as_str) else {
                    continue;
                };
                if action.contains("noop: true") {
                    continue;
                }
                paths.push(Value::String(path.to_string()));
                if !action.starts_with("Renamed") {
                    continue;
                }
                if let Some((_, dest)) = moves.iter().find(|(src, _)| src == path) {
                    paths.push(Value::String(dest.clone()));
                    renamed.push(serde_json::json!({ "from": path, "to": dest }));
                }
            }
            paths
        }
        _ => return extras,
    };
    extras.insert("paths".into(), Value::Array(paths));
    if !renamed.is_empty() {
        extras.insert("renamed".into(), Value::Array(renamed));
    }
    extras
}

/// Distinct mutated paths across ledger entries, first-mutation order
/// (the per-turn set the attachment renderer enriches): write/
/// edit `paths` and the `changed_paths` command change detection saw a
/// command change.
fn ledger_mutated_paths<'a>(
    entries: impl IntoIterator<Item = &'a serde_json::Value>,
) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for entry in entries {
        for key in ["paths", "changed_paths"] {
            for p in str_array(entry, key) {
                if !seen.iter().any(|s| s == p) {
                    seen.push(p.to_string());
                }
            }
        }
    }
    seen
}

/// The strings of a ledger entry's array field (none when absent).
fn str_array<'a>(entry: &'a serde_json::Value, key: &str) -> impl Iterator<Item = &'a str> {
    entry
        .get(key)
        .and_then(|p| p.as_array())
        .into_iter()
        .flatten()
        .filter_map(|p| p.as_str())
}

/// Largest file the fix runner snapshots before mutating it:
/// bigger, binary, or unreadable files get no snapshot (full-content
/// fallback downstream, never an error).
const MAX_SNAPSHOT_BYTES: u64 = 1024 * 1024;

/// Paths a write/edit call is about to touch, read from its arguments
/// (the `lsp_post_write` candidate rule: write's `path`, the edit patch's
/// section paths; an unparseable patch touches nothing).
fn mutation_targets(name: &str, args: &serde_json::Value) -> Vec<String> {
    match name {
        "write" => args
            .get("path")
            .and_then(|p| p.as_str())
            .map(|p| vec![p.to_string()])
            .unwrap_or_default(),
        "edit" => args
            .get("patch")
            .and_then(|p| p.as_str())
            .and_then(|p| polymorph_tools::hashline::parse(p).ok())
            .map(|patch| patch.sections.into_iter().map(|s| s.path).collect())
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// Pre-mutation snapshot: before a write/edit call, record each
/// target's content the FIRST time the turn is about to touch it, keyed by
/// [`attach_identity`] — `{path, content}` with `content: null` when the file
/// did not exist (created this turn). The executor diffs large peripheral
/// files against it. Best-effort: oversized, binary, or unreadable targets
/// record nothing.
fn snapshot_before_mutation(
    workdir: &Path,
    targets: Vec<String>,
    seen: &mut Vec<String>,
    snapshots: &mut Vec<serde_json::Value>,
) {
    for raw in targets {
        let id = attach_identity(workdir, &raw);
        if seen.contains(&id) {
            continue;
        }
        seen.push(id);
        let full = polymorph_tools::resolve_tool_path(workdir, &raw);
        let content = match std::fs::metadata(&full) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::Value::Null,
            Ok(m) if m.is_file() && m.len() <= MAX_SNAPSHOT_BYTES => {
                match std::fs::read(&full).ok().and_then(text_of) {
                    Some(text) => serde_json::Value::String(text),
                    None => continue,
                }
            }
            _ => continue,
        };
        snapshots.push(serde_json::json!({ "path": raw, "content": content }));
    }
}

/// UTF-8 text of a file's bytes, `None` for binary (a NUL in the first
/// 8 KiB, the `attach_tag` rule) or non-UTF-8 content. A leading BOM is
/// dropped so line 1 renders as the editor shows it.
pub(crate) fn text_of(bytes: Vec<u8>) -> Option<String> {
    if bytes.iter().take(8192).any(|&b| b == 0) {
        return None;
    }
    let text = String::from_utf8(bytes).ok()?;
    Some(match text.strip_prefix('\u{feff}') {
        Some(rest) => rest.to_string(),
        None => text,
    })
}

fn render_fix_brief(input: &ExecutorInput) -> String {
    let mode = match input.mode {
        crate::LoopMode::Plan => "plan",
        crate::LoopMode::Act => "act",
    };
    let mut out = format!("MODE:\n{mode}\n\nGOAL:\n{}\n\n", input.goal);
    // The criteria are the spec the reviewer grades against; the fixer gets
    // them verbatim, not only the reviewer's summary in the findings.
    // Omitted when empty so criteria-less briefs stay byte-identical.
    if !input.criteria.trim().is_empty() {
        out.push_str(&format!("ACCEPTANCE CRITERIA:\n{}\n\n", input.criteria));
    }
    out.push_str(&format!(
        "ARTIFACT:\n{}\n\nADDRESS EVERY FINDING:\n",
        input.artifact
    ));
    for f in &input.findings {
        out.push_str(&format!(
            "- [{:?}] {}:{} — {} (proposed fix: {})\n",
            f.severity,
            f.file,
            f.line.map(|l| l.to_string()).unwrap_or_else(|| "-".into()),
            f.issue,
            f.proposed_fix
        ));
    }
    out.push_str(
        "\nDELIVERABLE (act mode, tools on): the goal names files — CREATE missing ones with `write`, REPAIR existing ones with `edit`, VERIFY with `bash`, then report files changed + verification. Returned text alone never satisfies a finding about a missing file.\n",
    );
    out
}

/// Caps for the L-actuation (1) diagnostics feed: the brief is
/// a model prompt, not a log dump — bounded URIs × bounded items × bounded
/// chars, deterministic BTree order, omitted counts named (never silent).
/// Empty diagnostics → `None` (caller leaves the brief byte-identical,
/// preserving every pre-98 matrix when LSP is off).
pub const MAX_LSP_URIS: usize = 8;
pub const MAX_LSP_ITEMS_PER_URI: usize = 10;
pub const MAX_LSP_ITEM_CHARS: usize = 300;

/// Render last-recorded LSP diagnostics as a read-only advisory section
/// for the fix-turn brief. Pure (no IO) for unit tests. Returns `None`
/// when there is nothing to say — never an empty header.
pub fn render_lsp_section(
    diagnostics: &std::collections::BTreeMap<String, Vec<String>>,
) -> Option<String> {
    let non_empty: Vec<(&String, &Vec<String>)> = diagnostics
        .iter()
        .filter(|(_, items)| !items.is_empty())
        .collect();
    if non_empty.is_empty() {
        return None;
    }
    let mut out = String::from("LSP DIAGNOSTICS (read-only advisory, post-write):\n");
    for (uri, items) in non_empty.iter().take(MAX_LSP_URIS) {
        out.push_str(&format!("- {uri}:\n"));
        for item in items.iter().take(MAX_LSP_ITEMS_PER_URI) {
            let trimmed: String = item.chars().take(MAX_LSP_ITEM_CHARS).collect();
            let truncated = item.chars().count() > MAX_LSP_ITEM_CHARS;
            if truncated {
                out.push_str(&format!("  - {trimmed}…\n"));
            } else {
                out.push_str(&format!("  - {trimmed}\n"));
            }
        }
        if items.len() > MAX_LSP_ITEMS_PER_URI {
            out.push_str(&format!(
                "  - ({} more omitted)\n",
                items.len() - MAX_LSP_ITEMS_PER_URI
            ));
        }
    }
    if non_empty.len() > MAX_LSP_URIS {
        out.push_str(&format!(
            "({} more files omitted)\n",
            non_empty.len() - MAX_LSP_URIS
        ));
    }
    Some(out)
}

/// Count total LSP diagnostic messages: the
/// warn-first merge signal. Pure sum over the last-recorded map — 0 when
/// LSP is off or clean. Never gates, never denies (deny is queued behind
/// data); the runner records it on the outcome and warns past it.
pub fn lsp_findings_count(diagnostics: &std::collections::BTreeMap<String, Vec<String>>) -> usize {
    diagnostics.values().map(|v| v.len()).sum()
}

/// Dedupe key for a tool-supplied path (reused by #1): the
/// file URI with `./` segments collapsed, case-folded on Windows, so `a.rs`,
/// `./a.rs`, and the absolute spelling of one file compare equal.
fn path_identity(workdir: &Path, raw: &str) -> String {
    let mut uri = polymorph_tools::lsp::uri_for(workdir, raw);
    while uri.contains("/./") {
        uri = uri.replace("/./", "/");
    }
    if cfg!(windows) {
        uri.to_lowercase()
    } else {
        uri
    }
}

/// [`path_identity`] of the file a tool path argument actually names
/// Resolved the way the tools resolve it first, so Git Bash
/// drive spellings and leading-slash relatives collapse too.
pub(crate) fn attach_identity(workdir: &Path, raw: &str) -> String {
    let resolved = polymorph_tools::resolve_tool_path(workdir, raw);
    path_identity(workdir, &resolved.to_string_lossy())
}

/// Per-file LSP snapshots for the files this turn mutated:
/// the reviewer-evidence feed and the opt-in error gate's input. One
/// entry per distinct file — spellings that resolve to the same URI
/// (`a.rs`, `./a.rs`, absolute; case-folded on Windows) collapse, keyed
/// by the spelling of the file's LAST mutation. `items` is the pool's
/// record for exactly the URI that last write consulted (the pool
/// replaces a URI's record on every write, so this is the latest
/// snapshot, never history across rounds); `null` when there is none (no
/// server for the language, or the consult failed — the pool drops the
/// stale record then). Unmutated files never appear. Files a command
/// changed count too (their consult ran after the command), except those
/// past the per-command consult cap: `null` plus `"unconsulted": true`,
/// never the pool's stale record. A file whose last mutation renamed it
/// away or (command) deleted it is gone: no entry.
fn lsp_mutated_snapshots(
    details: &std::collections::BTreeMap<String, Vec<polymorph_tools::lsp::LspDiagnostic>>,
    workdir: &Path,
    outcomes: &[serde_json::Value],
) -> Vec<serde_json::Value> {
    use polymorph_tools::lsp;
    let identity = |raw: &str| path_identity(workdir, raw);
    // (identity, latest spelling, gone, unconsulted), first-mutation order.
    let mut files: Vec<(String, String, bool, bool)> = Vec::new();
    for e in outcomes {
        let gone: Vec<&str> = e
            .get("renamed")
            .and_then(|r| r.as_array())
            .into_iter()
            .flatten()
            .filter_map(|r| r.get("from").and_then(|f| f.as_str()))
            .chain(str_array(e, "deleted_paths"))
            .collect();
        let unconsulted: Vec<&str> = str_array(e, "lsp_unconsulted").collect();
        for raw in str_array(e, "paths").chain(str_array(e, "changed_paths")) {
            let id = identity(raw);
            let state = (gone.contains(&raw), unconsulted.contains(&raw));
            match files.iter_mut().find(|(k, ..)| *k == id) {
                Some(entry) => {
                    entry.1 = raw.to_string();
                    (entry.2, entry.3) = state;
                }
                None => files.push((id, raw.to_string(), state.0, state.1)),
            }
        }
    }
    files
        .into_iter()
        .filter(|(_, _, gone, _)| !gone)
        .map(|(_, raw, _, unconsulted)| {
            if unconsulted {
                return serde_json::json!({ "path": raw, "items": null, "unconsulted": true });
            }
            // The exact URI `Registry::lsp_post_write` consulted for this
            // spelling (same language routing, same verbatim choice).
            let record = lsp::language_of(&raw).and_then(|language| {
                let uri = if lsp::prefer_verbatim_uri(language) {
                    lsp::uri_for_verbatim(workdir, &raw)
                } else {
                    lsp::uri_for(workdir, &raw)
                };
                details.get(&uri)
            });
            let items = record.map(|items| {
                items
                    .iter()
                    .map(|d| {
                        serde_json::json!({
                            "line": d.line,
                            "severity": d.severity,
                            "message": d.message,
                        })
                    })
                    .collect::<Vec<_>>()
            });
            serde_json::json!({ "path": raw, "items": items })
        })
        .collect()
}

/// One recorded diagnostic row read back from turn data.
struct LspRow<'a> {
    line: Option<u64>,
    severity: Option<u64>,
    message: &'a str,
}

impl LspRow<'_> {
    fn label(&self) -> &'static str {
        match self.severity {
            Some(1) => "error",
            Some(2) => "warning",
            Some(3) => "info",
            Some(4) => "hint",
            _ => "unspecified",
        }
    }

    /// Errors first, absent/out-of-range severity last (always 1..=5).
    fn rank(&self) -> u64 {
        self.severity.filter(|s| (1..=4).contains(s)).unwrap_or(5)
    }

    fn is_error(&self) -> bool {
        self.severity == Some(1)
    }

    fn line_text(&self) -> String {
        self.line
            .map(|l| l.to_string())
            .unwrap_or_else(|| "?".into())
    }
}

/// Rows of one snapshot, severity-ranked then by line (stable, so equal
/// keys keep the server's order — deterministic for a given record).
fn lsp_rows(items: &[serde_json::Value]) -> Vec<LspRow<'_>> {
    let mut rows: Vec<LspRow<'_>> = items
        .iter()
        .map(|i| LspRow {
            line: i.get("line").and_then(|l| l.as_u64()),
            severity: i.get("severity").and_then(|s| s.as_u64()),
            message: i.get("message").and_then(|m| m.as_str()).unwrap_or(""),
        })
        .collect();
    rows.sort_by_key(|r| (r.rank(), r.line.unwrap_or(u64::MAX)));
    rows
}

/// `2 ERRORS, 1 warning` style tally (errors uppercased so they read as
/// the blocking class; zero classes omitted; `clean` when empty).
fn lsp_tally<'r, 'd: 'r>(rows: impl IntoIterator<Item = &'r LspRow<'d>>) -> String {
    let mut counts = [0usize; 5];
    for r in rows {
        counts[(r.rank() - 1) as usize] += 1;
    }
    let names: [(&str, &str); 5] = [
        ("ERROR", "ERRORS"),
        ("warning", "warnings"),
        ("info", "info"),
        ("hint", "hints"),
        ("unspecified", "unspecified"),
    ];
    let parts: Vec<String> = counts
        .iter()
        .zip(names)
        .filter(|(n, _)| **n > 0)
        .map(|(n, (one, many))| format!("{n} {}", if *n == 1 { one } else { many }))
        .collect();
    if parts.is_empty() {
        "clean".into()
    } else {
        parts.join(", ")
    }
}

/// LSP lines for the evidence block: per mutated file, the
/// latest snapshot's tally plus its rows as `file:line: severity:
/// message` (single-line, bounded), errors first. Files with errors sort
/// first so caps never hide them behind clean files. Capped (10 files, 5
/// rows per file, 20 rows total) with omissions named. Empty when no
/// mutated file has a snapshot (LSP off, tool-free or read-only turns),
/// so those evidence blocks stay byte-identical to pre-#3.
fn render_lsp_evidence(data: &serde_json::Value) -> String {
    const MAX_FILES: usize = 10;
    const MAX_ROWS_PER_FILE: usize = 5;
    const MAX_ROWS_TOTAL: usize = 20;
    const MAX_UNSNAPSHOTTED: usize = 10;
    let Some(entries) = data.get("lsp_diagnostics").and_then(|v| v.as_array()) else {
        return String::new();
    };
    let mut files: Vec<(&str, Vec<LspRow<'_>>)> = Vec::new();
    let mut unsnapshotted: Vec<&str> = Vec::new();
    let mut unconsulted: Vec<&str> = Vec::new();
    for e in entries {
        let path = e.get("path").and_then(|p| p.as_str()).unwrap_or("?");
        match e.get("items").and_then(|i| i.as_array()) {
            Some(items) => files.push((path, lsp_rows(items))),
            None if e.get("unconsulted").is_some() => unconsulted.push(path),
            None => unsnapshotted.push(path),
        }
    }
    if files.is_empty() {
        return String::new();
    }
    let errors_in = |rows: &[LspRow<'_>]| rows.iter().filter(|r| r.is_error()).count();
    files.sort_by_key(|(_, rows)| std::cmp::Reverse(errors_in(rows)));
    let with_issues = files.iter().filter(|(_, rows)| !rows.is_empty()).count();
    let mut out = String::from(
        "- LSP diagnostics on files mutated this turn (snapshot at each file's last write): ",
    );
    if with_issues == 0 {
        out.push_str(&format!("none ({} files clean)\n", files.len()));
    } else {
        out.push_str(&format!(
            "{} in {with_issues} of {} files\n",
            lsp_tally(files.iter().flat_map(|(_, rows)| rows.iter())),
            files.len()
        ));
    }
    let mut budget = MAX_ROWS_TOTAL;
    for (path, rows) in files.iter().take(MAX_FILES) {
        let path = summarize_command(path);
        out.push_str(&format!("  - {path}: {}\n", lsp_tally(rows)));
        let shown = rows.len().min(MAX_ROWS_PER_FILE).min(budget);
        for r in rows.iter().take(shown) {
            out.push_str(&format!(
                "    - {path}:{}: {}: {}\n",
                r.line_text(),
                r.label(),
                summarize_command(r.message)
            ));
        }
        budget -= shown;
        if rows.len() > shown {
            out.push_str(&format!("    - ({} more omitted)\n", rows.len() - shown));
        }
    }
    if files.len() > MAX_FILES {
        out.push_str(&format!(
            "  ({} more files omitted)\n",
            files.len() - MAX_FILES
        ));
    }
    let names = |paths: &[&str]| {
        let mut names = paths
            .iter()
            .take(MAX_UNSNAPSHOTTED)
            .map(|p| summarize_command(p))
            .collect::<Vec<_>>()
            .join(", ");
        if paths.len() > MAX_UNSNAPSHOTTED {
            names.push_str(&format!(
                " ({} more omitted)",
                paths.len() - MAX_UNSNAPSHOTTED
            ));
        }
        names
    };
    if !unsnapshotted.is_empty() {
        out.push_str(&format!(
            "- LSP: no snapshot for {} mutated files (no server for the language, or its consult failed): {}\n",
            unsnapshotted.len(),
            names(&unsnapshotted)
        ));
    }
    if !unconsulted.is_empty() {
        out.push_str(&format!(
            "- LSP: not consulted for {} files commands changed (over the {}-file per-command consult cap): {}\n",
            unconsulted.len(),
            crate::command_changes::MAX_LSP_CONSULTS_PER_COMMAND,
            names(&unconsulted)
        ));
    }
    out
}

/// Opt-in LSP error gate findings: one deterministic P1 per
/// mutated file whose latest snapshot still carries error-severity
/// diagnostics (warnings and unspecified severities never gate). Text is
/// a pure function of the snapshot, so an unchanged error set repeats
/// verbatim (the survivor/stall escalations see it) and any progress
/// changes it. Empty when the turn recorded no snapshot.
pub fn lsp_error_findings(data: &serde_json::Value) -> Vec<polymorph_core::Finding> {
    const MAX_CITED: usize = 3;
    let Some(entries) = data.get("lsp_diagnostics").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let mut findings = Vec::new();
    for e in entries {
        let Some(items) = e.get("items").and_then(|i| i.as_array()) else {
            continue;
        };
        let errors: Vec<LspRow<'_>> = lsp_rows(items)
            .into_iter()
            .filter(|r| r.is_error())
            .collect();
        if errors.is_empty() {
            continue;
        }
        let path = e.get("path").and_then(|p| p.as_str()).unwrap_or("?");
        let mut cited = errors
            .iter()
            .take(MAX_CITED)
            .map(|r| format!("L{}: {}", r.line_text(), summarize_command(r.message)))
            .collect::<Vec<_>>()
            .join("; ");
        if errors.len() > MAX_CITED {
            cited.push_str(&format!("; (+{} more)", errors.len() - MAX_CITED));
        }
        findings.push(polymorph_core::Finding {
            severity: polymorph_core::Severity::P1,
            file: path.to_string(),
            line: errors[0].line.and_then(|l| u32::try_from(l).ok()),
            issue: format!(
                "LSP error gate (harness-observed): {} error-severity diagnostic{} remain at this file's last write: {cited}",
                errors.len(),
                if errors.len() == 1 { "" } else { "s" }
            ),
            proposed_fix: "Fix the reported errors in this file (and any caller/callee they implicate), then re-run the build/tests.".into(),
        });
    }
    findings
}

/// Shared state of an opt-in harness gate (LSP errors,
/// verify commands): a fixer-side decorator ([`LspGateRunner`],
/// [`VerifyGateRunner`]) records the last fix turn's deterministic veto
/// findings; [`HarnessGateReviewer`] reads them when a verdict passes.
#[derive(Debug, Default)]
pub struct HarnessGate {
    last: std::sync::Mutex<Vec<polymorph_core::Finding>>,
}

impl HarnessGate {
    /// Gate pre-loaded with the missing-evidence veto: while the declared
    /// verify commands have never run, `pending` carries one finding so a
    /// round-1 PASS on existing artifacts cannot stand without verification
    /// ever executing. The first fix turn's `record` call replaces the seed
    /// (an empty veto on success clears it). Empty `cmds` seeds nothing —
    /// drives without verify commands behave exactly as before.
    pub fn with_unrun_verify(cmds: &[String]) -> Self {
        let seed = if cmds.is_empty() {
            Vec::new()
        } else {
            vec![polymorph_core::Finding {
                severity: polymorph_core::Severity::P1,
                file: VERIFY_FINDING_FILE.to_string(),
                line: None,
                issue: format!(
                    "Verify gate (harness-observed): no verify evidence this drive: {} declared verify command(s) have never run; a passing verdict requires at least one verify run",
                    cmds.len()
                ),
                proposed_fix: "Run the declared verify commands (the fix turn does this automatically), then re-submit.".into(),
            }]
        };
        Self {
            last: std::sync::Mutex::new(seed),
        }
    }

    pub(crate) fn record(&self, findings: Vec<polymorph_core::Finding>) {
        if let Ok(mut last) = self.last.lock() {
            *last = findings;
        }
    }

    /// Veto findings of the most recent fix turn (empty before any turn,
    /// after a clean turn, and after a failed one).
    pub fn pending(&self) -> Vec<polymorph_core::Finding> {
        self.last.lock().map(|l| l.clone()).unwrap_or_default()
    }
}

/// Fixer-runner decorator for the LSP error gate: runs the inner turn
/// unchanged, then records its veto findings (a failed turn clears them —
/// the loop escalates on executor failure anyway).
pub struct LspGateRunner {
    pub inner: Arc<dyn polymorph_delegate::TurnRunner>,
    pub gate: Arc<HarnessGate>,
}

#[async_trait]
impl polymorph_delegate::TurnRunner for LspGateRunner {
    async fn run_turn(
        &self,
        ctx: polymorph_delegate::TurnContext,
    ) -> anyhow::Result<polymorph_delegate::TurnOutput> {
        let out = self.inner.run_turn(ctx).await;
        let veto = match &out {
            Ok(o) => o.data.as_ref().map(lsp_error_findings).unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        self.gate.record(veto);
        out
    }
}

/// `Finding.file` of verify-gate findings: not a path, so the
/// evidence block's file-existence check skips it.
pub const VERIFY_FINDING_FILE: &str = "(verify-cmd)";

/// Verify-output digest bounds (#9 caveat: no blind byte cap).
/// Per stream: the last [`VERIFY_TAIL_LINES`] lines, plus up to
/// [`VERIFY_MAX_DIAG_LINES`] diagnostic-looking lines from before the tail
/// (each with its next line, where `rustc`/`gcc` put the location), every
/// gap named with its line count. Lines are char-bounded, never cut
/// mid-char.
const VERIFY_TAIL_LINES: usize = 20;
const VERIFY_MAX_DIAG_LINES: usize = 30;
const VERIFY_LINE_CHARS: usize = 300;
/// Diagnostic lines cited in a verify-gate finding.
const VERIFY_CITED_LINES: usize = 3;

// Diagnostic-line heuristic of the verify digest: the one the `bash`
// spill digest uses too (shared it; rendering unchanged).
use polymorph_tools::output::is_diagnostic_line;

/// One output line made safe for the evidence block (tabs to spaces,
/// control characters dropped, char-bounded with `...`; shared with the
/// spill digest).
fn verify_line(line: &str) -> String {
    polymorph_tools::output::clean_line(line, VERIFY_LINE_CHARS)
}

/// Bounded digest of one output stream: `{"total", "lines"}` where
/// `lines` holds the kept lines in order with `... (N lines omitted) ...`
/// markers for every gap. Pure, for unit tests.
fn verify_digest(text: &str) -> serde_json::Value {
    let lines: Vec<&str> = text.lines().collect();
    let tail_start = lines.len().saturating_sub(VERIFY_TAIL_LINES);
    let mut keep = std::collections::BTreeSet::new();
    let mut diags = 0;
    for (i, line) in lines[..tail_start].iter().enumerate() {
        if diags < VERIFY_MAX_DIAG_LINES && is_diagnostic_line(line) {
            diags += 1;
            keep.insert(i);
            if i + 1 < tail_start {
                keep.insert(i + 1);
            }
        }
    }
    keep.extend(tail_start..lines.len());
    let mut out = Vec::with_capacity(keep.len() + 4);
    let mut next = 0;
    for i in keep {
        if i > next {
            out.push(format!("... ({} lines omitted) ...", i - next));
        }
        out.push(verify_line(lines[i]));
        next = i + 1;
    }
    serde_json::json!({ "total": lines.len(), "lines": out })
}

/// Lines a verify-gate finding cites: the first diagnostic-looking lines
/// (stderr before stdout: compilers report there), else the last non-blank
/// ones (stderr first again), single-line bounded. Pure, for unit tests.
fn verify_key_lines(stdout: &str, stderr: &str) -> Vec<String> {
    let diag: Vec<String> = stderr
        .lines()
        .chain(stdout.lines())
        .filter(|l| is_diagnostic_line(l))
        .take(VERIFY_CITED_LINES)
        .map(summarize_command)
        .collect();
    if !diag.is_empty() {
        return diag;
    }
    let last = if stderr.trim().is_empty() {
        stdout
    } else {
        stderr
    };
    let mut tail: Vec<String> = last
        .lines()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .take(VERIFY_CITED_LINES)
        .map(summarize_command)
        .collect();
    tail.reverse();
    tail
}

/// Run the operator's verify commands in order in `workdir`,
/// stopping at the first one that does not exit 0 (later ones are recorded
/// `skipped`: a test run after a failed build only repeats its errors).
/// One JSON record per command. `None` = cancelled mid-command (the child
/// tree is already killed). Direct `polymorph_tools::exec` calls: operator
/// commands never pass the fixer's tool Gate.
async fn run_verify_cmds(
    cmds: &[String],
    workdir: &Path,
    timeout: std::time::Duration,
    cancel: &tokio_util::sync::CancellationToken,
) -> Option<Vec<serde_json::Value>> {
    use polymorph_tools::exec::{run_bash_in, ExecError};
    let mut runs = Vec::with_capacity(cmds.len());
    let mut failed = false;
    for cmd in cmds {
        if failed {
            runs.push(serde_json::json!({ "cmd": cmd, "status": "skipped" }));
            continue;
        }
        let started = std::time::Instant::now();
        let res = run_bash_in(cmd, timeout, cancel, &[], workdir).await;
        let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let run = match res {
            Ok(r) => serde_json::json!({
                "cmd": cmd,
                "status": if r.exit_code == 0 { "passed" } else { "failed" },
                "exit_code": r.exit_code,
                "duration_ms": duration_ms,
                "stdout": verify_digest(&r.stdout),
                "stderr": verify_digest(&r.stderr),
                "key": verify_key_lines(&r.stdout, &r.stderr),
            }),
            Err(ExecError::Interrupted) => return None,
            Err(ExecError::TimedOut(t)) => serde_json::json!({
                "cmd": cmd,
                "status": "timed_out",
                "duration_ms": duration_ms,
                "timeout_secs": t.as_secs(),
            }),
            Err(e) => serde_json::json!({
                "cmd": cmd,
                "status": "error",
                "duration_ms": duration_ms,
                "error": e.to_string(),
            }),
        };
        failed = run.get("status").and_then(|s| s.as_str()) != Some("passed");
        runs.push(run);
    }
    Some(runs)
}

/// One-line status of a verify record (shared by evidence and findings).
fn verify_status(run: &serde_json::Value) -> String {
    match run.get("status").and_then(|s| s.as_str()).unwrap_or("?") {
        "passed" => "PASSED exit 0".to_string(),
        "failed" => format!(
            "FAILED exit {}",
            run.get("exit_code").and_then(|c| c.as_i64()).unwrap_or(-1)
        ),
        "timed_out" => format!(
            "TIMED OUT after {}s (process tree killed, no output captured)",
            run.get("timeout_secs")
                .and_then(|t| t.as_u64())
                .unwrap_or(0)
        ),
        "skipped" => "SKIPPED (an earlier verify command failed)".to_string(),
        "error" => format!(
            "COULD NOT RUN ({})",
            summarize_command(run.get("error").and_then(|e| e.as_str()).unwrap_or("?"))
        ),
        other => format!("{other}?"),
    }
}

/// Verify-gate findings: one deterministic P1 for the failing
/// command (runs stop at the first failure), naming the command, its exit
/// status and the key output lines — no durations, so an unchanged failure
/// repeats verbatim (survivor/stall escalations see it). Empty when every
/// command passed or none ran.
pub fn verify_findings(runs: &[serde_json::Value]) -> Vec<polymorph_core::Finding> {
    runs.iter()
        .filter(|r| {
            !matches!(
                r.get("status").and_then(|s| s.as_str()),
                Some("passed" | "skipped")
            )
        })
        .map(|r| {
            let cmd = summarize_command(r.get("cmd").and_then(|c| c.as_str()).unwrap_or("?"));
            let key: Vec<&str> = r
                .get("key")
                .and_then(|k| k.as_array())
                .map(|k| k.iter().filter_map(|l| l.as_str()).collect())
                .unwrap_or_default();
            let cited = if key.is_empty() {
                "no output lines to cite".to_string()
            } else {
                key.join("; ")
            };
            polymorph_core::Finding {
                severity: polymorph_core::Severity::P1,
                file: VERIFY_FINDING_FILE.to_string(),
                line: None,
                issue: format!(
                    "Verify gate (harness-observed): `{cmd}` {} after the fix turn: {cited}",
                    verify_status(r)
                ),
                proposed_fix: format!(
                    "Make `{cmd}` exit 0: fix what it reports (output digest in HARNESS EVIDENCE), then re-run it after your last edit."
                ),
            }
        })
        .collect()
}

/// Verify-gate block for the evidence: every command with its
/// status and duration, each non-empty stream's digest, and the gate
/// result. Empty when the turn carries no verify records (gate off renders
/// exactly as before). The commands ran after the turn's last write, so
/// they are post-turn ground truth over the in-turn ledger and LSP
/// snapshots above.
fn render_verify_evidence(data: &serde_json::Value) -> String {
    let Some(runs) = data.get("verify").and_then(|v| v.as_array()) else {
        return String::new();
    };
    let mut out = String::from(
        "- VERIFY GATE (operator commands, run by the harness after the fix turn ended; post-turn ground truth):\n",
    );
    for (i, run) in runs.iter().enumerate() {
        let cmd = summarize_command(run.get("cmd").and_then(|c| c.as_str()).unwrap_or("?"));
        let took = match run.get("duration_ms").and_then(|d| d.as_u64()) {
            Some(ms) => format!(" in {:.1}s", ms as f64 / 1000.0),
            None => String::new(),
        };
        out.push_str(&format!(
            "  #{} {}{took}: {cmd}\n",
            i + 1,
            verify_status(run)
        ));
        for stream in ["stderr", "stdout"] {
            let Some(digest) = run.get(stream) else {
                continue;
            };
            let total = digest.get("total").and_then(|t| t.as_u64()).unwrap_or(0);
            let Some(lines) = digest.get("lines").and_then(|l| l.as_array()) else {
                continue;
            };
            if total == 0 {
                continue;
            }
            out.push_str(&format!("    {stream} ({total} lines):\n"));
            for line in lines.iter().filter_map(|l| l.as_str()) {
                out.push_str(&format!("      | {line}\n"));
            }
        }
    }
    let all_passed = runs
        .iter()
        .all(|r| r.get("status").and_then(|s| s.as_str()) == Some("passed"));
    out.push_str(if all_passed {
        "- VERIFY RESULT: PASSED (every verify command exited 0)\n"
    } else {
        "- VERIFY RESULT: FAILED (the harness blocks PASS until every verify command exits 0)\n"
    });
    out
}

/// Fixer-runner decorator for the verify-command gate: runs
/// the inner turn unchanged; when it finishes (`done`), runs the operator's
/// commands in the turn's workdir (the worktree in `--repo` mode), records
/// their veto findings, and attaches the records as `data.verify` for the
/// evidence block. A failed inner turn clears the gate and skips the
/// commands (the loop escalates on executor failure anyway); a cancel
/// mid-command kills the tree and fails the turn — never a partial pass.
pub struct VerifyGateRunner {
    pub inner: Arc<dyn polymorph_delegate::TurnRunner>,
    pub gate: Arc<HarnessGate>,
    /// Operator commands, in run order (`--verify-cmd` / `[agent]
    /// verify_cmds`). Not subject to the fixer's tool Gate.
    pub cmds: Vec<String>,
    /// Per-command timeout (the whole process tree is killed on expiry).
    pub timeout: std::time::Duration,
}

#[async_trait]
impl polymorph_delegate::TurnRunner for VerifyGateRunner {
    async fn run_turn(
        &self,
        ctx: polymorph_delegate::TurnContext,
    ) -> anyhow::Result<polymorph_delegate::TurnOutput> {
        let workdir = ctx.workdir.clone();
        let cancel = ctx.cancel.clone();
        let mut out = match self.inner.run_turn(ctx).await {
            Ok(out) => out,
            Err(e) => {
                self.gate.record(Vec::new());
                return Err(e);
            }
        };
        if !out.done {
            return Ok(out);
        }
        let Some(runs) = run_verify_cmds(&self.cmds, &workdir, self.timeout, &cancel).await else {
            self.gate.record(Vec::new());
            anyhow::bail!("verify gate cancelled mid-command (process tree killed)");
        };
        let veto = verify_findings(&runs);
        if !veto.is_empty() {
            tracing::warn!(
                commands = runs.len(),
                "verify gate: a verify command failed after the fix turn"
            );
        }
        self.gate.record(veto);
        if let Some(data) = out.data.as_mut().and_then(|d| d.as_object_mut()) {
            data.insert("verify".to_string(), serde_json::Value::Array(runs));
        }
        Ok(out)
    }
}

/// Reviewer decorator for the opt-in harness gates (LSP errors,
/// verify commands): a passing verdict while any gate holds
/// findings from the last fix turn becomes a FAIL carrying those
/// deterministic findings first (gates in `gates` order, the reviewer's own
/// findings after), so `run_loop` feeds them to the fixer like any
/// finding. Fail verdicts and errors pass through untouched (the fixer
/// still sees every gate's evidence in the next brief); empty `gates` is a
/// pure passthrough.
pub struct HarnessGateReviewer<R> {
    pub inner: R,
    pub gates: Vec<Arc<HarnessGate>>,
}

#[async_trait]
impl<R: Reviewer> Reviewer for HarnessGateReviewer<R> {
    async fn review(&self, input: ReviewerInput) -> Result<Verdict, ReviewerError> {
        let verdict = self.inner.review(input).await?;
        if verdict.verdict != polymorph_core::VerdictKind::Pass {
            return Ok(verdict);
        }
        let mut findings: Vec<polymorph_core::Finding> =
            self.gates.iter().flat_map(|g| g.pending()).collect();
        if findings.is_empty() {
            return Ok(verdict);
        }
        tracing::warn!(
            findings = findings.len(),
            "harness gate vetoed a passing verdict"
        );
        findings.extend(verdict.findings);
        Ok(Verdict {
            verdict: polymorph_core::VerdictKind::Fail,
            findings,
            confidence: verdict.confidence,
        })
    }
}

/// `Reviewer` over Bus children: one fresh spawn + join per call.
/// The child sees ONLY the spawned brief (goal + criteria + CURRENT
/// artifact) — reviewer independence, structurally enforced.
pub struct BusReviewer {
    pub bus: Arc<polymorph_delegate::Bus>,
    pub runner: Arc<dyn polymorph_delegate::TurnRunner>,
    pub workdir: polymorph_delegate::WorkdirSpec,
    /// Read-only source path of the artifact (live run #5: without it the
    /// model guessed filenames). Informational only — the loop passes artifact
    /// TEXT, never writes back to this path in F2b.
    pub artifact_path: Option<PathBuf>,
}

#[async_trait]
impl Reviewer for BusReviewer {
    async fn review(&self, input: ReviewerInput) -> Result<Verdict, ReviewerError> {
        let invalid = |d: String| ReviewerError::Invalid(d);
        let mut brief = render_review_brief(&input, self.artifact_path.as_deref());
        // CREATE-job annotation: missing file + empty text.
        // Pure addition — fires only in the gap, never rewrites the brief.
        if let Some(p) = self.artifact_path.as_deref() {
            if let Some(note) = create_job_note(&self.workdir, p, &input.artifact) {
                brief.push_str(&note);
            }
        }
        // Forensics: the rolling file log (DEBUG floor for our targets) is
        // the only place a drive's exact reviewer brief survives — no
        // session row is written. Local-only; stderr stays at warn.
        tracing::debug!(brief = %brief, "review brief");
        let mut spec =
            polymorph_delegate::SpawnSpec::new(Role::Review, brief, self.workdir.clone());
        // F2a: no tool calls (owns the registry). The opt-in
        // tool reviewer enforces its own read-only allowlist in its runner.
        spec.tools = Vec::new();
        let handle = self
            .bus
            .spawn(spec, self.runner.clone(), None)
            .await
            .map_err(|e| invalid(format!("review spawn failed: {e}")))?;
        let outcome = handle.join().await;
        match outcome.result.status {
            polymorph_delegate::FinishStatus::Completed => {
                let text = outcome
                    .result
                    .data
                    .get("verdict_text")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| invalid("review child returned no verdict_text".to_string()))?;
                extract_verdict(text).map_err(|e| {
                    // Log a truncated preview: otherwise the operator can never
                    // tell fenced-JSON from pure prose (live run #2 hit exactly
                    // this). Truncated + local-only; the full text stays in the
                    // child's typed result, never in an event field.
                    let preview: String = text.chars().take(300).collect();
                    tracing::warn!(preview = %preview, error = %e, "reviewer verdict failed to parse");
                    invalid(e.to_string())
                })
            }
            polymorph_delegate::FinishStatus::Stopped { reason } => {
                Err(invalid(format!("review child stopped: {reason:?}")))
            }
            polymorph_delegate::FinishStatus::Failed { error } => {
                Err(invalid(format!("review child failed: {error}")))
            }
        }
    }
}

/// Verdict text → typed verdict, tolerating a preamble or fences around
/// the JSON object (shared by [`BusReviewer`] and the tool
/// reviewer's cite-or-drop check, so both judge the same text alike).
pub(crate) fn extract_verdict(text: &str) -> Result<Verdict, crate::VerdictError> {
    match parse_verdict(text) {
        Ok(v) => Ok(v),
        Err(orig_err) => {
            // Resilient extraction for real-world LLMs (e.g. OpenCode Go / Muse Spark
            // emitting leading status banners or markdown code fences ```json ... ```):
            if let (Some(start), Some(end)) = (text.find('{'), text.rfind('}')) {
                if start < end {
                    match parse_verdict(&text[start..=end]) {
                        Ok(v) => {
                            tracing::info!("extracted valid verdict JSON from reviewer text with preamble/fences");
                            Ok(v)
                        }
                        Err(_) => Err(orig_err),
                    }
                } else {
                    Err(orig_err)
                }
            } else {
                Err(orig_err)
            }
        }
    }
}

/// Tool preamble prepended to the executor system prompt when tools are on
/// . Runtime text, NOT a pack file: packs are static, the flag is
/// dynamic. Documents the six tools + the read→tag→edit flow the runner
/// enforces. F5 followup: migrate into evaluated packs.
pub const TOOLS_PREAMBLE: &str =
    "You have tools — use them to inspect the work before rewriting.\n\
Available: read (files; results carry a `tag`), write, edit (hashline patches ONLY, \
with the tag you were given as `[PATH#TAG]`), grep, glob, bash.\n\
Tool results arrive as follow-up messages; denials and errors are recoverable — \
adjust and continue. Prefer reading over guessing; never invent file content.\n\
Edit patch rules (strict — a malformed patch is rejected whole, nothing applies):\n\
raw patch text ONLY — never fences, never prose around the patch;\n\
one section per file as `[PATH#TAG]` (TAG = 4 lowercase hex from your latest read);\n\
`PUT 12:` or `PUT 12.=15:` or `PUT <N:` or `PUT >N:` takes `+`-sigil body lines (`+` alone = empty line);\n\
`CUT N`, `REM`, `MV dest` take no body; line numbers are 1-based in the file AS READ.\n\
Example:\n\
[src/main.rs#a1b2]\n\
PUT 2:\n\
+fixed line\n\
On `StaleTag` re-read and re-emit; on parse errors fix the named line.\n\
When the artifact is ready, reply with the full rewritten text and no further tool calls.\n\n";

/// Minimal preamble for the preamble ablation: tool intro (so the model
/// still engages tools — without it the run would measure tools-off, not
/// preamble) + the raw-only fence ban (the hypothesized parse carrier).
/// Everything else from [`TOOLS_PREAMBLE`] — header/op/sigil shapes, the
/// example, the StaleTag action — is dropped. If parse holds at zero under
/// this, the dropped lines are dead weight; if parse returns, they carry
/// the effect. Pinned by test (presence + absence), never grown silently.
pub const TOOLS_PREAMBLE_MIN: &str =
    "You have tools — use them to inspect the work before rewriting.\n\
Available: read (files; results carry a `tag`), write, edit (hashline patches ONLY, \
with the tag you were given as `[PATH#TAG]`), grep, glob, bash.\n\
Tool results arrive as follow-up messages; denials and errors are recoverable — \
adjust and continue. Prefer reading over guessing; never invent file content.\n\
Edit patches must be raw patch text ONLY — never fences, never prose around the patch.\n\
When the artifact is ready, reply with the full rewritten text and no further tool calls.\n\n";

/// Select the fixer preamble: pure fn, unit-tested — call sites
/// pass bools, never sniff strings (spirit).
pub fn select_preamble(minimal: bool) -> &'static str {
    if minimal {
        TOOLS_PREAMBLE_MIN
    } else {
        TOOLS_PREAMBLE
    }
}

/// Max tool calls accepted from a single assistant turn (hardening: history
/// growth stays bounded by rounds × this; over-cap calls count as dropped).
pub const MAX_CALLS_PER_TURN: usize = 8;

/// Build genai tool definitions for the allowlisted names (descriptions +
/// schemas from the registry). Unknown names are skipped here; enforcement
/// happens per call in the feed loop (recoverable result, never bail).
pub fn to_genai_tools(
    registry: &polymorph_tools::Registry,
    allow: &[String],
) -> Vec<genai::chat::Tool> {
    allow
        .iter()
        .filter_map(|name| {
            let def = registry.get(name)?;
            let schema = registry.schema_of(name)?;
            Some(genai::chat::Tool {
                name: genai::chat::ToolName::Custom(name.clone()),
                description: Some(def.description.to_string()),
                schema: Some(schema),
                strict: None,
                config: None,
            })
        })
        .collect()
}

/// One agentic LLM turn: message history + tools in, collected turn out.
/// The F1-style seam for the feed loop: production runs the live client,
/// tests inject scripted turns (deterministic, no network).
#[async_trait]
pub trait AgenticTurnFn: Send + Sync {
    async fn turn(
        &self,
        messages: Vec<genai::chat::ChatMessage>,
        tools: Vec<genai::chat::Tool>,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<polymorph_llm::CollectedTurn>;
}

/// Rule retries per turn: a hit re-issues the same
/// messages with the reminder injected, then hands up honestly.
pub const DEFAULT_RULE_RETRIES: u32 = 2;

/// Pure retry policy: how reminders enter system +
/// when the budget is spent. Unit-tested; the live loop below applies it.
pub struct RuleRetryPolicy {
    pub max_retries: u32,
}

impl RuleRetryPolicy {
    /// Append the fired rule as a system reminder (matched text capped —
    /// same caps discipline as diagnostics; patterns can match long spans).
    pub fn inject(system: &str, hit: &polymorph_llm::rules::RuleHit) -> String {
        const MAX_MATCH_ECHO: usize = 200;
        let mut matched = hit.matched.as_str();
        if matched.len() > MAX_MATCH_ECHO {
            matched = matched.split_at(MAX_MATCH_ECHO).0;
        }
        format!(
            "{system}\n\n[Stream rule '{}' fired mid-stream on: {matched:?}]\n{}",
            hit.rule_id, hit.reminder
        )
    }

    /// Budget left after `used` retries.
    pub fn should_retry(&self, used: u32) -> bool {
        used < self.max_retries
    }
}

/// Live [`AgenticTurnFn`]: managed-or-fixed turn with the runner's system.
/// Mirrors [`single_turn`] (which stays untouched for the text-only paths).
/// `rules` ride to the stream fold; empty = pre-R behavior exactly.
pub struct LiveAgenticTurn {
    pub client: polymorph_llm::LlmClient,
    pub target: LlmTarget,
    pub system: String,
    pub rules: Vec<polymorph_llm::rules::CompiledRule>,
    pub rule_max_retries: u32,
    /// Parent-model pin (see `ReviewTurnRunner.pinned`).
    pub pinned: std::sync::Mutex<Option<String>>,
}

#[async_trait]
impl AgenticTurnFn for LiveAgenticTurn {
    async fn turn(
        &self,
        messages: Vec<genai::chat::ChatMessage>,
        tools: Vec<genai::chat::Tool>,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<polymorph_llm::CollectedTurn> {
        // R retry: a rule hit re-issues the SAME messages
        // with the reminder injected into system. Spent budget (or a
        // clean turn) hands up with the hit intact — the reviewer sees
        // misfire + reminder trail, never an infinite loop.
        let policy = RuleRetryPolicy {
            max_retries: self.rule_max_retries,
        };
        let mut system = self.system.clone();
        let mut used = 0u32;
        // Parent-model pin resolves once per turn (142 directive); the
        // retry loop re-issues under the same pin (same parent).
        let target = resolve_target(&self.pinned, &self.target);
        let mut served: Option<String> = None;
        loop {
            let mut sink = String::new();
            let turn = match &target {
                LlmTarget::Managed {
                    router,
                    providers,
                    state,
                    role,
                    scope,
                } => {
                    let mut state = state.lock().await;
                    let (turn, model) = self
                        .client
                        .stream_chat_managed_messages(
                            router,
                            providers,
                            &mut state,
                            *role,
                            scope.as_deref(),
                            &system,
                            &messages,
                            &tools,
                            cancel,
                            &mut sink,
                            &self.rules,
                        )
                        .await?;
                    served = Some(model);
                    turn
                }
                LlmTarget::Fixed(spec) => {
                    if let LlmTarget::Managed {
                        providers, state, ..
                    } = &self.target
                    {
                        let model_name = match spec {
                            genai::ModelSpec::Name(ref n) => Some(n.as_str()),
                            genai::ModelSpec::Target(ref t) => Some(t.model.model_name.as_str()),
                            genai::ModelSpec::Iden(ref i) => Some(i.model_name.as_str()),
                        };
                        if let Some(m) = model_name {
                            let mut state = state.lock().await;
                            served = Some(m.to_string());
                            self.client
                                .stream_chat_messages_single(
                                    providers,
                                    &mut state,
                                    m,
                                    &system,
                                    &messages,
                                    &tools,
                                    cancel,
                                    &mut sink,
                                    &self.rules,
                                )
                                .await
                                .map_err(|e| anyhow::anyhow!("{e}"))?
                        } else {
                            self.client
                                .stream_chat_messages_spec(
                                    spec.clone(),
                                    &system,
                                    messages.clone(),
                                    tools.clone(),
                                    cancel,
                                    &mut sink,
                                    &self.rules,
                                )
                                .await
                                .map_err(|e| anyhow::anyhow!("{e}"))?
                        }
                    } else {
                        self.client
                            .stream_chat_messages_spec(
                                spec.clone(),
                                &system,
                                messages.clone(),
                                tools.clone(),
                                cancel,
                                &mut sink,
                                &self.rules,
                            )
                            .await
                            .map_err(|e| anyhow::anyhow!("{e}"))?
                    }
                }
            };
            match &turn.rule_hit {
                Some(hit) if policy.should_retry(used) => {
                    // Permanent attempt telemetry: the JSON
                    // rolling logs are the retry forensics path.
                    tracing::info!(attempt = used + 1, rule = %hit.rule_id, "stream rule fired, retrying");
                    system = RuleRetryPolicy::inject(&system, hit);
                    used += 1;
                }
                _ => {
                    record_pin(&self.pinned, served.take());
                    return Ok(turn);
                }
            }
        }
    }
}

/// Watching-reviewer note severity: advisory only —
/// nothing here ever gates, denies, or rewrites (observe-first, 98/99).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AdvisorSeverity {
    Aside,
    Concern,
    Blocker,
}

/// One watching-reviewer note.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AdvisorNote {
    pub severity: AdvisorSeverity,
    pub text: String,
}

/// Watching-reviewer system: short const (not a pack —
/// eval-gated pack only if the signal warrants, stated).
const ADVISOR_SYSTEM: &str = "You are a watching reviewer reading one agent turn from a clean context. Reply with zero or more single-line notes, each starting with exactly one of ASIDE:, CONCERN:, BLOCKER:. Asides are observations, concerns want a second look, blockers flag likely-wrong outcomes. No other prose.";

/// Cap on the final text quoted into the advisor brief (caps precedent).
const MAX_ADVISED_TEXT_CHARS: usize = 4000;

/// Lenient note parser: prefixed lines keep their
/// severity; any other non-empty line degrades to Aside (signal never
/// dropped for formatting); empties vanish.
pub fn parse_notes(body: &str) -> Vec<AdvisorNote> {
    // Literal prefix lengths (not the match's): slicing the original by
    // the UPPERCASED match length mis-strips the bare-prefix degenerate
    // (`ASIDE:` alone sliced zero chars — caught by unit test).
    // ASCII-only prefixes keep byte boundaries exact.
    const PREFIXES: [(&str, AdvisorSeverity); 3] = [
        ("ASIDE:", AdvisorSeverity::Aside),
        ("CONCERN:", AdvisorSeverity::Concern),
        ("BLOCKER:", AdvisorSeverity::Blocker),
    ];
    let mut notes = Vec::new();
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let upper = line.to_ascii_uppercase();
        let mut found = None;
        for (prefix, severity) in PREFIXES {
            if upper.starts_with(prefix) {
                found = Some((severity, line[prefix.len()..].trim()));
                break;
            }
        }
        let (severity, rest) = found.unwrap_or((AdvisorSeverity::Aside, line));
        if rest.is_empty() {
            continue;
        }
        notes.push(AdvisorNote {
            severity,
            text: rest.to_string(),
        });
    }
    notes
}

/// Watching reviewer: a second model reading one turn
/// from its own clean context (brief + compact round trace + capped final
/// text — never the conversation, prompt-stability invariant intact).
/// Failures are the caller's to absorb (warn + empty, 99 posture).
pub struct AdvisorRunner {
    pub client: polymorph_llm::LlmClient,
    pub target: LlmTarget,
}

impl AdvisorRunner {
    /// Production constructor: advisor-role managed target (role-routed
    /// chains machinery — no new routing).
    pub fn managed(
        router: polymorph_llm::RoleRouter,
        providers: Vec<polymorph_core::config::ProviderEntry>,
        state: std::sync::Arc<tokio::sync::Mutex<polymorph_llm::FallbackState>>,
    ) -> Self {
        Self {
            client: polymorph_llm::LlmClient::with_fresh_conversation(),
            target: LlmTarget::Managed {
                router,
                providers,
                state,
                role: polymorph_core::Role::Advisor,
                scope: None,
            },
        }
    }

    /// One watching turn over a clean-context brief. Pure prompt render
    /// + parse (the model call is the only I/O).
    pub async fn advise(
        &self,
        brief: &str,
        outcomes: &[serde_json::Value],
        final_text: &str,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<Vec<AdvisorNote>> {
        let trace: Vec<String> = outcomes
            .iter()
            .map(|o| {
                format!(
                    "{} {}",
                    o.get("tool").and_then(|v| v.as_str()).unwrap_or("?"),
                    o.get("outcome").and_then(|v| v.as_str()).unwrap_or("?")
                )
            })
            .collect();
        let mut quoted = final_text;
        if quoted.len() > MAX_ADVISED_TEXT_CHARS {
            quoted = quoted.split_at(MAX_ADVISED_TEXT_CHARS).0;
        }
        let brief = format!(
            "Task: {brief}\nTurn trace ({} rounds): {}\nFinal text:\n{quoted}",
            trace.len(),
            trace.join(", ")
        );
        // Advisor stays role-canonical (unpinned by design — it observes
        // turns, it descends from none; 142 directive covers descendants).
        let (body, _) = single_turn(
            &self.client,
            &self.target,
            &self.target,
            ADVISOR_SYSTEM,
            &brief,
            cancel,
        )
        .await?;
        Ok(parse_notes(&body))
    }
}

/// Tools-wielding fixer: drives model→tools→results rounds until
/// the model answers with text (no calls) or `max_tool_rounds` feeds run out.
/// Denials, unknown tools, and registry errors feed back as tool RESULT text
/// (recoverable — the model adjusts); only transport/auth failures bail (the
/// loop maps those to `ExecutorFailed`). Final text — possibly empty — goes
/// to `data.artifact` for the REVIEWER to judge through normal paths; no new
/// escalate kind, no special bail.
pub struct AgenticFixRunner {
    pub turn: Arc<dyn AgenticTurnFn>,
    pub registry: Arc<polymorph_tools::Registry>,
    pub gate: polymorph_tools::Gate,
    pub tools: Vec<String>,
    pub max_tool_rounds: u32,
    /// Verification bonus rounds past `max_tool_rounds` (chunk
    /// 154 granted exactly one): granted one at a time, only while the
    /// previous round made progress — a successful write/edit, or a
    /// command that ran after a not-yet-verified mutation. Bounded, so the
    /// turn always terminates. 0 = none (the separate one-shot
    /// pending-mutation bonus still applies).
    pub max_bonus_tool_rounds: u32,
    /// Watching reviewer: `None` = pre-S behavior
    /// exactly (off unless the project opts in — advisor doubles spend).
    pub advisor: Option<AdvisorRunner>,
    /// Drive restore ledger: `Some` records every write/edit
    /// target's original before the call and the drive's state after it,
    /// so a failed in-place drive can roll back. `None` = no recording.
    pub restore: Option<Arc<crate::restore::RestoreLedger>>,
    /// In-turn tool-result history bound.
    pub history: ToolHistoryBudget,
    /// Command change detection ([`crate::command_changes`]): the workdir is
    /// stamped (at most this many files) right before and after every
    /// `bash`/`exec` call, and what the command created, modified or
    /// deleted joins the turn's mutations (ledger `changed_paths`), the
    /// LSP consult and the restore ledger. `0` = off (pre-detection
    /// behavior exactly).
    pub command_changes: usize,
}

/// Default [`AgenticFixRunner::max_bonus_tool_rounds`] (`[agent]
/// max_bonus_tool_rounds`; the CLI overrides it from config).
pub const DEFAULT_MAX_BONUS_TOOL_ROUNDS: u32 = 3;

/// Default [`ToolHistoryBudget::budget_bytes`] (`[agent]
/// tool_history_budget_bytes`).
pub const DEFAULT_TOOL_HISTORY_BUDGET_BYTES: usize = 256 * 1024;

/// Default [`ToolHistoryBudget::keep_rounds`] (`[agent]
/// tool_history_keep_rounds`).
pub const DEFAULT_TOOL_HISTORY_KEEP_ROUNDS: u32 = 2;

/// Tool results smaller than this are never compacted (a stub would save
/// next to nothing).
const MIN_COMPACT_BYTES: usize = 1024;

/// Bound on the tool results a fix turn re-sends every round.
/// Once their total exceeds `budget_bytes`, the oldest results outside the
/// latest `keep_rounds` tool rounds are replaced, oldest first and only
/// until the total fits again, by a one-paragraph stub naming the tool,
/// round, outcome, exit code and where the full data is: the result body
/// saved under `.polymorph/tool-output/`, plus any spill files; for `read`
/// the file itself (re-readable) and its hashline tag. `budget_bytes == 0`
/// = off (pre-#9 behavior). `keep_rounds` counts as at least 1: the
/// latest round's results always reach the model intact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolHistoryBudget {
    pub budget_bytes: usize,
    pub keep_rounds: u32,
}

impl Default for ToolHistoryBudget {
    fn default() -> Self {
        Self {
            budget_bytes: DEFAULT_TOOL_HISTORY_BUDGET_BYTES,
            keep_rounds: DEFAULT_TOOL_HISTORY_KEEP_ROUNDS,
        }
    }
}

impl ToolHistoryBudget {
    /// Never compact (bench apparatus stability).
    pub fn off() -> Self {
        Self {
            budget_bytes: 0,
            ..Self::default()
        }
    }
}

/// One tool result fed into a fix turn's history (compaction).
pub(crate) struct FedResult {
    /// Index of its message in the turn's `messages`.
    pub(crate) index: usize,
    pub(crate) round: u32,
    pub(crate) call_id: String,
    pub(crate) tool: String,
    pub(crate) outcome: &'static str,
    pub(crate) exit_code: Option<i64>,
    /// `read`'s `path` argument (the stub points back at the file).
    pub(crate) read_path: Option<String>,
    /// The fed text; emptied once compacted.
    pub(crate) body: String,
    /// Length of what is in `messages` now (body, or the stub).
    pub(crate) sent: usize,
    pub(crate) compacted: bool,
}

/// The stub that replaces a compacted result. Everything but a
/// successful `read` (the file is re-readable) is saved first, so nothing
/// is lost.
fn compaction_stub(f: &FedResult, workdir: &Path, keep_files: usize) -> String {
    let parsed: Option<serde_json::Value> = serde_json::from_str(&f.body).ok();
    let field = |key: &str| parsed.as_ref().and_then(|v| v.get(key));
    let mut stub = format!(
        "[harness: older tool result compacted to bound the conversation (tool-history budget): `{}` from tool round {}, outcome {}",
        f.tool, f.round, f.outcome
    );
    if let Some(code) = f.exit_code {
        stub.push_str(&format!(", exit_code {code}"));
    }
    stub.push_str(&format!(", {} bytes.", f.body.len()));
    if let (Some(path), "ok") = (&f.read_path, f.outcome) {
        let num = |key: &str| field(key).and_then(|v| v.as_u64()).unwrap_or(0);
        let shown = field("lines")
            .and_then(|l| l.as_array())
            .map_or(0, |l| l.len() as u64);
        let start = num("start_line");
        stub.push_str(&format!(
            " It showed `{path}` lines {start}-{} of {}; the file is on disk: `read` it again (offset/limit) for its current content.",
            (start + shown).saturating_sub(1),
            num("total_lines")
        ));
        if let Some(tag) = field("tag").and_then(|t| t.as_str()) {
            stub.push_str(&format!(
                " Its tag was {tag} (valid while the file is unchanged)."
            ));
        }
        stub.push(']');
        return stub;
    }
    match polymorph_tools::output::save_text(workdir, "result.txt", &f.body, keep_files) {
        Ok(saved) => stub.push_str(&format!(" Full result saved at {saved}")),
        Err(e) => stub.push_str(&format!(
            " The full result could NOT be saved ({e}); re-run the call if you need it"
        )),
    }
    for stream in ["stdout", "stderr"] {
        if let Some(path) = field(&format!("{stream}_spill"))
            .and_then(|s| s.get("path"))
            .and_then(|p| p.as_str())
        {
            stub.push_str(&format!("; full {stream} at {path}"));
        }
    }
    stub.push_str(" (`read` with offset/limit, or `grep`, if you still need it).]");
    stub
}

/// Compact the oldest eligible results until the history fits the budget
/// (rule on [`ToolHistoryBudget`]). Returns how many were
/// compacted.
pub(crate) fn compact_tool_history(
    messages: &mut [genai::chat::ChatMessage],
    fed: &mut [FedResult],
    round: u32,
    budget: ToolHistoryBudget,
    workdir: &Path,
    keep_files: usize,
) -> usize {
    if budget.budget_bytes == 0 {
        return 0;
    }
    let mut live: usize = fed.iter().map(|f| f.sent).sum();
    let newest_eligible = round.saturating_sub(budget.keep_rounds.max(1));
    let mut compacted = 0;
    for f in fed.iter_mut() {
        if live <= budget.budget_bytes {
            break;
        }
        if f.compacted || f.round > newest_eligible || f.body.len() < MIN_COMPACT_BYTES {
            continue;
        }
        let stub = compaction_stub(f, workdir, keep_files);
        live = live - f.sent + stub.len();
        f.sent = stub.len();
        f.compacted = true;
        f.body = String::new();
        messages[f.index] = genai::chat::ChatMessage::from(
            genai::chat::ToolResponse::new(f.call_id.clone(), stub).with_fn_name(f.tool.clone()),
        );
        compacted += 1;
    }
    compacted
}

/// One tool call through the registry (pre-hooks, Gate, execution),
/// shared by the fixer and the tool reviewer: the result value,
/// or the fed error text plus its outcome label.
pub(crate) async fn registry_call(
    registry: &polymorph_tools::Registry,
    tc: &genai::chat::ToolCall,
    tool_ctx: &polymorph_tools::ToolCallContext,
) -> Result<serde_json::Value, (String, &'static str)> {
    match registry
        .call(&tc.fn_name, tc.fn_arguments.clone(), tool_ctx)
        .await
    {
        Ok(value) => Ok(value),
        // Typed faults: denials label `denied`, execution
        // failures label `error`. Fed text is byte-identical to before
        // (`tool error: ...` in both cases) — observability-only change,
        // the model sees nothing new.
        Err(polymorph_tools::CallError::Denied(d)) => Err((format!("tool error: {d}"), "denied")),
        Err(polymorph_tools::CallError::Failed(e)) => {
            // Edit failures subtype; every other tool keeps
            // plain `error`. Fed text unchanged either way.
            let outcome = if tc.fn_name == "edit" {
                classify_edit_error(&e)
            } else {
                "error"
            };
            Err((format!("tool error: {e}"), outcome))
        }
    }
}

impl AgenticFixRunner {
    /// Production constructor: live turns over the managed/fixed target.
    /// The preamble goes on here (once), so scripted tests see raw systems.
    /// Full [`TOOLS_PREAMBLE`] — the default everywhere except the
    /// bench ablation (which uses [`Self::live_with_preamble`]).
    pub fn live(
        client: polymorph_llm::LlmClient,
        target: LlmTarget,
        system: String,
        registry: Arc<polymorph_tools::Registry>,
        gate: polymorph_tools::Gate,
        tools: Vec<String>,
        max_tool_rounds: u32,
    ) -> Self {
        Self {
            turn: Arc::new(LiveAgenticTurn {
                client,
                target,
                system: format!("{TOOLS_PREAMBLE}{system}"),
                rules: Vec::new(),
                rule_max_retries: DEFAULT_RULE_RETRIES,
                pinned: Default::default(),
            }),
            registry,
            gate,
            tools,
            max_tool_rounds,
            max_bonus_tool_rounds: DEFAULT_MAX_BONUS_TOOL_ROUNDS,
            // S advisor: off at construction; CLI attaches
            // post-construction when `[advisor] enabled` (both ctors).
            advisor: None,
            // The CLI attaches a ledger for rollback drives.
            restore: None,
            // The CLI overrides it from `[agent]`.
            history: ToolHistoryBudget::default(),
            // The CLI overrides it from `[agent] command_change_max_files`.
            command_changes: crate::command_changes::DEFAULT_COMMAND_CHANGE_MAX_FILES,
        }
    }

    /// Ablation constructor: caller-chosen preamble via
    /// [`select_preamble`] — the bench passes the `--preamble` flag here.
    /// Every other caller uses [`Self::live`] (full block, unchanged).
    #[allow(clippy::too_many_arguments)] // mirrors `live` + preamble (bench-only ctor)
    pub fn live_with_preamble(
        client: polymorph_llm::LlmClient,
        target: LlmTarget,
        system: String,
        registry: Arc<polymorph_tools::Registry>,
        gate: polymorph_tools::Gate,
        tools: Vec<String>,
        max_tool_rounds: u32,
        preamble: &str,
    ) -> Self {
        Self {
            turn: Arc::new(LiveAgenticTurn {
                client,
                target,
                system: format!("{preamble}{system}"),
                rules: Vec::new(),
                rule_max_retries: DEFAULT_RULE_RETRIES,
                pinned: Default::default(),
            }),
            registry,
            gate,
            tools,
            max_tool_rounds,
            max_bonus_tool_rounds: DEFAULT_MAX_BONUS_TOOL_ROUNDS,
            advisor: None,
            restore: None,
            history: ToolHistoryBudget::default(),
            // Bench apparatus stability: no command change detection.
            command_changes: 0,
        }
    }

    /// Outcome class for observability (bench taxonomy): the response text
    /// still carries detail for the model; this label is the machine-readable
    /// shape for logs and `TurnOutput.data["tool_outcomes"]`. The third
    /// element is the call's structured ledger fields ([`ledger_extras`]:
    /// command summary + exit code, mutated paths —).
    async fn execute_tool(
        &self,
        tc: &genai::chat::ToolCall,
        tool_ctx: &polymorph_tools::ToolCallContext,
    ) -> (
        genai::chat::ToolResponse,
        &'static str,
        serde_json::Map<String, serde_json::Value>,
    ) {
        use genai::chat::ToolResponse;
        if !self.tools.iter().any(|t| t == &tc.fn_name) {
            return (
                ToolResponse::from_tool_call(
                    tc,
                    format!(
                        "unknown tool '{}' (available: {})",
                        tc.fn_name,
                        self.tools.join(", ")
                    ),
                ),
                "refused-unknown",
                serde_json::Map::new(),
            );
        }
        // Failed/denied calls still name the command (it never ran, so no
        // exit code); mutations record paths only on success.
        let failed_extras = || ledger_extras(&tc.fn_name, &tc.fn_arguments, None);
        match registry_call(&self.registry, tc, tool_ctx).await {
            Ok(mut value) => {
                if tc.fn_name == "read" {
                    Self::attach_tag(&mut value, tc, tool_ctx, &self.registry).await;
                }
                let mut extras = ledger_extras(&tc.fn_name, &tc.fn_arguments, Some(&value));
                // A spilled stream's digest omits lines, so the
                // zero-test label reads the full spill files instead.
                if is_command_tool(&tc.fn_name) {
                    if let Some(marker) = spilled_zero_test_marker(&value, &tool_ctx.workdir) {
                        match marker {
                            Some(m) => extras.insert("zero_tests".into(), m.into()),
                            None => extras.remove("zero_tests"),
                        };
                    }
                }
                let text =
                    serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
                (ToolResponse::from_tool_call(tc, text), "ok", extras)
            }
            Err((text, outcome)) => (
                ToolResponse::from_tool_call(tc, text),
                outcome,
                failed_extras(),
            ),
        }
    }

    /// One stamp walk of `workdir` for command change detection; on
    /// overflow records the skip note (for the rest of the turn) and
    /// yields `None`.
    fn probe_tree(
        &self,
        workdir: &Path,
        skipped: &mut Option<String>,
    ) -> Option<polymorph_tools::fs::TreeStamps> {
        use crate::command_changes::{probe, Probe};
        match probe(workdir, self.registry.scan_hidden(), self.command_changes) {
            Probe::Stamps(stamps) => Some(stamps),
            Probe::Skipped(note) => {
                tracing::info!(
                    max_files = self.command_changes,
                    "command change detection skipped for the rest of the turn: tree too large"
                );
                *skipped = Some(note);
                None
            }
        }
    }

    /// Best-effort tag attach: re-read the FULL file, mint over normalized
    /// content, set `result.tag`. Tags bind content (lines are a view), so a
    /// paginated read still yields a valid tag; a changed file fails closed
    /// at edit time (StaleTag → model re-reads). Mint failures surface as
    /// `tag_error`, never silence (the model must know the tag is missing).
    async fn attach_tag(
        value: &mut serde_json::Value,
        tc: &genai::chat::ToolCall,
        tool_ctx: &polymorph_tools::ToolCallContext,
        registry: &polymorph_tools::Registry,
    ) {
        let rel = tc
            .fn_arguments
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if rel.is_empty() {
            return;
        }
        let full = if std::path::Path::new(rel).is_absolute() {
            PathBuf::from(rel)
        } else {
            let trimmed = rel.trim_start_matches(['/', '\\']);
            tool_ctx.workdir.join(trimmed)
        };
        let minted = (|| {
            let bytes = std::fs::read(&full).map_err(|e| e.to_string())?;
            if bytes.iter().take(8192).any(|&b| b == 0) {
                return Err("binary file".to_string());
            }
            Ok::<_, String>(polymorph_tools::hashline::apply::normalize(&bytes).text)
        })();
        match minted {
            Ok(text) => {
                let tag = registry.mint_tag(std::path::Path::new(rel), &text).await;
                value["tag"] = serde_json::Value::String(tag);
            }
            Err(e) => {
                value["tag_error"] = serde_json::Value::String(e);
            }
        }
    }
}

#[async_trait]
impl polymorph_delegate::TurnRunner for AgenticFixRunner {
    async fn run_turn(
        &self,
        ctx: polymorph_delegate::TurnContext,
    ) -> anyhow::Result<polymorph_delegate::TurnOutput> {
        use genai::chat::ChatMessage;
        let tool_ctx = polymorph_tools::ToolCallContext::new(
            ctx.workdir.clone(),
            self.gate.clone(),
            ctx.cancel.clone(),
        );
        let genai_tools = to_genai_tools(&self.registry, &self.tools);
        // L-actuation: last-recorded post-write diagnostics
        // ride the fix-turn brief as a read-only advisory. Empty (LSP off)
        // leaves the brief byte-identical — every pre-98 matrix preserved.
        // Reviewers stay tool-free and diagnostics-free (posture stands).
        let brief = match render_lsp_section(&self.registry.lsp_diagnostics().await) {
            Some(section) => format!("{}\n\n{section}", ctx.brief),
            None => ctx.brief.clone(),
        };
        // Cloned (not moved): the S advisor re-reads the brief post-loop.
        let mut messages = vec![ChatMessage::user(brief.clone())];
        let mut last_text: String;
        let mut total_calls = 0u32;
        let mut feeds = 0u32;
        let mut dropped = 0u32;
        let mut outcomes: Vec<serde_json::Value> = Vec::new();
        // Verification bonus rounds spent past `max_tool_rounds`.
        let mut bonus_rounds = 0u32;
        let mut granted_mutation_bonus = false;
        let mut mutated_files = false;
        let mut attempted_mutation = false;
        let mut nudged_text_only = false;
        // Repeat-command futility tracking: signature of the last
        // executed command tool (name + args + outcome).
        let mut last_cmd_sig: Option<(String, String, String)> = None;
        let mut repeat_nudged = false;
        // Progress signal for the next bonus grant: the last fed round
        // mutated, or verified a mutation no command had run after yet.
        let mut last_round_progress = false;
        let mut unverified_mutation = false;
        // Pre-mutation snapshots (first touch per file).
        let mut snapshot_seen: Vec<String> = Vec::new();
        let mut pre_mutation: Vec<serde_json::Value> = Vec::new();
        // Every fed tool result, for history compaction.
        let mut fed: Vec<FedResult> = Vec::new();
        let keep_files = self.registry.output_spill().keep_files;
        // Command change detection: once a walk overflows the cap, the
        // rest of the turn skips it (the tree will not shrink) and every
        // command carries the note instead.
        let mut detect_skipped: Option<String> = None;
        loop {
            let turn = self
                .turn
                .turn(messages.clone(), genai_tools.clone(), &ctx.cancel)
                .await?;
            last_text = turn.text.clone();
            if turn.tool_calls.is_empty() {
                // If the fixer offers mutating tools (write/edit) and no mutating tools have been attempted so far,
                // and we haven't nudged yet, remind the model once instead of silently accepting prose.
                let offers_mutation = self.tools.iter().any(|t| t == "write" || t == "edit");
                if offers_mutation && !attempted_mutation && !nudged_text_only {
                    nudged_text_only = true;
                    tracing::info!(
                        "fixer emitted text without file mutations; issuing one-shot reminder"
                    );
                    if !turn.text.trim().is_empty() {
                        let mut assistant = ChatMessage::assistant(turn.text.clone());
                        if let Some(reasoning) = turn.reasoning_content.clone() {
                            assistant = assistant.with_reasoning_content(Some(reasoning));
                        }
                        messages.push(assistant);
                    }
                    messages.push(ChatMessage::user(
                        "You replied with text/prose without modifying or creating any files on disk using `write` or `edit`. Text output does NOT write to disk. You MUST invoke the `write` tool to create files or `edit` tool to modify files. Call the tools now."
                            .to_string(),
                    ));
                    continue;
                }
                break;
            }
            let turn_has_mutation = turn
                .tool_calls
                .iter()
                .any(|tc| matches!(tc.fn_name.as_str(), "edit" | "write"));
            if feeds >= self.max_tool_rounds {
                if bonus_rounds < self.max_bonus_tool_rounds && last_round_progress {
                    bonus_rounds += 1;
                    tracing::info!(
                        feeds,
                        bonus_rounds,
                        max_bonus_tool_rounds = self.max_bonus_tool_rounds,
                        "adaptive bonus tool round granted for verification after mutating operations"
                    );
                } else if !granted_mutation_bonus && turn_has_mutation && !mutated_files {
                    granted_mutation_bonus = true;
                    tracing::info!(
                        feeds,
                        "adaptive bonus tool round granted for pending mutation before budget exhaustion"
                    );
                } else {
                    dropped += turn.tool_calls.len() as u32;
                    break;
                }
            }
            feeds += 1;
            let over = turn.tool_calls.len().saturating_sub(MAX_CALLS_PER_TURN);
            dropped += over as u32;
            let batch: Vec<_> = turn
                .tool_calls
                .into_iter()
                .take(MAX_CALLS_PER_TURN)
                .collect();
            let mut assistant = ChatMessage::from(batch.clone());
            if let Some(reasoning) = turn.reasoning_content.clone() {
                assistant = assistant.with_reasoning_content(Some(reasoning));
            }
            messages.push(assistant);
            let mut round_progress = false;
            for tc in &batch {
                let offered = self.tools.iter().any(|t| t == &tc.fn_name);
                if offered {
                    snapshot_before_mutation(
                        &ctx.workdir,
                        mutation_targets(&tc.fn_name, &tc.fn_arguments),
                        &mut snapshot_seen,
                        &mut pre_mutation,
                    );
                    if let Some(ledger) = &self.restore {
                        ledger.before_mutation(&ctx.workdir, &tc.fn_name, &tc.fn_arguments);
                    }
                }
                // Command change detection: stamp the tree right before the
                // command runs (after any earlier call's writes landed).
                let detect = offered && is_command_tool(&tc.fn_name) && self.command_changes > 0;
                let before = if detect && detect_skipped.is_none() {
                    self.probe_tree(&ctx.workdir, &mut detect_skipped)
                } else {
                    None
                };
                let (resp, outcome, mut extras) = self.execute_tool(tc, &tool_ctx).await;
                total_calls += 1;
                let is_mut = matches!(tc.fn_name.as_str(), "edit" | "write");
                // A denied command never ran: nothing it could have changed.
                let tree_diff = match before {
                    Some(before) if outcome != "denied" => self
                        .probe_tree(&ctx.workdir, &mut detect_skipped)
                        .map(|after| crate::command_changes::diff(&before, &after))
                        .unwrap_or_default(),
                    _ => Default::default(),
                };
                let changes = &tree_diff.changes;
                if detect {
                    if let Some(note) = &detect_skipped {
                        extras.insert("change_detection".into(), note.clone().into());
                    }
                }
                if !changes.is_empty() {
                    extras.extend(crate::command_changes::ledger_fields(changes));
                    // The server sees
                    // the changed files too (bounded per command; the rest
                    // are named unconsulted, never shown stale).
                    let mut consult: Vec<String> = Vec::new();
                    let mut unconsulted: Vec<String> = Vec::new();
                    for c in changes {
                        if c.kind == crate::command_changes::ChangeKind::Deleted
                            || polymorph_tools::lsp::language_of(&c.rel).is_none()
                        {
                            continue;
                        }
                        if consult.len() < crate::command_changes::MAX_LSP_CONSULTS_PER_COMMAND {
                            consult.push(c.rel.clone());
                        } else {
                            unconsulted.push(c.rel.clone());
                        }
                    }
                    self.registry.lsp_consult(&consult, &tool_ctx).await;
                    if !unconsulted.is_empty() {
                        extras.insert("lsp_unconsulted".into(), unconsulted.into());
                    }
                }
                // Record the drive's state after every mutation
                // (any outcome — a failed edit may have landed partly), track
                // what a command changed, and adopt the command's bytes in
                // tracked files.
                if let (Some(ledger), true) = (&self.restore, offered) {
                    if is_mut {
                        ledger.after_mutation(&ctx.workdir, &tc.fn_name, &tc.fn_arguments);
                    } else if is_command_tool(&tc.fn_name) {
                        ledger.after_command(&ctx.workdir, &tree_diff);
                        ledger.refresh();
                    }
                }
                if is_mut {
                    attempted_mutation = true;
                }
                if is_mut && outcome == "ok" {
                    round_progress = true;
                    mutated_files = true;
                    unverified_mutation = true;
                } else if is_command_tool(&tc.fn_name) && outcome == "ok" && unverified_mutation {
                    // A command that ran (any exit code — a red build is
                    // what the next round fixes) after a mutation.
                    round_progress = true;
                    unverified_mutation = false;
                }
                // A command that changed files is a mutation too, landing
                // at its end: nothing has verified it yet.
                if !changes.is_empty() {
                    round_progress = true;
                    mutated_files = true;
                    unverified_mutation = true;
                }
                // Repeat-command futility note: an identical command with
                // an identical outcome yields no new information, so say
                // so on the fed result (and once per turn, say it louder)
                // instead of letting re-runs pass as verification silently.
                // Execution is unchanged — flaky commands may legitimately
                // retry; only the information content is labelled.
                if is_command_tool(&tc.fn_name) {
                    let sig = (
                        tc.fn_name.clone(),
                        tc.fn_arguments.to_string(),
                        outcome.to_string(),
                    );
                    if last_cmd_sig.as_ref() == Some(&sig) {
                        extras.insert(
                            "repeat_note".into(),
                            "identical command already ran with the identical outcome; re-running it yields no new information — create/edit a file or run a different diagnostic instead".into(),
                        );
                        if !repeat_nudged {
                            repeat_nudged = true;
                            messages.push(ChatMessage::user(
                                "You just re-ran an identical command with an identical outcome. That consumes the tool budget without producing information. Do NOT run it a third time: write/edit the missing or broken file, or run a materially different diagnostic."
                                    .to_string(),
                            ));
                        }
                    }
                    last_cmd_sig = Some(sig);
                }
                // Array order is call order (the evidence block
                // reads mutation-after-last-command from it).
                let mut entry = serde_json::json!({
                    "tool": tc.fn_name.as_str(),
                    "outcome": outcome,
                });
                if let Some(obj) = entry.as_object_mut() {
                    obj.extend(extras);
                }
                fed.push(FedResult {
                    index: messages.len(),
                    round: feeds,
                    call_id: resp.call_id.clone(),
                    tool: tc.fn_name.clone(),
                    outcome,
                    exit_code: entry.get("exit_code").and_then(|c| c.as_i64()),
                    read_path: (tc.fn_name == "read")
                        .then(|| tc.fn_arguments.get("path").and_then(|p| p.as_str()))
                        .flatten()
                        .map(str::to_string),
                    // Kept only while compaction may need it (budget on).
                    body: if self.history.budget_bytes > 0 {
                        resp.content.clone()
                    } else {
                        String::new()
                    },
                    sent: resp.content.len(),
                    compacted: false,
                });
                outcomes.push(entry);
                // Forensics, not stdout (split logging): name + shape +
                // outcome only, never secrets (registry never logs values
                // either).
                tracing::info!(
                    tool = tc.fn_name.as_str(),
                    tool_round = feeds,
                    args = %tc.fn_arguments,
                    outcome = outcome,
                    "agentic fix tool call complete"
                );
                messages.push(ChatMessage::from(resp));
            }
            last_round_progress = round_progress;
            // Older results past the budget become stubs before
            // the history is re-sent; the latest rounds stay intact.
            let compacted = compact_tool_history(
                &mut messages,
                &mut fed,
                feeds,
                self.history,
                &ctx.workdir,
                keep_files,
            );
            if compacted > 0 {
                tracing::info!(
                    compacted,
                    tool_round = feeds,
                    live_bytes = fed.iter().map(|f| f.sent).sum::<usize>(),
                    "tool history compacted"
                );
            }
        }

        // Final Answer Synthesis
        // When intermediate or terminating turns produced only tool calls with empty text,
        // force a final model synthesis turn with zero tools so the operator receives a
        // comprehensive textual answer instead of an empty reply or fallback placeholder.
        if last_text.trim().is_empty() && total_calls > 0 {
            messages.push(ChatMessage::user(
                "Based on the tool results above, provide your final comprehensive response to the operator."
                    .to_string(),
            ));
            match self
                .turn
                .turn(messages.clone(), Vec::new(), &ctx.cancel)
                .await
            {
                Ok(turn) => {
                    if !turn.text.trim().is_empty() {
                        last_text = turn.text;
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        error = %format!("{e:#}"),
                        "final answer synthesis turn failed, keeping last text"
                    );
                }
            }
        }
        let summary = format!(
            "agentic fix: {feeds} tool rounds, {total_calls} calls, {dropped} dropped (cap)"
        );
        // J1 Stop hook: the tool loop terminated (observation only — the
        // report below is unchanged, handlers cannot alter the outcome).
        self.registry.fire_stop(summary.clone()).await;
        // L-actuation: warn-first merge signal. Record the
        // last-recorded diagnostic count on the outcome; warn past a
        // non-zero remainder, never deny (deny is queued behind data —
        // the merge arm below stays unconditional).
        let lsp_findings = lsp_findings_count(&self.registry.lsp_diagnostics().await);
        if lsp_findings > 0 {
            tracing::warn!(lsp_findings, "agentic fix completed with LSP diagnostics");
        }
        // S advisor: one watching turn over the finished
        // trace (clean context, never the conversation). Failure absorbs
        // to warn + empty (99 posture — a sick watcher never holds
        // fixes hostage); notes ride data + logs as their own surface.
        let advisor_notes: Vec<AdvisorNote> = match &self.advisor {
            Some(advisor) => match advisor
                .advise(&brief, &outcomes, &last_text, &ctx.cancel)
                .await
            {
                Ok(notes) => {
                    tracing::info!(advisor_notes = notes.len(), "advisor reviewed turn");
                    notes
                }
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "advisor failed, continuing");
                    Vec::new()
                }
            },
            None => Vec::new(),
        };
        // The turn's distinct write/edit targets, alongside the
        // `mutated_files` bool (kept as-is for existing consumers).
        // The attachment renderer reads both, plus `pre_mutation_snapshots`,
        // to attach
        // peripheral files to the reviewed text.
        let mutated_paths = ledger_mutated_paths(&outcomes);
        // Latest LSP snapshot per file this turn mutated (the
        // count above stays pool-wide).
        let lsp_diagnostics = lsp_mutated_snapshots(
            &self.registry.lsp_diagnostic_details().await,
            &ctx.workdir,
            &outcomes,
        );
        Ok(polymorph_delegate::TurnOutput {
            text: last_text.clone(),
            summary,
            tool_calls: total_calls,
            done: true,
            data: Some(
                serde_json::json!({ "artifact": last_text, "tool_outcomes": outcomes, "lsp_findings": lsp_findings, "advisor_notes": advisor_notes, "mutated_files": mutated_files, "mutated_paths": mutated_paths, "lsp_diagnostics": lsp_diagnostics, "pre_mutation_snapshots": pre_mutation }),
            ),
        })
    }
}

/// Per-turn merge-back config for loop worktrees.
/// When `BusExecutor.merge` is `Some`, each `execute()` spawns a kept
/// Branch worktree from `repo` under `parent`, then squash-merges it back
/// before returning the artifact text. When `None` (default) the executor
/// spawns `workdir` directly (pre-44 behavior, unchanged).
///
/// Explicit opt-in, never auto-detect: the caller decides whether the case
/// dir is a repo (bench `materialize_repo`) and passes both paths. The
/// reviewer stays tool-free InPlace either way — only fix turns merge.
#[derive(Debug, Clone)]
pub struct LoopMergeConfig {
    /// Case repo the Branch attaches to (bench case dir, seeded commit).
    pub repo: PathBuf,
    /// Worktree parent (bench pid-scoped temp parent; `create` refuses
    /// to adopt stray state, so fresh child ids never collide).
    pub parent: PathBuf,
    /// Merge-commit message (squash; branch name is per-child).
    pub message: String,
}

/// `Executor` over Bus children: one fresh spawn + join per call.
/// Missing `artifact` (or a stopped/failed child) becomes an anyhow error,
/// which the loop maps to its `ExecutorFailed` escalate path.
pub struct BusExecutor {
    pub bus: Arc<polymorph_delegate::Bus>,
    pub runner: Arc<dyn polymorph_delegate::TurnRunner>,
    pub workdir: polymorph_delegate::WorkdirSpec,
    /// Tool names the executor child may wield (spawn-record honesty; the
    /// runner enforces the same allowlist + Gate at call time). Empty =
    /// text-only (pre-F3b behavior, still the default).
    pub tools: Vec<String>,
    /// Bench cost ledger: per-fix-turn `tool_outcomes` arrays appended
    /// from child data (AgenticFixRunner records them since).
    /// Text-only children record none. Shared so the bench runner reads
    /// costs without touching loop semantics.
    pub tool_outcomes: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    /// Loop-worktree merge mode: `None` = spawn `workdir`
    /// directly (all pre-44 callers); `Some` = per-call kept Branch +
    /// squash-merge back into `repo` (bench loop arm behind
    /// `--loop-worktree`; `review run` and `eval-ab` stay `None`).
    pub merge: Option<LoopMergeConfig>,
    /// Artifact file for the empty-text fallback: when the
    /// child returns blank text, the executor best-effort re-reads this
    /// file (InPlace workdirs only) so round-2 grades file work instead
    /// of an empty string. Mirrors `BusReviewer.artifact_path`. Set only
    /// in `review run` (bench/eval-ab/plan-drive keep `None` — apparatus
    /// stability); `None` = pre-110 behavior, unchanged.
    pub artifact_path: Option<PathBuf>,
    /// Peripheral-file attachment: `Some` appends every file
    /// the drive's fix turns mutated other than `artifact_path` (current
    /// disk content, budgeted) to the scored text under
    /// [`PERIPHERAL_HEADER`], so the reviewer grades them too. Set only in
    /// `review run`/`drive` (bench/eval-ab/plan-drive keep `None` —
    /// apparatus stability); `None`, or no peripheral mutation, leaves the
    /// scored text byte-identical. One tracker per drive. Content rides
    /// the prompts only, never logs (posture).
    pub peripherals: Option<PeripheralFiles>,
    /// Multi-file / worktree-diff review scope: `Some` scores
    /// the set re-read from disk after every fix turn (InPlace workdirs
    /// only; a `--repo` drive is in place on its worktree) instead of
    /// `artifact_path` or the child's text, and drops set members from
    /// the peripheral section. A set that cannot be read (git gone) fails
    /// the turn loudly. Set
    /// only by `review run`/`drive` with `--artifact-files`/`--diff`;
    /// `None` = single-artifact behavior, unchanged.
    pub artifact_set: Option<crate::ArtifactSet>,
}

/// Best-effort artifact-file read for the empty-text fallback (chunk
/// 110): `Some(content)` only when the executor workdir is `InPlace`
/// (relative paths resolve against it; absolute pass through) and the
/// file reads as UTF-8. Worktree specs (auto-removed per-child trees,
/// merge-mode branches owning state via git) and read failures yield
/// `None` — the caller keeps the child text and warns. Pure (no IO
/// beyond the read itself) for unit tests; never spawns.
/// Harness-observed evidence appended to the scored fix text.
/// The reviewer never sees the tree, so gradeable file facts ride along:
/// per-finding-file existence + line counts, plus the turn's tool ledger
/// and the mutated files' LSP diagnostics (server messages,
/// bounded single lines — the fix-brief feed, now graded too).
/// Static facts only — never file content (secrets posture). Pure IO,
/// best-effort throughout (unresolvable paths are named MISSING-or-SKIP,
/// never errors). Capped (20 files) with the omission named.
fn render_harness_evidence(
    workdir: &polymorph_delegate::WorkdirSpec,
    findings: &[polymorph_core::Finding],
    data: &serde_json::Value,
) -> String {
    const MAX_FILES: usize = 20;
    let mut out = String::from("\n\nHARNESS EVIDENCE (harness-observed, not model claims):\n");
    // Tool ledger: what the turn actually wielded, with outcomes.
    if let Some(arr) = data.get("tool_outcomes").and_then(|v| v.as_array()) {
        let mut counts: std::collections::BTreeMap<String, usize> =
            std::collections::BTreeMap::new();
        for entry in arr {
            if let Some(tool) = entry.get("tool").and_then(|t| t.as_str()) {
                let outcome = entry
                    .get("outcome")
                    .and_then(|o| o.as_str())
                    .unwrap_or("ok");
                *counts.entry(format!("{tool} ({outcome})")).or_insert(0) += 1;
            }
        }
        if counts.is_empty() {
            out.push_str("- tools: none recorded this turn\n");
        } else {
            let formatted: Vec<String> = counts
                .into_iter()
                .map(|(k, c)| format!("{k}: {c}"))
                .collect();
            out.push_str(&format!("- tools: {}\n", formatted.join(", ")));
        }
        out.push_str(&render_command_ledger(arr));
    } else {
        out.push_str("- tools: none recorded this turn\n");
    }
    // LSP snapshots of the mutated files (empty without one).
    out.push_str(&render_lsp_evidence(data));
    // Verify-gate commands (empty when the gate is off).
    out.push_str(&render_verify_evidence(data));
    // Finding files: existence + size on disk right now. Verify-gate
    // findings name a command, and an empty-diff CREATE seed
    // a changeset, not a file — nothing to stat.
    let base: Option<std::path::PathBuf> = match workdir {
        polymorph_delegate::WorkdirSpec::InPlace(dir) => Some(dir.clone()),
        _ => None,
    };
    let findings: Vec<&polymorph_core::Finding> = findings
        .iter()
        .filter(|f| f.file != VERIFY_FINDING_FILE && f.file != crate::DIFF_SET_FINDING_FILE)
        .collect();
    for f in findings.iter().take(MAX_FILES) {
        let raw = std::path::Path::new(&f.file);
        let full = if raw.is_absolute() {
            Some(raw.to_path_buf())
        } else {
            base.as_ref().map(|b| b.join(raw))
        };
        match full {
            None => out.push_str(&format!("- {}: SKIPPED (worktree path)\n", f.file)),
            Some(p) => match std::fs::read_to_string(&p) {
                Ok(content) => out.push_str(&format!(
                    "- {}: EXISTS ({} lines)\n",
                    f.file,
                    content.lines().count()
                )),
                Err(_) => out.push_str(&format!("- {}: MISSING\n", f.file)),
            },
        }
    }
    if findings.len() > MAX_FILES {
        out.push_str(&format!(
            "({} more files omitted)\n",
            findings.len() - MAX_FILES
        ));
    }
    out
}

/// Command + mutation ledger lines for the evidence block:
/// every bash/exec call in call order with its exit code (nonzero flagged),
/// whether a write/edit landed AFTER the last command that ran, and the
/// turn's mutated files. Empty when the turn neither ran a command nor
/// mutated (read-only and tool-free turns render exactly as pre-#6).
/// Capped (commands keep the LAST 12, files the first 20) with omissions
/// named; `#N` is the call's 1-based position in the turn's ledger.
///
/// Files a command itself changed (harness-detected `changed_paths`) are
/// mutations that land at the END of that command: the command is named
/// with them, and when it is the last command that ran they count as
/// mutations after it (a formatter run after the tests changed files no
/// command verified).
fn render_command_ledger(arr: &[serde_json::Value]) -> String {
    const MAX_COMMANDS: usize = 12;
    const MAX_PATHS: usize = 20;
    fn tool_of(e: &serde_json::Value) -> &str {
        e.get("tool").and_then(|t| t.as_str()).unwrap_or("")
    }
    // Same absent-outcome default as the `- tools:` tally above.
    fn outcome_of(e: &serde_json::Value) -> &str {
        e.get("outcome").and_then(|o| o.as_str()).unwrap_or("ok")
    }
    // A recorded empty `paths` (all-noop edit) changed nothing; an absent
    // one (pre-#6 ledger) still counts, rendered as paths unrecorded. A
    // command mutated when detection saw it change files (any outcome: a
    // timed-out command may have changed some).
    fn is_mutation(e: &serde_json::Value) -> bool {
        match tool_of(e) {
            "write" | "edit" => {
                outcome_of(e) == "ok"
                    && e.get("paths")
                        .and_then(|p| p.as_array())
                        .is_none_or(|p| !p.is_empty())
            }
            tool if is_command_tool(tool) => str_array(e, "changed_paths").next().is_some(),
            _ => false,
        }
    }
    let commands: Vec<(usize, &serde_json::Value)> = arr
        .iter()
        .enumerate()
        .filter(|(_, e)| is_command_tool(tool_of(e)))
        .collect();
    let any_mutation = arr.iter().any(is_mutation);
    if commands.is_empty() && !any_mutation {
        return String::new();
    }
    let join_paths = |paths: &[String]| -> String {
        // Model-supplied write paths are arbitrary strings: same one-line
        // bound as commands so a hostile path cannot break the block.
        let mut s = paths
            .iter()
            .take(MAX_PATHS)
            .map(|p| summarize_command(p))
            .collect::<Vec<_>>()
            .join(", ");
        if paths.len() > MAX_PATHS {
            s.push_str(&format!(" ({} more omitted)", paths.len() - MAX_PATHS));
        }
        s
    };
    let mut out = String::new();
    if commands.is_empty() {
        out.push_str("- commands: NONE ran this turn (no bash/exec verification observed)\n");
    } else {
        out.push_str("- commands (call order, #N = call position):\n");
        if commands.len() > MAX_COMMANDS {
            out.push_str(&format!(
                "  ({} earlier commands omitted)\n",
                commands.len() - MAX_COMMANDS
            ));
        }
        for (i, e) in commands
            .iter()
            .skip(commands.len().saturating_sub(MAX_COMMANDS))
        {
            let cmd = e.get("cmd").and_then(|c| c.as_str()).unwrap_or("?");
            let status = match outcome_of(e) {
                "ok" => match e.get("exit_code").and_then(|c| c.as_i64()) {
                    Some(0) => "exit 0".to_string(),
                    Some(code) => format!("exit {code} NONZERO"),
                    None => "exit unknown".to_string(),
                },
                // Denied never ran; harness errors (spawn failure, timeout,
                // cancel) may have run partway — either way no exit code.
                other => format!("{other}, no exit code"),
            };
            out.push_str(&format!("  #{} {status}: {cmd}", i + 1));
            if let Some(marker) = e.get("zero_tests").and_then(|z| z.as_str()) {
                out.push_str(&format!(
                    " [HEURISTIC, not a verdict: output suggests zero tests ran (\"{marker}\")]"
                ));
            }
            out.push_str(&command_change_marker(e));
            out.push('\n');
        }
    }
    // Mutation-after-last-command: only commands that actually ran count
    // (a denied/failed call verified nothing).
    let last_ran = arr
        .iter()
        .rposition(|e| is_command_tool(tool_of(e)) && outcome_of(e) == "ok");
    // The last command's own changes land after it ran: `idx..`.
    let mutated = ledger_mutated_paths(arr.iter().filter(|e| is_mutation(e)));
    match last_ran {
        None => out.push_str(&format!(
            "- MUTATIONS AFTER LAST COMMAND: {}\n",
            if any_mutation {
                "n/a - no command ran; every mutation is unverified"
            } else {
                "no (no files mutated this turn; no command ran)"
            }
        )),
        Some(idx) => {
            let after: Vec<&serde_json::Value> =
                arr[idx..].iter().filter(|e| is_mutation(e)).collect();
            if after.is_empty() {
                out.push_str("- MUTATIONS AFTER LAST COMMAND: no\n");
            } else {
                let paths = ledger_mutated_paths(after.iter().copied());
                let files = if paths.is_empty() {
                    "(paths unrecorded)".to_string()
                } else {
                    join_paths(&paths)
                };
                out.push_str(&format!(
                    "- MUTATIONS AFTER LAST COMMAND: yes - files: {files}\n"
                ));
            }
        }
    }
    if any_mutation {
        if mutated.is_empty() {
            out.push_str("- files mutated this turn: (paths unrecorded)\n");
        } else {
            out.push_str(&format!(
                "- files mutated this turn ({}): {}\n",
                mutated.len(),
                join_paths(&mutated)
            ));
        }
    } else {
        out.push_str("- files mutated this turn: none\n");
    }
    out
}

/// Changed files shown per command line (the full list is the ledger's).
const MAX_CHANGED_NAMED: usize = 5;

/// A command line's change-detection suffix: the files the command itself
/// changed (`(deleted)` marked), or why detection did not run. Empty when
/// detection saw nothing, so such lines read exactly as before.
fn command_change_marker(e: &serde_json::Value) -> String {
    let mut out = String::new();
    let changed: Vec<&str> = str_array(e, "changed_paths").collect();
    if !changed.is_empty() {
        let deleted: Vec<&str> = str_array(e, "deleted_paths").collect();
        let total = changed.len()
            + e.get("changed_omitted")
                .and_then(|n| n.as_u64())
                .unwrap_or(0) as usize;
        let mut names = changed
            .iter()
            .take(MAX_CHANGED_NAMED)
            .map(|p| {
                let p_shown = summarize_command(p);
                if deleted.contains(p) {
                    format!("{p_shown} (deleted)")
                } else {
                    p_shown
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        if total > MAX_CHANGED_NAMED {
            names.push_str(&format!(" (+{} more)", total - MAX_CHANGED_NAMED));
        }
        out.push_str(&format!(
            " [CHANGED {total} file{} (harness-detected): {names}]",
            if total == 1 { "" } else { "s" }
        ));
    }
    if let Some(note) = e.get("change_detection").and_then(|n| n.as_str()) {
        out.push_str(&format!(" [change detection {note}]"));
    }
    out
}

/// Peripheral-file attachment header. Also the
/// marker [`render_review_brief`] and [`create_job_note`] look for, so a
/// review without peripheral files renders byte-identically to before.
pub const PERIPHERAL_HEADER: &str = "PERIPHERAL FILES MUTATED THIS DRIVE";

/// Attachment caps: the brief is a model prompt, not a tree
/// dump. Files past [`ATTACH_MAX_FILES`] are listed by name only; each
/// file shows at most [`ATTACH_FILE_MAX_ROWS`] rows (lines, or diff rows
/// for large files with a pre-drive snapshot); all files together share
/// [`ATTACH_BUDGET_ROWS`] rows and [`ATTACH_BUDGET_CHARS`] chars (UTF-8
/// bytes, so non-ASCII text counts conservatively). A row that does not
/// fit whole is never cut — rendering stops at the row (or hunk)
/// boundary and the file's status line names what was omitted.
pub const ATTACH_MAX_FILES: usize = 20;
pub const ATTACH_FILE_MAX_ROWS: usize = 400;
pub const ATTACH_BUDGET_ROWS: usize = 1_500;
pub const ATTACH_BUDGET_CHARS: usize = 120_000;
/// Files larger than this are named with their size, never read.
pub const ATTACH_MAX_READ_BYTES: u64 = 2 * 1024 * 1024;
/// Unchanged lines kept around each diff hunk.
const ATTACH_DIFF_CONTEXT: usize = 3;
/// Drive-wide memory bound on stored pre-drive snapshots (past it, files
/// fall back to full content).
const MAX_BASELINE_BYTES: usize = 16 * 1024 * 1024;

/// Disk state of one attached file after the fix turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachedState {
    /// UTF-8 text (BOM dropped).
    Text(String),
    /// Mutated, now absent (deleted, or renamed away by an edit).
    Deleted,
    /// NUL bytes or non-UTF-8: size only.
    Binary { bytes: u64 },
    /// Over [`ATTACH_MAX_READ_BYTES`]: size only, never read.
    TooLarge { bytes: u64 },
    /// Exists but cannot be read as a file (error kind named).
    Unreadable(String),
    /// Not readable from the harness (e.g. a removed worktree).
    Skipped(&'static str),
}

/// One file for [`render_file_attachments`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachedFile {
    /// Spelling shown to the reviewer (and cited back in findings).
    pub path: String,
    pub state: AttachedState,
    /// Content before the drive first mutated the file: `Some(Some(_))`
    /// diffable, `Some(None)` created by the drive, `None` no snapshot.
    pub baseline: Option<Option<String>>,
    /// Harness provenance note leading the status line (`RENAMED from
    /// <old>`, a command-made change); `None` renders as before.
    pub note: Option<String>,
}

/// Read one attached file from `base` (resolved as the tools resolve it).
/// `None` base = the turn's tree is not readable here (worktree drives).
/// Best-effort: every fault is a state, never an error.
pub fn read_attached(base: Option<&Path>, raw: &str) -> AttachedState {
    let Some(base) = base else {
        return AttachedState::Skipped("worktree drive: the turn's tree is gone after it");
    };
    let full = polymorph_tools::resolve_tool_path(base, raw);
    let meta = match std::fs::metadata(&full) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return AttachedState::Deleted,
        Err(e) => return AttachedState::Unreadable(e.kind().to_string()),
    };
    if !meta.is_file() {
        return AttachedState::Unreadable("not a regular file".to_string());
    }
    if meta.len() > ATTACH_MAX_READ_BYTES {
        return AttachedState::TooLarge { bytes: meta.len() };
    }
    match std::fs::read(&full) {
        Ok(bytes) => {
            let len = bytes.len() as u64;
            match text_of(bytes) {
                Some(text) => AttachedState::Text(text),
                None => AttachedState::Binary { bytes: len },
            }
        }
        Err(e) => AttachedState::Unreadable(e.kind().to_string()),
    }
}

/// Rows left for attached content, shared across files (rows AND chars).
struct AttachBudget {
    rows: usize,
    chars: usize,
}

impl AttachBudget {
    /// Charge one whole row; `false` (nothing charged) when it does not fit.
    fn take(&mut self, row: &str) -> bool {
        let cost = row.len() + 1;
        if self.rows == 0 || cost > self.chars {
            return false;
        }
        self.rows -= 1;
        self.chars -= cost;
        true
    }
}

/// Render files into a brief section headed `header` (a
/// multi-artifact review mode can reuse it with its own header). Pure:
/// the caller reads disk ([`read_attached`]). Small files render in full
/// with `N | ` line numbers; files over [`ATTACH_FILE_MAX_ROWS`] lines
/// render as a numbered diff when a pre-drive snapshot exists, else their
/// first rows; deleted, binary, oversized, and unreadable files are named
/// with their status. Every omission is stated on the file's status line.
pub fn render_file_attachments(header: &str, files: &[AttachedFile]) -> String {
    let mut out = format!(
        "\n\n{header} (harness-read from disk after the fix turn; `N | ` prefixes are 1-based file line numbers, not file content):\n"
    );
    out.push_str(&render_attachment_rows(files, &PERIPHERAL_WORDS));
    out
}

/// The section-specific words of an attachment rendering: the
/// peripheral section and the multi-file artifact set share every row,
/// budget and omission rule, but name their baseline, created and absent
/// files, and the post-change line numbers, differently.
pub(crate) struct AttachWords<'a> {
    /// What a baseline is: `DIFF vs {baseline}`, `UNCHANGED vs {baseline}`.
    pub baseline: &'a str,
    /// Why a large file without one renders partially.
    pub no_baseline: &'a str,
    /// Status prefix of a file whose baseline is `Some(None)`.
    pub created: &'a str,
    /// Status of an absent file.
    pub deleted: &'a str,
    /// Which line numbers diff rows carry (`@@ {now} lines …`).
    pub now: &'a str,
}

/// The attachment renderer's wording, byte for byte.
const PERIPHERAL_WORDS: AttachWords<'static> = AttachWords {
    baseline: "its pre-drive snapshot",
    no_baseline: "no pre-drive snapshot to diff",
    created: "CREATED this drive",
    deleted: "DELETED (absent on disk after the turn: deleted, or renamed away; no content)",
    now: "post-turn",
};

/// The file rows of [`render_file_attachments`] (no section header): one
/// shared budget, files past [`ATTACH_MAX_FILES`] named, never read.
pub(crate) fn render_attachment_rows(files: &[AttachedFile], words: &AttachWords) -> String {
    let mut out = String::new();
    let mut budget = AttachBudget {
        rows: ATTACH_BUDGET_ROWS,
        chars: ATTACH_BUDGET_CHARS,
    };
    for f in files.iter().take(ATTACH_MAX_FILES) {
        // Model-supplied paths: same one-line bound as the ledger.
        let path = summarize_command(&f.path);
        // A provenance note leads the status: `--- p: NOTE; STATUS`.
        let lead = format!("--- {path}: ");
        let noted = |rendered: String| match &f.note {
            Some(note) if rendered.starts_with(&lead) => format!(
                "{lead}{}; {}",
                summarize_command(note),
                &rendered[lead.len()..]
            ),
            _ => rendered,
        };
        let status = match &f.state {
            AttachedState::Text(text) => {
                out.push_str(&noted(render_attached_text(
                    &path,
                    text,
                    f.baseline.as_ref(),
                    words,
                    &mut budget,
                )));
                continue;
            }
            AttachedState::Deleted => words.deleted.to_string(),
            AttachedState::Binary { bytes } => {
                format!("BINARY or non-UTF-8 ({bytes} bytes; content not shown)")
            }
            AttachedState::TooLarge { bytes } => format!(
                "TOO LARGE ({bytes} bytes, over the {ATTACH_MAX_READ_BYTES}-byte read cap; content not shown)"
            ),
            AttachedState::Unreadable(why) => format!("UNREADABLE ({why}; content not shown)"),
            AttachedState::Skipped(why) => format!("SKIPPED ({why}; content not shown)"),
        };
        out.push_str(&noted(format!("--- {path}: {status}\n")));
    }
    if files.len() > ATTACH_MAX_FILES {
        let rest = &files[ATTACH_MAX_FILES..];
        let mut names = rest
            .iter()
            .take(ATTACH_MAX_FILES)
            .map(|f| summarize_command(&f.path))
            .collect::<Vec<_>>()
            .join(", ");
        if rest.len() > ATTACH_MAX_FILES {
            names.push_str(&format!(" and {} more", rest.len() - ATTACH_MAX_FILES));
        }
        out.push_str(&format!(
            "({} more files OMITTED, over the {ATTACH_MAX_FILES}-file cap: {names})\n",
            rest.len()
        ));
    }
    out
}

/// One text file: full numbered content, or a numbered diff against its
/// pre-drive snapshot when the file is over the per-file row cap.
fn render_attached_text(
    path: &str,
    text: &str,
    baseline: Option<&Option<String>>,
    words: &AttachWords,
    budget: &mut AttachBudget,
) -> String {
    let now = text.replace("\r\n", "\n");
    let lines: Vec<&str> = now.lines().collect();
    let n = lines.len();
    let width = n.max(1).to_string().len();
    // A diff only pays for itself when the whole file would not fit.
    if let Some(Some(old)) = baseline {
        if n > ATTACH_FILE_MAX_ROWS {
            let old = old.replace("\r\n", "\n");
            return render_attached_diff(path, &old, &now, width, words, budget);
        }
    }
    let mut rows = String::new();
    let mut shown = 0;
    for (i, line) in lines.iter().enumerate() {
        if shown == ATTACH_FILE_MAX_ROWS {
            break;
        }
        let row = format!("{:>width$} | {line}", i + 1);
        if !budget.take(&row) {
            break;
        }
        rows.push_str(&row);
        rows.push('\n');
        shown += 1;
    }
    let created = if matches!(baseline, Some(None)) {
        format!("{}, ", words.created)
    } else {
        String::new()
    };
    let status = if shown == n {
        format!("FULL ({created}{n} lines)")
    } else if shown == 0 {
        format!("OMITTED ({created}{n} lines; attachment budget exhausted; content not shown)")
    } else {
        let why = if shown == ATTACH_FILE_MAX_ROWS {
            format!(
                "per-file cap of {ATTACH_FILE_MAX_ROWS} lines, {}",
                words.no_baseline
            )
        } else {
            "attachment budget exhausted".to_string()
        };
        format!(
            "PARTIAL ({created}{n} lines; lines 1-{shown} shown, lines {}-{n} OMITTED: {why})",
            shown + 1
        )
    };
    format!("--- {path}: {status}\n{rows}")
}

/// Numbered unified diff of a large file against its baseline (pre-drive
/// snapshot, or the diff base's version — `words`): `@@` hunks, rows
/// `N   | ` (context) / `N + | ` (added, current line N) / `  - | `
/// (removed). Cut only at row boundaries, cut point named.
fn render_attached_diff(
    path: &str,
    old: &str,
    now: &str,
    width: usize,
    words: &AttachWords,
    budget: &mut AttachBudget,
) -> String {
    use similar::{ChangeTag, TextDiff};
    let n = now.lines().count();
    let diff = TextDiff::from_lines(old, now);
    let groups = diff.grouped_ops(ATTACH_DIFF_CONTEXT);
    let (against, when) = (words.baseline, words.now);
    if groups.is_empty() {
        return format!("--- {path}: UNCHANGED vs {against} ({n} lines; content not repeated)\n");
    }
    let mut rows = String::new();
    let mut shown = 0usize;
    let mut full_hunks = 0usize;
    // Whether the cut landed inside a hunk (some of its rows shown).
    let mut cut_inside = false;
    'hunks: for group in &groups {
        let start = group.first().map(|op| op.new_range().start).unwrap_or(0);
        let end = group.last().map(|op| op.new_range().end).unwrap_or(start);
        let header = if end > start {
            format!("@@ {when} lines {}-{end} @@", start + 1)
        } else {
            format!("@@ lines removed after {when} line {start} @@")
        };
        if shown == ATTACH_FILE_MAX_ROWS || !budget.take(&header) {
            break;
        }
        rows.push_str(&header);
        rows.push('\n');
        shown += 1;
        for op in group {
            for change in diff.iter_changes(op) {
                let body = change.value().trim_end_matches(['\n', '\r']);
                let row = match (change.tag(), change.new_index()) {
                    (ChangeTag::Delete, _) | (_, None) => format!("{:>width$} - | {body}", ""),
                    (ChangeTag::Insert, Some(i)) => format!("{:>width$} + | {body}", i + 1),
                    (ChangeTag::Equal, Some(i)) => format!("{:>width$}   | {body}", i + 1),
                };
                if shown == ATTACH_FILE_MAX_ROWS || !budget.take(&row) {
                    cut_inside = true;
                    break 'hunks;
                }
                rows.push_str(&row);
                rows.push('\n');
                shown += 1;
            }
        }
        full_hunks += 1;
    }
    let total = groups.len();
    let mut status = format!(
        "DIFF vs {against} ({n} lines now, over the {ATTACH_FILE_MAX_ROWS}-line full-content cap; {total} hunks, {ATTACH_DIFF_CONTEXT} context lines; lines outside the hunks are unchanged and not shown"
    );
    if full_hunks < total {
        let why = if shown == ATTACH_FILE_MAX_ROWS {
            format!("per-file cap of {ATTACH_FILE_MAX_ROWS} rows")
        } else {
            "attachment budget exhausted".to_string()
        };
        let cut = if cut_inside {
            format!(
                "hunk {} cut at a row boundary, later hunks OMITTED",
                full_hunks + 1
            )
        } else {
            format!("hunks {}-{total} OMITTED", full_hunks + 1)
        };
        status.push_str(&format!(
            "; {full_hunks} of {total} hunks shown in full, {cut}: {why}"
        ));
    }
    status.push(')');
    format!("--- {path}: {status}\n{rows}")
}

/// Drive-scoped peripheral-file tracker for [`BusExecutor`]:
/// every file a fix turn mutated other than the artifact file, deduped by
/// [`attach_identity`] (relative / `./` / absolute / Windows case and
/// separators collapse; first spelling kept, so an untouched file renders
/// byte-identically round to round). Files stay tracked for the rest of
/// the drive: a broken file the next turn leaves alone must stay in front
/// of the reviewer, or the round after a finding on it passes blind.
#[derive(Debug, Default)]
pub struct PeripheralFiles {
    state: std::sync::Mutex<PeripheralState>,
}

#[derive(Debug, Default)]
struct PeripheralState {
    /// (identity, spelling), first-mutation order.
    files: Vec<(String, String)>,
    /// Pre-drive content per identity (`None` = did not exist).
    baselines: std::collections::BTreeMap<String, Option<String>>,
    baseline_bytes: usize,
    /// Provenance note per identity (renames; command-made changes).
    notes: std::collections::BTreeMap<String, String>,
}

impl PeripheralFiles {
    /// Fold one finished fix turn's data (`mutated_paths`,
    /// `pre_mutation_snapshots`) in, then read every tracked file's current
    /// state from `workdir` (InPlace only; other specs yield SKIPPED).
    /// Empty while no peripheral file has been mutated.
    pub fn observe(
        &self,
        workdir: &polymorph_delegate::WorkdirSpec,
        artifact_path: Option<&Path>,
        data: &serde_json::Value,
    ) -> Vec<AttachedFile> {
        let base: Option<&Path> = match workdir {
            polymorph_delegate::WorkdirSpec::InPlace(dir) => Some(dir.as_path()),
            _ => None,
        };
        let id_base = base.unwrap_or(Path::new(""));
        let artifact_id = artifact_path.map(|p| attach_identity(id_base, &p.to_string_lossy()));
        let Ok(mut st) = self.state.lock() else {
            return Vec::new();
        };
        // Files first tracked this turn: only their snapshots predate the
        // drive's first mutation of them (a later turn's is mid-drive).
        let mut fresh: Vec<String> = Vec::new();
        for raw in data
            .get("mutated_paths")
            .and_then(|p| p.as_array())
            .into_iter()
            .flatten()
            .filter_map(|p| p.as_str())
        {
            let id = attach_identity(id_base, raw);
            if artifact_id.as_ref() == Some(&id) || st.files.iter().any(|(k, _)| *k == id) {
                continue;
            }
            st.files.push((id.clone(), raw.to_string()));
            fresh.push(id);
        }
        for snap in data
            .get("pre_mutation_snapshots")
            .and_then(|s| s.as_array())
            .into_iter()
            .flatten()
        {
            let Some(raw) = snap.get("path").and_then(|p| p.as_str()) else {
                continue;
            };
            let id = attach_identity(id_base, raw);
            if !fresh.contains(&id) || st.baselines.contains_key(&id) {
                continue;
            }
            let content = snap
                .get("content")
                .and_then(|c| c.as_str())
                .map(str::to_string);
            let cost = content.as_ref().map_or(0, String::len);
            if st.baseline_bytes + cost > MAX_BASELINE_BYTES {
                continue;
            }
            st.baseline_bytes += cost;
            st.baselines.insert(id, content);
        }
        // Provenance: an edit `MV` names both ends (the destination diffs
        // against the source's pre-drive content); a file first seen
        // through a command's detected changes says so.
        for entry in data
            .get("tool_outcomes")
            .and_then(|o| o.as_array())
            .into_iter()
            .flatten()
        {
            for r in entry
                .get("renamed")
                .and_then(|r| r.as_array())
                .into_iter()
                .flatten()
            {
                let (Some(from), Some(to)) = (
                    r.get("from").and_then(|f| f.as_str()),
                    r.get("to").and_then(|t| t.as_str()),
                ) else {
                    continue;
                };
                let (from_id, to_id) =
                    (attach_identity(id_base, from), attach_identity(id_base, to));
                st.notes.insert(from_id.clone(), format!("RENAMED to {to}"));
                st.notes
                    .insert(to_id.clone(), format!("RENAMED from {from}"));
                if fresh.contains(&to_id) && !st.baselines.contains_key(&to_id) {
                    if let Some(baseline) = st.baselines.get(&from_id).cloned() {
                        let cost = baseline.as_ref().map_or(0, String::len);
                        if st.baseline_bytes + cost <= MAX_BASELINE_BYTES {
                            st.baseline_bytes += cost;
                            st.baselines.insert(to_id, baseline);
                        }
                    }
                }
            }
            for raw in str_array(entry, "changed_paths") {
                let id = attach_identity(id_base, raw);
                if fresh.contains(&id) && !st.notes.contains_key(&id) {
                    st.notes.insert(
                        id,
                        "first changed by a fixer command (harness-detected)".into(),
                    );
                }
            }
        }
        st.files
            .iter()
            .map(|(id, raw)| AttachedFile {
                path: raw.clone(),
                state: read_attached(base, raw),
                baseline: st.baselines.get(id).cloned(),
                note: st.notes.get(id).cloned(),
            })
            .collect()
    }
}

/// Split reviewed text at the peripheral section: the primary
/// artifact text, and whether peripheral files are attached.
fn split_peripheral(artifact: &str) -> (&str, bool) {
    let marker = format!("\n\n{PERIPHERAL_HEADER} (");
    match artifact.find(&marker) {
        Some(i) => (&artifact[..i], true),
        None => (artifact, false),
    }
}

fn artifact_from_file(workdir: &polymorph_delegate::WorkdirSpec, path: &Path) -> Option<String> {
    let base = match workdir {
        polymorph_delegate::WorkdirSpec::InPlace(dir) => dir.clone(),
        _ => return None,
    };
    let full = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    std::fs::read_to_string(&full).ok()
}

#[async_trait]
impl Executor for BusExecutor {
    async fn execute(&self, input: ExecutorInput) -> anyhow::Result<String> {
        // Loop-worktree mode: per-call kept Branch worktree from
        // `repo` under `parent`, merged back on Completed. Default (`None`)
        // spawns `workdir` directly — all pre-44 behavior unchanged.
        let workdir = match &self.merge {
            Some(cfg) => polymorph_delegate::WorkdirSpec::Worktree {
                repo: cfg.repo.clone(),
                parent: cfg.parent.clone(),
                mode: polymorph_delegate::worktree::WorktreeMode::Branch(None),
            },
            None => self.workdir.clone(),
        };
        let brief = render_fix_brief(&input);
        // Forensics twin of the reviewer's `review brief` line.
        tracing::debug!(brief = %brief, "fix brief");
        let mut spec =
            polymorph_delegate::SpawnSpec::new(input.mode.executor_role(), brief, workdir);
        spec.tools = self.tools.clone();
        spec.keep_worktree = self.merge.is_some();
        let handle = self.bus.spawn(spec, self.runner.clone(), None).await?;
        let outcome = handle.join().await;
        // Cost ledger: adopt the child's tool-outcome array when present
        // (text-only runners record none — absence is data, not an error).
        if let Some(arr) = outcome
            .result
            .data
            .get("tool_outcomes")
            .and_then(|v| v.as_array())
        {
            if let Ok(mut log) = self.tool_outcomes.lock() {
                log.extend(arr.iter().cloned());
            }
        }
        // Merge-back: Completed +
        // kept merges before scoring; trouble warns and scores as-is (never
        // rewrites the Completed result). Non-Completed discards best-effort
        // then bails the existing error (failed trees are evidence, never
        // merged).
        if let Some(cfg) = &self.merge {
            match &outcome.result.status {
                polymorph_delegate::FinishStatus::Completed => {
                    if let Some(kept) = outcome.kept {
                        let branch = kept.branch().unwrap_or("?").to_string();
                        match polymorph_delegate::worktree::merge_back(kept, &cfg.message).await {
                            Ok(report) => tracing::info!(
                                branch = report.branch.as_str(),
                                commit = report.commit.as_str(),
                                "loop fix merged"
                            ),
                            Err(e) => tracing::warn!(
                                branch = branch.as_str(),
                                "loop fix merge troubled ({e}) — scoring as-is"
                            ),
                        }
                    } else {
                        tracing::warn!("loop fix: no kept worktree — scoring as-is");
                    }
                }
                polymorph_delegate::FinishStatus::Stopped { reason } => {
                    if let Some(kept) = outcome.kept {
                        if let Err(e) = kept.discard().await {
                            tracing::warn!("loop fix discard troubled ({e:#})");
                        }
                    }
                    anyhow::bail!("fix child stopped: {reason:?}")
                }
                polymorph_delegate::FinishStatus::Failed { error } => {
                    if let Some(kept) = outcome.kept {
                        if let Err(e) = kept.discard().await {
                            tracing::warn!("loop fix discard troubled ({e:#})");
                        }
                    }
                    anyhow::bail!("fix child failed: {error}")
                }
            }
        }
        match outcome.result.status {
            polymorph_delegate::FinishStatus::Completed => {
                let text = outcome
                    .result
                    .data
                    .get("artifact")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .ok_or_else(|| anyhow::anyhow!("fix child returned no artifact"))?;
                // Empty-text fallback: the child edited the
                // file but returned blank text — grade the file work, not
                // the empty string (the 109 4× empty-artifact pattern).
                // File-first, always: the child's closing chat text is a
                // summary at best, and at worst raw tool-call markup
                // (Frontier SSIM drive, round 3: the reviewer graded DSML
                // instead of the file on disk) — or, worse still, code that
                // was never written (drive false-pass, 2026-10-20). When an
                // artifact path is known, the file decides; the text is used
                // only when the file cannot be read.
                let mutated = outcome
                    .result
                    .data
                    .get("mutated_files")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let mut rendered_set: Option<crate::RenderedSet> = None;
                let mut scored = if let Some(set) = &self.artifact_set {
                    // The set is the artifact — always graded
                    // from disk (the child's text is a summary at best).
                    let polymorph_delegate::WorkdirSpec::InPlace(dir) = &self.workdir else {
                        anyhow::bail!("artifact set review needs an in-place workdir");
                    };
                    let rendered = set.render(dir)?;
                    tracing::info!(mutated, "graded artifact set from disk");
                    let text = rendered.text.clone();
                    rendered_set = Some(rendered);
                    text
                } else if let Some(path) = self.artifact_path.as_deref() {
                    // File-first, always (drive false-pass lesson, 2026-10-20): with a known artifact
                    // file the FILE is the artifact. Grading the fix child's closing text let a turn
                    // that wrote nothing pass the reviewer — observed as `DRIVE PASSED in 2 rounds`
                    // with zero findings and an empty `git diff`, twice in one session. `mutated` is
                    // still logged, but it can no longer decide what gets graded.
                    match artifact_from_file(&self.workdir, path) {
                        // Forensics (the 110 validation lesson: silent
                        // fallback is indistinguishable from snippet
                        // grading in round-2 issues): static message
                        // only — never file content (secrets
                        // posture). The unreadable-path warn below
                        // carries the path (finding_issues logs
                        // file:line the same way); content never logs.
                        Some(content) => {
                            tracing::info!(
                                mutated,
                                "graded artifact file from disk (file-first, text not graded)"
                            );
                            content
                        }
                        None => {
                            tracing::warn!(
                                path = %path.display(),
                                mutated,
                                "artifact file unreadable — scoring fix child text as-is"
                            );
                            text
                        }
                    }
                } else {
                    text
                };
                // Peripheral files: the reviewer
                // otherwise sees only the artifact file and passes drives
                // whose other edited files are broken. Read from disk now,
                // after the whole turn (and any verify commands).
                if let Some(peripherals) = &self.peripherals {
                    let mut files = peripherals.observe(
                        &self.workdir,
                        self.artifact_path.as_deref(),
                        &outcome.result.data,
                    );
                    // A set member is already graded above: never twice.
                    if let Some(set) = &rendered_set {
                        files.retain(|f| !set.covers(&f.path));
                    }
                    if !files.is_empty() {
                        scored.push_str(&render_file_attachments(PERIPHERAL_HEADER, &files));
                    }
                }
                // Harness evidence: the reviewer never sees
                // the tree, so gradeable file facts ride the scored text
                // — observed by the harness, never model claims. Skipped
                // when there is nothing to prove (empty findings keeps
                // every pre-165 matrix byte-identical) — unless the verify
                // gate ran: its results always reach review.
                if !input.findings.is_empty() || outcome.result.data.get("verify").is_some() {
                    scored.push_str(&render_harness_evidence(
                        &self.workdir,
                        &input.findings,
                        &outcome.result.data,
                    ));
                }
                Ok(scored)
            }
            polymorph_delegate::FinishStatus::Stopped { reason } => {
                anyhow::bail!("fix child stopped: {reason:?}")
            }
            polymorph_delegate::FinishStatus::Failed { error } => {
                anyhow::bail!("fix child failed: {error}")
            }
        }
    }
}

/// Ordered candidate locations for a prompt pack, most specific first.
///
/// Pure — no filesystem reads, no environment access — so the search order is
/// unit-testable and `load_pack` is the only caller that touches the disk.
///
/// Order: (a) an absolute pack path is used as given; (b) `POLYMORPH_PROMPTS_DIR`
/// joined with the pack path minus a leading `prompts` component (and joined
/// with the whole path, for packs referenced outside that directory); (c) the
/// path as given, i.e. relative to the current directory; (d) each ancestor of
/// the current directory, nearest first, so a checkout works from any of its
/// subdirectories; (e) the executable and its three parents, so an installed
/// binary finds a `prompts` directory beside or above it. Duplicates are
/// dropped, first occurrence wins.
pub fn pack_candidates(
    path: &std::path::Path,
    prompts_dir: Option<&std::path::Path>,
    cwd: Option<&std::path::Path>,
    exe: Option<&std::path::Path>,
) -> Vec<std::path::PathBuf> {
    fn push(out: &mut Vec<std::path::PathBuf>, candidate: std::path::PathBuf) {
        if !out.iter().any(|p| p == &candidate) {
            out.push(candidate);
        }
    }
    let mut out: Vec<std::path::PathBuf> = Vec::new();
    if path.is_absolute() {
        out.push(path.to_path_buf());
        return out;
    }
    if let Some(dir) = prompts_dir {
        let stripped = path.strip_prefix("prompts").unwrap_or(path);
        push(&mut out, dir.join(stripped));
        push(&mut out, dir.join(path));
    }
    push(&mut out, path.to_path_buf());
    if let Some(cwd) = cwd {
        for ancestor in cwd.ancestors() {
            push(&mut out, ancestor.join(path));
        }
    }
    if let Some(exe_dir) = exe {
        push(&mut out, exe_dir.join(path));
        for up in ["..", "../..", "../../.."] {
            push(&mut out, exe_dir.join(up).join(path));
        }
    }
    out
}

/// Load a prompt pack file (read-only; the manifest is never touched here).
/// Refuses missing/empty files with the path named and the candidates tried.
pub fn load_pack(path: &Path) -> anyhow::Result<String> {
    let prompts_dir = std::env::var_os("POLYMORPH_PROMPTS_DIR").map(std::path::PathBuf::from);
    let cwd = std::env::current_dir().ok();
    let exe = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.to_path_buf()));
    let tried = pack_candidates(path, prompts_dir.as_deref(), cwd.as_deref(), exe.as_deref());
    for candidate in &tried {
        if let Ok(text) = std::fs::read_to_string(candidate) {
            if text.trim().is_empty() {
                anyhow::bail!("pack {} is empty", candidate.display());
            }
            return Ok(text);
        }
    }
    anyhow::bail!(
        "cannot read pack {}: path not found (tried: {}); set POLYMORPH_PROMPTS_DIR to the directory holding the prompt packs (e.g. <checkout>/prompts) to resolve it from any working directory",
        path.display(),
        tried
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{run_loop, LoopMode, Outcome};
    use polymorph_core::LoopConfig;
    use polymorph_delegate::{Bus, TurnContext, TurnOutput, TurnRunner};
    use std::collections::VecDeque;

    fn managed_target() -> LlmTarget {
        LlmTarget::Managed {
            router: polymorph_llm::RoleRouter::default(),
            providers: Vec::new(),
            state: Arc::new(tokio::sync::Mutex::new(polymorph_llm::FallbackState::new(
                std::time::Duration::from_secs(1),
            ))),
            role: polymorph_core::Role::Review,
            scope: None,
        }
    }

    #[test]
    fn pin_unset_returns_configured_chain() {
        // Cold pin = today's behavior verbatim (142 directive: unknown
        // parent resolves the role chain, byte for byte).
        let pin = std::sync::Mutex::new(None);
        let out = resolve_target(&pin, &managed_target());
        assert!(matches!(out, LlmTarget::Managed { .. }));
    }

    #[test]
    fn pin_set_returns_fixed_parent_model() {
        let pin = std::sync::Mutex::new(Some("opencode_go::glm-5.3".to_string()));
        let out = resolve_target(&pin, &managed_target());
        match out {
            LlmTarget::Fixed(genai::ModelSpec::Name(name)) => {
                assert_eq!(name.as_str(), "opencode_go::glm-5.3")
            }
            other => panic!("expected Fixed, got {other:?}"),
        }
    }

    #[test]
    fn record_stores_serving_model() {
        let pin = std::sync::Mutex::new(None);
        record_pin(&pin, Some("m1".to_string()));
        assert_eq!(pin.lock().unwrap().as_deref(), Some("m1"));
        // None-served (Fixed turns) leaves the pin untouched.
        record_pin(&pin, None);
        assert_eq!(pin.lock().unwrap().as_deref(), Some("m1"));
    }

    fn mock_spec(server: &wiremock::MockServer) -> genai::ModelSpec {
        use genai::resolver::{AuthData, Endpoint};
        genai::ModelSpec::from_target(genai::ServiceTarget {
            endpoint: Endpoint::from_owned(server.uri()),
            auth: AuthData::from_single("test-key"),
            model: genai::ModelIden::new(genai::adapter::AdapterKind::OpenAI, "w-bind"),
        })
    }

    /// Minimal OpenAI SSE stream with one text chunk.
    fn sse_text(content: &str) -> String {
        let body = serde_json::json!({
            "id": "t", "object": "chat.completion.chunk", "created": 1, "model": "t",
            "choices": [{"index": 0, "delta": {"content": content}, "finish_reason": null}]
        });
        format!("data: {body}\n\ndata: [DONE]\n\n")
    }

    fn fail_json() -> String {
        serde_json::json!({
            "verdict": "fail",
            "findings": [{"severity": "P1", "file": "a.rs", "line": 1,
                "issue": "wired bug", "proposed_fix": "fix it"}],
            "confidence": 0.8
        })
        .to_string()
    }

    fn pass_json() -> String {
        serde_json::json!({"verdict": "pass", "findings": [], "confidence": 0.9}).to_string()
    }

    fn turn_ctx() -> TurnContext {
        TurnContext {
            child_id: "c-test".to_string(),
            role: Role::Review,
            brief: "brief".to_string(),
            workdir: std::env::temp_dir(),
            turn: 1,
            steer: Vec::new(),
            cancel: tokio_util::sync::CancellationToken::new(),
        }
    }

    /// Scripted runner: pops one fixed output per turn; exhaustion panics
    /// (a test that runs dry is a test bug, and it must be loud).
    struct StubRunner {
        script: std::sync::Mutex<VecDeque<TurnOutput>>,
    }

    impl StubRunner {
        fn new(outputs: Vec<TurnOutput>) -> Self {
            Self {
                script: std::sync::Mutex::new(outputs.into_iter().collect()),
            }
        }
    }

    fn done_out(text: &str, data: serde_json::Value) -> TurnOutput {
        TurnOutput {
            text: text.to_string(),
            summary: "stub".to_string(),
            tool_calls: 0,
            done: true,
            data: Some(data),
        }
    }

    #[async_trait]
    impl TurnRunner for StubRunner {
        async fn run_turn(&self, _ctx: TurnContext) -> anyhow::Result<TurnOutput> {
            let out = self
                .script
                .lock()
                .unwrap()
                .pop_front()
                .expect("script exhausted");
            Ok(out)
        }
    }

    #[tokio::test]
    async fn review_runner_emits_raw_verdict_text() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse_text(&fail_json())),
            )
            .mount(&server)
            .await;
        let runner = ReviewTurnRunner {
            client: polymorph_llm::LlmClient::new(),
            target: LlmTarget::Fixed(mock_spec(&server)),
            system: "sys".to_string(),
            pinned: Default::default(),
        };
        let out = runner.run_turn(turn_ctx()).await.unwrap();
        assert!(out.done);
        assert_eq!(out.tool_calls, 0);
        let raw = out.data.as_ref().unwrap()["verdict_text"].as_str().unwrap();
        // The SAME text the loop will judge parses to the Fail verdict.
        let v = parse_verdict(raw).unwrap();
        assert_eq!(v.findings.len(), 1);
        assert_eq!(v.findings[0].issue, "wired bug");
    }

    /// Live-path Survivor (top-up): wiremock SSE reviewer repeating
    /// the SAME Fail verdict through the REAL parse/track chain, fixer
    /// returning the artifact unchanged — asserts `Survivor`@3 with zero
    /// scripted verdicts. Everything except live-model wording is exercised:
    /// stream → verdict_text → strict parse → FindingId → tracker.
    #[tokio::test]
    async fn live_path_survivor_fires_at_three_strikes() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let dir = tempfile::tempdir().unwrap();
        let rev_server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse_text(&fail_json())),
            )
            .mount(&rev_server)
            .await;
        let fix_server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse_text("artifact-0")),
            )
            .mount(&fix_server)
            .await;
        let bus = Arc::new(Bus::flat());
        let workdir = polymorph_delegate::WorkdirSpec::InPlace(dir.path().to_path_buf());
        let reviewer = BusReviewer {
            bus: bus.clone(),
            runner: Arc::new(ReviewTurnRunner {
                client: polymorph_llm::LlmClient::new(),
                target: LlmTarget::Fixed(mock_spec(&rev_server)),
                system: "sys".to_string(),
                pinned: Default::default(),
            }),
            workdir: workdir.clone(),
            artifact_path: None,
        };
        let executor = BusExecutor {
            bus,
            runner: Arc::new(FixTurnRunner {
                client: polymorph_llm::LlmClient::new(),
                target: LlmTarget::Fixed(mock_spec(&fix_server)),
                system: "sys".to_string(),
                pinned: Default::default(),
            }),
            workdir,
            tools: Vec::new(),
            tool_outcomes: Arc::new(std::sync::Mutex::new(Vec::new())),
            merge: None,
            artifact_path: None,
            peripherals: None,
            artifact_set: None,
        };
        let report = crate::run_loop(
            &LoopConfig {
                max_iterations: 10,
                surviving_finding_escalation: 3,
            },
            "goal",
            "criteria",
            "artifact-0",
            crate::LoopMode::Act,
            &reviewer,
            &executor,
        )
        .await;
        match &report.outcome {
            crate::Outcome::Escalated {
                reason: crate::EscalateReason::Survivor(found),
                rounds: 3,
            } => assert_eq!(found.issue, "wired bug"),
            other => panic!("expected live-path Survivor@3, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fix_runner_returns_rewritten_artifact() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse_text("fixed artifact v2")),
            )
            .mount(&server)
            .await;
        let runner = FixTurnRunner {
            client: polymorph_llm::LlmClient::new(),
            target: LlmTarget::Fixed(mock_spec(&server)),
            system: "sys".to_string(),
            pinned: Default::default(),
        };
        let out = runner.run_turn(turn_ctx()).await.unwrap();
        assert!(out.done);
        assert_eq!(out.data.as_ref().unwrap()["artifact"], "fixed artifact v2");
    }

    #[tokio::test]
    async fn bus_reviewer_returns_typed_fail() {
        let dir = tempfile::tempdir().unwrap();
        let runner = Arc::new(StubRunner::new(vec![done_out(
            &fail_json(),
            serde_json::json!({ "verdict_text": fail_json() }),
        )]));
        let reviewer = BusReviewer {
            bus: Arc::new(Bus::flat()),
            runner,
            workdir: polymorph_delegate::WorkdirSpec::InPlace(dir.path().to_path_buf()),
            artifact_path: None,
        };
        let v = reviewer
            .review(ReviewerInput {
                goal: "g".into(),
                criteria: "c".into(),
                artifact: "a".into(),
                mode: LoopMode::Act,
            })
            .await
            .unwrap();
        assert_eq!(v.findings.len(), 1);
        assert_eq!(v.findings[0].issue, "wired bug");
    }

    #[tokio::test]
    async fn bus_reviewer_maps_prose_child_to_invalid() {
        let dir = tempfile::tempdir().unwrap();
        let runner = Arc::new(StubRunner::new(vec![done_out(
            "looks fine, ship it",
            serde_json::json!({ "verdict_text": "looks fine, ship it" }),
        )]));
        let reviewer = BusReviewer {
            bus: Arc::new(Bus::flat()),
            runner,
            workdir: polymorph_delegate::WorkdirSpec::InPlace(dir.path().to_path_buf()),
            artifact_path: None,
        };
        let err = reviewer
            .review(ReviewerInput {
                goal: "g".into(),
                criteria: "".into(),
                artifact: "a".into(),
                mode: LoopMode::Act,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, ReviewerError::Invalid(_)), "{err:?}");
    }

    #[tokio::test]
    async fn bus_reviewer_extracts_verdict_from_prefixed_or_fenced_child() {
        let dir = tempfile::tempdir().unwrap();
        let prefixed = format!("Building your widget — wiring up.\n{}", fail_json());
        let runner = Arc::new(StubRunner::new(vec![done_out(
            &prefixed,
            serde_json::json!({ "verdict_text": prefixed }),
        )]));
        let reviewer = BusReviewer {
            bus: Arc::new(Bus::flat()),
            runner,
            workdir: polymorph_delegate::WorkdirSpec::InPlace(dir.path().to_path_buf()),
            artifact_path: None,
        };
        let v = reviewer
            .review(ReviewerInput {
                goal: "g".into(),
                criteria: "c".into(),
                artifact: "a".into(),
                mode: LoopMode::Act,
            })
            .await
            .unwrap();
        assert_eq!(v.findings.len(), 1);
        assert_eq!(v.findings[0].issue, "wired bug");
    }

    #[tokio::test]
    async fn bus_executor_returns_text_and_bails_without_key() {
        let dir = tempfile::tempdir().unwrap();
        let bus = Arc::new(Bus::flat());
        let ok_runner = Arc::new(StubRunner::new(vec![done_out(
            "fixed!",
            serde_json::json!({ "artifact": "fixed!", "tool_outcomes": [{"tool": "read", "outcome": "ok"}] }),
        )]));
        let outcomes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let ex = BusExecutor {
            bus: bus.clone(),
            runner: ok_runner,
            workdir: polymorph_delegate::WorkdirSpec::InPlace(dir.path().to_path_buf()),
            tools: Vec::new(),
            tool_outcomes: outcomes.clone(),
            merge: None,
            artifact_path: None,
            peripherals: None,
            artifact_set: None,
        };
        let out = ex
            .execute(ExecutorInput {
                goal: "g".into(),
                criteria: String::new(),
                artifact: "v1".into(),
                findings: Vec::new(),
                mode: LoopMode::Act,
            })
            .await
            .unwrap();
        assert_eq!(out, "fixed!");
        // The child's outcome array lands in the shared ledger.
        assert_eq!(
            *outcomes.lock().unwrap(),
            vec![serde_json::json!({"tool": "read", "outcome": "ok"})]
        );
        // A child that forgets the artifact key is an executor failure,
        // which the loop escalates (never silently kept).
        let bad_runner = Arc::new(StubRunner::new(vec![done_out(
            "oops",
            serde_json::json!({}),
        )]));
        let ex = BusExecutor {
            bus,
            runner: bad_runner,
            workdir: polymorph_delegate::WorkdirSpec::InPlace(dir.path().to_path_buf()),
            tools: Vec::new(),
            tool_outcomes: Arc::new(std::sync::Mutex::new(Vec::new())),
            merge: None,
            artifact_path: None,
            peripherals: None,
            artifact_set: None,
        };
        assert!(ex
            .execute(ExecutorInput {
                goal: "g".into(),
                criteria: String::new(),
                artifact: "v1".into(),
                findings: Vec::new(),
                mode: LoopMode::Act,
            })
            .await
            .is_err());
    }

    /// Empty-text fallback: a child that edits the file but
    /// returns blank text grades the FILE work, not the empty string.
    #[tokio::test]
    async fn bus_executor_falls_back_to_file_on_empty_text() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "fixed on disk\n").unwrap();
        let ex = BusExecutor {
            bus: Arc::new(Bus::flat()),
            runner: Arc::new(StubRunner::new(vec![done_out(
                "",
                serde_json::json!({ "artifact": "" }),
            )])),
            workdir: polymorph_delegate::WorkdirSpec::InPlace(dir.path().to_path_buf()),
            tools: Vec::new(),
            tool_outcomes: Arc::new(std::sync::Mutex::new(Vec::new())),
            merge: None,
            artifact_path: Some(std::path::PathBuf::from("a.txt")),
            peripherals: None,
            artifact_set: None,
        };
        let out = ex
            .execute(ExecutorInput {
                goal: "g".into(),
                criteria: String::new(),
                artifact: "v1".into(),
                findings: Vec::new(),
                mode: LoopMode::Act,
            })
            .await
            .unwrap();
        assert_eq!(out, "fixed on disk\n");
    }

    /// File-first, always (drive false-pass lesson): a child that wrote the
    /// file grades the FILE, even when its closing text is non-empty; a child
    /// that only *narrated* the change grades the unchanged FILE too, so a
    /// no-op turn can no longer pass on its own prose. Text is graded only
    /// when no artifact path is known (pure-text loops).
    #[tokio::test]
    async fn bus_executor_grades_file_when_child_mutated_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "fixed on disk\n").unwrap();
        let make = |mutated: bool| BusExecutor {
            bus: Arc::new(Bus::flat()),
            runner: Arc::new(StubRunner::new(vec![done_out(
                "<｜tool call begin｜>write",
                serde_json::json!({ "artifact": "<｜tool call begin｜>write", "mutated_files": mutated }),
            )])),
            workdir: polymorph_delegate::WorkdirSpec::InPlace(dir.path().to_path_buf()),
            tools: Vec::new(),
            tool_outcomes: Arc::new(std::sync::Mutex::new(Vec::new())),
            merge: None,
            artifact_path: Some(std::path::PathBuf::from("a.txt")),
            peripherals: None,
            artifact_set: None,
        };
        let input = || ExecutorInput {
            goal: "g".into(),
            criteria: String::new(),
            artifact: "v1".into(),
            findings: Vec::new(),
            mode: LoopMode::Act,
        };
        assert_eq!(
            make(true).execute(input()).await.unwrap(),
            "fixed on disk\n"
        );
        // No mutation: the narration must NOT become the graded artifact.
        assert_eq!(
            make(false).execute(input()).await.unwrap(),
            "fixed on disk\n"
        );
    }

    /// Without an artifact path (pure-text loops) the child's text is still the
    /// artifact — file-first must not turn those loops into stale-file graders.
    #[tokio::test]
    async fn bus_executor_grades_text_when_no_artifact_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "old\n").unwrap();
        let ex = BusExecutor {
            bus: Arc::new(Bus::flat()),
            runner: Arc::new(StubRunner::new(vec![done_out(
                "new text",
                serde_json::json!({ "artifact": "new text", "mutated_files": false }),
            )])),
            workdir: polymorph_delegate::WorkdirSpec::InPlace(dir.path().to_path_buf()),
            tools: Vec::new(),
            tool_outcomes: Arc::new(std::sync::Mutex::new(Vec::new())),
            merge: None,
            artifact_path: None,
            peripherals: None,
            artifact_set: None,
        };
        let out = ex
            .execute(ExecutorInput {
                goal: "g".into(),
                criteria: String::new(),
                artifact: "v1".into(),
                findings: Vec::new(),
                mode: LoopMode::Act,
            })
            .await
            .unwrap();
        assert_eq!(out, "new text");
    }

    /// Evidence attaches on finding-bearing turns (harness-observed file
    /// facts + tool ledger ride the scored text to the reviewer).
    #[tokio::test]
    async fn bus_executor_appends_evidence_on_findings() {
        use polymorph_core::{Finding, Severity};
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("game.py"), "print(1)\nprint(2)\n").unwrap();
        let ex = BusExecutor {
            bus: Arc::new(Bus::flat()),
            runner: Arc::new(StubRunner::new(vec![done_out(
                "done",
                serde_json::json!({ "artifact": "done" }),
            )])),
            workdir: polymorph_delegate::WorkdirSpec::InPlace(dir.path().to_path_buf()),
            tools: Vec::new(),
            tool_outcomes: Arc::new(std::sync::Mutex::new(Vec::new())),
            merge: None,
            artifact_path: None,
            peripherals: None,
            artifact_set: None,
        };
        let out = ex
            .execute(ExecutorInput {
                goal: "g".into(),
                criteria: String::new(),
                artifact: "v1".into(),
                findings: vec![Finding {
                    severity: Severity::P0,
                    file: "game.py".into(),
                    line: Some(1),
                    issue: "missing".into(),
                    proposed_fix: "write it".into(),
                }],
                mode: LoopMode::Act,
            })
            .await
            .unwrap();
        assert!(out.starts_with("done"), "{out}");
        assert!(out.contains("HARNESS EVIDENCE"), "{out}");
        assert!(out.contains("game.py: EXISTS (2 lines)"), "{out}");
        assert!(out.contains("tools: none recorded"), "{out}");
    }

    /// Missing finding files are named MISSING, never errors.
    #[test]
    fn harness_evidence_names_missing_files() {
        use polymorph_core::{Finding, Severity};
        let dir = tempfile::tempdir().unwrap();
        let text = render_harness_evidence(
            &polymorph_delegate::WorkdirSpec::InPlace(dir.path().to_path_buf()),
            &[Finding {
                severity: Severity::P1,
                file: "ghost.py".into(),
                line: None,
                issue: "x".into(),
                proposed_fix: "y".into(),
            }],
            &serde_json::json!({}),
        );
        assert!(text.contains("ghost.py: MISSING"), "{text}");
    }

    fn one_finding(file: &str) -> Vec<polymorph_core::Finding> {
        vec![polymorph_core::Finding {
            severity: polymorph_core::Severity::P1,
            file: file.into(),
            line: None,
            issue: "x".into(),
            proposed_fix: "y".into(),
        }]
    }

    /// Turns that neither ran a command nor mutated
    /// render byte-identically to pre-#6 (tool-free and read-only ledgers).
    #[test]
    fn harness_evidence_without_commands_or_mutations_is_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let wd = polymorph_delegate::WorkdirSpec::InPlace(dir.path().to_path_buf());
        let findings = one_finding("ghost.py");
        assert_eq!(
            render_harness_evidence(&wd, &findings, &serde_json::json!({})),
            "\n\nHARNESS EVIDENCE (harness-observed, not model claims):\n\
             - tools: none recorded this turn\n- ghost.py: MISSING\n"
        );
        assert_eq!(
            render_harness_evidence(
                &wd,
                &findings,
                &serde_json::json!({ "tool_outcomes": [
                    {"tool": "read", "outcome": "ok"},
                    {"tool": "edit", "outcome": "edit-stale"},
                    {"tool": "read", "outcome": "ok"},
                ]})
            ),
            "\n\nHARNESS EVIDENCE (harness-observed, not model claims):\n\
             - tools: edit (edit-stale): 1, read (ok): 2\n- ghost.py: MISSING\n"
        );
    }

    /// Nonzero exits are flagged per command, and an edit after the last
    /// command that ran is named with its files.
    #[test]
    fn harness_evidence_flags_nonzero_exit_and_late_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let text = render_harness_evidence(
            &polymorph_delegate::WorkdirSpec::InPlace(dir.path().to_path_buf()),
            &one_finding("ghost.py"),
            &serde_json::json!({ "tool_outcomes": [
                {"tool": "write", "outcome": "ok", "paths": ["src/a.rs"]},
                {"tool": "bash", "outcome": "ok", "cmd": "cargo build", "exit_code": 101},
                {"tool": "bash", "outcome": "ok", "cmd": "cargo test", "exit_code": 0},
                {"tool": "edit", "outcome": "ok", "paths": ["src/b.rs", "src/a.rs"]},
                {"tool": "bash", "outcome": "denied", "cmd": "rm -rf /"},
            ]}),
        );
        assert!(
            text.contains("  #2 exit 101 NONZERO: cargo build\n"),
            "{text}"
        );
        assert!(text.contains("  #3 exit 0: cargo test\n"), "{text}");
        assert!(
            text.contains("  #5 denied, no exit code: rm -rf /\n"),
            "{text}"
        );
        // The denied #5 verified nothing: #3 is the last command that ran.
        assert!(
            text.contains("- MUTATIONS AFTER LAST COMMAND: yes - files: src/b.rs, src/a.rs\n"),
            "{text}"
        );
        assert!(
            text.contains("- files mutated this turn (2): src/a.rs, src/b.rs\n"),
            "{text}"
        );
        // Ledger lines precede the per-finding file facts.
        assert!(text.find("MUTATIONS").unwrap() < text.find("ghost.py").unwrap());
    }

    #[test]
    fn harness_evidence_says_no_when_commands_follow_mutations() {
        let dir = tempfile::tempdir().unwrap();
        let wd = polymorph_delegate::WorkdirSpec::InPlace(dir.path().to_path_buf());
        let text = render_harness_evidence(
            &wd,
            &one_finding("ghost.py"),
            &serde_json::json!({ "tool_outcomes": [
                {"tool": "edit", "outcome": "ok", "paths": ["a.py"]},
                {"tool": "exec", "outcome": "ok", "cmd": "make check"},
                // All-noop edit after the command: nothing changed.
                {"tool": "edit", "outcome": "ok", "paths": []},
            ]}),
        );
        assert!(text.contains("  #2 exit unknown: make check\n"), "{text}");
        assert!(
            text.contains("- MUTATIONS AFTER LAST COMMAND: no\n"),
            "{text}"
        );
        assert!(
            text.contains("- files mutated this turn (1): a.py\n"),
            "{text}"
        );
        // Mutations with no command at all: unverified, said plainly.
        let text = render_harness_evidence(
            &wd,
            &one_finding("ghost.py"),
            &serde_json::json!({ "tool_outcomes": [
                {"tool": "write", "outcome": "ok", "paths": ["a.py"]},
            ]}),
        );
        assert!(text.contains("- commands: NONE ran this turn"), "{text}");
        assert!(
            text.contains("- MUTATIONS AFTER LAST COMMAND: n/a - no command ran"),
            "{text}"
        );
        // Commands without mutations.
        let text = render_harness_evidence(
            &wd,
            &one_finding("ghost.py"),
            &serde_json::json!({ "tool_outcomes": [
                {"tool": "bash", "outcome": "ok", "cmd": "pytest", "exit_code": 0,
                 "zero_tests": "no tests ran"},
            ]}),
        );
        assert!(
            text.contains(
                "  #1 exit 0: pytest [HEURISTIC, not a verdict: output suggests zero tests ran (\"no tests ran\")]\n"
            ),
            "{text}"
        );
        assert!(
            text.contains("- MUTATIONS AFTER LAST COMMAND: no\n"),
            "{text}"
        );
        assert!(text.contains("- files mutated this turn: none\n"), "{text}");
    }

    #[test]
    fn harness_evidence_caps_commands_keeping_the_latest() {
        let dir = tempfile::tempdir().unwrap();
        let outcomes: Vec<serde_json::Value> = (0..15)
            .map(|i| serde_json::json!({"tool": "bash", "outcome": "ok", "cmd": format!("c{i}"), "exit_code": 0}))
            .collect();
        let text = render_harness_evidence(
            &polymorph_delegate::WorkdirSpec::InPlace(dir.path().to_path_buf()),
            &one_finding("ghost.py"),
            &serde_json::json!({ "tool_outcomes": outcomes }),
        );
        assert!(text.contains("  (3 earlier commands omitted)\n"), "{text}");
        assert!(!text.contains(": c2\n"), "{text}");
        assert!(text.contains("  #4 exit 0: c3\n"), "{text}");
        assert!(text.contains("  #15 exit 0: c14\n"), "{text}");
    }

    #[test]
    fn zero_test_marker_heuristic() {
        assert_eq!(
            zero_test_marker("> No tests found for given includes: [X]"),
            Some("No tests found for given includes")
        );
        assert_eq!(
            zero_test_marker("===== no tests ran in 0.01s ====="),
            Some("no tests ran")
        );
        assert_eq!(
            zero_test_marker("collected 0 items\n"),
            Some("collected 0 items")
        );
        // Cargo: all harnesses empty → flagged; any non-empty harness (the
        // usual green run with empty doc-tests) → not flagged.
        assert_eq!(
            zero_test_marker("running 0 tests\n\ntest result: ok. 0 passed\n"),
            Some("running 0 tests")
        );
        assert_eq!(
            zero_test_marker("running 3 tests\n...\n   Doc-tests x\n\nrunning 0 tests\n"),
            None
        );
        assert_eq!(zero_test_marker("running 1 test\n"), None);
        assert_eq!(zero_test_marker("BUILD SUCCESSFUL in 2s"), None);
    }

    #[test]
    fn command_summary_is_single_line_and_char_bounded() {
        assert_eq!(
            summarize_command("  cd x &&\n\tcargo  test\r\n"),
            "cd x && cargo test"
        );
        let long = "é".repeat(500);
        let s = summarize_command(&long);
        assert_eq!(s.chars().count(), MAX_CMD_SUMMARY_CHARS);
        assert!(s.ends_with("..."));
        assert_eq!(summarize_command(""), "");
    }

    #[test]
    fn ledger_extras_shapes() {
        let bash_ok = ledger_extras(
            "bash",
            &serde_json::json!({"cmd": "gradle test\n--tests X"}),
            Some(&serde_json::json!({
                "stdout": "", "stderr": "No tests found for given includes", "exit_code": 0
            })),
        );
        assert_eq!(
            serde_json::Value::Object(bash_ok),
            serde_json::json!({
                "cmd": "gradle test --tests X",
                "exit_code": 0,
                "zero_tests": "No tests found for given includes",
            })
        );
        // Denied/failed command: named, no exit code (it never ran).
        let denied = ledger_extras("bash", &serde_json::json!({"cmd": "ls"}), None);
        assert_eq!(
            serde_json::Value::Object(denied),
            serde_json::json!({"cmd": "ls"})
        );
        // exec with `command` and no exit code in its result.
        let exec = ledger_extras(
            "exec",
            &serde_json::json!({"command": "make"}),
            Some(&serde_json::json!({"output": "ok"})),
        );
        assert_eq!(
            serde_json::Value::Object(exec),
            serde_json::json!({"cmd": "make"})
        );
        let write = ledger_extras(
            "write",
            &serde_json::json!({"path": "a.txt", "content": "x"}),
            Some(&serde_json::json!({"bytes_written": 1})),
        );
        assert_eq!(
            serde_json::Value::Object(write),
            serde_json::json!({"paths": ["a.txt"]})
        );
        // Noop edits mutated nothing.
        let edit = ledger_extras(
            "edit",
            &serde_json::json!({"patch": "..."}),
            Some(&serde_json::json!({"files": [
                {"path": "a.txt", "action": "Edited { noop: false }"},
                {"path": "b.txt", "action": "Edited { noop: true }"},
                {"path": "c.txt", "action": "Deleted"},
            ]})),
        );
        assert_eq!(
            serde_json::Value::Object(edit),
            serde_json::json!({"paths": ["a.txt", "c.txt"]})
        );
        // Failed mutation and non-ledger tools: no extras.
        assert!(ledger_extras("write", &serde_json::json!({"path": "a"}), None).is_empty());
        assert!(ledger_extras(
            "read",
            &serde_json::json!({"path": "a"}),
            Some(&serde_json::json!({}))
        )
        .is_empty());
    }

    /// File-first even for a pure-text turn (drive false-pass fix): with an
    /// artifact path the file decides, so a child that only *narrates* a fix
    /// cannot pass the reviewer on its own prose. The stale file is the honest
    /// artifact — the fix is to write it, not to grade the narration.
    #[tokio::test]
    async fn bus_executor_grades_file_over_nonempty_text() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "stale\n").unwrap();
        let ex = BusExecutor {
            bus: Arc::new(Bus::flat()),
            runner: Arc::new(StubRunner::new(vec![done_out(
                "fixed!",
                serde_json::json!({ "artifact": "fixed!" }),
            )])),
            workdir: polymorph_delegate::WorkdirSpec::InPlace(dir.path().to_path_buf()),
            tools: Vec::new(),
            tool_outcomes: Arc::new(std::sync::Mutex::new(Vec::new())),
            merge: None,
            artifact_path: Some(std::path::PathBuf::from("a.txt")),
            peripherals: None,
            artifact_set: None,
        };
        let out = ex
            .execute(ExecutorInput {
                goal: "g".into(),
                criteria: String::new(),
                artifact: "v1".into(),
                findings: Vec::new(),
                mode: LoopMode::Act,
            })
            .await
            .unwrap();
        assert_eq!(out, "stale\n");
    }

    /// Unreadable path degrades to the child text (best-effort never
    /// fails the loop — warn-and-score-as-is).
    #[tokio::test]
    async fn bus_executor_unreadable_path_keeps_text() {
        let dir = tempfile::tempdir().unwrap();
        let ex = BusExecutor {
            bus: Arc::new(Bus::flat()),
            runner: Arc::new(StubRunner::new(vec![done_out(
                "sometext",
                serde_json::json!({ "artifact": "sometext" }),
            )])),
            workdir: polymorph_delegate::WorkdirSpec::InPlace(dir.path().to_path_buf()),
            tools: Vec::new(),
            tool_outcomes: Arc::new(std::sync::Mutex::new(Vec::new())),
            merge: None,
            artifact_path: Some(std::path::PathBuf::from("ghost.txt")),
            peripherals: None,
            artifact_set: None,
        };
        let out = ex
            .execute(ExecutorInput {
                goal: "g".into(),
                criteria: String::new(),
                artifact: "v1".into(),
                findings: Vec::new(),
                mode: LoopMode::Act,
            })
            .await
            .unwrap();
        assert_eq!(out, "sometext");
    }

    /// Pure helper pins (no spawn): relative resolves against InPlace,
    /// absolute passes through, missing reads None, Worktree specs skip.
    #[test]
    fn artifact_from_file_resolution_rules() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "body\n").unwrap();
        let inplace = polymorph_delegate::WorkdirSpec::InPlace(dir.path().to_path_buf());
        assert_eq!(
            artifact_from_file(&inplace, std::path::Path::new("a.txt")),
            Some("body\n".to_string())
        );
        let abs = dir.path().join("a.txt");
        assert_eq!(
            artifact_from_file(&inplace, &abs),
            Some("body\n".to_string())
        );
        assert_eq!(
            artifact_from_file(&inplace, std::path::Path::new("ghost")),
            None
        );
        let wt = polymorph_delegate::WorkdirSpec::Worktree {
            repo: dir.path().to_path_buf(),
            parent: dir.path().to_path_buf(),
            mode: polymorph_delegate::worktree::WorktreeMode::Detached,
        };
        assert_eq!(artifact_from_file(&wt, std::path::Path::new("a.txt")), None);
    }

    #[tokio::test]
    async fn full_act_loop_over_bus_backed_scripted_runners() {
        // The F1↔F2 seam live: run_loop drives Bus adapters whose children
        // are scripted (fail once with one finding, then pass).
        let dir = tempfile::tempdir().unwrap();
        let bus = Arc::new(Bus::flat());
        let reviewer = BusReviewer {
            bus: bus.clone(),
            runner: Arc::new(StubRunner::new(vec![
                done_out(
                    &fail_json(),
                    serde_json::json!({ "verdict_text": fail_json() }),
                ),
                done_out(
                    &pass_json(),
                    serde_json::json!({ "verdict_text": pass_json() }),
                ),
            ])),
            workdir: polymorph_delegate::WorkdirSpec::InPlace(dir.path().to_path_buf()),
            artifact_path: None,
        };
        let executor = BusExecutor {
            bus,
            runner: Arc::new(StubRunner::new(vec![done_out(
                "fixed artifact",
                serde_json::json!({ "artifact": "fixed artifact" }),
            )])),
            workdir: polymorph_delegate::WorkdirSpec::InPlace(dir.path().to_path_buf()),
            tools: Vec::new(),
            tool_outcomes: Arc::new(std::sync::Mutex::new(Vec::new())),
            merge: None,
            artifact_path: None,
            peripherals: None,
            artifact_set: None,
        };
        let cfg = LoopConfig {
            max_iterations: 5,
            surviving_finding_escalation: 3,
        };
        let rep = run_loop(
            &cfg,
            "goal",
            "crit",
            "v1",
            LoopMode::Act,
            &reviewer,
            &executor,
        )
        .await;
        assert_eq!(rep.outcome, Outcome::Passed { rounds: 2 });
        assert_eq!(rep.rounds_log.len(), 2);
    }

    #[test]
    fn review_brief_carries_mode_and_artifact_path() {
        // Live run #5: the reviewer guessed filenames because the brief had
        // no path. The brief must name the mode and, when known, the path.
        let with_path = render_review_brief(
            &ReviewerInput {
                goal: "g".into(),
                criteria: "c".into(),
                artifact: "a".into(),
                mode: LoopMode::Act,
            },
            Some(std::path::Path::new("src/artifact.rs")),
        );
        assert!(with_path.contains("MODE:\nact"), "{with_path}");
        assert!(with_path.contains("src/artifact.rs"), "{with_path}");
        assert!(
            with_path.contains("ARTIFACT UNDER REVIEW:\na"),
            "{with_path}"
        );
        let bare = render_review_brief(
            &ReviewerInput {
                goal: "g".into(),
                criteria: "".into(),
                artifact: "a".into(),
                mode: LoopMode::Plan,
            },
            None,
        );
        assert!(bare.contains("MODE:\nplan"), "{bare}");
        assert!(!bare.contains("ARTIFACT PATH:"), "{bare}");
    }

    #[test]
    fn review_brief_carries_verdict_discipline() {
        // Every brief orders findings to cite their evidence or
        // be dropped — the VkDevice hallucination cited a type it never saw.
        let brief = render_review_brief(
            &ReviewerInput {
                goal: "g".into(),
                criteria: "c".into(),
                artifact: "a".into(),
                mode: LoopMode::Act,
            },
            None,
        );
        assert!(brief.contains("VERDICT DISCIPLINE"), "{brief}");
        assert!(brief.contains("drop it, do not emit it"), "{brief}");
    }

    #[test]
    fn create_job_note_fires_only_for_missing_file_with_empty_artifact() {
        // The 9 "File MISSING" findings came from reviewing ""
        // without knowing the file was never written (CREATE jobs map
        // NotFound to empty). The note fires only in that gap.
        let dir = tempfile::tempdir().unwrap();
        let inplace = polymorph_delegate::WorkdirSpec::InPlace(dir.path().to_path_buf());
        let missing = std::path::Path::new("ghost-target.rs");
        let note = create_job_note(&inplace, missing, "").expect("note expected");
        assert!(note.contains("CREATE"), "{note}");
        assert!(note.contains("do NOT emit"), "{note}");
        // The note must prescribe the ONE legal failing shape — otherwise
        // the reviewer fails with zero findings (schema-invalid) and the
        // drive burns every round without the executor ever running.
        assert!(note.contains("exactly ONE finding"), "{note}");
        // Same missing file but content present: silent — a moved path
        // must never prime a false CREATE narrative.
        assert!(create_job_note(&inplace, missing, "some text").is_none());
        // Existing file + empty artifact: silent — brief byte-identical.
        std::fs::write(dir.path().join("real.rs"), "fn main() {}").unwrap();
        assert!(create_job_note(&inplace, std::path::Path::new("real.rs"), "").is_none());
    }

    #[test]
    fn fix_brief_carries_criteria_verbatim() {
        // Regression: the brief carried GOAL and the
        // findings but never the criteria the reviewer grades against.
        let criteria = "Signature: f(int) -> int
Verify:
  gradlew test --tests a.BTest";
        let brief = render_fix_brief(&ExecutorInput {
            goal: "g".into(),
            criteria: criteria.into(),
            artifact: "v1".into(),
            findings: Vec::new(),
            mode: LoopMode::Act,
        });
        assert!(
            brief.contains(&format!(
                "ACCEPTANCE CRITERIA:
{criteria}
"
            )),
            "{brief}"
        );
        let goal_at = brief.find("GOAL:").unwrap();
        let crit_at = brief.find("ACCEPTANCE CRITERIA:").unwrap();
        let artifact_at = brief.find("ARTIFACT:").unwrap();
        assert!(goal_at < crit_at && crit_at < artifact_at, "{brief}");
    }

    #[test]
    fn fix_brief_omits_empty_criteria() {
        let brief = render_fix_brief(&ExecutorInput {
            goal: "g".into(),
            criteria: "  
"
            .into(),
            artifact: "v1".into(),
            findings: Vec::new(),
            mode: LoopMode::Act,
        });
        assert!(!brief.contains("ACCEPTANCE CRITERIA"), "{brief}");
        assert!(
            brief.starts_with(
                "MODE:
act

GOAL:
g

ARTIFACT:
v1
"
            ),
            "{brief}"
        );
    }

    #[test]
    fn fix_brief_carries_mode() {
        let brief = render_fix_brief(&ExecutorInput {
            goal: "g".into(),
            criteria: String::new(),
            artifact: "v1".into(),
            findings: Vec::new(),
            mode: LoopMode::Act,
        });
        assert!(brief.contains("MODE:\nact"), "{brief}");
        assert!(brief.contains("GOAL:\ng"), "{brief}");
        assert!(brief.contains("DELIVERABLE"), "{brief}");
    }

    #[test]
    fn lsp_section_none_when_empty() {
        // LSP off (or clean consult) must leave the brief
        // byte-identical — never an empty header.
        assert!(render_lsp_section(&std::collections::BTreeMap::new()).is_none());
        let mut all_empty = std::collections::BTreeMap::new();
        all_empty.insert("file:///a.rs".to_string(), Vec::new());
        assert!(render_lsp_section(&all_empty).is_none());
    }

    #[test]
    fn lsp_section_renders_single_uri() {
        let mut m = std::collections::BTreeMap::new();
        m.insert(
            "file:///a.rs".to_string(),
            vec!["unused variable `x`".to_string()],
        );
        let s = render_lsp_section(&m).expect("section");
        assert!(s.contains("LSP DIAGNOSTICS"), "{s}");
        assert!(s.contains("file:///a.rs"), "{s}");
        assert!(s.contains("unused variable"), "{s}");
    }

    #[test]
    fn lsp_section_caps_items_and_names_omitted() {
        let mut m = std::collections::BTreeMap::new();
        let items: Vec<String> = (0..MAX_LSP_ITEMS_PER_URI + 3)
            .map(|i| format!("fault {i}"))
            .collect();
        m.insert("file:///a.rs".to_string(), items);
        let s = render_lsp_section(&m).expect("section");
        assert!(s.contains("3 more omitted"), "{s}");
        assert!(s.contains("fault 0"), "{s}");
        assert!(!s.contains("fault 13"), "{s}");
    }

    #[test]
    fn lsp_section_caps_uris_and_truncates_long_items() {
        let mut m = std::collections::BTreeMap::new();
        for i in 0..MAX_LSP_URIS + 2 {
            m.insert(
                format!("file:///{i:02}.rs"),
                vec!["e".to_string(), "x".repeat(500)],
            );
        }
        let s = render_lsp_section(&m).expect("section");
        assert!(s.contains("2 more files omitted"), "{s}");
        assert!(
            s.contains("…"),
            "long item must truncate with ellipsis: {s}"
        );
        // Deterministic BTree order: first URI wins the cap.
        assert!(s.contains("file:///00.rs"), "{s}");
        assert!(!s.contains("file:///09.rs"), "{s}");
    }

    #[tokio::test]
    async fn agentic_runner_enriches_brief_with_seeded_diagnostics() {
        // Seeded pool diagnostics reach the model's
        // first user message as an advisory; empty pool passes through.
        use std::collections::BTreeMap;
        struct CaptureTurn {
            seen: std::sync::Mutex<Vec<Vec<genai::chat::ChatMessage>>>,
        }
        #[async_trait]
        impl AgenticTurnFn for CaptureTurn {
            async fn turn(
                &self,
                messages: Vec<genai::chat::ChatMessage>,
                _tools: Vec<genai::chat::Tool>,
                _cancel: &tokio_util::sync::CancellationToken,
            ) -> anyhow::Result<polymorph_llm::CollectedTurn> {
                self.seen.lock().unwrap().push(messages);
                Ok(polymorph_llm::CollectedTurn {
                    text: "done".to_string(),
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                    aborted: false,
                    rule_hit: None,
                })
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let registry = std::sync::Arc::new(polymorph_tools::registry::v1_registry());
        let mut seed = BTreeMap::new();
        seed.insert(
            "file:///a.rs".to_string(),
            vec!["type mismatch".to_string()],
        );
        registry.test_seed_lsp_diagnostics(seed).await;
        let capture = std::sync::Arc::new(CaptureTurn {
            seen: std::sync::Mutex::new(Vec::new()),
        });
        let runner = AgenticFixRunner {
            turn: capture.clone(),
            registry,
            gate: polymorph_tools::Gate::new(vec![], vec![]),
            tools: Vec::new(),
            max_tool_rounds: 0,
            max_bonus_tool_rounds: DEFAULT_MAX_BONUS_TOOL_ROUNDS,
            advisor: None,
            restore: None,
            history: Default::default(),
            command_changes: 0,
        };
        let ctx = TurnContext {
            child_id: "c-lsp".to_string(),
            role: Role::Act,
            brief: "BASE-BRIEF".to_string(),
            workdir: dir.path().to_path_buf(),
            turn: 1,
            steer: Vec::new(),
            cancel: tokio_util::sync::CancellationToken::new(),
        };
        runner.run_turn(ctx).await.unwrap();
        let seen = capture.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        let first = format!("{:?}", seen[0][0]);
        assert!(first.contains("BASE-BRIEF"), "{first}");
        assert!(first.contains("LSP DIAGNOSTICS"), "{first}");
        assert!(first.contains("type mismatch"), "{first}");
    }

    #[test]
    fn lsp_findings_count_sums_messages() {
        // 0 when off/clean; plain sum otherwise.
        assert_eq!(lsp_findings_count(&std::collections::BTreeMap::new()), 0);
        let mut m = std::collections::BTreeMap::new();
        m.insert("file:///a.rs".to_string(), Vec::new());
        assert_eq!(lsp_findings_count(&m), 0);
        m.insert(
            "file:///b.rs".to_string(),
            vec!["e1".to_string(), "e2".to_string(), "e3".to_string()],
        );
        assert_eq!(lsp_findings_count(&m), 3);
    }

    #[tokio::test]
    async fn agentic_runner_records_lsp_findings_on_outcome() {
        // Warn-first signal rides the outcome data — 0 when the
        // pool is empty, N when seeded. Artifact text is unchanged either
        // way (advisory only, never a deny).
        async fn run_with_seed(
            seed: std::collections::BTreeMap<String, Vec<String>>,
        ) -> serde_json::Value {
            struct DoneTurn;
            #[async_trait]
            impl AgenticTurnFn for DoneTurn {
                async fn turn(
                    &self,
                    _messages: Vec<genai::chat::ChatMessage>,
                    _tools: Vec<genai::chat::Tool>,
                    _cancel: &tokio_util::sync::CancellationToken,
                ) -> anyhow::Result<polymorph_llm::CollectedTurn> {
                    Ok(polymorph_llm::CollectedTurn {
                        text: "fixed text".to_string(),
                        tool_calls: Vec::new(),
                        reasoning_content: None,
                        aborted: false,
                        rule_hit: None,
                    })
                }
            }
            let dir = tempfile::tempdir().unwrap();
            let registry = std::sync::Arc::new(polymorph_tools::registry::v1_registry());
            registry.test_seed_lsp_diagnostics(seed).await;
            let runner = AgenticFixRunner {
                turn: std::sync::Arc::new(DoneTurn),
                registry,
                gate: polymorph_tools::Gate::new(vec![], vec![]),
                tools: Vec::new(),
                max_tool_rounds: 0,
                max_bonus_tool_rounds: DEFAULT_MAX_BONUS_TOOL_ROUNDS,
                advisor: None,
                restore: None,
                history: Default::default(),
                command_changes: 0,
            };
            let out = runner
                .run_turn(TurnContext {
                    child_id: "c-findings".to_string(),
                    role: Role::Act,
                    brief: "brief".to_string(),
                    workdir: dir.path().to_path_buf(),
                    turn: 1,
                    steer: Vec::new(),
                    cancel: tokio_util::sync::CancellationToken::new(),
                })
                .await
                .unwrap();
            assert_eq!(out.text, "fixed text");
            out.data.unwrap()
        }
        let empty = run_with_seed(std::collections::BTreeMap::new()).await;
        assert_eq!(empty["lsp_findings"], 0);
        assert_eq!(empty["artifact"], "fixed text");
        let mut seed = std::collections::BTreeMap::new();
        seed.insert(
            "file:///a.rs".to_string(),
            vec!["boom".to_string(), "bam".to_string()],
        );
        let full = run_with_seed(seed).await;
        assert_eq!(full["lsp_findings"], 2);
        assert_eq!(full["artifact"], "fixed text");
    }

    #[tokio::test]
    async fn bus_reviewer_brief_reaches_child_with_path() {
        // The spawned child must SEE the enriched brief (path + mode), not
        // just the raw artifact — capture ctx.brief at the runner boundary.
        struct CaptureRunner {
            seen: std::sync::Mutex<Vec<String>>,
        }
        #[async_trait]
        impl TurnRunner for CaptureRunner {
            async fn run_turn(&self, ctx: TurnContext) -> anyhow::Result<TurnOutput> {
                self.seen.lock().unwrap().push(ctx.brief.clone());
                Ok(done_out(
                    &pass_json(),
                    serde_json::json!({ "verdict_text": pass_json() }),
                ))
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let capture = Arc::new(CaptureRunner {
            seen: std::sync::Mutex::new(Vec::new()),
        });
        let reviewer = BusReviewer {
            bus: Arc::new(Bus::flat()),
            runner: capture.clone(),
            workdir: polymorph_delegate::WorkdirSpec::InPlace(dir.path().to_path_buf()),
            artifact_path: Some(std::path::PathBuf::from("src/artifact.rs")),
        };
        reviewer
            .review(ReviewerInput {
                goal: "g".into(),
                criteria: "c".into(),
                artifact: "artifact-text".into(),
                mode: LoopMode::Act,
            })
            .await
            .unwrap();
        let seen = capture.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert!(seen[0].contains("src/artifact.rs"), "{}", seen[0]);
        assert!(seen[0].contains("MODE:\nact"), "{}", seen[0]);
        assert!(seen[0].contains("artifact-text"), "{}", seen[0]);
    }

    /// Scripted [`AgenticTurnFn`]: pops one collected turn per call and
    /// records every message history it was shown (exhaustion panics —
    /// a test that runs dry is a test bug, and it must be loud).
    struct StubAgenticTurn {
        script: std::sync::Mutex<VecDeque<polymorph_llm::CollectedTurn>>,
        seen: std::sync::Mutex<Vec<Vec<genai::chat::ChatMessage>>>,
        fail_with: Option<String>,
    }

    impl StubAgenticTurn {
        fn new(turns: Vec<polymorph_llm::CollectedTurn>) -> Self {
            Self {
                script: std::sync::Mutex::new(turns.into_iter().collect()),
                seen: std::sync::Mutex::new(Vec::new()),
                fail_with: None,
            }
        }

        fn failing(message: &str) -> Self {
            Self {
                script: std::sync::Mutex::new(VecDeque::new()),
                seen: std::sync::Mutex::new(Vec::new()),
                fail_with: Some(message.to_string()),
            }
        }
    }

    fn collected(text: &str, calls: Vec<genai::chat::ToolCall>) -> polymorph_llm::CollectedTurn {
        polymorph_llm::CollectedTurn {
            text: text.to_string(),
            tool_calls: calls,
            reasoning_content: None,
            aborted: false,
            rule_hit: None,
        }
    }

    fn tool_call(id: &str, name: &str, args: serde_json::Value) -> genai::chat::ToolCall {
        genai::chat::ToolCall {
            call_id: id.to_string(),
            fn_name: name.to_string(),
            fn_arguments: args,
            thought_signatures: None,
        }
    }

    #[async_trait]
    impl AgenticTurnFn for StubAgenticTurn {
        async fn turn(
            &self,
            messages: Vec<genai::chat::ChatMessage>,
            _tools: Vec<genai::chat::Tool>,
            _cancel: &tokio_util::sync::CancellationToken,
        ) -> anyhow::Result<polymorph_llm::CollectedTurn> {
            self.seen.lock().unwrap().push(messages);
            if let Some(msg) = &self.fail_with {
                return Err(anyhow::anyhow!("{msg}"));
            }
            Ok(self
                .script
                .lock()
                .unwrap()
                .pop_front()
                .expect("script exhausted"))
        }
    }

    fn agentic_runner(
        stub: Arc<StubAgenticTurn>,
        tools: Vec<String>,
        deny: Vec<String>,
        max_tool_rounds: u32,
    ) -> AgenticFixRunner {
        AgenticFixRunner {
            turn: stub,
            registry: Arc::new(polymorph_tools::registry::v1_registry()),
            gate: polymorph_tools::Gate::new(vec![], deny),
            tools,
            max_tool_rounds,
            max_bonus_tool_rounds: DEFAULT_MAX_BONUS_TOOL_ROUNDS,
            advisor: None,
            restore: None,
            history: Default::default(),
            command_changes: 0,
        }
    }

    fn agentic_ctx(dir: &tempfile::TempDir, brief: &str) -> TurnContext {
        TurnContext {
            child_id: "c-agentic".to_string(),
            role: Role::Act,
            brief: brief.to_string(),
            workdir: dir.path().to_path_buf(),
            turn: 1,
            steer: Vec::new(),
            cancel: tokio_util::sync::CancellationToken::new(),
        }
    }

    /// Tool-response contents fed back in one history (in feed order).
    fn fed_responses(history: &[genai::chat::ChatMessage]) -> Vec<String> {
        use genai::chat::ContentPart;
        history
            .iter()
            .flat_map(|m| m.content.parts())
            .filter_map(|p| match p {
                ContentPart::ToolResponse(r) => Some(r.content.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn genai_tool_definitions_carry_schemas() {
        let r = polymorph_tools::registry::v1_registry();
        let tools = to_genai_tools(&r, &["read".to_string(), "frobnicate".to_string()]);
        // Allowlisted + known only: the unknown name is skipped here and
        // refused (recoverably) at call time.
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name.as_str(), "read");
        assert!(tools[0]
            .description
            .as_deref()
            .unwrap_or("")
            .contains("file"));
        assert!(tools[0].schema.is_some());
        // Preamble documents the flow the runner enforces (tested by
        // reading, not by word-matching — pin existence + tag mention).
        assert!(TOOLS_PREAMBLE.contains("tag"));
    }

    #[test]
    fn genai_tool_definitions_carry_mcp_schemas() {
        // Enrolled server tools flow to the model through the
        // same `to_genai_tools` call — instance schemas first, V1 static
        // second, unknowns still skipped.
        use polymorph_tools::mcp::{enroll_mcp_tools, McpToolInfo};
        let mut r = polymorph_tools::registry::v1_registry();
        enroll_mcp_tools(
            &mut r,
            "probe",
            &[McpToolInfo {
                name: "t".into(),
                description: "server tool".into(),
                schema: serde_json::json!({"type": "object", "required": ["q"]}),
            }],
        )
        .unwrap();
        let tools = to_genai_tools(
            &r,
            &[
                "mcp__probe__t".to_string(),
                "read".to_string(),
                "frobnicate".to_string(),
            ],
        );
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name.as_str(), "mcp__probe__t");
        assert!(tools[0].schema.is_some());
        assert_eq!(tools[1].name.as_str(), "read");
    }

    #[test]
    fn lsp_tools_opt_in_through_fixer_allowlist() {
        // No binding change — the fixer wields the LSP
        // tools exactly when the allowlist names them (reviewers keep
        // `tools = []`, so they never see these). Schemas ride along.
        let r = polymorph_tools::registry::v1_registry();
        let tools = to_genai_tools(&r, &["lsp_hover".to_string(), "lsp_references".to_string()]);
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name.as_str(), "lsp_hover");
        assert!(tools[0].schema.is_some());
        assert_eq!(tools[1].name.as_str(), "lsp_references");
        // Absent from the allowlist = absent from the definitions.
        let bare = to_genai_tools(&r, &["read".to_string()]);
        assert!(bare.iter().all(|t| t.name.as_str() != "lsp_hover"));
    }

    #[test]
    fn minimal_preamble_keeps_intro_and_ban_only() {
        // The minimal variant must keep the tool intro
        // (otherwise the run measures tools-off, not preamble) + the raw-only
        // ban (the hypothesized carrier) — and NOTHING else. Absence asserts
        // keep it from silently regrowing the dropped teaching.
        assert!(TOOLS_PREAMBLE_MIN.contains("You have tools"), "tool intro");
        assert!(
            TOOLS_PREAMBLE_MIN.contains("raw patch text ONLY"),
            "fence/prose ban"
        );
        assert!(TOOLS_PREAMBLE_MIN.contains("[PATH#TAG]"), "tag shape");
        for dropped in [
            "PUT 12:",
            "+`-sigil",
            "[src/main.rs#a1b2]",
            "StaleTag",
            "CUT N",
        ] {
            assert!(!TOOLS_PREAMBLE_MIN.contains(dropped), "dropped: {dropped}");
        }
        // Selection is a pure fn (call sites pass bools, never strings).
        assert_eq!(select_preamble(false), TOOLS_PREAMBLE);
        assert_eq!(select_preamble(true), TOOLS_PREAMBLE_MIN);
    }

    #[test]
    fn tools_preamble_hardens_patch_wellformedness() {
        // The in-loop
        // fixer sees act.md + this preamble, never the full edit.md grammar.
        // Pin the teaching that must survive future edits — raw-only,
        // header shape, op shape, sigil, and the failure actions.
        assert!(
            TOOLS_PREAMBLE.contains("raw patch text ONLY"),
            "fence/prose ban"
        );
        assert!(TOOLS_PREAMBLE.contains("[PATH#TAG]"), "header shape");
        assert!(TOOLS_PREAMBLE.contains("PUT 12:"), "op shape");
        assert!(TOOLS_PREAMBLE.contains("+`-sigil"), "body sigil");
        assert!(
            TOOLS_PREAMBLE.contains("[src/main.rs#a1b2]"),
            "positive example"
        );
        assert!(TOOLS_PREAMBLE.contains("StaleTag"), "failure action");
    }

    #[tokio::test]
    async fn fenced_patch_labels_edit_parse_and_raw_applies() {
        // Production shape (Registry::call_edit → .context chain →
        // classify_edit_error): a fenced patch — the eval-proven default
        // model habit (eval.rs extract_patch tolerates one fence, the tool
        // does not) — must label `edit-parse`, while the identical raw
        // patch applies. Documents the prime suspect without logging
        // patch text (labels only, never values).
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "l1\nl2\n").unwrap();
        let registry = std::sync::Arc::new(polymorph_tools::registry::v1_registry());
        let text = polymorph_tools::hashline::apply::normalize(
            &std::fs::read(dir.path().join("a.txt")).unwrap(),
        )
        .text;
        let tag = registry
            .mint_tag(std::path::Path::new("a.txt"), &text)
            .await;
        let tool_ctx = polymorph_tools::ToolCallContext::new(
            dir.path().to_path_buf(),
            polymorph_tools::Gate::new(vec![], vec![]),
            tokio_util::sync::CancellationToken::new(),
        );
        let raw = format!("[a.txt#{tag}]\nPUT 1:\n+X\n");
        let fenced = format!("```\n{raw}```\n");
        let err = registry
            .call("edit", serde_json::json!({ "patch": fenced }), &tool_ctx)
            .await
            .unwrap_err();
        let failed = match err {
            polymorph_tools::CallError::Failed(e) => e,
            polymorph_tools::CallError::Denied(d) => panic!("want Failed, got Denied: {d}"),
        };
        assert_eq!(classify_edit_error(&failed), "edit-parse");
        // Control: the same raw patch applies through the same registry.
        let out = registry
            .call("edit", serde_json::json!({ "patch": raw }), &tool_ctx)
            .await
            .unwrap();
        assert_eq!(out["files"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn agentic_runner_reads_then_returns_text() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello\n").unwrap();
        let stub = Arc::new(StubAgenticTurn::new(vec![
            collected(
                "",
                vec![tool_call(
                    "c1",
                    "read",
                    serde_json::json!({"path": "a.txt"}),
                )],
            ),
            collected("fixed!", vec![]),
        ]));
        let runner = agentic_runner(stub.clone(), vec!["read".into()], vec![], 3);
        let out = runner.run_turn(agentic_ctx(&dir, "fix it")).await.unwrap();
        assert_eq!(out.text, "fixed!");
        assert_eq!(out.tool_calls, 1);
        assert!(out.done);
        assert_eq!(out.data.as_ref().unwrap()["artifact"], "fixed!");
        // Second turn saw brief + assistant calls + the read result.
        let seen = stub.seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].len(), 1); // just the brief.
        assert_eq!(seen[1].len(), 3); // brief + assistant + tool result.
        let fed = fed_responses(&seen[1]);
        assert_eq!(fed.len(), 1);
        assert!(fed[0].contains("hello"), "{fed:?}");
    }

    /// Past the history budget, older tool results become
    /// stubs (oldest first) while the latest `keep_rounds` rounds stay
    /// intact; a `read` stub points back at the file and keeps its tag, any
    /// other result is saved under `.polymorph/tool-output/` first.
    #[tokio::test]
    async fn agentic_runner_compacts_old_tool_results_past_the_budget() {
        let dir = tempfile::tempdir().unwrap();
        let big: String = (1..=300)
            .map(|i| format!("line {i} of the big file, padded for size\n"))
            .collect();
        std::fs::write(dir.path().join("big.txt"), &big).unwrap();
        let round = |n: u32| {
            collected(
                "",
                vec![
                    tool_call(
                        &format!("r{n}"),
                        "read",
                        serde_json::json!({"path": "big.txt"}),
                    ),
                    tool_call(
                        &format!("g{n}"),
                        "grep",
                        serde_json::json!({"pattern": "padded", "max_hits": 40}),
                    ),
                ],
            )
        };
        let stub = Arc::new(StubAgenticTurn::new(vec![
            round(1),
            round(2),
            round(3),
            round(4),
            collected("done", vec![]),
        ]));
        let mut runner =
            agentic_runner(stub.clone(), vec!["read".into(), "grep".into()], vec![], 10);
        runner.history = ToolHistoryBudget {
            budget_bytes: 40 * 1024,
            keep_rounds: 2,
        };
        let out = runner.run_turn(agentic_ctx(&dir, "fix it")).await.unwrap();
        assert_eq!(out.text, "done");
        let seen = stub.seen.lock().unwrap();
        let fed = fed_responses(&seen[4]);
        assert_eq!(fed.len(), 8);
        let intact_read = &fed[6];
        assert!(
            intact_read.contains("line 300 of the big file"),
            "{intact_read}"
        );
        // Rounds 3-4 intact; the oldest are stubs, round 1 first.
        for f in &fed[4..] {
            assert!(!f.starts_with("[harness:"), "{f}");
        }
        assert!(
            fed[0].starts_with("[harness: older tool result compacted"),
            "{}",
            fed[0]
        );
        assert!(
            fed[0].contains("`read` from tool round 1, outcome ok"),
            "{}",
            fed[0]
        );
        assert!(
            fed[0].contains("`big.txt` lines 1-300 of 300"),
            "{}",
            fed[0]
        );
        assert!(fed[0].contains("Its tag was "), "{}", fed[0]);
        assert!(fed[1].contains("`grep` from tool round 1"), "{}", fed[1]);
        let saved = fed[1]
            .split("Full result saved at ")
            .nth(1)
            .and_then(|r| r.split(' ').next())
            .unwrap();
        assert!(saved.starts_with(".polymorph/tool-output/"), "{saved}");
        let body = std::fs::read_to_string(dir.path().join(saved)).unwrap();
        assert!(body.contains("\"hits\""), "{body}");
        // Still over budget after round 4, so round 2 went too; rounds 3-4
        // stay whole even though they alone exceed the budget.
        assert!(fed[2].contains("`read` from tool round 2"), "{}", fed[2]);
        assert!(fed[3].contains("`grep` from tool round 2"), "{}", fed[3]);
        assert!(fed[4..].iter().map(String::len).sum::<usize>() > 40 * 1024);
        // An earlier history (after round 2) was still under budget.
        assert!(fed_responses(&seen[2])
            .iter()
            .all(|f| !f.starts_with("[harness:")));
    }

    /// The stub for a spilled command names exit code and the
    /// spill files; `budget_bytes == 0` never compacts.
    #[test]
    fn compaction_stub_names_spills_and_off_budget_is_inert() {
        let dir = tempfile::tempdir().unwrap();
        let body = serde_json::to_string_pretty(&serde_json::json!({
            "stdout": "x".repeat(4000),
            "stderr": "",
            "exit_code": 101,
            "stdout_spill": {"path": ".polymorph/tool-output/1-2-3.stdout.log", "total_bytes": 9, "total_lines": 1},
        }))
        .unwrap();
        let mk = || FedResult {
            index: 0,
            round: 1,
            call_id: "c".into(),
            tool: "bash".into(),
            outcome: "ok",
            exit_code: Some(101),
            read_path: None,
            body: body.clone(),
            sent: body.len(),
            compacted: false,
        };
        let stub = compaction_stub(&mk(), dir.path(), 10);
        assert!(
            stub.contains("`bash` from tool round 1, outcome ok, exit_code 101"),
            "{stub}"
        );
        assert!(
            stub.contains("full stdout at .polymorph/tool-output/1-2-3.stdout.log"),
            "{stub}"
        );
        assert!(
            stub.contains("Full result saved at .polymorph/tool-output/"),
            "{stub}"
        );
        let mut messages = vec![genai::chat::ChatMessage::user("x")];
        let mut fed = vec![mk()];
        let n = compact_tool_history(
            &mut messages,
            &mut fed,
            9,
            ToolHistoryBudget::off(),
            dir.path(),
            10,
        );
        assert_eq!(n, 0);
        assert!(!fed[0].compacted);
        // keep_rounds 0 still protects the latest round.
        let n = compact_tool_history(
            &mut messages,
            &mut fed,
            1,
            ToolHistoryBudget {
                budget_bytes: 1,
                keep_rounds: 0,
            },
            dir.path(),
            10,
        );
        assert_eq!(n, 0);
    }

    /// A spilled command's zero-test label reads the full spill
    /// file — a `running 5 tests` the digest omitted still clears it.
    #[test]
    fn zero_test_marker_scans_spill_files() {
        let dir = tempfile::tempdir().unwrap();
        let spill = dir.path().join("out.log");
        std::fs::write(&spill, "running 5 tests\r\n...\r\nrunning 0 tests\r\n").unwrap();
        let result = serde_json::json!({
            "stdout": "[digest]\nrunning 0 tests",
            "stderr": "",
            "stdout_spill": {"path": "out.log"},
        });
        assert_eq!(spilled_zero_test_marker(&result, dir.path()), Some(None));
        std::fs::write(&spill, "running 0 tests\n").unwrap();
        assert_eq!(
            spilled_zero_test_marker(&result, dir.path()),
            Some(Some("running 0 tests"))
        );
        let inline = serde_json::json!({"stdout": "running 0 tests", "stderr": ""});
        assert_eq!(spilled_zero_test_marker(&inline, dir.path()), None);
        assert_eq!(
            zero_test_marker_lines(["collected 0 items", "running 3 tests"]),
            Some("collected 0 items")
        );
    }

    #[tokio::test]
    async fn agentic_runner_records_ok_outcome() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello\n").unwrap();
        let stub = Arc::new(StubAgenticTurn::new(vec![
            collected(
                "",
                vec![tool_call(
                    "c1",
                    "read",
                    serde_json::json!({"path": "a.txt"}),
                )],
            ),
            collected("fixed!", vec![]),
        ]));
        let runner = agentic_runner(stub, vec!["read".into()], vec![], 3);
        let out = runner.run_turn(agentic_ctx(&dir, "fix it")).await.unwrap();
        assert_eq!(
            out.data.as_ref().unwrap()["tool_outcomes"],
            serde_json::json!([{"tool": "read", "outcome": "ok"}])
        );
    }

    #[test]
    fn edit_error_classifier_reads_typed_chain() {
        use polymorph_tools::hashline::HashlineError;
        // Production shape: typed error wrapped in `.context()` at the call
        // site (Display text preserved, type retained).
        let stale: anyhow::Error = HashlineError::StaleTag {
            path: "a".into(),
            tag: "t".into(),
        }
        .into();
        assert_eq!(
            classify_edit_error(&stale.context("edit: apply failed")),
            "edit-stale"
        );
        let parse: anyhow::Error = HashlineError::Parse {
            line: 2,
            msg: "x".into(),
        }
        .into();
        assert_eq!(
            classify_edit_error(&parse.context("edit: invalid patch")),
            "edit-parse"
        );
        let overlap: anyhow::Error = HashlineError::Overlap {
            path: "a".into(),
            msg: "x".into(),
        }
        .into();
        assert_eq!(
            classify_edit_error(&overlap.context("edit: apply failed")),
            "edit-apply"
        );
        // Non-hashline failures fall through to plain `error`.
        assert_eq!(classify_edit_error(&anyhow::anyhow!("boom")), "error");
    }

    #[tokio::test]
    async fn agentic_runner_labels_stale_edit_subtype() {
        // Live-shaped stale: mint the true tag, rotate one hex digit (the
        // corpus `corrupt_tag_for` precedent), then edit against a real file.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "l1\nl2\n").unwrap();
        let registry = Arc::new(polymorph_tools::registry::v1_registry());
        let text = polymorph_tools::hashline::apply::normalize(
            &std::fs::read(dir.path().join("a.txt")).unwrap(),
        )
        .text;
        let tag = registry
            .mint_tag(std::path::Path::new("a.txt"), &text)
            .await;
        let mut stale = tag.clone();
        stale.replace_range(0..1, if &tag[0..1] == "0" { "1" } else { "0" });
        let patch = format!("[a.txt#{stale}]\nPUT 1:\n+X\n");
        let stub = Arc::new(StubAgenticTurn::new(vec![
            collected(
                "",
                vec![tool_call(
                    "c1",
                    "edit",
                    serde_json::json!({ "patch": patch }),
                )],
            ),
            collected("done", vec![]),
        ]));
        let runner = AgenticFixRunner {
            turn: stub,
            registry,
            gate: polymorph_tools::Gate::new(vec![], vec![]),
            tools: vec!["edit".into()],
            max_tool_rounds: 3,
            max_bonus_tool_rounds: DEFAULT_MAX_BONUS_TOOL_ROUNDS,
            advisor: None,
            restore: None,
            history: Default::default(),
            command_changes: 0,
        };
        let out = runner.run_turn(agentic_ctx(&dir, "fix it")).await.unwrap();
        assert_eq!(
            out.data.as_ref().unwrap()["tool_outcomes"],
            serde_json::json!([{"tool": "edit", "outcome": "edit-stale"}])
        );
    }

    #[tokio::test]
    async fn agentic_runner_marks_unknown_denied_and_failed_tools() {
        let dir = tempfile::tempdir().unwrap();
        let stub = Arc::new(StubAgenticTurn::new(vec![
            collected(
                "",
                vec![
                    tool_call("c1", "frobnicate", serde_json::json!({})),
                    tool_call("c2", "read", serde_json::json!({"path": "a.txt"})),
                    tool_call("c3", "read", serde_json::json!({"path": "ghost.txt"})),
                ],
            ),
            collected("done", vec![]),
        ]));
        // frobnicate is not allowlisted (refused); a.txt is Gate-denied;
        // ghost.txt passes the gate but is missing (execution failure).
        let runner = agentic_runner(stub, vec!["read".into()], vec!["read:a.txt".into()], 3);
        let out = runner.run_turn(agentic_ctx(&dir, "fix it")).await.unwrap();
        assert_eq!(out.tool_calls, 3);
        assert_eq!(
            out.data.as_ref().unwrap()["tool_outcomes"],
            serde_json::json!([
                {"tool": "frobnicate", "outcome": "refused-unknown"},
                {"tool": "read", "outcome": "denied"},
                {"tool": "read", "outcome": "error"},
            ])
        );
    }

    /// A nonzero bash exit is still outcome `ok` (the harness
    /// ran it) but the ledger now carries the exit code, the command, and
    /// the write target — and the turn names its mutated paths.
    #[tokio::test]
    async fn agentic_runner_records_exit_code_and_mutated_paths() {
        let dir = tempfile::tempdir().unwrap();
        let stub = Arc::new(StubAgenticTurn::new(vec![
            collected(
                "",
                vec![tool_call(
                    "c1",
                    "bash",
                    serde_json::json!({"cmd": "exit 3"}),
                )],
            ),
            collected(
                "",
                vec![tool_call(
                    "c2",
                    "write",
                    serde_json::json!({"path": "b.txt", "content": "x\n"}),
                )],
            ),
            collected("done", vec![]),
        ]));
        let runner = agentic_runner(stub, vec!["bash".into(), "write".into()], vec![], 3);
        let out = runner.run_turn(agentic_ctx(&dir, "fix it")).await.unwrap();
        let data = out.data.as_ref().unwrap();
        assert_eq!(
            data["tool_outcomes"],
            serde_json::json!([
                {"tool": "bash", "outcome": "ok", "cmd": "exit 3", "exit_code": 3},
                {"tool": "write", "outcome": "ok", "paths": ["b.txt"]},
            ])
        );
        assert_eq!(data["mutated_paths"], serde_json::json!(["b.txt"]));
        assert_eq!(data["mutated_files"], true);
    }

    /// Repeat-command futility: an identical command with an identical
    /// outcome is labelled on the second fed result (and nudged once per
    /// turn) instead of passing silently as verification; a different
    /// command is untouched.
    #[tokio::test]
    async fn agentic_runner_labels_repeated_identical_commands() {
        let dir = tempfile::tempdir().unwrap();
        let stub = Arc::new(StubAgenticTurn::new(vec![
            collected(
                "",
                vec![tool_call(
                    "c1",
                    "bash",
                    serde_json::json!({"cmd": "exit 3"}),
                )],
            ),
            collected(
                "",
                vec![tool_call(
                    "c2",
                    "bash",
                    serde_json::json!({"cmd": "exit 3"}),
                )],
            ),
            collected(
                "",
                vec![tool_call(
                    "c3",
                    "bash",
                    serde_json::json!({"cmd": "exit 0"}),
                )],
            ),
            collected("done", vec![]),
        ]));
        let runner = agentic_runner(stub, vec!["bash".into()], vec![], 5);
        let out = runner.run_turn(agentic_ctx(&dir, "fix it")).await.unwrap();
        let outcomes = out.data.as_ref().unwrap()["tool_outcomes"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(outcomes.len(), 3);
        assert!(outcomes[0].get("repeat_note").is_none(), "{}", outcomes[0]);
        assert!(
            outcomes[1]["repeat_note"]
                .as_str()
                .unwrap()
                .contains("no new information"),
            "{}",
            outcomes[1]
        );
        assert!(outcomes[2].get("repeat_note").is_none(), "{}", outcomes[2]);
    }

    /// A write whose LSP consult cannot run (no
    /// server) leaves the file with NO snapshot — the stale pre-write
    /// record is dropped, never shown as this turn's state — and files the
    /// turn did not mutate never appear, whatever the pool holds for them.
    #[tokio::test]
    async fn agentic_runner_lsp_snapshots_cover_only_this_turns_writes() {
        use polymorph_tools::lsp::{uri_for, LspDiagnostic};
        let dir = tempfile::tempdir().unwrap();
        let stub = Arc::new(StubAgenticTurn::new(vec![
            collected(
                "",
                vec![tool_call(
                    "c1",
                    "write",
                    serde_json::json!({"path": "a.rs", "content": "fn main() {}\n"}),
                )],
            ),
            collected("done", vec![]),
        ]));
        let mut runner = agentic_runner(stub, vec!["write".into()], vec![], 3);
        let stale = LspDiagnostic {
            line: Some(1),
            severity: Some(1),
            message: "stale round-1 error".into(),
        };
        let mut seed = std::collections::BTreeMap::new();
        seed.insert(uri_for(dir.path(), "a.rs"), vec![stale.clone()]);
        seed.insert(uri_for(dir.path(), "untouched.rs"), vec![stale]);
        let registry = Arc::new(polymorph_tools::registry::v1_registry());
        registry.test_seed_lsp_diagnostic_details(seed).await;
        runner.registry = registry;
        let out = runner.run_turn(agentic_ctx(&dir, "fix it")).await.unwrap();
        let data = out.data.as_ref().unwrap();
        assert_eq!(
            data["lsp_diagnostics"],
            serde_json::json!([{"path": "a.rs", "items": null}])
        );
        // No snapshot anywhere → the evidence block carries no LSP lines.
        assert_eq!(render_lsp_evidence(data), "");
        assert!(lsp_error_findings(data).is_empty());
    }

    /// Snapshots are looked up for exactly the mutated files,
    /// one entry per file (spellings of one URI collapse, the LAST spelling
    /// wins and selects its own record), failed calls and unmutated files
    /// excluded, unknown languages recorded as no-snapshot.
    #[test]
    fn lsp_mutated_snapshots_select_latest_record_per_mutated_file() {
        use polymorph_tools::lsp::{uri_for, LspDiagnostic};
        let wd = std::path::Path::new("/w");
        let err = |line, message: &str| LspDiagnostic {
            line: Some(line),
            severity: Some(1),
            message: message.into(),
        };
        let mut details = std::collections::BTreeMap::new();
        // `a.rs` was first written bare, last written as `./a.rs`: only the
        // last spelling's URI holds the latest consult.
        details.insert(uri_for(wd, "a.rs"), vec![err(1, "older spelling")]);
        details.insert(uri_for(wd, "./a.rs"), vec![err(4, "latest")]);
        details.insert(uri_for(wd, "src/b.rs"), Vec::new());
        details.insert(uri_for(wd, "never.rs"), vec![err(2, "not mutated")]);
        let outcomes = vec![
            serde_json::json!({"tool": "write", "outcome": "ok", "paths": ["a.rs"]}),
            serde_json::json!({"tool": "edit", "outcome": "ok", "paths": ["src/b.rs", "./a.rs"]}),
            serde_json::json!({"tool": "write", "outcome": "denied"}),
            serde_json::json!({"tool": "write", "outcome": "ok", "paths": ["notes.zzz"]}),
            serde_json::json!({"tool": "bash", "outcome": "ok", "cmd": "cargo check", "exit_code": 0}),
        ];
        assert_eq!(
            serde_json::Value::Array(lsp_mutated_snapshots(&details, wd, &outcomes)),
            serde_json::json!([
                {"path": "./a.rs", "items": [{"line": 4, "severity": 1, "message": "latest"}]},
                {"path": "src/b.rs", "items": []},
                {"path": "notes.zzz", "items": null},
            ])
        );
        assert!(lsp_mutated_snapshots(&details, wd, &[]).is_empty());
    }

    fn lsp_item(line: u64, severity: Option<u64>, message: &str) -> serde_json::Value {
        serde_json::json!({"line": line, "severity": severity, "message": message})
    }

    /// Errors flagged and listed first (files and
    /// rows), warnings tallied, `file:line: severity: message` rows on one
    /// line, clean and no-snapshot files named.
    #[test]
    fn harness_evidence_renders_lsp_errors_and_warnings() {
        let dir = tempfile::tempdir().unwrap();
        let wd = polymorph_delegate::WorkdirSpec::InPlace(dir.path().to_path_buf());
        let data = serde_json::json!({ "lsp_diagnostics": [
            {"path": "src/warn.rs", "items": [lsp_item(3, Some(2), "unused import")]},
            {"path": "src/clean.rs", "items": []},
            {"path": "src/a.rs", "items": [
                lsp_item(30, Some(2), "unused variable"),
                lsp_item(12, Some(1), "mismatched types\nexpected `i32`"),
                lsp_item(5, None, "odd"),
                lsp_item(7, Some(1), "cannot find value `x`"),
            ]},
            {"path": "README.md", "items": null},
        ]});
        let text = render_harness_evidence(&wd, &one_finding("ghost.py"), &data);
        assert_eq!(
            text,
            "\n\nHARNESS EVIDENCE (harness-observed, not model claims):\n\
             - tools: none recorded this turn\n\
             - LSP diagnostics on files mutated this turn (snapshot at each file's last write): \
             2 ERRORS, 2 warnings, 1 unspecified in 2 of 3 files\n\
             \x20 - src/a.rs: 2 ERRORS, 1 warning, 1 unspecified\n\
             \x20   - src/a.rs:7: error: cannot find value `x`\n\
             \x20   - src/a.rs:12: error: mismatched types expected `i32`\n\
             \x20   - src/a.rs:30: warning: unused variable\n\
             \x20   - src/a.rs:5: unspecified: odd\n\
             \x20 - src/warn.rs: 1 warning\n\
             \x20   - src/warn.rs:3: warning: unused import\n\
             \x20 - src/clean.rs: clean\n\
             - LSP: no snapshot for 1 mutated files (no server for the language, or its consult failed): README.md\n\
             - ghost.py: MISSING\n"
        );
    }

    /// 5 rows per file, 20 rows in total, 10 files, every
    /// omission counted; tallies still cover everything.
    #[test]
    fn lsp_evidence_caps_rows_and_files_with_omitted_counts() {
        let files: Vec<serde_json::Value> = (0..12)
            .map(|f| {
                let items: Vec<serde_json::Value> = (0..7)
                    .map(|i| lsp_item(i + 1, Some(1), &format!("e{f}-{i}")))
                    .collect();
                serde_json::json!({"path": format!("f{f:02}.rs"), "items": items})
            })
            .collect();
        let text = render_lsp_evidence(&serde_json::json!({ "lsp_diagnostics": files }));
        assert!(text.contains(": 84 ERRORS in 12 of 12 files\n"), "{text}");
        let rows = text.lines().filter(|l| l.starts_with("    - f")).count();
        assert_eq!(rows, 20, "{text}");
        // Files 0..=3 show 5 rows (2 omitted each); file 4 is out of budget.
        assert_eq!(
            text.matches("    - (2 more omitted)\n").count(),
            4,
            "{text}"
        );
        assert!(
            text.contains("  - f04.rs: 7 ERRORS\n    - (7 more omitted)\n"),
            "{text}"
        );
        let file_lines = text.lines().filter(|l| l.starts_with("  - f")).count();
        assert_eq!(file_lines, 10, "{text}");
        assert!(text.ends_with("  (2 more files omitted)\n"), "{text}");
        // Long messages stay one bounded line.
        let long = "x".repeat(500);
        let one = render_lsp_evidence(&serde_json::json!({ "lsp_diagnostics": [
            {"path": "a.rs", "items": [lsp_item(1, Some(1), &long)]},
        ]}));
        let row = one
            .lines()
            .find(|l| l.starts_with("    - a.rs:1:"))
            .unwrap();
        assert!(row.ends_with("...") && row.chars().count() < 200, "{row}");
        // Out-of-range severity codes render as unspecified, never panic.
        let odd = render_lsp_evidence(&serde_json::json!({ "lsp_diagnostics": [
            {"path": "a.rs", "items": [lsp_item(1, Some(9), "m")]},
        ]}));
        assert!(odd.contains("  - a.rs: 1 unspecified\n"), "{odd}");
    }

    /// Byte-identical invariant: an LSP field with no snapshot (LSP off,
    /// unknown languages) or no mutated files renders nothing new.
    #[test]
    fn harness_evidence_without_lsp_snapshots_is_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let wd = polymorph_delegate::WorkdirSpec::InPlace(dir.path().to_path_buf());
        let findings = one_finding("ghost.py");
        let base = render_harness_evidence(&wd, &findings, &serde_json::json!({}));
        for data in [
            serde_json::json!({ "lsp_diagnostics": [] }),
            serde_json::json!({ "lsp_diagnostics": [{"path": "a.rs", "items": null}] }),
            serde_json::json!({ "lsp_diagnostics": "garbage" }),
        ] {
            assert_eq!(render_harness_evidence(&wd, &findings, &data), base);
        }
        let clean = serde_json::json!({ "lsp_diagnostics": [{"path": "a.rs", "items": []}] });
        assert!(render_lsp_evidence(&clean).contains("none (1 files clean)"));
    }

    /// Gate findings: errors only, one per file, deterministic text (the
    /// survivor/stall escalations compare it verbatim), first error line.
    #[test]
    fn lsp_error_findings_cite_errors_only() {
        let data = serde_json::json!({ "lsp_diagnostics": [
            {"path": "warn.rs", "items": [lsp_item(1, Some(2), "w")]},
            {"path": "gone.rs", "items": null},
            {"path": "a.rs", "items": [
                lsp_item(40, Some(1), "e4"),
                lsp_item(9, Some(1), "e1"),
                lsp_item(10, Some(1), "e2"),
                lsp_item(11, Some(1), "e3"),
                lsp_item(2, Some(2), "w"),
            ]},
        ]});
        let findings = lsp_error_findings(&data);
        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!(f.file, "a.rs");
        assert_eq!(f.line, Some(9));
        assert_eq!(f.severity, polymorph_core::Severity::P1);
        assert_eq!(
            f.issue,
            "LSP error gate (harness-observed): 4 error-severity diagnostics remain at \
             this file's last write: L9: e1; L10: e2; L11: e3; (+1 more)"
        );
        assert_eq!(lsp_error_findings(&data)[0].issue, f.issue);
        assert!(lsp_error_findings(&serde_json::json!({})).is_empty());
    }

    /// Scripted reviewer for the gate tests: pops one verdict per call.
    struct VerdictScript(std::sync::Mutex<VecDeque<Verdict>>);

    #[async_trait]
    impl Reviewer for VerdictScript {
        async fn review(&self, _input: ReviewerInput) -> Result<Verdict, ReviewerError> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .pop_front()
                .expect("script exhausted"))
        }
    }

    fn lsp_gate_verdict(kind: polymorph_core::VerdictKind) -> Verdict {
        Verdict {
            verdict: kind,
            findings: if kind == polymorph_core::VerdictKind::Fail {
                one_finding("a.rs")
            } else {
                Vec::new()
            },
            confidence: 0.9,
        }
    }

    /// Runs Fail → Pass → Pass through `run_loop` over the real executor;
    /// fix turn 1 leaves an error on `a.rs`, fix turn 2 is clean.
    async fn run_lsp_gate_loop(gate: Option<Arc<HarnessGate>>) -> crate::LoopReport {
        use polymorph_core::VerdictKind;
        let dir = tempfile::tempdir().unwrap();
        let broken = serde_json::json!({ "artifact": "v2", "lsp_diagnostics": [
            {"path": "a.rs", "items": [lsp_item(3, Some(1), "expected `;`")]},
        ]});
        let clean = serde_json::json!({ "artifact": "v3", "lsp_diagnostics": [
            {"path": "a.rs", "items": []},
        ]});
        let stub: Arc<dyn TurnRunner> = Arc::new(StubRunner::new(vec![
            done_out("v2", broken),
            done_out("v3", clean),
        ]));
        let runner: Arc<dyn TurnRunner> = match &gate {
            Some(gate) => Arc::new(LspGateRunner {
                inner: stub,
                gate: gate.clone(),
            }),
            None => stub,
        };
        let executor = BusExecutor {
            bus: Arc::new(Bus::flat()),
            runner,
            workdir: polymorph_delegate::WorkdirSpec::InPlace(dir.path().to_path_buf()),
            tools: Vec::new(),
            tool_outcomes: Arc::new(std::sync::Mutex::new(Vec::new())),
            merge: None,
            artifact_path: None,
            peripherals: None,
            artifact_set: None,
        };
        let reviewer = HarnessGateReviewer {
            inner: VerdictScript(std::sync::Mutex::new(
                [VerdictKind::Fail, VerdictKind::Pass, VerdictKind::Pass]
                    .into_iter()
                    .map(lsp_gate_verdict)
                    .collect(),
            )),
            gates: gate.into_iter().collect(),
        };
        let cfg = LoopConfig {
            max_iterations: 3,
            ..Default::default()
        };
        run_loop(&cfg, "g", "", "v1", LoopMode::Act, &reviewer, &executor).await
    }

    /// Gate OFF (default): the reviewer's pass stands over live LSP errors
    /// (pre-#3 posture — evidence only).
    #[tokio::test]
    async fn lsp_gate_off_keeps_reviewer_pass() {
        let report = run_lsp_gate_loop(None).await;
        assert_eq!(report.outcome, Outcome::Passed { rounds: 2 });
    }

    /// Gate ON: the round-2 pass over `a.rs`'s error becomes a fail
    /// carrying the gate finding (fed to the fixer like any finding); the
    /// clean fix turn lets round 3's pass stand.
    #[tokio::test]
    async fn lsp_gate_on_vetoes_pass_until_errors_clear() {
        let gate = Arc::new(HarnessGate::default());
        let report = run_lsp_gate_loop(Some(gate.clone())).await;
        assert_eq!(report.outcome, Outcome::Passed { rounds: 3 });
        let round2 = &report.rounds_log[1];
        assert_eq!(round2.verdict, crate::RoundVerdict::Fail);
        assert_eq!(round2.findings, 1);
        assert!(
            round2.issues[0].starts_with("P1 a.rs:3 LSP error gate"),
            "{:?}",
            round2.issues
        );
        assert!(gate.pending().is_empty());
    }

    /// The runner decorator records per turn and clears on a failed turn;
    /// fail verdicts pass the reviewer decorator untouched.
    #[tokio::test]
    async fn lsp_gate_records_per_turn_and_leaves_fails_alone() {
        struct ErrRunner;
        #[async_trait]
        impl TurnRunner for ErrRunner {
            async fn run_turn(&self, _ctx: TurnContext) -> anyhow::Result<TurnOutput> {
                anyhow::bail!("boom")
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let gate = Arc::new(HarnessGate::default());
        let broken = serde_json::json!({ "lsp_diagnostics": [
            {"path": "a.rs", "items": [lsp_item(1, Some(1), "e")]},
        ]});
        let ok = LspGateRunner {
            inner: Arc::new(StubRunner::new(vec![done_out("x", broken)])),
            gate: gate.clone(),
        };
        ok.run_turn(agentic_ctx(&dir, "b")).await.unwrap();
        assert_eq!(gate.pending().len(), 1);
        let failing = LspGateRunner {
            inner: Arc::new(ErrRunner),
            gate: gate.clone(),
        };
        assert!(failing.run_turn(agentic_ctx(&dir, "b")).await.is_err());
        assert!(gate.pending().is_empty());
        // A pending veto never touches a FAIL verdict.
        ok.gate.record(one_finding("z.rs"));
        let reviewer = HarnessGateReviewer {
            inner: VerdictScript(std::sync::Mutex::new(
                [lsp_gate_verdict(polymorph_core::VerdictKind::Fail)].into(),
            )),
            gates: vec![gate],
        };
        let input = ReviewerInput {
            goal: "g".into(),
            criteria: String::new(),
            artifact: "a".into(),
            mode: LoopMode::Act,
        };
        let v = reviewer.review(input).await.unwrap();
        assert_eq!(v, lsp_gate_verdict(polymorph_core::VerdictKind::Fail));
    }

    /// Verify digest: the tail stays whole, earlier diagnostic
    /// lines survive with their location line, every gap is counted, and
    /// lines are cleaned + char-bounded (never a blind byte cut).
    #[test]
    fn verify_digest_keeps_error_lines_and_tail() {
        let mut text = String::new();
        for i in 0..50 {
            text.push_str(&format!("noise {i}\n"));
        }
        text.push_str("error[E0425]: cannot find value `x` in this scope\n");
        text.push_str("  --> src/lib.rs:3:5\n");
        for i in 0..40 {
            text.push_str(&format!("more {i}\n"));
        }
        text.push_str(&format!("\x1b[31mtail\tline\r\n{}\n", "é".repeat(400)));
        let d = verify_digest(&text);
        assert_eq!(d["total"], 94);
        let lines: Vec<&str> = d["lines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l.as_str().unwrap())
            .collect();
        assert_eq!(lines[0], "... (50 lines omitted) ...");
        assert_eq!(
            lines[1],
            "error[E0425]: cannot find value `x` in this scope"
        );
        assert_eq!(lines[2], "  --> src/lib.rs:3:5");
        assert_eq!(lines[3], "... (22 lines omitted) ...");
        assert_eq!(lines[4], "more 22");
        assert_eq!(lines.len(), 4 + VERIFY_TAIL_LINES);
        assert_eq!(lines[lines.len() - 2], "[31mtail line");
        let long = lines[lines.len() - 1];
        assert_eq!(long.chars().count(), VERIFY_LINE_CHARS);
        assert!(long.ends_with("..."));
        // Short output is kept whole; empty output is empty.
        assert_eq!(
            verify_digest("a\nb\n"),
            serde_json::json!({"total": 2, "lines": ["a", "b"]})
        );
        assert_eq!(
            verify_digest(""),
            serde_json::json!({"total": 0, "lines": []})
        );
    }

    /// Cited lines: diagnostics first (stderr before stdout), else the
    /// last non-blank lines.
    #[test]
    fn verify_key_lines_prefer_diagnostics() {
        assert_eq!(
            verify_key_lines("test a ... FAILED\nok\n", "warning: x\nerror: boom\n"),
            ["error: boom", "test a ... FAILED"]
        );
        assert_eq!(
            verify_key_lines("one\ntwo\n\nthree\nfour\n\n", ""),
            ["two", "three", "four"]
        );
        assert!(verify_key_lines("", "").is_empty());
    }

    fn verify_run(cmd: &str, status: &str, exit: i64) -> serde_json::Value {
        serde_json::json!({
            "cmd": cmd, "status": status, "exit_code": exit, "duration_ms": 1234,
            "stdout": {"total": 0, "lines": []},
            "stderr": {"total": 3, "lines": ["... (1 lines omitted) ...", "error: boom", "  --> a.rs:1:1"]},
            "key": ["error: boom"],
        })
    }

    /// Findings: one per failing command, deterministic (no duration),
    /// none for passed/skipped records.
    #[test]
    fn verify_findings_name_command_exit_and_key_lines() {
        let runs = vec![
            verify_run("cargo build", "passed", 0),
            verify_run("cargo test", "failed", 101),
            serde_json::json!({"cmd": "cargo clippy", "status": "skipped"}),
        ];
        let findings = verify_findings(&runs);
        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!(f.file, VERIFY_FINDING_FILE);
        assert_eq!(f.line, None);
        assert_eq!(f.severity, polymorph_core::Severity::P1);
        assert_eq!(
            f.issue,
            "Verify gate (harness-observed): `cargo test` FAILED exit 101 after the fix turn: error: boom"
        );
        let mut slower = runs.clone();
        slower[1]["duration_ms"] = serde_json::json!(99_999);
        assert_eq!(verify_findings(&slower), findings);
        let timed_out = serde_json::json!({"cmd": "t", "status": "timed_out", "timeout_secs": 5});
        assert_eq!(
            verify_findings(&[timed_out])[0].issue,
            "Verify gate (harness-observed): `t` TIMED OUT after 5s (process tree killed, \
             no output captured) after the fix turn: no output lines to cite"
        );
        assert!(verify_findings(&runs[..1]).is_empty());
    }

    /// Golden evidence block: statuses, durations, non-empty stream
    /// digests, the gate result; verify findings get no MISSING line.
    #[test]
    fn harness_evidence_renders_verify_block() {
        let dir = tempfile::tempdir().unwrap();
        let wd = polymorph_delegate::WorkdirSpec::InPlace(dir.path().to_path_buf());
        let data = serde_json::json!({ "verify": [
            verify_run("cargo build", "passed", 0),
            verify_run("cargo test", "failed", 101),
            {"cmd": "cargo clippy", "status": "skipped"},
        ]});
        let gate_finding = verify_findings(&data["verify"].as_array().unwrap()[..]);
        assert_eq!(
            render_harness_evidence(&wd, &gate_finding, &data),
            "\n\nHARNESS EVIDENCE (harness-observed, not model claims):\n\
             - tools: none recorded this turn\n\
             - VERIFY GATE (operator commands, run by the harness after the fix turn ended; post-turn ground truth):\n\
             \x20 #1 PASSED exit 0 in 1.2s: cargo build\n\
             \x20   stderr (3 lines):\n\
             \x20     | ... (1 lines omitted) ...\n\
             \x20     | error: boom\n\
             \x20     |   --> a.rs:1:1\n\
             \x20 #2 FAILED exit 101 in 1.2s: cargo test\n\
             \x20   stderr (3 lines):\n\
             \x20     | ... (1 lines omitted) ...\n\
             \x20     | error: boom\n\
             \x20     |   --> a.rs:1:1\n\
             \x20 #3 SKIPPED (an earlier verify command failed): cargo clippy\n\
             - VERIFY RESULT: FAILED (the harness blocks PASS until every verify command exits 0)\n"
        );
        assert_eq!(render_verify_evidence(&serde_json::json!({})), "");
    }

    /// Writes `ok.txt` into the turn's workdir on the scripted turns
    /// (`true`), records every brief it was handed.
    struct WritingRunner {
        script: std::sync::Mutex<VecDeque<bool>>,
        briefs: std::sync::Mutex<Vec<String>>,
    }

    impl WritingRunner {
        fn new(script: Vec<bool>) -> Self {
            Self {
                script: std::sync::Mutex::new(script.into()),
                briefs: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl TurnRunner for WritingRunner {
        async fn run_turn(&self, ctx: TurnContext) -> anyhow::Result<TurnOutput> {
            self.briefs.lock().unwrap().push(ctx.brief.clone());
            let write = self.script.lock().unwrap().pop_front().expect("script");
            if write {
                std::fs::write(ctx.workdir.join("ok.txt"), "ok\n").unwrap();
            }
            Ok(done_out("fixed", serde_json::json!({"artifact": "fixed"})))
        }
    }

    /// Reviewer double: scripted verdicts, records the artifacts it saw.
    struct SeeingScript {
        verdicts: std::sync::Mutex<VecDeque<polymorph_core::VerdictKind>>,
        seen: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait]
    impl Reviewer for SeeingScript {
        async fn review(&self, input: ReviewerInput) -> Result<Verdict, ReviewerError> {
            self.seen.lock().unwrap().push(input.artifact);
            let kind = self.verdicts.lock().unwrap().pop_front().expect("script");
            Ok(lsp_gate_verdict(kind))
        }
    }

    /// `run_loop` over the real executor with an optional verify gate:
    /// returns the report, the reviewer's seen artifacts and the fixer's
    /// briefs.
    async fn run_verify_loop(
        cmds: Vec<String>,
        writes: Vec<bool>,
        verdicts: Vec<polymorph_core::VerdictKind>,
        max_iterations: u32,
    ) -> (crate::LoopReport, Vec<String>, Vec<String>) {
        let dir = tempfile::tempdir().unwrap();
        let fixer = Arc::new(WritingRunner::new(writes));
        let gate = Arc::new(HarnessGate::default());
        let runner: Arc<dyn TurnRunner> = if cmds.is_empty() {
            fixer.clone()
        } else {
            Arc::new(VerifyGateRunner {
                inner: fixer.clone(),
                gate: gate.clone(),
                cmds: cmds.clone(),
                timeout: std::time::Duration::from_secs(30),
            })
        };
        let executor = BusExecutor {
            bus: Arc::new(Bus::flat()),
            runner,
            workdir: polymorph_delegate::WorkdirSpec::InPlace(dir.path().to_path_buf()),
            tools: Vec::new(),
            tool_outcomes: Arc::new(std::sync::Mutex::new(Vec::new())),
            merge: None,
            artifact_path: None,
            peripherals: None,
            artifact_set: None,
        };
        let reviewer = HarnessGateReviewer {
            inner: SeeingScript {
                verdicts: std::sync::Mutex::new(verdicts.into()),
                seen: std::sync::Mutex::new(Vec::new()),
            },
            gates: if cmds.is_empty() {
                Vec::new()
            } else {
                vec![gate]
            },
        };
        let cfg = LoopConfig {
            max_iterations,
            ..Default::default()
        };
        let report = run_loop(&cfg, "g", "", "v1", LoopMode::Act, &reviewer, &executor).await;
        let seen = reviewer.inner.seen.lock().unwrap().clone();
        let briefs = fixer.briefs.lock().unwrap().clone();
        (report, seen, briefs)
    }

    /// Exits nonzero until `ok.txt` exists in the workdir.
    fn needs_ok_txt() -> String {
        if cfg!(windows) {
            "type ok.txt".to_string()
        } else {
            "cat ok.txt".to_string()
        }
    }

    /// Verify passes → the reviewer's pass stands; the reviewer saw the
    /// verify block.
    #[tokio::test]
    async fn verify_gate_pass_lets_reviewer_pass_stand() {
        use polymorph_core::VerdictKind::{Fail, Pass};
        let (report, seen, _) =
            run_verify_loop(vec!["exit 0".into()], vec![false], vec![Fail, Pass], 3).await;
        assert_eq!(report.outcome, Outcome::Passed { rounds: 2 });
        assert!(seen[1].contains("  #1 PASSED exit 0 in "), "{}", seen[1]);
        assert!(seen[1].contains("- VERIFY RESULT: PASSED"), "{}", seen[1]);
    }

    /// Verify fails → the reviewer's pass is vetoed with the command's
    /// finding, which reaches the fixer; once the fix lands the next
    /// round's pass stands.
    #[tokio::test]
    async fn verify_gate_vetoes_pass_until_command_passes() {
        use polymorph_core::VerdictKind::{Fail, Pass};
        let (report, seen, briefs) = run_verify_loop(
            vec![needs_ok_txt()],
            vec![false, true],
            vec![Fail, Pass, Pass],
            3,
        )
        .await;
        assert_eq!(report.outcome, Outcome::Passed { rounds: 3 });
        let round2 = &report.rounds_log[1];
        assert_eq!(round2.verdict, crate::RoundVerdict::Fail);
        assert_eq!(round2.findings, 1);
        let want = format!(
            "P1 {VERIFY_FINDING_FILE} Verify gate (harness-observed): `{}` FAILED exit ",
            needs_ok_txt()
        );
        assert!(round2.issues[0].starts_with(&want), "{:?}", round2.issues);
        // The reviewer saw the failure (round 2) and the green rerun.
        assert!(seen[1].contains("- VERIFY RESULT: FAILED"), "{}", seen[1]);
        assert!(seen[2].contains("- VERIFY RESULT: PASSED"), "{}", seen[2]);
        // The fixer's second brief carries the gate finding verbatim.
        assert!(
            briefs[1].contains("(verify-cmd):- — Verify gate (harness-observed)"),
            "{}",
            briefs[1]
        );
    }

    /// Final round: a verify failure under a reviewer pass escalates
    /// (MaxIterations, the gate finding surviving) — never Passed.
    #[tokio::test]
    async fn verify_gate_failure_on_final_round_is_not_passed() {
        use polymorph_core::VerdictKind::{Fail, Pass};
        let (report, _, _) = run_verify_loop(
            vec!["echo error: still broken && exit 3".into()],
            vec![false],
            vec![Fail, Pass],
            2,
        )
        .await;
        assert_eq!(
            report.outcome,
            Outcome::Escalated {
                reason: crate::EscalateReason::MaxIterations,
                rounds: 2
            }
        );
        assert_eq!(report.surviving.len(), 1);
        assert_eq!(
            report.surviving[0].issue,
            "Verify gate (harness-observed): `echo error: still broken && exit 3` FAILED \
             exit 3 after the fix turn: error: still broken"
        );
    }

    /// No verify gate: the scored artifact carries no verify block and a
    /// clean pass stands (gate-off renders as before).
    #[tokio::test]
    async fn verify_gate_off_adds_nothing() {
        use polymorph_core::VerdictKind::{Fail, Pass};
        let (report, seen, _) = run_verify_loop(Vec::new(), vec![false], vec![Fail, Pass], 3).await;
        assert_eq!(report.outcome, Outcome::Passed { rounds: 2 });
        assert!(!seen[1].contains("VERIFY"), "{}", seen[1]);
    }

    /// Seeded gate: pending carries the missing-evidence veto if and only
    /// if verify commands are declared; the first verify run replaces it.
    #[tokio::test]
    async fn verify_gate_seeds_missing_evidence_until_first_run() {
        let off = HarnessGate::with_unrun_verify(&[]);
        assert!(off.pending().is_empty());
        let cmds = vec!["exit 0".to_string()];
        let gate = Arc::new(HarnessGate::with_unrun_verify(&cmds));
        let pending = gate.pending();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].file, VERIFY_FINDING_FILE);
        assert!(
            pending[0].issue.contains("no verify evidence"),
            "{}",
            pending[0].issue
        );
        // A clean first run clears the seed.
        let dir = tempfile::tempdir().unwrap();
        let runner = VerifyGateRunner {
            inner: Arc::new(StubRunner::new(vec![done_out(
                "x",
                serde_json::json!({"artifact": "x"}),
            )])),
            gate: gate.clone(),
            cmds: cmds.clone(),
            timeout: std::time::Duration::from_secs(30),
        };
        runner.run_turn(agentic_ctx(&dir, "b")).await.unwrap();
        assert!(gate.pending().is_empty());
        // A failing first run replaces the seed with the failure finding.
        let failing = VerifyGateRunner {
            inner: Arc::new(StubRunner::new(vec![done_out(
                "x",
                serde_json::json!({"artifact": "x"}),
            )])),
            gate: gate.clone(),
            cmds: vec!["echo boom-marker && exit 1".into()],
            timeout: std::time::Duration::from_secs(30),
        };
        failing.run_turn(agentic_ctx(&dir, "b")).await.unwrap();
        let pending = gate.pending();
        assert_eq!(pending.len(), 1);
        assert!(
            pending[0].issue.contains("FAILED exit 1"),
            "{}",
            pending[0].issue
        );
    }

    /// Loop level: a scripted round-1 PASS is vetoed by the seed (the old
    /// evidence-free pass), then stands on round 2 once verify has run.
    #[tokio::test]
    async fn verify_gate_seed_vetoes_round_one_pass_until_verify_runs() {
        use polymorph_core::VerdictKind::Pass;
        let dir = tempfile::tempdir().unwrap();
        let cmds = vec!["exit 0".to_string()];
        let gate = Arc::new(HarnessGate::with_unrun_verify(&cmds));
        let fixer = Arc::new(WritingRunner::new(vec![false]));
        let runner: Arc<dyn TurnRunner> = Arc::new(VerifyGateRunner {
            inner: fixer,
            gate: gate.clone(),
            cmds: cmds.clone(),
            timeout: std::time::Duration::from_secs(30),
        });
        let executor = BusExecutor {
            bus: Arc::new(Bus::flat()),
            runner,
            workdir: polymorph_delegate::WorkdirSpec::InPlace(dir.path().to_path_buf()),
            tools: Vec::new(),
            tool_outcomes: Arc::new(std::sync::Mutex::new(Vec::new())),
            merge: None,
            artifact_path: None,
            peripherals: None,
            artifact_set: None,
        };
        let reviewer = HarnessGateReviewer {
            inner: SeeingScript {
                verdicts: std::sync::Mutex::new(vec![Pass, Pass].into()),
                seen: std::sync::Mutex::new(Vec::new()),
            },
            gates: vec![gate],
        };
        let cfg = LoopConfig {
            max_iterations: 3,
            ..Default::default()
        };
        let report = run_loop(&cfg, "g", "", "v1", LoopMode::Act, &reviewer, &executor).await;
        assert_eq!(report.outcome, Outcome::Passed { rounds: 2 });
        let round1 = &report.rounds_log[0];
        assert_eq!(round1.verdict, crate::RoundVerdict::Fail);
        assert_eq!(round1.findings, 1);
        assert!(
            round1.issues[0].contains("no verify evidence"),
            "{:?}",
            round1.issues
        );
    }

    /// Several commands run in order and stop at the first failure (the
    /// rest are recorded skipped); only the failing one yields a finding.
    #[tokio::test]
    async fn verify_gate_stops_at_first_failing_command() {
        let dir = tempfile::tempdir().unwrap();
        let gate = Arc::new(HarnessGate::default());
        let runner = VerifyGateRunner {
            inner: Arc::new(StubRunner::new(vec![done_out(
                "x",
                serde_json::json!({"artifact": "x"}),
            )])),
            gate: gate.clone(),
            cmds: vec![
                "exit 0".into(),
                "echo boom-marker && exit 1".into(),
                "exit 0".into(),
            ],
            timeout: std::time::Duration::from_secs(30),
        };
        let out = runner.run_turn(agentic_ctx(&dir, "b")).await.unwrap();
        let runs = out.data.unwrap()["verify"].as_array().unwrap().clone();
        let statuses: Vec<&str> = runs.iter().map(|r| r["status"].as_str().unwrap()).collect();
        assert_eq!(statuses, ["passed", "failed", "skipped"]);
        assert_eq!(runs[1]["exit_code"], 1);
        assert_eq!(runs[1]["key"], serde_json::json!(["boom-marker"]));
        let pending = gate.pending();
        assert_eq!(pending.len(), 1);
        assert!(
            pending[0]
                .issue
                .contains("`echo boom-marker && exit 1` FAILED exit 1"),
            "{}",
            pending[0].issue
        );
    }

    /// Timeout: the tree is killed and the run counts as a failure.
    #[tokio::test]
    async fn verify_gate_timeout_is_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        let gate = Arc::new(HarnessGate::default());
        let sleeper = if cfg!(windows) {
            "powershell -NoProfile -Command Start-Sleep -Seconds 30"
        } else {
            "sleep 30"
        };
        let runner = VerifyGateRunner {
            inner: Arc::new(StubRunner::new(vec![done_out(
                "x",
                serde_json::json!({"artifact": "x"}),
            )])),
            gate: gate.clone(),
            cmds: vec![sleeper.into(), "exit 0".into()],
            timeout: std::time::Duration::from_millis(500),
        };
        let started = std::time::Instant::now();
        let out = runner.run_turn(agentic_ctx(&dir, "b")).await.unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(20));
        let runs = out.data.unwrap()["verify"].clone();
        assert_eq!(runs[0]["status"], "timed_out");
        assert_eq!(runs[1]["status"], "skipped");
        let pending = gate.pending();
        assert_eq!(pending.len(), 1);
        assert!(
            pending[0].issue.contains("TIMED OUT after 0s"),
            "{}",
            pending[0].issue
        );
    }

    /// A command that does not exist is a failure (shell exit), never a
    /// silent pass; a failed inner turn clears the gate without running
    /// anything; a cancelled turn fails instead of reporting a pass.
    #[tokio::test]
    async fn verify_gate_missing_command_failed_turn_and_cancel() {
        struct ErrRunner;
        #[async_trait]
        impl TurnRunner for ErrRunner {
            async fn run_turn(&self, _ctx: TurnContext) -> anyhow::Result<TurnOutput> {
                anyhow::bail!("boom")
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let gate = Arc::new(HarnessGate::default());
        let missing = VerifyGateRunner {
            inner: Arc::new(StubRunner::new(vec![done_out(
                "x",
                serde_json::json!({"artifact": "x"}),
            )])),
            gate: gate.clone(),
            cmds: vec!["polymorph-no-such-command-xyz".into()],
            timeout: std::time::Duration::from_secs(30),
        };
        let out = missing.run_turn(agentic_ctx(&dir, "b")).await.unwrap();
        assert_eq!(out.data.unwrap()["verify"][0]["status"], "failed");
        assert_eq!(gate.pending().len(), 1);
        let failing = VerifyGateRunner {
            inner: Arc::new(ErrRunner),
            gate: gate.clone(),
            cmds: vec!["exit 0".into()],
            timeout: std::time::Duration::from_secs(30),
        };
        assert!(failing.run_turn(agentic_ctx(&dir, "b")).await.is_err());
        assert!(gate.pending().is_empty());
        gate.record(one_finding("z.rs"));
        let cancelled = VerifyGateRunner {
            inner: Arc::new(StubRunner::new(vec![done_out(
                "x",
                serde_json::json!({"artifact": "x"}),
            )])),
            gate: gate.clone(),
            cmds: vec!["exit 0".into()],
            timeout: std::time::Duration::from_secs(30),
        };
        let ctx = agentic_ctx(&dir, "b");
        ctx.cancel.cancel();
        let Err(err) = cancelled.run_turn(ctx).await else {
            panic!("a cancelled verify must fail the turn");
        };
        assert!(err.to_string().contains("verify gate cancelled"), "{err}");
        assert!(gate.pending().is_empty());
    }

    /// Both gates armed: verify findings lead, then LSP, then the
    /// reviewer's own; a fail verdict stays untouched.
    #[tokio::test]
    async fn harness_gates_combine_in_order() {
        let verify = Arc::new(HarnessGate::default());
        let lsp = Arc::new(HarnessGate::default());
        verify.record(verify_findings(&[verify_run("cargo test", "failed", 101)]));
        lsp.record(one_finding("a.rs"));
        let pass_with_note = Verdict {
            verdict: polymorph_core::VerdictKind::Pass,
            findings: one_finding("note.rs"),
            confidence: 0.9,
        };
        let reviewer = HarnessGateReviewer {
            inner: VerdictScript(std::sync::Mutex::new(
                [
                    pass_with_note,
                    lsp_gate_verdict(polymorph_core::VerdictKind::Fail),
                ]
                .into(),
            )),
            gates: vec![verify, lsp],
        };
        let input = || ReviewerInput {
            goal: "g".into(),
            criteria: String::new(),
            artifact: "a".into(),
            mode: LoopMode::Act,
        };
        let v = reviewer.review(input()).await.unwrap();
        assert_eq!(v.verdict, polymorph_core::VerdictKind::Fail);
        let files: Vec<&str> = v.findings.iter().map(|f| f.file.as_str()).collect();
        assert_eq!(files, [VERIFY_FINDING_FILE, "a.rs", "note.rs"]);
        let v = reviewer.review(input()).await.unwrap();
        assert_eq!(v, lsp_gate_verdict(polymorph_core::VerdictKind::Fail));
    }

    #[tokio::test]
    async fn agentic_runner_read_result_carries_mintable_tag() {
        // The read→tag→edit loop closed in one test: the fed tag mints
        // through the same registry, and an edit quoting it applies.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), "l1\nl2\n").unwrap();
        let stub = Arc::new(StubAgenticTurn::new(vec![
            collected(
                "",
                vec![tool_call("c1", "read", serde_json::json!({"path": "a"}))],
            ),
            collected("edited!", vec![]),
            collected("done", vec![]),
        ]));
        let registry = Arc::new(polymorph_tools::registry::v1_registry());
        let runner = AgenticFixRunner {
            turn: stub.clone(),
            registry: registry.clone(),
            gate: polymorph_tools::Gate::new(vec![], vec![]),
            tools: vec!["read".into(), "edit".into()],
            max_tool_rounds: 3,
            max_bonus_tool_rounds: DEFAULT_MAX_BONUS_TOOL_ROUNDS,
            advisor: None,
            restore: None,
            history: Default::default(),
            command_changes: 0,
        };
        runner.run_turn(agentic_ctx(&dir, "fix it")).await.unwrap();
        // Drop the guard before the awaits below (clippy::await-holding-lock).
        let fed = {
            let seen = stub.seen.lock().unwrap();
            fed_responses(&seen[1])
        };
        assert_eq!(fed.len(), 1);
        let fed_json: serde_json::Value = serde_json::from_str(&fed[0]).unwrap();
        let tag = fed_json["tag"]
            .as_str()
            .expect("read result must carry tag");
        // The tag verifies: an edit quoting it applies through the registry.
        let tool_ctx = polymorph_tools::ToolCallContext::new(
            dir.path().to_path_buf(),
            polymorph_tools::Gate::new(vec![], vec![]),
            tokio_util::sync::CancellationToken::new(),
        );
        let out = registry
            .call(
                "edit",
                serde_json::json!({"patch": format!("[a#{tag}]\nPUT 1.=1:\n+X\n")}),
                &tool_ctx,
            )
            .await
            .unwrap();
        assert_eq!(out["files"].as_array().unwrap().len(), 1);
        // PUT 1.=1 replaces line 1 with X (hashline replace semantics).
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a")).unwrap(),
            "X\nl2\n"
        );
    }

    #[tokio::test]
    async fn agentic_runner_denials_and_unknown_tools_feed_back() {
        let dir = tempfile::tempdir().unwrap();
        let stub = Arc::new(StubAgenticTurn::new(vec![
            collected(
                "",
                vec![
                    tool_call("c1", "read", serde_json::json!({"path": "a"})),
                    tool_call("c2", "frobnicate", serde_json::json!({})),
                ],
            ),
            collected("done anyway", vec![]),
        ]));
        // Reads denied; frobnicate not allowlisted either.
        let runner = agentic_runner(stub.clone(), vec!["read".into()], vec!["read".into()], 3);
        let out = runner.run_turn(agentic_ctx(&dir, "fix it")).await.unwrap();
        assert_eq!(out.text, "done anyway");
        assert_eq!(out.tool_calls, 2);
        let seen = stub.seen.lock().unwrap();
        let fed = fed_responses(&seen[1]);
        assert_eq!(fed.len(), 2);
        assert!(fed[0].contains("permission denied"), "{fed:?}");
        assert!(fed[1].contains("unknown tool"), "{fed:?}");
    }

    #[tokio::test]
    async fn agentic_runner_budget_exhaustion_returns_last_text() {
        // max_tool_rounds = 0: single text turn, calls dropped but counted.
        let stub = Arc::new(StubAgenticTurn::new(vec![collected(
            "partial",
            vec![tool_call("c1", "read", serde_json::json!({"path": "a"}))],
        )]));
        let dir = tempfile::tempdir().unwrap();
        let runner = agentic_runner(stub, vec!["read".into()], vec![], 0);
        let out = runner.run_turn(agentic_ctx(&dir, "fix it")).await.unwrap();
        assert!(out.done);
        assert_eq!(out.text, "partial");
        assert_eq!(out.tool_calls, 0);
        assert!(out.summary.contains("1 dropped"), "{}", out.summary);
    }

    #[tokio::test]
    async fn agentic_runner_synthesizes_response_when_tool_loop_ends_with_empty_text() {
        // When tool calls are executed and the loop ends with empty text,
        // an extra synthesis turn with 0 tools is run to force textual explanation.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "files in dir\n").unwrap();
        let stub = Arc::new(StubAgenticTurn::new(vec![
            // Round 1: Model calls tool with empty text
            collected(
                "",
                vec![tool_call(
                    "c1",
                    "read",
                    serde_json::json!({"path": "a.txt"}),
                )],
            ),
            // Round 2: Model finishes tool loop with empty text
            collected("", vec![]),
            // Synthesis round: 0 tools provided, model synthesizes answer
            collected("Here is the content: files in dir", vec![]),
        ]));
        let runner = agentic_runner(stub.clone(), vec!["read".into()], vec![], 3);
        let out = runner
            .run_turn(agentic_ctx(&dir, "what is in a.txt"))
            .await
            .unwrap();
        assert_eq!(out.text, "Here is the content: files in dir");
        assert_eq!(out.tool_calls, 1);
        let seen = stub.seen.lock().unwrap();
        assert_eq!(seen.len(), 3);
        let synth_prompt = seen[2]
            .last()
            .unwrap()
            .content
            .parts()
            .iter()
            .filter_map(|p| match p {
                genai::chat::ContentPart::Text(t) => Some(t.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        assert!(synth_prompt.contains("Based on the tool results above"));
    }

    #[tokio::test]
    async fn agentic_runner_adaptive_continuation_grants_bonus_round_on_mutating_tool() {
        // Max_tool_rounds is 1, but round 1 executes mutating write.
        // Adaptive continuation grants 1 bonus round to read/verify.
        let dir = tempfile::tempdir().unwrap();
        let stub = Arc::new(StubAgenticTurn::new(vec![
            // Round 1 (regular): write tool executed
            collected(
                "",
                vec![tool_call(
                    "c1",
                    "write",
                    serde_json::json!({"path": "out.txt", "content": "hello"}),
                )],
            ),
            // Round 2 (adaptive bonus round): read tool executed for verification
            collected(
                "",
                vec![tool_call(
                    "c2",
                    "read",
                    serde_json::json!({"path": "out.txt"}),
                )],
            ),
            // Round 3: next tool call dropped because bonus was already spent
            collected(
                "",
                vec![tool_call(
                    "c3",
                    "read",
                    serde_json::json!({"path": "out.txt"}),
                )],
            ),
            // Synthesis turn
            collected("Verified: out.txt contains hello", vec![]),
        ]));
        let runner = agentic_runner(
            stub,
            vec!["write".into(), "read".into()],
            vec![],
            1, // max_tool_rounds = 1
        );
        let out = runner
            .run_turn(agentic_ctx(&dir, "write then verify"))
            .await
            .unwrap();
        assert_eq!(out.text, "Verified: out.txt contains hello");
        assert_eq!(out.tool_calls, 2); // 1 normal + 1 bonus round executed
        assert!(
            out.summary.contains("2 tool rounds, 2 calls, 1 dropped"),
            "{}",
            out.summary
        );
    }

    #[tokio::test]
    async fn agentic_runner_fires_stop_hook_at_turn_end() {
        // J1 wiring proof: the turn terminal point notifies Stop observers
        // with the rounds/calls/dropped summary (observation only — the
        // report below is byte-identical with or without handlers).
        use std::sync::{Arc, Mutex};
        let stops = Arc::new(Mutex::new(Vec::new()));
        let mut registry = polymorph_tools::registry::v1_registry();
        let stops_c = Arc::clone(&stops);
        registry.add_hook(move |event| {
            if let polymorph_tools::HookEvent::Stop { reason } = event {
                stops_c.lock().unwrap().push(reason.clone());
            }
            polymorph_tools::HookDecision::Allow
        });
        let stub = Arc::new(StubAgenticTurn::new(vec![collected("done", vec![])]));
        let runner = AgenticFixRunner {
            turn: stub,
            registry: Arc::new(registry),
            gate: polymorph_tools::Gate::new(vec![], vec![]),
            tools: vec![],
            max_tool_rounds: 3,
            max_bonus_tool_rounds: DEFAULT_MAX_BONUS_TOOL_ROUNDS,
            advisor: None,
            restore: None,
            history: Default::default(),
            command_changes: 0,
        };
        let dir = tempfile::tempdir().unwrap();
        let out = runner.run_turn(agentic_ctx(&dir, "fix it")).await.unwrap();
        assert!(out.done);
        let stops = stops.lock().unwrap();
        assert_eq!(stops.len(), 1);
        assert!(stops[0].contains("0 tool rounds"), "{}", stops[0]);
    }

    #[tokio::test]
    async fn agentic_runner_transport_failure_bails_loud() {
        // Transport/auth failures are NOT fed back (unlike denials) — they
        // bail so the loop escalates ExecutorFailed (unchanged F1 path).
        let stub = Arc::new(StubAgenticTurn::failing("connection refused"));
        let dir = tempfile::tempdir().unwrap();
        let runner = agentic_runner(stub, vec!["read".into()], vec![], 3);
        // TurnOutput is not Debug, so match instead of unwrap_err.
        let err = match runner.run_turn(agentic_ctx(&dir, "fix it")).await {
            Ok(_) => panic!("transport failure must bail"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("connection refused"), "{err}");
    }

    #[tokio::test]
    async fn live_turn_serves_text_over_fixed_target() {
        // Thin-glue proof: LiveAgenticTurn over a Fixed target returns the
        // turn (session header + capture ride the conversation client in
        // prod; here the bare client proves the request/response path).
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse_text("live text")),
            )
            .mount(&server)
            .await;
        let live = LiveAgenticTurn {
            client: polymorph_llm::LlmClient::new(),
            target: LlmTarget::Fixed(mock_spec(&server)),
            system: "sys".into(),
            rules: Vec::new(),
            rule_max_retries: DEFAULT_RULE_RETRIES,
            pinned: Default::default(),
        };
        let turn = live
            .turn(
                vec![genai::chat::ChatMessage::user("hi")],
                vec![],
                &tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(turn.text, "live text");
        assert!(turn.tool_calls.is_empty());
    }

    fn tense_rule() -> polymorph_llm::rules::CompiledRule {
        polymorph_llm::rules::StreamRule {
            id: "tense".into(),
            pattern: r"will\s+edits?\b".into(),
            reminder: "use present tense for tool args".into(),
        }
        .compile()
        .unwrap()
    }

    #[test]
    fn rule_retry_policy_injects_and_budgets() {
        let hit = polymorph_llm::rules::RuleHit {
            rule_id: "tense".into(),
            matched: "will edit".into(),
            reminder: "use present tense".into(),
        };
        let sys = RuleRetryPolicy::inject("base sys", &hit);
        assert!(sys.starts_with("base sys"), "{sys}");
        assert!(sys.contains("[Stream rule 'tense' fired"), "{sys}");
        assert!(sys.contains("use present tense"), "{sys}");
        // Long matches cap at 200 chars (caps precedent).
        let long = polymorph_llm::rules::RuleHit {
            matched: "x".repeat(500),
            ..hit
        };
        let sys = RuleRetryPolicy::inject("s", &long);
        assert!(sys.len() < "s".len() + 500, "matched text must cap");
        let policy = RuleRetryPolicy { max_retries: 2 };
        assert!(policy.should_retry(0));
        assert!(policy.should_retry(1));
        assert!(!policy.should_retry(2));
        assert!(!RuleRetryPolicy { max_retries: 0 }.should_retry(0));
    }

    #[tokio::test]
    async fn rule_hit_retries_bounded_with_reminder_on_wire() {
        // Slice-2 loop over a Fixed target, fully scripted (wiremock SSE
        // misfires every attempt): 1 + 2 retries hit the server, the
        // retry bodies carry the injected reminder, the spent turn hands
        // up WITH its hit (reviewer-visible, never a loop).
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse_text("it will edit x")),
            )
            .mount(&server)
            .await;
        let live = LiveAgenticTurn {
            client: polymorph_llm::LlmClient::new(),
            target: LlmTarget::Fixed(mock_spec(&server)),
            system: "sys".into(),
            rules: vec![tense_rule()],
            rule_max_retries: 2,
            pinned: Default::default(),
        };
        let turn = live
            .turn(
                vec![genai::chat::ChatMessage::user("hi")],
                vec![],
                &tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(turn.text, "it will edit x");
        assert_eq!(turn.rule_hit.map(|h| h.rule_id), Some("tense".to_string()));
        let seen = server.received_requests().await.unwrap_or_default();
        assert_eq!(seen.len(), 3, "initial + 2 bounded retries");
        let bodies: Vec<String> = seen
            .iter()
            .filter_map(|r| String::from_utf8(r.body.clone()).ok())
            .collect();
        assert!(
            !bodies[0].contains("Stream rule"),
            "first attempt is clean system"
        );
        assert!(
            bodies[1].contains("[Stream rule 'tense' fired"),
            "retry injects"
        );
        assert!(
            bodies[1].contains("use present tense for tool args"),
            "reminder rides"
        );
    }

    #[tokio::test]
    async fn rule_hit_clean_second_attempt_stops_at_one_retry() {
        // Recovery shape WITHOUT distinct responses: a single-attempt
        // budget on a misfiring server proves the counter starts at one
        // request and stops the moment budget spends (no busy loop).
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse_text("it will edit x")),
            )
            .mount(&server)
            .await;
        let live = LiveAgenticTurn {
            client: polymorph_llm::LlmClient::new(),
            target: LlmTarget::Fixed(mock_spec(&server)),
            system: "sys".into(),
            rules: vec![tense_rule()],
            rule_max_retries: 0,
            pinned: Default::default(),
        };
        let turn = live
            .turn(
                vec![genai::chat::ChatMessage::user("hi")],
                vec![],
                &tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(turn.rule_hit.is_some());
        let seen = server.received_requests().await.unwrap_or_default();
        assert_eq!(seen.len(), 1, "zero budget = single attempt, hands up");
    }

    #[test]
    fn parse_notes_grades_prefixes_and_keeps_strays() {
        let notes = parse_notes("ASIDE: looks fine\nCONCERN: check edge\nBLOCKER: wrong total\nplain observation\n\nASIDE:   \n");
        assert_eq!(notes.len(), 4);
        assert_eq!(notes[0].severity, AdvisorSeverity::Aside);
        assert_eq!(notes[0].text, "looks fine");
        assert_eq!(notes[1].severity, AdvisorSeverity::Concern);
        assert_eq!(notes[2].severity, AdvisorSeverity::Blocker);
        assert_eq!(notes[2].text, "wrong total");
        // Bare lines degrade to Aside (never dropped); bare prefixes skip.
        assert_eq!(notes[3].severity, AdvisorSeverity::Aside);
        assert_eq!(notes[3].text, "plain observation");
        assert!(parse_notes("   \n").is_empty());
    }

    fn advisor_at(server: &wiremock::MockServer) -> AdvisorRunner {
        AdvisorRunner {
            client: polymorph_llm::LlmClient::new(),
            target: LlmTarget::Fixed(mock_spec(server)),
        }
    }

    #[tokio::test]
    async fn advise_parses_three_severities_over_fixed_target() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse_text(
                        "ASIDE: cache note\nCONCERN: retry path\nBLOCKER: total wrong",
                    )),
            )
            .mount(&server)
            .await;
        let advisor = advisor_at(&server);
        let notes = advisor
            .advise(
                "fix it",
                &[],
                "done",
                &tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(notes.len(), 3);
        assert_eq!(notes[2].severity, AdvisorSeverity::Blocker);
    }

    #[tokio::test]
    async fn run_turn_with_advisor_records_notes_in_data() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse_text("CONCERN: double-check")),
            )
            .mount(&server)
            .await;
        // Fixer turn is scripted (done, no calls); only the advisor
        // touches the mock — the mount above serves it alone.
        let stub = Arc::new(StubAgenticTurn::new(vec![collected("done", vec![])]));
        let mut runner = agentic_runner(stub, vec![], vec![], 3);
        runner.advisor = Some(advisor_at(&server));
        let dir = tempfile::tempdir().unwrap();
        let out = runner.run_turn(agentic_ctx(&dir, "fix it")).await.unwrap();
        assert!(out.done);
        let notes = out.data.as_ref().unwrap()["advisor_notes"]
            .as_array()
            .unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0]["severity"], serde_json::json!("concern"));
    }

    #[tokio::test]
    async fn run_turn_advisor_failure_stays_ok_with_empty_notes() {
        // Dead server shape (500 envelope): the turn succeeds anyway —
        // a sick watcher never holds fixes hostage (99 posture).
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500).set_body_string(
                r#"{"success":false,"error":{"code":"INTERNAL_ERROR","message":"boom"}}"#,
            ))
            .mount(&server)
            .await;
        let stub = Arc::new(StubAgenticTurn::new(vec![collected("done", vec![])]));
        let mut runner = agentic_runner(stub, vec![], vec![], 3);
        runner.advisor = Some(advisor_at(&server));
        let dir = tempfile::tempdir().unwrap();
        let out = runner.run_turn(agentic_ctx(&dir, "fix it")).await.unwrap();
        assert!(out.done);
        assert_eq!(
            out.data.as_ref().unwrap()["advisor_notes"]
                .as_array()
                .map(Vec::len),
            Some(0)
        );
    }

    #[test]
    fn load_pack_refuses_missing_and_empty() {
        let missing = std::path::PathBuf::from("definitely-not-a-pack-xyz.md");
        let err = load_pack(&missing).unwrap_err().to_string();
        assert!(err.contains("definitely-not-a-pack-xyz.md"), "{err}");
        let dir = tempfile::tempdir().unwrap();
        let empty = dir.path().join("empty.md");
        std::fs::write(&empty, "  \n ").unwrap();
        let err = load_pack(&empty).unwrap_err().to_string();
        assert!(err.contains("empty"), "{err}");
        let full = dir.path().join("review.md");
        std::fs::write(&full, "# review\nBe strict.\n").unwrap();
        assert_eq!(load_pack(&full).unwrap(), "# review\nBe strict.\n");
    }

    /// Pack search order is pure and therefore testable: an absolute path
    /// short-circuits, `POLYMORPH_PROMPTS_DIR` strips the `prompts` prefix, the
    /// cwd ancestors are walked nearest-first, and duplicates never repeat.
    #[test]
    fn pack_candidates_order_and_dedup() {
        use std::path::{Path as P, PathBuf as B};
        let pack = P::new("prompts/muse/muse-spark-1.3/review.md");

        assert_eq!(
            pack_candidates(
                P::new("C:/abs/pack.md"),
                Some(P::new("D:/p")),
                Some(P::new("C:/w")),
                None
            ),
            vec![B::from("C:/abs/pack.md")],
            "an absolute pack path is used as given"
        );

        let with_dir = pack_candidates(pack, Some(P::new("D:/packs")), None, None);
        assert_eq!(
            with_dir[0],
            B::from("D:/packs/muse/muse-spark-1.3/review.md")
        );
        assert_eq!(
            with_dir[1],
            B::from("D:/packs/prompts/muse/muse-spark-1.3/review.md")
        );
        assert_eq!(
            with_dir[2],
            B::from("prompts/muse/muse-spark-1.3/review.md")
        );

        let from_cwd = pack_candidates(
            pack,
            None,
            Some(P::new("C:/repo/crates/x")),
            Some(P::new("C:/bin")),
        );
        assert_eq!(
            from_cwd,
            vec![
                B::from("prompts/muse/muse-spark-1.3/review.md"),
                B::from("C:/repo/crates/x/prompts/muse/muse-spark-1.3/review.md"),
                B::from("C:/repo/crates/prompts/muse/muse-spark-1.3/review.md"),
                B::from("C:/repo/prompts/muse/muse-spark-1.3/review.md"),
                B::from("C:/prompts/muse/muse-spark-1.3/review.md"),
                B::from("C:/bin/prompts/muse/muse-spark-1.3/review.md"),
                B::from("C:/bin/../prompts/muse/muse-spark-1.3/review.md"),
                B::from("C:/bin/../../prompts/muse/muse-spark-1.3/review.md"),
                B::from("C:/bin/../../../prompts/muse/muse-spark-1.3/review.md"),
            ],
            "cwd-relative, then the ancestor walk nearest-first, then the exe fallbacks"
        );
    }

    /// The override actually resolves a pack that is not reachable from the cwd
    /// or the executable: the failure mode that forced `cd`-into-the-checkout.
    #[test]
    fn pack_candidates_override_reaches_an_unrelated_directory() {
        let dir = tempfile::tempdir().unwrap();
        let pack = dir.path().join("muse/muse-spark-1.3/review.md");
        std::fs::create_dir_all(pack.parent().unwrap()).unwrap();
        std::fs::write(&pack, "# review\nBe strict.\n").unwrap();
        let candidates = pack_candidates(
            std::path::Path::new("prompts/muse/muse-spark-1.3/review.md"),
            Some(dir.path()),
            Some(std::path::Path::new("C:/elsewhere")),
            Some(std::path::Path::new("C:/bin")),
        );
        assert_eq!(candidates[0], pack);
        assert_eq!(
            std::fs::read_to_string(&candidates[0]).unwrap(),
            "# review\nBe strict.\n"
        );
    }

    /// Loop-worktree merge mode: a fix turn that changes nothing
    /// merges as an empty-diff no-op (live catch) and returns the
    /// artifact text; the repo stays clean and no worktree/branch litters.
    #[tokio::test]
    async fn loop_merge_empty_noop_returns_artifact() {
        let repo = tempfile::tempdir().unwrap();
        let parent = tempfile::tempdir().unwrap();
        let mut files = std::collections::BTreeMap::new();
        files.insert("a.txt".to_string(), "orig\n".to_string());
        crate::bench::materialize_repo(repo.path(), &files).unwrap();
        let bus = Arc::new(Bus::flat());
        let runner = Arc::new(StubRunner::new(vec![done_out(
            "fixed artifact",
            serde_json::json!({ "artifact": "fixed artifact" }),
        )]));
        let ex = BusExecutor {
            bus,
            runner,
            workdir: polymorph_delegate::WorkdirSpec::InPlace(repo.path().to_path_buf()),
            tools: Vec::new(),
            tool_outcomes: Arc::new(std::sync::Mutex::new(Vec::new())),
            merge: Some(LoopMergeConfig {
                repo: repo.path().to_path_buf(),
                parent: parent.path().to_path_buf(),
                message: "polymorph loop fix test".to_string(),
            }),
            artifact_path: None,
            peripherals: None,
            artifact_set: None,
        };
        let out = ex
            .execute(ExecutorInput {
                goal: "g".into(),
                criteria: String::new(),
                artifact: "v1".into(),
                findings: Vec::new(),
                mode: LoopMode::Act,
            })
            .await
            .unwrap();
        assert_eq!(out, "fixed artifact");
        // Repo untouched, clean, and no worktree/branch litter.
        assert_eq!(
            std::fs::read_to_string(repo.path().join("a.txt")).unwrap(),
            "orig\n"
        );
        let list = std::process::Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(["worktree", "list"])
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&list.stdout);
        assert_eq!(stdout.lines().count(), 1, "{stdout}");
        let branches = std::process::Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(["branch", "--list", "polymorph/*"])
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&branches.stdout).trim().is_empty(),
            "{}",
            String::from_utf8_lossy(&branches.stdout)
        );
    }

    /// Loop-worktree merge mode with a real edit: the runner writes inside
    /// its worktree (the `ctx.workdir` the bus resolves), the bus commits,
    /// and `execute()` squash-merges before returning — the repo lands the
    /// fix and no worktree/branch litters.
    #[tokio::test]
    async fn loop_merge_writer_lands_in_repo() {
        struct WriteRunner;
        #[async_trait]
        impl TurnRunner for WriteRunner {
            async fn run_turn(&self, ctx: TurnContext) -> anyhow::Result<TurnOutput> {
                std::fs::write(ctx.workdir.join("a.txt"), "fixed\n").unwrap();
                Ok(done_out(
                    "fixed artifact",
                    serde_json::json!({ "artifact": "fixed artifact" }),
                ))
            }
        }
        let repo = tempfile::tempdir().unwrap();
        let parent = tempfile::tempdir().unwrap();
        let mut files = std::collections::BTreeMap::new();
        files.insert("a.txt".to_string(), "orig\n".to_string());
        crate::bench::materialize_repo(repo.path(), &files).unwrap();
        let ex = BusExecutor {
            bus: Arc::new(Bus::flat()),
            runner: Arc::new(WriteRunner),
            workdir: polymorph_delegate::WorkdirSpec::InPlace(repo.path().to_path_buf()),
            tools: Vec::new(),
            tool_outcomes: Arc::new(std::sync::Mutex::new(Vec::new())),
            merge: Some(LoopMergeConfig {
                repo: repo.path().to_path_buf(),
                parent: parent.path().to_path_buf(),
                message: "polymorph loop fix test".to_string(),
            }),
            artifact_path: None,
            peripherals: None,
            artifact_set: None,
        };
        let out = ex
            .execute(ExecutorInput {
                goal: "g".into(),
                criteria: String::new(),
                artifact: "v1".into(),
                findings: Vec::new(),
                mode: LoopMode::Act,
            })
            .await
            .unwrap();
        assert_eq!(out, "fixed artifact");
        assert_eq!(
            std::fs::read_to_string(repo.path().join("a.txt")).unwrap(),
            "fixed\n"
        );
        let list = std::process::Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(["worktree", "list"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&list.stdout).lines().count(), 1);
    }

    /// Loop-worktree merge mode never merges failures: a
    /// bailing runner discards its dirty tree and `execute()` bails the
    /// existing error — the repo keeps the pre-fix content.
    #[tokio::test]
    async fn loop_merge_failure_discards_and_bails() {
        struct DirtyFailRunner;
        #[async_trait]
        impl TurnRunner for DirtyFailRunner {
            async fn run_turn(&self, ctx: TurnContext) -> anyhow::Result<TurnOutput> {
                std::fs::write(ctx.workdir.join("a.txt"), "dirty\n").unwrap();
                anyhow::bail!("boom")
            }
        }
        let repo = tempfile::tempdir().unwrap();
        let parent = tempfile::tempdir().unwrap();
        let mut files = std::collections::BTreeMap::new();
        files.insert("a.txt".to_string(), "orig\n".to_string());
        crate::bench::materialize_repo(repo.path(), &files).unwrap();
        let ex = BusExecutor {
            bus: Arc::new(Bus::flat()),
            runner: Arc::new(DirtyFailRunner),
            workdir: polymorph_delegate::WorkdirSpec::InPlace(repo.path().to_path_buf()),
            tools: Vec::new(),
            tool_outcomes: Arc::new(std::sync::Mutex::new(Vec::new())),
            merge: Some(LoopMergeConfig {
                repo: repo.path().to_path_buf(),
                parent: parent.path().to_path_buf(),
                message: "polymorph loop fix test".to_string(),
            }),
            artifact_path: None,
            peripherals: None,
            artifact_set: None,
        };
        let err = ex
            .execute(ExecutorInput {
                goal: "g".into(),
                criteria: String::new(),
                artifact: "v1".into(),
                findings: Vec::new(),
                mode: LoopMode::Act,
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("boom"), "{err}");
        assert_eq!(
            std::fs::read_to_string(repo.path().join("a.txt")).unwrap(),
            "orig\n"
        );
        let list = std::process::Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(["worktree", "list"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&list.stdout).lines().count(), 1);
    }

    #[tokio::test]
    async fn agentic_runner_nudges_when_prose_emitted_without_mutating_tools() {
        let dir = tempfile::tempdir().unwrap();
        let stub = Arc::new(StubAgenticTurn::new(vec![
            collected("Here is the code in chat:\npublic class Hello {}", vec![]),
            collected(
                "",
                vec![tool_call(
                    "c1",
                    "write",
                    serde_json::json!({"path": "Hello.java", "content": "public class Hello {}"}),
                )],
            ),
            collected("File created!", vec![]),
        ]));
        let runner = agentic_runner(stub.clone(), vec!["write".into()], vec![], 3);
        let out = runner
            .run_turn(agentic_ctx(&dir, "write Hello.java"))
            .await
            .unwrap();
        assert_eq!(out.text, "File created!");
        assert_eq!(out.tool_calls, 1);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("Hello.java")).unwrap(),
            "public class Hello {}"
        );
        let seen = stub.seen.lock().unwrap();
        // Turn 0 was initial brief.
        // Turn 1 saw nudge reminding it to use write tool.
        // Turn 2 saw write result.
        assert_eq!(seen.len(), 3);
        let second_msgs = &seen[1];
        let last_user_msg = second_msgs
            .iter()
            .rfind(|m| m.role == genai::chat::ChatRole::User)
            .unwrap();
        assert!(format!("{last_user_msg:?}").contains(
            "You replied with text/prose without modifying or creating any files on disk"
        ));
    }

    #[tokio::test]
    async fn agentic_runner_nudges_after_reads_when_prose_emitted_without_writes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("existing.txt"), "hello").unwrap();
        let stub = Arc::new(StubAgenticTurn::new(vec![
            // Round 1: calls read
            collected(
                "",
                vec![tool_call(
                    "c1",
                    "read",
                    serde_json::json!({"path": "existing.txt"}),
                )],
            ),
            // Round 2: model replies with prose only (total_calls was 1 from read, but no files mutated!)
            collected(
                "I inspected existing.txt, here is code:\nclass Foo {}",
                vec![],
            ),
            // Round 3: model sees nudge and calls write
            collected(
                "",
                vec![tool_call(
                    "c2",
                    "write",
                    serde_json::json!({"path": "Foo.java", "content": "class Foo {}"}),
                )],
            ),
            collected("File created!", vec![]),
        ]));
        let runner = agentic_runner(stub.clone(), vec!["read".into(), "write".into()], vec![], 5);
        let out = runner
            .run_turn(agentic_ctx(&dir, "inspect then write Foo.java"))
            .await
            .unwrap();
        assert_eq!(out.text, "File created!");
        assert_eq!(out.tool_calls, 2);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("Foo.java")).unwrap(),
            "class Foo {}"
        );
        let seen = stub.seen.lock().unwrap();
        assert_eq!(seen.len(), 4);
        let third_msgs = &seen[2];
        let last_user_msg = third_msgs
            .iter()
            .rfind(|m| m.role == genai::chat::ChatRole::User)
            .unwrap();
        assert!(format!("{last_user_msg:?}").contains(
            "You replied with text/prose without modifying or creating any files on disk"
        ));
    }

    #[tokio::test]
    async fn agentic_runner_adaptive_bonus_on_pending_mutation() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("existing.txt"), "abc").unwrap();
        let stub = Arc::new(StubAgenticTurn::new(vec![
            // Round 1: reads existing file (consumes max_tool_rounds = 1)
            collected(
                "",
                vec![tool_call(
                    "c1",
                    "read",
                    serde_json::json!({"path": "existing.txt"}),
                )],
            ),
            // Round 2: pending write at budget cap (should be granted mutation bonus!)
            collected(
                "",
                vec![tool_call(
                    "c2",
                    "write",
                    serde_json::json!({"path": "out.txt", "content": "def"}),
                )],
            ),
            collected("done", vec![]),
        ]));
        let runner = agentic_runner(stub, vec!["read".into(), "write".into()], vec![], 1);
        let out = runner
            .run_turn(agentic_ctx(&dir, "inspect then write"))
            .await
            .unwrap();
        assert_eq!(out.tool_calls, 2);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("out.txt")).unwrap(),
            "def"
        );
    }

    /// Executor over a stub child that reports `data` (the
    /// artifact file is `a.txt`, peripheral attachment on).
    fn peripheral_executor(dir: &tempfile::TempDir, data: serde_json::Value) -> BusExecutor {
        BusExecutor {
            bus: Arc::new(Bus::flat()),
            runner: Arc::new(StubRunner::new(vec![done_out("closing text", data)])),
            workdir: polymorph_delegate::WorkdirSpec::InPlace(dir.path().to_path_buf()),
            tools: Vec::new(),
            tool_outcomes: Arc::new(std::sync::Mutex::new(Vec::new())),
            merge: None,
            artifact_path: Some(std::path::PathBuf::from("a.txt")),
            peripherals: Some(PeripheralFiles::default()),
            artifact_set: None,
        }
    }

    fn act_input(findings: Vec<polymorph_core::Finding>) -> ExecutorInput {
        ExecutorInput {
            goal: "g".into(),
            criteria: String::new(),
            artifact: "v1".into(),
            findings,
            mode: LoopMode::Act,
        }
    }

    /// A peripheral file's post-turn content rides the scored
    /// text, numbered, after the artifact file — which is never repeated
    /// in the list, whatever spelling the fixer used for it.
    #[tokio::test]
    async fn peripheral_file_content_attached_and_artifact_not_duplicated() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "artifact body\n").unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/b.rs"), "fn b() {\n}}\n").unwrap();
        let abs_artifact = dir.path().join("a.txt").to_string_lossy().into_owned();
        let ex = peripheral_executor(
            &dir,
            serde_json::json!({
                "artifact": "closing text",
                "mutated_files": true,
                "mutated_paths": ["./a.txt", abs_artifact, "src/b.rs"],
            }),
        );
        let out = ex.execute(act_input(Vec::new())).await.unwrap();
        assert_eq!(
            out,
            "artifact body\n\n\nPERIPHERAL FILES MUTATED THIS DRIVE (harness-read from disk after the fix turn; `N | ` prefixes are 1-based file line numbers, not file content):\n--- src/b.rs: FULL (2 lines)\n1 | fn b() {\n2 | }}\n"
        );
    }

    /// Spellings of one file (`./`, absolute, Windows
    /// separators and case) collapse to one entry, first spelling kept,
    /// and files stay attached on later turns that leave them alone.
    #[tokio::test]
    async fn peripheral_files_dedupe_spellings_and_persist_across_turns() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/b.rs"), "x\n").unwrap();
        let wd = polymorph_delegate::WorkdirSpec::InPlace(dir.path().to_path_buf());
        let abs = dir.path().join("src/b.rs").to_string_lossy().into_owned();
        let mut spellings = vec![
            serde_json::json!("src/b.rs"),
            serde_json::json!("./src/b.rs"),
            serde_json::json!(abs),
        ];
        if cfg!(windows) {
            spellings.push(serde_json::json!("SRC\\B.rs"));
        }
        let tracker = PeripheralFiles::default();
        let files = tracker.observe(
            &wd,
            None,
            &serde_json::json!({ "mutated_paths": spellings }),
        );
        assert_eq!(files.len(), 1, "{files:?}");
        assert_eq!(files[0].path, "src/b.rs");
        assert_eq!(files[0].state, AttachedState::Text("x\n".into()));
        // Next turn mutates nothing: the file is still in front of review.
        std::fs::write(dir.path().join("src/b.rs"), "y\n").unwrap();
        let again = tracker.observe(&wd, None, &serde_json::json!({ "mutated_paths": [] }));
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].state, AttachedState::Text("y\n".into()));
        // Nothing peripheral ever mutated: empty (the caller appends nothing).
        let fresh = PeripheralFiles::default();
        assert!(fresh
            .observe(&wd, None, &serde_json::json!({ "artifact": "t" }))
            .is_empty());
    }

    /// A deleted file is reported DELETED, a binary one by size
    /// only, a worktree drive as SKIPPED — never content, never an error.
    #[test]
    fn peripheral_files_report_deleted_binary_and_skipped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("img.png"), [0x89u8, b'P', 0, 1, 2]).unwrap();
        std::fs::write(dir.path().join("latin1.txt"), [b'a', 0xE9, b'\n']).unwrap();
        let wd = polymorph_delegate::WorkdirSpec::InPlace(dir.path().to_path_buf());
        let data = serde_json::json!({ "mutated_paths": ["gone.rs", "img.png", "latin1.txt"] });
        let files = PeripheralFiles::default().observe(&wd, None, &data);
        let out = render_file_attachments(PERIPHERAL_HEADER, &files);
        assert!(
            out.contains("--- gone.rs: DELETED (absent on disk after the turn"),
            "{out}"
        );
        assert!(
            out.contains("--- img.png: BINARY or non-UTF-8 (5 bytes; content not shown)"),
            "{out}"
        );
        assert!(
            out.contains("--- latin1.txt: BINARY or non-UTF-8 (3 bytes"),
            "{out}"
        );
        let wt = polymorph_delegate::WorkdirSpec::Worktree {
            repo: dir.path().to_path_buf(),
            parent: dir.path().to_path_buf(),
            mode: polymorph_delegate::worktree::WorktreeMode::Detached,
        };
        let skipped = PeripheralFiles::default().observe(&wt, None, &data);
        assert!(skipped
            .iter()
            .all(|f| matches!(f.state, AttachedState::Skipped(_))));
    }

    fn numbered_text(lines: usize, width: usize) -> String {
        (1..=lines)
            .map(|i| format!("{i:0width$}\n"))
            .collect::<String>()
    }

    fn text_file(path: &str, text: String, baseline: Option<Option<String>>) -> AttachedFile {
        AttachedFile {
            path: path.into(),
            state: AttachedState::Text(text),
            baseline,
            note: None,
        }
    }

    /// Per-file and total caps cut only at row
    /// boundaries, every cut is named on the status line, files past the
    /// budget are listed OMITTED, files past the file cap by name.
    #[test]
    fn peripheral_budget_overflow_names_every_omission() {
        // 4 × 400 rows > the 1500-row budget: file 4 is cut at row 300.
        let mut files: Vec<AttachedFile> = (1..=4)
            .map(|i| text_file(&format!("f{i}.rs"), numbered_text(400, 4), None))
            .collect();
        files.push(text_file("late.rs", numbered_text(3, 1), None));
        let out = render_file_attachments(PERIPHERAL_HEADER, &files);
        assert!(out.contains("--- f1.rs: FULL (400 lines)\n"), "{out}");
        assert!(
            out.contains("--- f4.rs: PARTIAL (400 lines; lines 1-300 shown, lines 301-400 OMITTED: attachment budget exhausted)\n"),
            "{out}"
        );
        assert!(out.contains("300 | 0300\n--- late.rs: OMITTED (3 lines; attachment budget exhausted; content not shown)\n"), "{out}");
        assert_eq!(out.matches("301 | 0301\n").count(), 3, "{out}");
        // Per-file cap without a snapshot: first 400 lines, cut named.
        let big = render_file_attachments(
            PERIPHERAL_HEADER,
            &[text_file("big.rs", numbered_text(450, 4), None)],
        );
        assert!(
            big.contains("--- big.rs: PARTIAL (450 lines; lines 1-400 shown, lines 401-450 OMITTED: per-file cap of 400 lines, no pre-drive snapshot to diff)\n"),
            "{big}"
        );
        // A single row over the char budget is omitted whole, never sliced.
        let wide = render_file_attachments(
            PERIPHERAL_HEADER,
            &[text_file(
                "min.js",
                "x".repeat(ATTACH_BUDGET_CHARS + 1),
                None,
            )],
        );
        assert!(wide.contains("--- min.js: OMITTED (1 lines;"), "{wide}");
        assert!(!wide.contains("xxx"), "{wide}");
        // File cap: names past it are listed, not silently dropped.
        let many: Vec<AttachedFile> = (0..ATTACH_MAX_FILES + 2)
            .map(|i| text_file(&format!("m{i}.rs"), "a\n".into(), None))
            .collect();
        let capped = render_file_attachments(PERIPHERAL_HEADER, &many);
        assert!(
            capped.contains("(2 more files OMITTED, over the 20-file cap: m20.rs, m21.rs)"),
            "{capped}"
        );
    }

    /// A large file with a pre-drive snapshot renders as a
    /// numbered diff (post-turn line numbers, removed rows unnumbered);
    /// a created file renders in full and says so.
    #[test]
    fn peripheral_large_file_with_snapshot_renders_numbered_diff() {
        let old = numbered_text(1000, 4);
        let now = old
            .replace("0500\n", "0500 changed\n")
            .replace("0900\n", "");
        let out = render_file_attachments(
            PERIPHERAL_HEADER,
            &[
                text_file("big.rs", now, Some(Some(old))),
                text_file("new.rs", "fn n() {}\n".into(), Some(None)),
            ],
        );
        assert!(
            out.contains("--- big.rs: DIFF vs its pre-drive snapshot (999 lines now, over the 400-line full-content cap; 2 hunks, 3 context lines; lines outside the hunks are unchanged and not shown)\n@@ post-turn lines 497-503 @@\n497   | 0497\n"),
            "{out}"
        );
        assert!(
            out.contains("    - | 0500\n500 + | 0500 changed\n"),
            "{out}"
        );
        assert!(out.contains("    - | 0900\n"), "{out}");
        assert!(
            out.contains("--- new.rs: FULL (CREATED this drive, 1 lines)\n1 | fn n() {}\n"),
            "{out}"
        );
    }

    /// No peripheral file → reviewed text and review brief are
    /// byte-identical to the pre-#1 shapes (golden).
    #[tokio::test]
    async fn no_peripheral_files_keeps_scored_text_and_brief_byte_identical() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "artifact body\n").unwrap();
        for data in [
            serde_json::json!({ "artifact": "closing text", "mutated_files": true, "mutated_paths": ["a.txt"] }),
            serde_json::json!({ "artifact": "closing text" }),
        ] {
            let out = peripheral_executor(&dir, data)
                .execute(act_input(Vec::new()))
                .await
                .unwrap();
            // File-first: the file decides whether or not the turn mutated it.
            assert_eq!(out, "artifact body\n");
        }
        let brief = render_review_brief(
            &ReviewerInput {
                goal: "g".into(),
                criteria: "c".into(),
                artifact: "artifact body\n".into(),
                mode: LoopMode::Act,
            },
            Some(std::path::Path::new("a.txt")),
        );
        assert_eq!(
            brief,
            "MODE:\nact\n\nGOAL:\ng\n\nACCEPTANCE CRITERIA:\nc\n\nARTIFACT UNDER REVIEW:\nartifact body\n\n\nARTIFACT PATH:\na.txt\n\nVERDICT DISCIPLINE:\nEvery finding MUST cite the accused file and line (`line` is a 1-based integer, or null) and quote the accused code in its issue; the quoted content must appear in ARTIFACT UNDER REVIEW above. A finding that cites no file, cites a file whose content is not in the artifact text, or describes a type's fields or signatures without having that type's definition in evidence, is a hypothesis — drop it, do not emit it. When prior-round context is absent, say what was checked, not what was assumed."
        );
    }

    /// With peripheral files attached, VERDICT DISCIPLINE puts
    /// them in scope; the CREATE-job note still judges only the primary
    /// artifact text (attachments are not artifact content).
    #[test]
    fn peripheral_section_in_scope_for_discipline_and_create_note() {
        let attached = render_file_attachments(
            PERIPHERAL_HEADER,
            &[text_file("b.rs", "fn b() {}\n".into(), None)],
        );
        let brief = render_review_brief(
            &ReviewerInput {
                goal: "g".into(),
                criteria: "c".into(),
                artifact: format!("body{attached}"),
                mode: LoopMode::Act,
            },
            None,
        );
        assert!(
            brief.ends_with(" Files under PERIPHERAL FILES MUTATED THIS DRIVE are part of ARTIFACT UNDER REVIEW: findings on them are in scope — cite the listed path and the shown line number. A file listed there without content (DELETED, BINARY, TOO LARGE, UNREADABLE, SKIPPED, OMITTED) has no lines in evidence."),
            "{brief}"
        );
        let dir = tempfile::tempdir().unwrap();
        let wd = polymorph_delegate::WorkdirSpec::InPlace(dir.path().to_path_buf());
        let ghost = std::path::Path::new("ghost.rs");
        assert!(create_job_note(&wd, ghost, &attached).is_some());
        assert!(create_job_note(&wd, ghost, &format!("body{attached}")).is_none());
    }

    /// The fix runner snapshots each write/edit target once,
    /// before its first mutation (`null` = did not exist yet).
    #[tokio::test]
    async fn agentic_runner_snapshots_targets_before_first_mutation() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("old.txt"), "before\n").unwrap();
        let stub = Arc::new(StubAgenticTurn::new(vec![
            collected(
                "",
                vec![
                    tool_call(
                        "c1",
                        "write",
                        serde_json::json!({"path": "old.txt", "content": "after\n"}),
                    ),
                    tool_call(
                        "c2",
                        "write",
                        serde_json::json!({"path": "new.txt", "content": "n\n"}),
                    ),
                    tool_call(
                        "c3",
                        "write",
                        serde_json::json!({"path": "./old.txt", "content": "again\n"}),
                    ),
                ],
            ),
            collected("done", vec![]),
        ]));
        let runner = agentic_runner(stub, vec!["write".into()], vec![], 3);
        let out = runner.run_turn(agentic_ctx(&dir, "fix it")).await.unwrap();
        assert_eq!(
            out.data.as_ref().unwrap()["pre_mutation_snapshots"],
            serde_json::json!([
                {"path": "old.txt", "content": "before\n"},
                {"path": "new.txt", "content": null},
            ])
        );
    }

    /// Records every brief its inner runner is handed (fixer-brief tap).
    struct BriefTap {
        inner: Arc<dyn TurnRunner>,
        seen: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait]
    impl TurnRunner for BriefTap {
        async fn run_turn(&self, ctx: TurnContext) -> anyhow::Result<TurnOutput> {
            self.seen.lock().unwrap().push(ctx.brief.clone());
            self.inner.run_turn(ctx).await
        }
    }

    /// Stub reviewer that grades only what its brief shows: the artifact
    /// must define `fn a() {}`, and any visible `BROKEN` line fails b.rs.
    struct GradingReviewer;

    #[async_trait]
    impl TurnRunner for GradingReviewer {
        async fn run_turn(&self, ctx: TurnContext) -> anyhow::Result<TurnOutput> {
            let finding = |file: &str, line: u32, issue: &str| {
                serde_json::json!({
                    "verdict": "fail",
                    "findings": [{"severity": "P0", "file": file, "line": line,
                        "issue": issue, "proposed_fix": "repair it"}],
                    "confidence": 0.9
                })
                .to_string()
            };
            let text = if ctx.brief.contains("BROKEN") {
                finding("src/b.rs", 2, "stray brace breaks the build")
            } else if !ctx.brief.contains("fn a() {}") {
                finding("a.rs", 1, "a is a stub")
            } else {
                pass_json()
            };
            Ok(done_out(&text, serde_json::json!({ "verdict_text": text })))
        }
    }

    /// End-to-end: the fixer repairs the
    /// artifact but breaks a peripheral file in the same turn. Attached,
    /// the reviewer catches it, the finding reaches the next fixer brief,
    /// and the drive passes only once the file is repaired; without the
    /// attachment the same drive passes blind over the broken file.
    #[tokio::test]
    async fn multi_file_loop_reviewer_catches_broken_peripheral_file() {
        async fn drive(
            attach: bool,
            turns: Vec<polymorph_llm::CollectedTurn>,
        ) -> (Outcome, Vec<String>, String) {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("a.rs"), "fn a() { todo!() }\n").unwrap();
            std::fs::create_dir(dir.path().join("src")).unwrap();
            std::fs::write(dir.path().join("src/b.rs"), "fn b() {\n}\n").unwrap();
            let wd = polymorph_delegate::WorkdirSpec::InPlace(dir.path().to_path_buf());
            let bus = Arc::new(Bus::flat());
            let tap = Arc::new(BriefTap {
                inner: Arc::new(agentic_runner(
                    Arc::new(StubAgenticTurn::new(turns)),
                    vec!["write".into()],
                    vec![],
                    3,
                )),
                seen: std::sync::Mutex::new(Vec::new()),
            });
            let reviewer = BusReviewer {
                bus: bus.clone(),
                runner: Arc::new(GradingReviewer),
                workdir: wd.clone(),
                artifact_path: Some("a.rs".into()),
            };
            let executor = BusExecutor {
                bus,
                runner: tap.clone(),
                workdir: wd,
                tools: vec!["write".into()],
                tool_outcomes: Arc::new(std::sync::Mutex::new(Vec::new())),
                merge: None,
                artifact_path: Some("a.rs".into()),
                peripherals: attach.then(PeripheralFiles::default),
                artifact_set: None,
            };
            let cfg = LoopConfig {
                max_iterations: 5,
                surviving_finding_escalation: 3,
            };
            let rep = run_loop(
                &cfg,
                "make a() real",
                "",
                "fn a() { todo!() }\n",
                LoopMode::Act,
                &reviewer,
                &executor,
            )
            .await;
            let b = std::fs::read_to_string(dir.path().join("src/b.rs")).unwrap();
            let briefs = tap.seen.lock().unwrap().clone();
            (rep.outcome, briefs, b)
        }
        let write = |id: &str, path: &str, content: &str| {
            tool_call(
                id,
                "write",
                serde_json::json!({"path": path, "content": content}),
            )
        };
        let turn1 = || {
            collected(
                "",
                vec![
                    write("c1", "a.rs", "fn a() {}\n"),
                    write("c2", "src/b.rs", "fn b() {\n}} // BROKEN\n"),
                ],
            )
        };
        let (outcome, briefs, b) = drive(
            true,
            vec![
                turn1(),
                collected("fixed a", vec![]),
                collected("", vec![write("c3", "src/b.rs", "fn b() {\n}\n")]),
                collected("fixed b", vec![]),
            ],
        )
        .await;
        assert_eq!(outcome, Outcome::Passed { rounds: 3 });
        assert_eq!(b, "fn b() {\n}\n");
        assert_eq!(briefs.len(), 2);
        assert!(
            briefs[1].contains("src/b.rs:2 — stray brace breaks the build"),
            "{}",
            briefs[1]
        );
        assert!(briefs[1].contains("2 | }} // BROKEN"), "{}", briefs[1]);
        // Blind (pre-#1): passes in round 2 with the build broken.
        let (outcome, _, b) = drive(false, vec![turn1(), collected("fixed a", vec![])]).await;
        assert_eq!(outcome, Outcome::Passed { rounds: 2 });
        assert!(b.contains("BROKEN"));
    }

    /// Run git (no shell) in a temp repo; panics loudly on failure.
    fn git_in(dir: &std::path::Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git on PATH");
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    /// A temp git repo (local identity — never the real repo's) with
    /// `files` committed as the base.
    fn committed_repo(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        git_in(d, &["init", "-q"]);
        git_in(d, &["config", "user.name", "t"]);
        git_in(d, &["config", "user.email", "t@example.invalid"]);
        git_in(d, &["config", "core.autocrlf", "false"]);
        git_in(d, &["config", "commit.gpgsign", "false"]);
        for (path, body) in files {
            let full = d.join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, body).unwrap();
        }
        git_in(d, &["add", "-A"]);
        git_in(d, &["commit", "-q", "-m", "base"]);
        dir
    }

    /// In set mode a mutated file that is a set member renders
    /// once (in the set, whatever the spelling), a non-member still rides
    /// the peripheral section, and the brief puts the set in scope.
    #[tokio::test]
    async fn artifact_set_members_never_duplicated_as_peripheral() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(dir.path().join("notes.md"), "n\n").unwrap();
        let mut ex = peripheral_executor(
            &dir,
            serde_json::json!({
                "artifact": "closing text",
                "mutated_files": true,
                "mutated_paths": ["./a.rs", "notes.md"],
            }),
        );
        ex.artifact_path = None;
        ex.artifact_set = Some(crate::ArtifactSet {
            declared: vec!["a.rs".into()],
            diff_base: None,
        });
        let out = ex.execute(act_input(Vec::new())).await.unwrap();
        assert_eq!(
            out,
            "ARTIFACT FILES UNDER REVIEW (1 files, declared; harness-read from disk; `N | ` prefixes are 1-based file line numbers, not file content — cite findings as path:line):\n--- a.rs: FULL (1 lines)\n1 | fn a() {}\n\n\nPERIPHERAL FILES MUTATED THIS DRIVE (harness-read from disk after the fix turn; `N | ` prefixes are 1-based file line numbers, not file content):\n--- notes.md: FULL (1 lines)\n1 | n\n"
        );
        let brief = render_review_brief(
            &ReviewerInput {
                goal: "g".into(),
                criteria: "c".into(),
                artifact: out,
                mode: LoopMode::Act,
            },
            None,
        );
        assert!(!brief.contains("ARTIFACT PATH:"), "{brief}");
        assert!(
            brief.contains(" Every file under ARTIFACT FILES UNDER REVIEW is the artifact: findings on any of them are in scope — cite the listed path and the shown line number."),
            "{brief}"
        );
        assert!(
            brief.ends_with(" Files under PERIPHERAL FILES MUTATED THIS DRIVE are part of ARTIFACT UNDER REVIEW: findings on them are in scope — cite the listed path and the shown line number. A file listed there without content (DELETED, BINARY, TOO LARGE, UNREADABLE, SKIPPED, OMITTED) has no lines in evidence."),
            "{brief}"
        );
        // A set that cannot be read fails the turn loudly (never grades
        // stale or empty text): diff mode outside a git work tree.
        let mut ex = peripheral_executor(&dir, serde_json::json!({ "artifact": "t" }));
        ex.artifact_set = Some(crate::ArtifactSet {
            declared: Vec::new(),
            diff_base: Some("HEAD".into()),
        });
        let err = ex.execute(act_input(Vec::new())).await.unwrap_err();
        assert!(
            err.to_string().contains("not inside a git work tree"),
            "{err}"
        );
    }

    /// End-to-end (worktree-diff mode): the
    /// fixer repairs `a.rs` but breaks `src/b.rs` and adds `src/c.rs` in
    /// the same turn. The diff set grows to all three, the reviewer
    /// catches the broken second file by `path:line`, and the drive passes
    /// only after the repair. Single-file grading of the same drive passes
    /// blind over the broken file.
    #[tokio::test]
    async fn diff_mode_drive_catches_broken_second_file() {
        async fn drive(
            diff_mode: bool,
            turns: Vec<polymorph_llm::CollectedTurn>,
        ) -> (Outcome, Vec<String>, Vec<String>, String) {
            let dir = committed_repo(&[
                ("a.rs", "fn a() { todo!() }\n"),
                ("src/b.rs", "fn b() {\n}\n"),
            ]);
            std::fs::write(dir.path().join("a.rs"), "fn a() { todo!() } // wip\n").unwrap();
            let wd = polymorph_delegate::WorkdirSpec::InPlace(dir.path().to_path_buf());
            let set = crate::ArtifactSet {
                declared: Vec::new(),
                diff_base: Some("HEAD".into()),
            };
            let initial = if diff_mode {
                set.render(dir.path()).unwrap().text
            } else {
                std::fs::read_to_string(dir.path().join("a.rs")).unwrap()
            };
            let bus = Arc::new(Bus::flat());
            let fix_tap = Arc::new(BriefTap {
                inner: Arc::new(agentic_runner(
                    Arc::new(StubAgenticTurn::new(turns)),
                    vec!["write".into()],
                    vec![],
                    3,
                )),
                seen: std::sync::Mutex::new(Vec::new()),
            });
            let review_tap = Arc::new(BriefTap {
                inner: Arc::new(GradingReviewer),
                seen: std::sync::Mutex::new(Vec::new()),
            });
            let reviewer = BusReviewer {
                bus: bus.clone(),
                runner: review_tap.clone(),
                workdir: wd.clone(),
                artifact_path: (!diff_mode).then(|| "a.rs".into()),
            };
            let executor = BusExecutor {
                bus,
                runner: fix_tap.clone(),
                workdir: wd,
                tools: vec!["write".into()],
                tool_outcomes: Arc::new(std::sync::Mutex::new(Vec::new())),
                merge: None,
                artifact_path: (!diff_mode).then(|| "a.rs".into()),
                peripherals: diff_mode.then(PeripheralFiles::default),
                artifact_set: diff_mode.then_some(set),
            };
            let cfg = LoopConfig {
                max_iterations: 5,
                surviving_finding_escalation: 3,
            };
            let rep = run_loop(
                &cfg,
                "make a() real",
                "",
                &initial,
                LoopMode::Act,
                &reviewer,
                &executor,
            )
            .await;
            let b = std::fs::read_to_string(dir.path().join("src/b.rs")).unwrap();
            let fixes = fix_tap.seen.lock().unwrap().clone();
            let reviews = review_tap.seen.lock().unwrap().clone();
            (rep.outcome, fixes, reviews, b)
        }
        let write = |id: &str, path: &str, content: &str| {
            tool_call(
                id,
                "write",
                serde_json::json!({"path": path, "content": content}),
            )
        };
        let turn1 = || {
            collected(
                "",
                vec![
                    write("c1", "a.rs", "fn a() {}\n"),
                    write("c2", "src/b.rs", "fn b() {\n}} // BROKEN\n"),
                    write("c3", "src/c.rs", "fn c() {}\n"),
                ],
            )
        };
        let (outcome, fixes, reviews, b) = drive(
            true,
            vec![
                turn1(),
                collected("fixed a", vec![]),
                collected("", vec![write("c4", "src/b.rs", "fn b() {\n}\n")]),
                collected("fixed b", vec![]),
            ],
        )
        .await;
        assert_eq!(outcome, Outcome::Passed { rounds: 3 });
        assert_eq!(b, "fn b() {\n}\n");
        assert_eq!(reviews.len(), 3);
        // Round 1: only the pre-drive change is in the set.
        assert!(
            reviews[0].contains("- a.rs: MODIFIED vs HEAD\n"),
            "{}",
            reviews[0]
        );
        assert!(!reviews[0].contains("src/b.rs"), "{}", reviews[0]);
        // Round 2: the fix turn's files joined, each once, citable.
        let r2 = &reviews[1];
        for want in [
            "ARTIFACT FILES UNDER REVIEW (3 files, worktree diff vs HEAD: 3 changed;",
            "- src/b.rs: MODIFIED vs HEAD\n",
            "- src/c.rs: UNTRACKED (new, not ignored)\n",
            "--- src/b.rs: FULL (2 lines)\n1 | fn b() {\n2 | }} // BROKEN\n",
            "Every file under ARTIFACT FILES UNDER REVIEW is the artifact",
        ] {
            assert!(r2.contains(want), "missing {want:?} in {r2}");
        }
        assert!(!r2.contains(PERIPHERAL_HEADER), "{r2}");
        assert_eq!(r2.matches("--- src/b.rs:").count(), 1, "{r2}");
        // The finding reached the next fixer brief by path:line.
        assert_eq!(fixes.len(), 2);
        assert!(
            fixes[1].contains("src/b.rs:2 — stray brace breaks the build"),
            "{}",
            fixes[1]
        );
        // Single-file grading of the same drive: passes blind in round 2.
        let (outcome, _, _, b) = drive(false, vec![turn1(), collected("fixed a", vec![])]).await;
        assert_eq!(outcome, Outcome::Passed { rounds: 2 });
        assert!(b.contains("BROKEN"));
    }

    /// The seed fires exactly when the CREATE note does, names
    /// the target file, and the note's bytes are unchanged by the shared
    /// status sentence.
    #[test]
    fn create_job_seed_matches_create_note_condition() {
        let dir = tempfile::tempdir().unwrap();
        let wd = polymorph_delegate::WorkdirSpec::InPlace(dir.path().to_path_buf());
        let ghost = std::path::Path::new("src/ghost.rs");
        assert_eq!(
            create_job_note(&wd, ghost, "").unwrap(),
            "\n\nARTIFACT FILE STATUS:\nThe artifact path does not exist on disk and no current content was provided (CREATE job or deleted path). Judge goal/criteria fit against the empty artifact; do NOT emit findings merely claiming the file is missing. If the file should be created, emit exactly ONE finding that names the target file, states the required contents from the goal and criteria (that file citation satisfies VERDICT DISCIPLINE), and proposes the creation — a failing verdict with zero findings is schema-invalid and wastes the round.\n"
        );
        let seed = create_job_seed(&wd, ghost, "").expect("seed expected");
        assert_eq!(seed.kind, crate::SeedKind::Create);
        let seed = seed.findings;
        assert_eq!(seed.len(), 1);
        assert_eq!(seed[0].file, "src/ghost.rs");
        assert_eq!(seed[0].line, None);
        assert_eq!(seed[0].severity, polymorph_core::Severity::P1);
        assert!(
            seed[0].issue.contains(CREATE_JOB_STATUS),
            "{}",
            seed[0].issue
        );
        assert!(
            seed[0].issue.contains("GOAL and ACCEPTANCE CRITERIA"),
            "{}",
            seed[0].issue
        );
        assert!(seed[0].proposed_fix.starts_with("Create src/ghost.rs "));
        // Attached peripheral files are not artifact content (note parity).
        let attached = render_file_attachments(
            PERIPHERAL_HEADER,
            &[text_file("b.rs", "fn b() {}\n".into(), None)],
        );
        assert!(create_job_seed(&wd, ghost, &attached).is_some());
        // Content given (moved path): review-first.
        assert!(create_job_seed(&wd, ghost, "some text").is_none());
        // Existing file, empty or whitespace-only: not a CREATE job.
        for body in ["", "  \n\t\n"] {
            std::fs::write(dir.path().join("real.rs"), body).unwrap();
            let real = std::path::Path::new("real.rs");
            assert!(create_job_note(&wd, real, body).is_none());
            assert!(create_job_seed(&wd, real, body).is_none());
        }
    }

    /// Fix-turn double for the seeded CREATE loop: turn 1 creates the
    /// artifact plus a peripheral helper with an LSP error on the artifact;
    /// turn 2 fixes it and makes the verify command pass.
    struct CreatingRunner {
        turns: std::sync::Mutex<u32>,
        briefs: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait]
    impl TurnRunner for CreatingRunner {
        async fn run_turn(&self, ctx: TurnContext) -> anyhow::Result<TurnOutput> {
            self.briefs.lock().unwrap().push(ctx.brief.clone());
            let turn = {
                let mut n = self.turns.lock().unwrap();
                *n += 1;
                *n
            };
            let data = if turn == 1 {
                std::fs::write(ctx.workdir.join("new.rs"), "fn a() {\n    1\n    x\n}\n").unwrap();
                std::fs::write(ctx.workdir.join("helper.rs"), "fn h() {}\n").unwrap();
                serde_json::json!({
                    "artifact": "",
                    "mutated_files": true,
                    "mutated_paths": ["new.rs", "helper.rs"],
                    "pre_mutation_snapshots": [
                        {"path": "new.rs", "content": null},
                        {"path": "helper.rs", "content": null},
                    ],
                    "tool_outcomes": [
                        {"tool": "write", "outcome": "ok", "paths": ["new.rs"]},
                        {"tool": "write", "outcome": "ok", "paths": ["helper.rs"]},
                    ],
                    "lsp_diagnostics": [
                        {"path": "new.rs", "items": [lsp_item(3, Some(1), "expected `;`")]},
                    ],
                })
            } else {
                std::fs::write(ctx.workdir.join("new.rs"), "fn a() {}\n").unwrap();
                std::fs::write(ctx.workdir.join("ok.txt"), "ok\n").unwrap();
                serde_json::json!({
                    "artifact": "",
                    "mutated_files": true,
                    "mutated_paths": ["new.rs"],
                    "tool_outcomes": [
                        {"tool": "edit", "outcome": "ok", "paths": ["new.rs"]},
                    ],
                    "lsp_diagnostics": [{"path": "new.rs", "items": []}],
                })
            };
            Ok(done_out("", data))
        }
    }

    /// A seeded CREATE loop skips the round-1
    /// reviewer, runs the first fix turn in round 1, and that turn is
    /// gated (verify + LSP) and attached (artifact file + peripheral) like
    /// any later fix turn.
    #[tokio::test]
    async fn seeded_create_round_one_fix_turn_is_gated_and_attached() {
        use polymorph_core::VerdictKind::Pass;
        let dir = tempfile::tempdir().unwrap();
        let wd = polymorph_delegate::WorkdirSpec::InPlace(dir.path().to_path_buf());
        let artifact_path = std::path::PathBuf::from("new.rs");
        let seed = create_job_seed(&wd, &artifact_path, "");
        assert!(seed.is_some());
        let fixer = Arc::new(CreatingRunner {
            turns: std::sync::Mutex::new(0),
            briefs: std::sync::Mutex::new(Vec::new()),
        });
        let verify_gate = Arc::new(HarnessGate::default());
        let lsp_gate = Arc::new(HarnessGate::default());
        let executor = BusExecutor {
            bus: Arc::new(Bus::flat()),
            runner: Arc::new(VerifyGateRunner {
                inner: Arc::new(LspGateRunner {
                    inner: fixer.clone(),
                    gate: lsp_gate.clone(),
                }),
                gate: verify_gate.clone(),
                cmds: vec![needs_ok_txt()],
                timeout: std::time::Duration::from_secs(30),
            }),
            workdir: wd,
            tools: Vec::new(),
            tool_outcomes: Arc::new(std::sync::Mutex::new(Vec::new())),
            merge: None,
            artifact_path: Some(artifact_path),
            peripherals: Some(PeripheralFiles::default()),
            artifact_set: None,
        };
        let reviewer = HarnessGateReviewer {
            inner: SeeingScript {
                verdicts: std::sync::Mutex::new(vec![Pass, Pass].into()),
                seen: std::sync::Mutex::new(Vec::new()),
            },
            gates: vec![verify_gate, lsp_gate],
        };
        let cfg = LoopConfig {
            max_iterations: 3,
            ..Default::default()
        };
        let report = crate::run_loop_seeded(
            &cfg,
            "create new.rs",
            "",
            "",
            LoopMode::Act,
            &reviewer,
            &executor,
            seed,
        )
        .await;
        assert_eq!(report.outcome, Outcome::Passed { rounds: 3 });
        // Two reviewer calls (rounds 2 and 3) — round 1 was the seed.
        let seen = reviewer.inner.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 2);
        assert_eq!(
            report.rounds_log[0].synthesized,
            Some(crate::SeedKind::Create)
        );
        assert_eq!(report.rounds_log[1].synthesized, None);
        // The first fix brief carries the harness creation finding.
        let briefs = fixer.briefs.lock().unwrap().clone();
        assert_eq!(briefs.len(), 2);
        assert!(
            briefs[0].contains("new.rs:- — Harness-synthesized CREATE finding"),
            "{}",
            briefs[0]
        );
        // Round 2 grades the created file + the peripheral helper, with
        // the round-1 fix turn's verify and LSP evidence.
        assert!(
            seen[0].starts_with("fn a() {\n    1\n    x\n}\n"),
            "{}",
            seen[0]
        );
        assert!(seen[0].contains(PERIPHERAL_HEADER), "{}", seen[0]);
        assert!(seen[0].contains("helper.rs"), "{}", seen[0]);
        assert!(
            seen[0].contains("- new.rs: EXISTS (4 lines)"),
            "{}",
            seen[0]
        );
        assert!(seen[0].contains("- VERIFY RESULT: FAILED"), "{}", seen[0]);
        // ...and both gates veto the round-2 pass (verify first).
        let round2 = &report.rounds_log[1];
        assert_eq!(round2.verdict, crate::RoundVerdict::Fail);
        assert_eq!(round2.findings, 2);
        assert!(
            round2.issues[0].starts_with(&format!("P1 {VERIFY_FINDING_FILE} Verify gate")),
            "{:?}",
            round2.issues
        );
        assert!(
            round2.issues[1].starts_with("P1 new.rs:3 LSP error gate"),
            "{:?}",
            round2.issues
        );
        assert!(seen[1].contains("- VERIFY RESULT: PASSED"), "{}", seen[1]);
    }

    /// Verification bonus rounds scale past the single
    /// one while every round mutates or verifies, and stop at the cap.
    #[tokio::test]
    async fn agentic_runner_bonus_rounds_scale_to_cap_while_progressing() {
        let dir = tempfile::tempdir().unwrap();
        let write = |id: &str, body: &str| {
            tool_call(
                id,
                "write",
                serde_json::json!({"path": "a.txt", "content": body}),
            )
        };
        let stub = Arc::new(StubAgenticTurn::new(vec![
            collected("", vec![write("c1", "one\n")]),
            // Bonus 1 (round 1 mutated).
            collected("", vec![write("c2", "two\n")]),
            // Bonus 2 (bonus 1 mutated): verify.
            collected(
                "",
                vec![tool_call(
                    "c3",
                    "bash",
                    serde_json::json!({"cmd": "exit 1"}),
                )],
            ),
            // Bonus 3 (bonus 2 verified a mutation): repair.
            collected("", vec![write("c4", "four\n")]),
            // Cap reached: dropped although the last round mutated.
            collected("", vec![write("c5", "five\n")]),
            collected("done", vec![]),
        ]));
        let runner = agentic_runner(stub, vec!["write".into(), "bash".into()], vec![], 1);
        assert_eq!(runner.max_bonus_tool_rounds, DEFAULT_MAX_BONUS_TOOL_ROUNDS);
        let out = runner
            .run_turn(agentic_ctx(&dir, "create a.txt"))
            .await
            .unwrap();
        assert_eq!(out.tool_calls, 4);
        assert!(
            out.summary.contains("4 tool rounds, 4 calls, 1 dropped"),
            "{}",
            out.summary
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "four\n"
        );
        // The ledger stays chronological across bonus rounds: the
        // last write came after the last command that ran.
        let data = out.data.as_ref().unwrap();
        let ledger = render_command_ledger(data["tool_outcomes"].as_array().unwrap());
        assert!(
            ledger.contains("- MUTATIONS AFTER LAST COMMAND: yes - files: a.txt\n"),
            "{ledger}"
        );
    }

    /// Progress ends the extension before the cap — a second
    /// check of an already-verified mutation is not progress.
    #[tokio::test]
    async fn agentic_runner_bonus_stops_when_rounds_stop_progressing() {
        let dir = tempfile::tempdir().unwrap();
        let bash = |id: &str| tool_call(id, "bash", serde_json::json!({"cmd": "exit 0"}));
        let stub = Arc::new(StubAgenticTurn::new(vec![
            collected(
                "",
                vec![tool_call(
                    "c1",
                    "write",
                    serde_json::json!({"path": "a.txt", "content": "x\n"}),
                )],
            ),
            // Bonus 1 (write): verifies the mutation.
            collected("", vec![bash("c2")]),
            // Bonus 2 (bonus 1 verified a mutation): re-runs the check.
            collected("", vec![bash("c3")]),
            // Bonus 2 made no progress: dropped with cap to spare.
            collected("", vec![bash("c4")]),
            collected("done", vec![]),
        ]));
        let mut runner = agentic_runner(stub, vec!["write".into(), "bash".into()], vec![], 1);
        runner.max_bonus_tool_rounds = 10;
        let out = runner.run_turn(agentic_ctx(&dir, "write")).await.unwrap();
        assert_eq!(out.tool_calls, 3);
        assert!(
            out.summary.contains("3 tool rounds, 3 calls, 1 dropped"),
            "{}",
            out.summary
        );
        let data = out.data.as_ref().unwrap();
        let ledger = render_command_ledger(data["tool_outcomes"].as_array().unwrap());
        assert!(
            ledger.contains("- MUTATIONS AFTER LAST COMMAND: no\n"),
            "{ledger}"
        );
    }

    /// No bonus when the last allowed round neither mutated nor
    /// verified (reads only), denied tools are never progress, and a cap
    /// of 0 turns the verification bonus off.
    #[tokio::test]
    async fn agentic_runner_no_bonus_without_progress_or_with_zero_cap() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "x\n").unwrap();
        let read = |id: &str| tool_call(id, "read", serde_json::json!({"path": "a.txt"}));
        let stub = Arc::new(StubAgenticTurn::new(vec![
            collected("", vec![read("c1")]),
            collected("", vec![read("c2")]),
            collected("done", vec![]),
        ]));
        let runner = agentic_runner(stub, vec!["read".into()], vec![], 1);
        let out = runner.run_turn(agentic_ctx(&dir, "look")).await.unwrap();
        assert_eq!(out.tool_calls, 1);
        // Denied write + denied bash: neither counts, so no bonus.
        let stub = Arc::new(StubAgenticTurn::new(vec![
            collected(
                "",
                vec![
                    tool_call(
                        "c1",
                        "write",
                        serde_json::json!({"path": "a.txt", "content": "y\n"}),
                    ),
                    tool_call("c2", "bash", serde_json::json!({"cmd": "exit 0"})),
                ],
            ),
            collected("", vec![read("c3")]),
            collected("done", vec![]),
        ]));
        let runner = agentic_runner(
            stub,
            vec!["write".into(), "bash".into(), "read".into()],
            vec!["write".into(), "bash".into()],
            1,
        );
        let out = runner.run_turn(agentic_ctx(&dir, "try")).await.unwrap();
        assert_eq!(out.tool_calls, 2);
        assert!(out.summary.contains("1 tool rounds"), "{}", out.summary);
        // Cap 0: a mutating last round earns nothing.
        let stub = Arc::new(StubAgenticTurn::new(vec![
            collected(
                "",
                vec![tool_call(
                    "c1",
                    "write",
                    serde_json::json!({"path": "b.txt", "content": "z\n"}),
                )],
            ),
            collected("", vec![read("c2")]),
            collected("done", vec![]),
        ]));
        let mut runner = agentic_runner(stub, vec!["write".into(), "read".into()], vec![], 1);
        runner.max_bonus_tool_rounds = 0;
        let out = runner.run_turn(agentic_ctx(&dir, "write")).await.unwrap();
        assert_eq!(out.tool_calls, 1);
    }

    /// Edit renames (`MV`): the ledger names both ends (source, then
    /// destination, plus the `renamed` pair), the destination is attached
    /// with its provenance and its source's pre-drive content as baseline,
    /// the renamed-away source never carries an LSP snapshot, and rollback
    /// puts the original back and removes the destination.
    #[tokio::test]
    async fn edit_rename_tracks_both_ends_attaches_and_rolls_back() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.txt"), "artifact\n").unwrap();
        std::fs::write(root.join("old.rs"), "fn moved() {}\r\n").unwrap();
        let registry = Arc::new(polymorph_tools::registry::v1_registry());
        let text = polymorph_tools::hashline::apply::normalize(
            &std::fs::read(root.join("old.rs")).unwrap(),
        )
        .text;
        let tag = registry
            .mint_tag(std::path::Path::new("old.rs"), &text)
            .await;
        let stub = Arc::new(StubAgenticTurn::new(vec![
            collected(
                "",
                vec![tool_call(
                    "c1",
                    "edit",
                    serde_json::json!({"patch": format!("[old.rs#{tag}]\nMV new.rs\n")}),
                )],
            ),
            collected("done", vec![]),
        ]));
        let mut runner = agentic_runner(stub, vec!["edit".into()], vec![], 3);
        runner.registry = registry;
        let ledger = Arc::new(crate::RestoreLedger::with_limits(
            root.to_path_buf(),
            Some(root.join(".polymorph").join("attempts").join("t")),
            crate::RESTORE_MAX_FILE_BYTES,
            crate::RESTORE_MAX_TOTAL_BYTES,
        ));
        runner.restore = Some(ledger.clone());
        let out = runner.run_turn(agentic_ctx(&dir, "rename")).await.unwrap();
        let data = out.data.unwrap();
        assert_eq!(
            data["tool_outcomes"],
            serde_json::json!([{
                "tool": "edit",
                "outcome": "ok",
                "paths": ["old.rs", "new.rs"],
                "renamed": [{"from": "old.rs", "to": "new.rs"}],
            }])
        );
        assert_eq!(
            data["mutated_paths"],
            serde_json::json!(["old.rs", "new.rs"])
        );
        // The destination was consulted (no server here: no
        // snapshot); the source is gone, never a stale record.
        assert_eq!(
            data["lsp_diagnostics"],
            serde_json::json!([{"path": "new.rs", "items": null}])
        );
        // Both ends attached, provenance on the status line.
        let wd = polymorph_delegate::WorkdirSpec::InPlace(root.to_path_buf());
        let files = PeripheralFiles::default().observe(&wd, Some(Path::new("a.txt")), &data);
        assert_eq!(files[1].baseline, Some(Some("fn moved() {}\r\n".into())));
        assert_eq!(
            render_file_attachments(PERIPHERAL_HEADER, &files),
            "\n\nPERIPHERAL FILES MUTATED THIS DRIVE (harness-read from disk after the fix turn; `N | ` prefixes are 1-based file line numbers, not file content):\n--- old.rs: RENAMED to new.rs; DELETED (absent on disk after the turn: deleted, or renamed away; no content)\n--- new.rs: RENAMED from old.rs; FULL (1 lines)\n1 | fn moved() {}\n"
        );
        // The rename unwinds byte for byte.
        let report = ledger.rollback();
        assert_eq!(
            std::fs::read(root.join("old.rs")).unwrap(),
            b"fn moved() {}\r\n"
        );
        assert!(!root.join("new.rs").exists());
        assert_eq!(report.restored, ["old.rs"]);
        assert_eq!(report.removed, ["new.rs"]);
    }

    /// Command change detection: a command that changes files is both a
    /// command and a mutation landing at its end — named on its ledger
    /// line, in the turn's mutated files, after the last command (nothing
    /// verified it), attached with provenance. Commands that change nothing
    /// record exactly what they did before.
    #[tokio::test]
    async fn command_changes_join_ledger_ordering_and_attachments() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.txt"), "artifact\n").unwrap();
        std::fs::write(root.join("fmt.txt"), "unformatted source\n").unwrap();
        let stub = Arc::new(StubAgenticTurn::new(vec![
            collected(
                "",
                vec![
                    tool_call(
                        "c1",
                        "write",
                        serde_json::json!({"path": "a.txt", "content": "fixed\n"}),
                    ),
                    tool_call("c2", "bash", serde_json::json!({"cmd": "echo tests ok"})),
                    tool_call(
                        "c3",
                        "bash",
                        serde_json::json!({"cmd": "echo formatted> fmt.txt && echo made> new.txt"}),
                    ),
                ],
            ),
            collected("done", vec![]),
        ]));
        let mut runner = agentic_runner(stub, vec!["write".into(), "bash".into()], vec![], 3);
        runner.command_changes = crate::DEFAULT_COMMAND_CHANGE_MAX_FILES;
        let out = runner.run_turn(agentic_ctx(&dir, "fix")).await.unwrap();
        let data = out.data.unwrap();
        let ledger = data["tool_outcomes"].as_array().unwrap();
        assert_eq!(
            ledger[1],
            serde_json::json!({"tool": "bash", "outcome": "ok", "cmd": "echo tests ok", "exit_code": 0})
        );
        assert_eq!(
            ledger[2]["changed_paths"],
            serde_json::json!(["fmt.txt", "new.txt"])
        );
        assert!(ledger[2].get("deleted_paths").is_none());
        assert_eq!(
            data["mutated_paths"],
            serde_json::json!(["a.txt", "fmt.txt", "new.txt"])
        );
        assert_eq!(data["mutated_files"], true);
        let evidence = render_command_ledger(ledger);
        assert!(
            evidence.contains(
                "  #3 exit 0: echo formatted> fmt.txt && echo made> new.txt [CHANGED 2 files (harness-detected): fmt.txt, new.txt]\n"
            ),
            "{evidence}"
        );
        assert!(
            evidence.contains("- MUTATIONS AFTER LAST COMMAND: yes - files: fmt.txt, new.txt\n"),
            "{evidence}"
        );
        assert!(
            evidence.contains("- files mutated this turn (3): a.txt, fmt.txt, new.txt\n"),
            "{evidence}"
        );
        let wd = polymorph_delegate::WorkdirSpec::InPlace(root.to_path_buf());
        let files = PeripheralFiles::default().observe(&wd, Some(Path::new("a.txt")), &data);
        let rendered = render_file_attachments(PERIPHERAL_HEADER, &files);
        assert!(
            rendered.contains(
                "--- fmt.txt: first changed by a fixer command (harness-detected); FULL (1 lines)\n1 | formatted"
            ),
            "{rendered}"
        );
        assert!(
            rendered.contains(
                "--- new.txt: first changed by a fixer command (harness-detected); FULL (1 lines)\n1 | made\n"
            ),
            "{rendered}"
        );
    }

    /// Ordering semantics of command-made changes (pure ledger render): a
    /// later command that ran verifies them; a failed (timed-out) command's
    /// partial changes still count; a deletion is marked.
    #[test]
    fn command_change_ordering_in_the_ledger() {
        let write = serde_json::json!({"tool": "write", "outcome": "ok", "paths": ["a.rs"]});
        let fmt = serde_json::json!({
            "tool": "bash", "outcome": "ok", "cmd": "fmt", "exit_code": 0,
            "changed_paths": ["a.rs", "old.rs"], "deleted_paths": ["old.rs"],
        });
        let test =
            serde_json::json!({"tool": "bash", "outcome": "ok", "cmd": "test", "exit_code": 0});
        let verified = render_command_ledger(&[write.clone(), fmt, test.clone()]);
        assert!(
            verified.contains(
                "  #2 exit 0: fmt [CHANGED 2 files (harness-detected): a.rs, old.rs (deleted)]\n"
            ),
            "{verified}"
        );
        assert!(
            verified.contains("- MUTATIONS AFTER LAST COMMAND: no\n"),
            "{verified}"
        );
        assert!(verified.contains("- files mutated this turn (2): a.rs, old.rs\n"));
        let timed_out = serde_json::json!({
            "tool": "bash", "outcome": "error", "cmd": "gen",
            "changed_paths": ["gen.rs"], "changed_omitted": 7,
        });
        let late = render_command_ledger(&[write, test, timed_out]);
        assert!(
            late.contains(
                "  #3 error, no exit code: gen [CHANGED 8 files (harness-detected): gen.rs (+3 more)]\n"
            ),
            "{late}"
        );
        assert!(
            late.contains("- MUTATIONS AFTER LAST COMMAND: yes - files: gen.rs\n"),
            "{late}"
        );
        // A skipped detection is named on the line; nothing is invented.
        let quiet = serde_json::json!({
            "tool": "bash", "outcome": "ok", "cmd": "ls", "exit_code": 0,
            "change_detection": "skipped: tree too large (over the 2-file change-detection cap)",
        });
        let skipped = render_command_ledger(&[quiet]);
        assert!(
            skipped.contains(
                "  #1 exit 0: ls [change detection skipped: tree too large (over the 2-file change-detection cap)]\n"
            ),
            "{skipped}"
        );
        assert!(skipped.contains("- files mutated this turn: none\n"));
    }

    /// Detection on, nothing changed by commands: the turn's data, the fed
    /// tool results and the evidence block are byte-identical to detection
    /// off.
    #[tokio::test]
    async fn command_change_detection_is_invisible_when_commands_change_nothing() {
        async fn run(detect: usize) -> (serde_json::Value, Vec<String>) {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("a.txt"), "x\n").unwrap();
            let stub = Arc::new(StubAgenticTurn::new(vec![
                collected(
                    "",
                    vec![
                        tool_call(
                            "c1",
                            "write",
                            serde_json::json!({"path": "b.txt", "content": "y\n"}),
                        ),
                        tool_call("c2", "bash", serde_json::json!({"cmd": "echo hi"})),
                    ],
                ),
                collected("done", vec![]),
            ]));
            let mut runner =
                agentic_runner(stub.clone(), vec!["write".into(), "bash".into()], vec![], 3);
            runner.command_changes = detect;
            let out = runner.run_turn(agentic_ctx(&dir, "fix")).await.unwrap();
            let seen = stub.seen.lock().unwrap();
            (out.data.unwrap(), fed_responses(seen.last().unwrap()))
        }
        let (off, fed_off) = run(0).await;
        let (on, fed_on) = run(crate::DEFAULT_COMMAND_CHANGE_MAX_FILES).await;
        assert_eq!(on, off);
        assert_eq!(fed_on, fed_off);
        let wd = polymorph_delegate::WorkdirSpec::InPlace(std::path::PathBuf::from("/none"));
        assert_eq!(
            render_harness_evidence(&wd, &[], &on),
            render_harness_evidence(&wd, &[], &off)
        );
    }

    /// A tree over the cap: detection is skipped for the rest of the turn
    /// (one walk finds out), every command says so, nothing is invented.
    #[tokio::test]
    async fn command_change_detection_skips_huge_trees_with_a_note() {
        let dir = tempfile::tempdir().unwrap();
        for f in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(dir.path().join(f), "x\n").unwrap();
        }
        let stub = Arc::new(StubAgenticTurn::new(vec![
            collected(
                "",
                vec![
                    tool_call("c1", "bash", serde_json::json!({"cmd": "echo y> made.txt"})),
                    tool_call("c2", "bash", serde_json::json!({"cmd": "echo z"})),
                ],
            ),
            collected("done", vec![]),
        ]));
        let mut runner = agentic_runner(stub, vec!["bash".into()], vec![], 3);
        runner.command_changes = 2;
        let out = runner.run_turn(agentic_ctx(&dir, "fix")).await.unwrap();
        let data = out.data.unwrap();
        let note = "skipped: tree too large (over the 2-file change-detection cap)";
        for entry in data["tool_outcomes"].as_array().unwrap() {
            assert_eq!(entry["change_detection"], note, "{entry}");
            assert!(entry.get("changed_paths").is_none());
        }
        assert_eq!(data["mutated_files"], false);
        assert!(
            render_command_ledger(data["tool_outcomes"].as_array().unwrap()).contains(&format!(
                "  #1 exit 0: echo y> made.txt [change detection {note}]\n"
            ))
        );
    }

    /// Changed files carry the record
    /// their post-command consult left; files past the consult cap say so
    /// (never the pool's stale record); deleted files are gone.
    #[test]
    fn lsp_snapshots_cover_command_changes() {
        use polymorph_tools::lsp::{uri_for, LspDiagnostic};
        let wd = std::path::Path::new("/w");
        let stale = vec![LspDiagnostic {
            line: Some(1),
            severity: Some(1),
            message: "stale".into(),
        }];
        let mut details = std::collections::BTreeMap::new();
        for f in ["a.rs", "b.rs", "c.rs"] {
            details.insert(uri_for(wd, f), stale.clone());
        }
        let outcomes = vec![
            serde_json::json!({"tool": "write", "outcome": "ok", "paths": ["c.rs"]}),
            serde_json::json!({
                "tool": "bash", "outcome": "ok", "cmd": "gen", "exit_code": 0,
                "changed_paths": ["a.rs", "b.rs", "c.rs"], "deleted_paths": ["c.rs"],
                "lsp_unconsulted": ["b.rs"],
            }),
        ];
        let snaps = lsp_mutated_snapshots(&details, wd, &outcomes);
        assert_eq!(
            serde_json::Value::Array(snaps.clone()),
            serde_json::json!([
                {"path": "a.rs", "items": [{"line": 1, "severity": 1, "message": "stale"}]},
                {"path": "b.rs", "items": null, "unconsulted": true},
            ])
        );
        let evidence = render_lsp_evidence(&serde_json::json!({ "lsp_diagnostics": snaps }));
        assert!(
            evidence.contains(
                "- LSP: not consulted for 1 files commands changed (over the 8-file per-command consult cap): b.rs\n"
            ),
            "{evidence}"
        );
    }
}
