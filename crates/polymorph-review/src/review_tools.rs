//! Opt-in read-only reviewer tools.
//!
//! The default reviewer ([`crate::ReviewTurnRunner`]) is one text turn: it sees only its brief, so
//! 's VERDICT DISCIPLINE has it drop every claim about code outside the artifact — which stops
//! hallucinated callee signatures but hides real cross-file contract breaks. With `--reviewer-
//! tools`, [`ToolReviewTurnRunner`] replaces it: the same brief and review pack, plus a small,
//! bounded, read-only tool loop over the drive's workdir, ending in the same JSON verdict.
//!
//! Rules: - The tool set is the harness's own allowlist (`read`, `grep`, `glob`, plus the `lsp_*`
//! lookups when LSP servers are wired), whatever the operator's `[permissions]` allow; the
//! operator's deny list (and any pre-tool hooks) still apply on top — deny wins. Anything else the
//! model asks for is refused as a tool result. `read` URL schemes and paths outside the workdir are
//! refused too. - Nothing here mutates or is tracked as a mutation: no restore ledger, no pre-
//! mutation snapshots, no fixer `tool_outcomes`, no hashline tags. - Results are bounded per call
//! (a `read` page of at most [`REVIEW_READ_MAX_LINES`] lines / [`REVIEW_RESULT_MAX_BYTES`] bytes,
//! with the next `offset` named — demand paging, nothing lost) and the history is compacted across
//! rounds like the fixer's. - The final answer is always a tool-free turn: when the budget is spent
//! (or the tool loop ends on empty text) the harness asks for the verdict with no tools offered. -
//! Cite-or-drop has teeth: a finding whose file is neither in the reviewed text nor among the files
//! the reviewer read, grepped or looked up this turn is dropped (logged, counted in
//! [`ReviewToolReport`]). - Clean context stays: the reviewer gets the spawned brief only, never
//! fixer history; every review call is a fresh conversation.

use async_trait::async_trait;
use std::path::{Component, Path};
use std::sync::Arc;

use crate::binding::{
    compact_tool_history, extract_verdict, registry_call, to_genai_tools, AgenticTurnFn, FedResult,
    LiveAgenticTurn, LlmTarget, ToolHistoryBudget, DEFAULT_RULE_RETRIES, MAX_CALLS_PER_TURN,
};

/// File inspection tools every tool reviewer gets.
pub const REVIEWER_FILE_TOOLS: [&str; 3] = ["read", "grep", "glob"];

/// LSP lookups a tool reviewer gets when LSP servers are wired.
pub const REVIEWER_LSP_TOOLS: [&str; 3] = ["lsp_hover", "lsp_definition", "lsp_references"];

/// Default `[agent] reviewer_tool_rounds`.
pub const DEFAULT_REVIEWER_TOOL_ROUNDS: u32 = 4;

/// Most lines one reviewer `read` returns (a larger or open-ended `limit`
/// is clamped; the result names the next `offset`).
pub const REVIEW_READ_MAX_LINES: usize = 400;

/// Longest line a reviewer result carries, chars (the cut is marked).
pub const REVIEW_LINE_MAX_CHARS: usize = 1000;

/// Bound on the lines (read) or hit lines (grep) of one reviewer result,
/// bytes: past it the page ends early and says where to continue.
pub const REVIEW_RESULT_MAX_BYTES: usize = 48 * 1024;

/// Most `grep` hits one reviewer call returns.
pub const REVIEW_GREP_MAX_HITS: u64 = 50;

/// The ask that ends every tool reviewer turn that did not end on its own
/// verdict (tool budget spent, or empty text after tool calls).
const FINAL_VERDICT_PROMPT: &str = "No more tool calls: reply now with ONLY the JSON verdict object (`verdict`, `findings`, `confidence`), based on the artifact and the tool results above — no prose, no tool calls.";

/// The reviewer's tool names: the file tools, plus the LSP lookups when
/// `lsp` (servers wired), each only if the registry has it.
pub fn reviewer_tool_names(registry: &polymorph_tools::Registry, lsp: bool) -> Vec<String> {
    let lsp_tools: &[&str] = if lsp { &REVIEWER_LSP_TOOLS } else { &[] };
    REVIEWER_FILE_TOOLS
        .iter()
        .chain(lsp_tools)
        .filter(|name| registry.get(name).is_some())
        .map(|name| name.to_string())
        .collect()
}

/// The reviewer's Gate: the harness allowlist (never the operator's
/// allow list, so `write`/`edit`/`bash`/MCP stay closed whatever it says)
/// with the operator's deny list on top (deny wins).
pub fn reviewer_gate(operator: &polymorph_core::config::Permissions) -> polymorph_tools::Gate {
    let allow = REVIEWER_FILE_TOOLS
        .iter()
        .chain(REVIEWER_LSP_TOOLS.iter())
        .map(|name| name.to_string())
        .collect();
    polymorph_tools::Gate::new(allow, operator.deny.clone())
}

/// What one tool reviewer turn did: one per review call, in call order, for the board and
/// `--events-json`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReviewToolReport {
    /// Tool rounds fed back to the model.
    pub tool_rounds: u32,
    /// Tool calls handled (run, refused, denied or failed).
    pub calls: u32,
    /// Calls that did not run ok (refused, denied, failed).
    pub refused: u32,
    /// Calls the model asked for that were never handled (per-turn cap,
    /// budget spent, or asked for in the tool-free final turn).
    pub dropped_calls: u32,
    /// Workdir-relative files the reviewer read, grepped (hit files) or
    /// looked up via LSP, first-seen order.
    pub inspected: Vec<String>,
    /// One-liners of the findings dropped as unverified (cite-or-drop).
    pub dropped_findings: Vec<String>,
    /// The verdict came from a forced tool-free final turn.
    pub forced_final: bool,
    /// The first tool-bearing turn failed and the review fell back to one
    /// tool-free turn (e.g. a model without tool calling).
    pub tool_free_fallback: bool,
    /// Per-call ledger `{tool, outcome, round[, target]}`, call order.
    pub ledger: Vec<serde_json::Value>,
}

impl ReviewToolReport {
    /// The additive `reviewer_tools` object of a `verdict` event.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "tool_rounds": self.tool_rounds,
            "calls": self.calls,
            "refused": self.refused,
            "dropped_calls": self.dropped_calls,
            "inspected": self.inspected,
            "dropped_unverified": self.dropped_findings.len(),
            "dropped_findings": self.dropped_findings,
            "forced_final": self.forced_final,
            "tool_free_fallback": self.tool_free_fallback,
            "ledger": self.ledger,
        })
    }

    /// Board lines under a reviewed round (indented like its issues).
    pub fn board_lines(&self) -> Vec<String> {
        const SHOWN: usize = 5;
        let mut head = format!(
            "  reviewer tools: {} call{} in {} round{}, {} refused",
            self.calls,
            if self.calls == 1 { "" } else { "s" },
            self.tool_rounds,
            if self.tool_rounds == 1 { "" } else { "s" },
            self.refused
        );
        if self.forced_final {
            head.push_str("; final verdict forced");
        }
        if self.tool_free_fallback {
            head.push_str("; tool-free fallback");
        }
        head.push_str(&format!(
            "; {} unverified finding{} dropped",
            self.dropped_findings.len(),
            if self.dropped_findings.len() == 1 {
                ""
            } else {
                "s"
            }
        ));
        let mut lines = vec![head];
        if !self.inspected.is_empty() {
            let mut shown = self
                .inspected
                .iter()
                .take(SHOWN)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            if self.inspected.len() > SHOWN {
                shown.push_str(&format!(" (+{} more)", self.inspected.len() - SHOWN));
            }
            lines.push(format!("  reviewer inspected: {shown}"));
        }
        for dropped in &self.dropped_findings {
            lines.push(format!("  dropped unverified finding: {dropped}"));
        }
        lines
    }
}

/// Tool reviewer turn: the review pack + spawned brief, a bounded read-only tool loop, then the
/// verdict text — `data.verdict_text` exactly like [`crate::ReviewTurnRunner`], plus
/// `data.review_tools`.
pub struct ToolReviewTurnRunner {
    pub turn: Arc<dyn AgenticTurnFn>,
    pub registry: Arc<polymorph_tools::Registry>,
    /// Built by [`reviewer_gate`] (harness allowlist + operator deny).
    pub gate: polymorph_tools::Gate,
    /// Offered tool names ([`reviewer_tool_names`]); any other call is
    /// refused before the registry sees it.
    pub tools: Vec<String>,
    /// Tool rounds fed back before the forced tool-free verdict turn.
    pub max_tool_rounds: u32,
    /// In-turn tool-result history bound.
    pub history: ToolHistoryBudget,
    /// One [`ReviewToolReport`] per turn, in call order (the CLI maps them
    /// onto reviewed rounds).
    pub log: Arc<std::sync::Mutex<Vec<ReviewToolReport>>>,
}

impl ToolReviewTurnRunner {
    /// Production constructor: live turns over the review target (the
    /// managed role chain with key injection, parent-model pinned across
    /// the turn's model calls), the review pack as system, unchanged.
    #[allow(clippy::too_many_arguments)] // each maps 1:1 onto a CLI/config input
    pub fn live(
        client: polymorph_llm::LlmClient,
        target: LlmTarget,
        system: String,
        registry: Arc<polymorph_tools::Registry>,
        operator: &polymorph_core::config::Permissions,
        lsp: bool,
        max_tool_rounds: u32,
        history: ToolHistoryBudget,
    ) -> Self {
        let tools = reviewer_tool_names(&registry, lsp);
        Self {
            turn: Arc::new(LiveAgenticTurn {
                client,
                target,
                system,
                rules: Vec::new(),
                rule_max_retries: DEFAULT_RULE_RETRIES,
                pinned: Default::default(),
            }),
            registry,
            gate: reviewer_gate(operator),
            tools,
            max_tool_rounds,
            history,
            log: Default::default(),
        }
    }

    /// Reports so far, call order.
    pub fn reports(&self) -> Vec<ReviewToolReport> {
        self.log.lock().map(|l| l.clone()).unwrap_or_default()
    }

    /// The tool loop; the report fills in as it goes (kept on errors too).
    async fn review(
        &self,
        ctx: &polymorph_delegate::TurnContext,
        report: &mut ReviewToolReport,
    ) -> anyhow::Result<String> {
        use genai::chat::ChatMessage;
        let tool_ctx = polymorph_tools::ToolCallContext::new(
            ctx.workdir.clone(),
            self.gate.clone(),
            ctx.cancel.clone(),
        );
        let genai_tools = to_genai_tools(&self.registry, &self.tools);
        let brief = format!(
            "{}{}",
            ctx.brief,
            tools_note(&self.tools, self.max_tool_rounds)
        );
        let mut messages = vec![ChatMessage::user(brief)];
        let mut fed: Vec<FedResult> = Vec::new();
        let keep_files = self.registry.output_spill().keep_files;
        let mut last_text: String;
        let mut need_final = false;
        loop {
            if ctx.cancel.is_cancelled() {
                anyhow::bail!("review turn cancelled");
            }
            let turn = match self
                .turn
                .turn(messages.clone(), genai_tools.clone(), &ctx.cancel)
                .await
            {
                Ok(turn) => turn,
                // A model or gateway that rejects tool definitions fails
                // the very first call: one tool-free turn instead, so the
                // review degrades to the text-only reviewer, never to an
                // invalid round. Later failures are transport failures.
                Err(e) if report.tool_rounds == 0 && !ctx.cancel.is_cancelled() => {
                    tracing::warn!(
                        error = %format!("{e:#}"),
                        "reviewer tool turn failed, retrying once without tools"
                    );
                    report.tool_free_fallback = true;
                    let turn = self
                        .turn
                        .turn(messages.clone(), Vec::new(), &ctx.cancel)
                        .await?;
                    report.dropped_calls += turn.tool_calls.len() as u32;
                    last_text = turn.text;
                    break;
                }
                Err(e) => return Err(e),
            };
            if turn.aborted {
                anyhow::bail!("review turn cancelled");
            }
            last_text = turn.text.clone();
            if turn.tool_calls.is_empty() {
                // After tool use, anything but a parseable verdict (empty
                // text, narration) gets the tool-free verdict ask.
                need_final = report.calls > 0 && extract_verdict(&last_text).is_err();
                break;
            }
            if report.tool_rounds >= self.max_tool_rounds {
                // Budget spent: unanswered calls are dropped; a verdict
                // already given beside them stands, else ask for one.
                report.dropped_calls += turn.tool_calls.len() as u32;
                need_final = extract_verdict(&last_text).is_err();
                break;
            }
            report.tool_rounds += 1;
            let round = report.tool_rounds;
            let over = turn.tool_calls.len().saturating_sub(MAX_CALLS_PER_TURN);
            report.dropped_calls += over as u32;
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
            for tc in &batch {
                let (resp, outcome, target) = self.execute(tc, &tool_ctx, report).await;
                report.calls += 1;
                if outcome != "ok" {
                    report.refused += 1;
                }
                let mut entry = serde_json::json!({
                    "tool": tc.fn_name.as_str(),
                    "outcome": outcome,
                    "round": round,
                });
                if let Some(target) = target {
                    entry["target"] = serde_json::Value::String(target);
                }
                report.ledger.push(entry);
                fed.push(FedResult {
                    index: messages.len(),
                    round,
                    call_id: resp.call_id.clone(),
                    tool: tc.fn_name.clone(),
                    outcome,
                    exit_code: None,
                    read_path: (tc.fn_name == "read")
                        .then(|| tc.fn_arguments.get("path").and_then(|p| p.as_str()))
                        .flatten()
                        .map(str::to_string),
                    body: if self.history.budget_bytes > 0 {
                        resp.content.clone()
                    } else {
                        String::new()
                    },
                    sent: resp.content.len(),
                    compacted: false,
                });
                // Forensics ledger (the fixer's line, reviewer-labelled).
                tracing::info!(
                    tool = tc.fn_name.as_str(),
                    tool_round = round,
                    args = %tc.fn_arguments,
                    outcome = outcome,
                    "review tool call complete"
                );
                messages.push(ChatMessage::from(resp));
            }
            let compacted = compact_tool_history(
                &mut messages,
                &mut fed,
                round,
                self.history,
                &ctx.workdir,
                keep_files,
            );
            if compacted > 0 {
                tracing::info!(
                    compacted,
                    tool_round = round,
                    "review tool history compacted"
                );
            }
        }
        if need_final {
            report.forced_final = true;
            messages.push(ChatMessage::user(FINAL_VERDICT_PROMPT.to_string()));
            let turn = self
                .turn
                .turn(messages.clone(), Vec::new(), &ctx.cancel)
                .await?;
            if turn.aborted {
                anyhow::bail!("review turn cancelled");
            }
            if !turn.tool_calls.is_empty() {
                tracing::warn!(
                    calls = turn.tool_calls.len(),
                    "reviewer asked for tools in the tool-free final turn; ignored"
                );
                report.dropped_calls += turn.tool_calls.len() as u32;
            }
            last_text = turn.text;
        }
        let (text, dropped) = cite_or_drop(&last_text, &ctx.brief, &ctx.workdir, &report.inspected);
        for line in &dropped {
            tracing::warn!(finding = %line, "unverified reviewer finding dropped (cite-or-drop)");
        }
        report.dropped_findings = dropped;
        Ok(text)
    }

    /// One call: allowlist, workdir scope, per-call bounds, then the
    /// registry (pre-hooks + Gate). Returns the fed response, its outcome
    /// label and the call's target (path or pattern) for the ledger.
    async fn execute(
        &self,
        tc: &genai::chat::ToolCall,
        tool_ctx: &polymorph_tools::ToolCallContext,
        report: &mut ReviewToolReport,
    ) -> (genai::chat::ToolResponse, &'static str, Option<String>) {
        use genai::chat::ToolResponse;
        let name = tc.fn_name.as_str();
        let arg = |key: &str| {
            tc.fn_arguments
                .get(key)
                .and_then(|v| v.as_str())
                .map(str::to_string)
        };
        if !self.tools.iter().any(|t| t == name) {
            let text = format!(
                "tool '{name}' refused: the reviewer's tools are read-only inspection (available: {})",
                self.tools.join(", ")
            );
            return (
                ToolResponse::from_tool_call(tc, text),
                "refused-readonly",
                None,
            );
        }
        let is_lsp = REVIEWER_LSP_TOOLS.contains(&name);
        let (scoped, target) = match name {
            "read" => (arg("path"), arg("path")),
            "grep" | "glob" => (arg("dir"), arg("pattern")),
            _ if is_lsp => (arg("path"), arg("path")),
            _ => (None, None),
        };
        let mut rel_scoped: Option<String> = None;
        if let Some(raw) = &scoped {
            match confine(&tool_ctx.workdir, raw) {
                Ok(rel) => rel_scoped = Some(rel),
                Err(why) => {
                    let text = format!("tool '{name}' refused: {why}");
                    return (
                        ToolResponse::from_tool_call(tc, text),
                        "refused-scope",
                        target,
                    );
                }
            }
        }
        // Per-call bounds ride the arguments (a non-object argument is the
        // registry's to reject).
        let mut call = tc.clone();
        let mut clamped = false;
        if let Some(args) = call.fn_arguments.as_object_mut() {
            if name == "read" {
                let limit = args.get("limit").and_then(|v| v.as_u64());
                if limit.is_none_or(|l| l == 0 || l > REVIEW_READ_MAX_LINES as u64) {
                    args.insert("limit".into(), REVIEW_READ_MAX_LINES.into());
                    clamped = true;
                }
            } else if name == "grep" {
                let max = args.get("max_hits").and_then(|v| v.as_u64());
                if max.is_none_or(|m| m > REVIEW_GREP_MAX_HITS) {
                    args.insert("max_hits".into(), REVIEW_GREP_MAX_HITS.into());
                }
            }
        }
        match registry_call(&self.registry, &call, tool_ctx).await {
            Ok(mut value) => {
                bound_result(name, &mut value, clamped);
                let mut seen = |rel: String| {
                    if !rel.is_empty() && !report.inspected.contains(&rel) {
                        report.inspected.push(rel);
                    }
                };
                // An LSP note without a result (no server, no route)
                // showed nothing, so it verifies nothing.
                let shown = !is_lsp || value.get("result").is_some_and(|r| !r.is_null());
                if name == "read" || is_lsp {
                    if let (Some(rel), true) = (rel_scoped, shown) {
                        seen(rel);
                    }
                } else if name == "grep" {
                    let hits = value.get("hits").and_then(|h| h.as_array());
                    for path in hits
                        .into_iter()
                        .flatten()
                        .filter_map(|h| h.get("path").and_then(|p| p.as_str()))
                    {
                        if let Some(rel) = workdir_relative(&tool_ctx.workdir, Path::new(path)) {
                            seen(rel);
                        }
                    }
                }
                let text =
                    serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
                (ToolResponse::from_tool_call(tc, text), "ok", target)
            }
            Err((text, outcome)) => (ToolResponse::from_tool_call(tc, text), outcome, target),
        }
    }
}

#[async_trait]
impl polymorph_delegate::TurnRunner for ToolReviewTurnRunner {
    async fn run_turn(
        &self,
        ctx: polymorph_delegate::TurnContext,
    ) -> anyhow::Result<polymorph_delegate::TurnOutput> {
        let mut report = ReviewToolReport::default();
        let reviewed = self.review(&ctx, &mut report).await;
        // One report per call, failed turns included, so the CLI's
        // round mapping stays aligned with the reviewer calls.
        if let Ok(mut log) = self.log.lock() {
            log.push(report.clone());
        }
        let text = reviewed?;
        Ok(polymorph_delegate::TurnOutput {
            text: text.clone(),
            summary: format!(
                "review turn complete ({} tool calls in {} rounds)",
                report.calls, report.tool_rounds
            ),
            tool_calls: report.calls,
            done: true,
            data: Some(serde_json::json!({
                "verdict_text": text,
                "review_tools": report.to_json(),
            })),
        })
    }
}

/// The brief addition for a tool reviewer: what it may use, the budget,
/// and VERDICT DISCIPLINE's tools-on rule (appended after the unchanged
/// brief, so it reads as the later, overriding instruction).
fn tools_note(tools: &[String], rounds: u32) -> String {
    format!(
        "\n\nREVIEWER TOOLS:\nYou may inspect the drive's workdir READ-ONLY with: {}. At most {rounds} tool round{}; then you must answer. Use them to verify cross-file facts — callee and caller signatures, type definitions, invariants — before asserting them. VERDICT DISCIPLINE with tools: a finding on code outside ARTIFACT UNDER REVIEW is allowed ONLY if you inspected that code with a tool call in this review — set its file and line to the inspected path:line and quote the inspected line in the issue. The harness drops every finding whose file is neither in ARTIFACT UNDER REVIEW nor among the files you read, grepped or looked up in this review. Your final reply is the JSON verdict only, with no tool calls.",
        tools.join(", "),
        if rounds == 1 { "" } else { "s" }
    )
}

/// Bound one ok result in place: long lines cut (marked), the page ended
/// early past [`REVIEW_RESULT_MAX_BYTES`], and a `note` naming how to
/// continue whenever the reviewer's page stops before the file does.
fn bound_result(name: &str, value: &mut serde_json::Value, clamped: bool) {
    let key = match name {
        "read" => "lines",
        "grep" => "hits",
        _ => return,
    };
    let Some(items) = value.get_mut(key).and_then(|v| v.as_array_mut()) else {
        return;
    };
    let mut bytes = 0usize;
    let mut keep = items.len();
    for (i, item) in items.iter_mut().enumerate() {
        let line = if name == "read" {
            Some(item)
        } else {
            item.get_mut("line")
        };
        if let Some(line) = line {
            if let Some(text) = line.as_str() {
                let chars = text.chars().count();
                if chars > REVIEW_LINE_MAX_CHARS {
                    let cut: String = text.chars().take(REVIEW_LINE_MAX_CHARS).collect();
                    *line = serde_json::Value::String(format!(
                        "{cut}... [line cut: {chars} chars in full; grep the file for the rest]"
                    ));
                }
                bytes += line.as_str().map_or(0, str::len);
            }
        }
        // At least one row always stays (a page never comes back empty).
        if bytes > REVIEW_RESULT_MAX_BYTES && i > 0 {
            keep = i;
            break;
        }
    }
    let shortened = keep < items.len();
    items.truncate(keep);
    let shown = items.len();
    match name {
        "read" => {
            if shortened {
                value["truncated_after"] = serde_json::Value::Bool(true);
            }
            let more = value.get("truncated_after").and_then(|v| v.as_bool()) == Some(true);
            if (clamped || shortened) && more {
                let start = value
                    .get("start_line")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(1);
                value["note"] = serde_json::Value::String(format!(
                    "reviewer read pages are at most {REVIEW_READ_MAX_LINES} lines / {} KiB: continue with offset={}",
                    REVIEW_RESULT_MAX_BYTES / 1024,
                    start + shown as u64
                ));
            }
        }
        _ => {
            if shortened {
                value["capped"] = serde_json::Value::Bool(true);
                value["note"] = serde_json::Value::String(format!(
                    "reviewer grep results are at most {} KiB: narrow the pattern or `dir`",
                    REVIEW_RESULT_MAX_BYTES / 1024
                ));
            }
        }
    }
}

/// Lexical components of a path: `.` dropped, `..` popped, drive
/// prefixes spelled one way (`C:`; verbatim `\\?\C:` too). No filesystem
/// access (a missing file still resolves).
fn lexical(path: &Path) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if out.last().is_some_and(|l| l != "/" && !l.ends_with(':')) {
                    out.pop();
                }
            }
            Component::Prefix(p) => out.push(match p.kind() {
                std::path::Prefix::Disk(d) | std::path::Prefix::VerbatimDisk(d) => {
                    format!("{}:", (d as char).to_ascii_uppercase())
                }
                _ => p.as_os_str().to_string_lossy().into_owned(),
            }),
            Component::RootDir => out.push("/".to_string()),
            Component::Normal(s) => out.push(s.to_string_lossy().into_owned()),
        }
    }
    out
}

/// [`lexical`] components as one `/`-separated spelling.
fn joined(parts: &[String]) -> String {
    let mut out = String::new();
    for part in parts {
        if part != "/" && !out.is_empty() && !out.ends_with('/') {
            out.push('/');
        }
        if part != "/" || !out.ends_with('/') {
            out.push_str(part);
        }
    }
    out
}

/// `path` relative to `workdir` (`/`-separated, the path's own case;
/// `""` for the workdir itself), or `None` when it lies outside
/// (lexically; case-insensitive on Windows).
fn workdir_relative(workdir: &Path, path: &Path) -> Option<String> {
    let base = lexical(workdir);
    let full = lexical(path);
    let same = |a: &String, b: &String| {
        if cfg!(windows) {
            a.to_lowercase() == b.to_lowercase()
        } else {
            a == b
        }
    };
    if full.len() < base.len() || !base.iter().zip(&full).all(|(a, b)| same(a, b)) {
        return None;
    }
    Some(full[base.len()..].join("/"))
}

/// Scope check of one path argument: a file path inside the workdir
/// (relative, or absolute under it). URL schemes (`web://`, `gh://`, …)
/// and paths outside the workdir are refused. Ok = the relative spelling.
fn confine(workdir: &Path, raw: &str) -> Result<String, String> {
    if raw.contains("://") {
        return Err(format!(
            "{raw:?} is a URL; the reviewer reads only files in the workdir"
        ));
    }
    let resolved = polymorph_tools::resolve_tool_path(workdir, raw);
    let workdir = std::path::absolute(workdir).unwrap_or_else(|_| workdir.to_path_buf());
    let resolved = std::path::absolute(&resolved).unwrap_or(resolved);
    workdir_relative(&workdir, &resolved).ok_or_else(|| {
        format!("{raw:?} is outside the workdir; the reviewer reads only the workdir")
    })
}

/// One finding as a board/log line (the loop's own one-liner shape).
fn finding_line(f: &polymorph_core::Finding) -> String {
    crate::r#loop::finding_issues(std::slice::from_ref(f))
        .pop()
        .unwrap_or_default()
}

/// A path-ish spelling compared case-insensitively with `/` separators.
fn norm(s: &str) -> String {
    s.replace('\\', "/").to_lowercase()
}

/// The path candidates a finding's `file` field names: split on spaces
/// and punctuation, `./` and `:line[:col]` stripped; path-looking tokens
/// (with `.` or `/`) when there are any, else every token.
fn file_tokens(file: &str) -> Vec<String> {
    let file = norm(file);
    let tokens: Vec<String> = file
        .split(|c: char| c.is_whitespace() || ",;()[]`\"'".contains(c))
        .map(|t| {
            let mut t = t.trim_start_matches("./");
            // `src/a.rs:12` / `src/a.rs:12:4` → `src/a.rs`.
            while let Some((head, tail)) = t.rsplit_once(':') {
                if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) {
                    t = head;
                } else {
                    break;
                }
            }
            t.trim_end_matches(['.', ':']).to_string()
        })
        .filter(|t| !t.is_empty())
        .collect();
    let pathy: Vec<String> = tokens
        .iter()
        .filter(|t| t.contains('.') || t.contains('/'))
        .cloned()
        .collect();
    if pathy.is_empty() {
        tokens
    } else {
        pathy
    }
}

/// `needle` occurs in `hay` as a whole path token: not glued to a longer
/// name on either side (a `/` before it is fine — a basename cite).
fn contains_path_token(hay: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let glued = |c: char| c.is_alphanumeric() || c == '_' || c == '-';
    let mut from = 0;
    while let Some(pos) = hay[from..].find(needle) {
        let start = from + pos;
        let end = start + needle.len();
        let before_ok = hay[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !glued(c) && c != '.');
        let after_ok = hay[end..].chars().next().is_none_or(|c| !glued(c));
        if before_ok && after_ok {
            return true;
        }
        from = start + needle.len().max(1);
        while !hay.is_char_boundary(from) {
            from += 1;
        }
    }
    false
}

/// The reviewed text a finding may cite without a tool call: the brief
/// from `ARTIFACT UNDER REVIEW:` on (artifact, path, attachments, notes —
/// not GOAL/CRITERIA, which name files without showing them).
fn reviewed_scope(brief: &str) -> String {
    let at = brief.find("ARTIFACT UNDER REVIEW:").unwrap_or(0);
    norm(&brief[at..])
}

/// Whether a finding's file is in evidence: in the reviewed text, or one
/// of the files the reviewer inspected this turn (relative or absolute
/// spelling, or a basename/suffix of one).
fn finding_in_scope(file: &str, scope: &str, workdir: &str, inspected: &[String]) -> bool {
    file_tokens(file).iter().any(|token| {
        let rel = token
            .strip_prefix(workdir)
            .and_then(|r| r.strip_prefix('/'))
            .unwrap_or(token);
        let seen = inspected.iter().map(|p| norm(p)).any(|p| {
            p == rel || p.ends_with(&format!("/{rel}")) || rel.ends_with(&format!("/{p}"))
        });
        seen || contains_path_token(scope, rel) || contains_path_token(scope, token)
    })
}

/// Cite-or-drop with teeth: drop every finding whose file is neither in the reviewed text nor among
/// the inspected files. Returns the verdict text to judge — the RAW text, untouched, unless
/// something was dropped (then the filtered verdict re-serialized; a fail left with no findings
/// becomes a pass: what remains is hypotheses only) — and the dropped findings' one-liners.
/// Unparseable text passes through for the loop to judge invalid as usual.
pub(crate) fn cite_or_drop(
    text: &str,
    brief: &str,
    workdir: &Path,
    inspected: &[String],
) -> (String, Vec<String>) {
    let Ok(verdict) = extract_verdict(text) else {
        return (text.to_string(), Vec::new());
    };
    let scope = reviewed_scope(brief);
    let workdir = norm(&joined(&lexical(workdir)));
    let (kept, dropped): (Vec<_>, Vec<_>) = verdict
        .findings
        .into_iter()
        .partition(|f| finding_in_scope(&f.file, &scope, &workdir, inspected));
    if dropped.is_empty() {
        return (text.to_string(), Vec::new());
    }
    let kind = if kept.is_empty() {
        polymorph_core::VerdictKind::Pass
    } else {
        verdict.verdict
    };
    let filtered = polymorph_core::Verdict {
        verdict: kind,
        findings: kept,
        confidence: verdict.confidence,
    };
    let text = serde_json::to_string(&filtered).unwrap_or_else(|_| text.to_string());
    (text, dropped.iter().map(finding_line).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use polymorph_delegate::{TurnContext, TurnRunner};
    use std::collections::VecDeque;

    /// Scripted reviewer model: pops one collected turn per call, records
    /// each history and the tool names it was offered; `fail_first` makes
    /// the first call error (a model that rejects tool definitions).
    struct ScriptTurn {
        script: std::sync::Mutex<VecDeque<polymorph_llm::CollectedTurn>>,
        seen: std::sync::Mutex<Vec<(Vec<genai::chat::ChatMessage>, Vec<String>)>>,
        fail_first: bool,
    }

    impl ScriptTurn {
        fn new(turns: Vec<polymorph_llm::CollectedTurn>) -> Arc<Self> {
            Arc::new(Self {
                script: std::sync::Mutex::new(turns.into_iter().collect()),
                seen: std::sync::Mutex::new(Vec::new()),
                fail_first: false,
            })
        }

        fn offered(&self) -> Vec<Vec<String>> {
            self.seen
                .lock()
                .unwrap()
                .iter()
                .map(|(_, t)| t.clone())
                .collect()
        }
    }

    #[async_trait]
    impl AgenticTurnFn for ScriptTurn {
        async fn turn(
            &self,
            messages: Vec<genai::chat::ChatMessage>,
            tools: Vec<genai::chat::Tool>,
            _cancel: &tokio_util::sync::CancellationToken,
        ) -> anyhow::Result<polymorph_llm::CollectedTurn> {
            let names = tools.iter().map(|t| t.name.as_str().to_string()).collect();
            let first = {
                let mut seen = self.seen.lock().unwrap();
                seen.push((messages, names));
                seen.len() == 1
            };
            if self.fail_first && first {
                anyhow::bail!("400 tools not supported by this model");
            }
            Ok(self
                .script
                .lock()
                .unwrap()
                .pop_front()
                .expect("script exhausted"))
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

    fn call(id: &str, name: &str, args: serde_json::Value) -> genai::chat::ToolCall {
        genai::chat::ToolCall {
            call_id: id.to_string(),
            fn_name: name.to_string(),
            fn_arguments: args,
            thought_signatures: None,
        }
    }

    fn runner(turn: Arc<ScriptTurn>, deny: Vec<String>, rounds: u32) -> ToolReviewTurnRunner {
        let registry = Arc::new(polymorph_tools::registry::v1_registry());
        let operator = polymorph_core::config::Permissions {
            allow: Vec::new(),
            deny,
        };
        ToolReviewTurnRunner {
            turn,
            tools: reviewer_tool_names(&registry, false),
            registry,
            gate: reviewer_gate(&operator),
            max_tool_rounds: rounds,
            history: ToolHistoryBudget::default(),
            log: Default::default(),
        }
    }

    fn ctx(dir: &Path, brief: &str) -> TurnContext {
        TurnContext {
            child_id: "c-review".to_string(),
            role: polymorph_core::Role::Review,
            brief: brief.to_string(),
            workdir: dir.to_path_buf(),
            turn: 1,
            steer: Vec::new(),
            cancel: tokio_util::sync::CancellationToken::new(),
        }
    }

    /// Tool-response contents fed back in one history (feed order).
    fn fed(history: &[genai::chat::ChatMessage]) -> Vec<String> {
        history
            .iter()
            .flat_map(|m| m.content.parts())
            .filter_map(|p| match p {
                genai::chat::ContentPart::ToolResponse(r) => Some(r.content.clone()),
                _ => None,
            })
            .collect()
    }

    fn verdict_of(out: &polymorph_delegate::TurnOutput) -> polymorph_core::Verdict {
        crate::parse_verdict(out.data.as_ref().unwrap()["verdict_text"].as_str().unwrap()).unwrap()
    }

    const PASS: &str = r#"{"verdict":"pass","findings":[],"confidence":0.9}"#;

    fn fail_on(file: &str, line: u32, issue: &str) -> String {
        serde_json::json!({
            "verdict": "fail",
            "findings": [{"severity": "P1", "file": file, "line": line,
                "issue": issue, "proposed_fix": "fix it"}],
            "confidence": 0.8
        })
        .to_string()
    }

    const BRIEF: &str =
        "MODE:\nact\n\nGOAL:\ncall src/callee.rs right\n\nACCEPTANCE CRITERIA:\n\n\nARTIFACT UNDER REVIEW:\nfn main() { add(1, 2); }\n\nARTIFACT PATH:\nsrc/main.rs";

    #[test]
    fn tool_names_and_gate_are_the_harness_allowlist() {
        let registry = polymorph_tools::registry::v1_registry();
        assert_eq!(
            reviewer_tool_names(&registry, false),
            ["read", "grep", "glob"]
        );
        assert_eq!(
            reviewer_tool_names(&registry, true),
            [
                "read",
                "grep",
                "glob",
                "lsp_hover",
                "lsp_definition",
                "lsp_references"
            ]
        );
        // The operator allowing everything (empty allow) or naming write/
        // bash never opens them: the allowlist is the harness's own.
        let operator = polymorph_core::config::Permissions {
            allow: vec!["write".into(), "bash".into(), "edit".into()],
            deny: vec!["read:secret.txt".into()],
        };
        let gate = reviewer_gate(&operator);
        assert!(gate.check("read:src/a.rs").is_ok());
        assert!(gate.check("lsp_hover:src/a.rs").is_ok());
        for closed in [
            "write:a",
            "edit",
            "bash:ls",
            "mcp__probe__t",
            "web_search:q",
        ] {
            assert!(gate.check(closed).is_err(), "{closed}");
        }
        // Operator deny wins.
        assert!(gate.check("read:secret.txt").is_err());
    }

    #[tokio::test]
    async fn reviewer_reads_and_greps_then_returns_verdict() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/callee.rs"),
            "pub fn add(a: i32) -> i32 { a }\n",
        )
        .unwrap();
        let turn = ScriptTurn::new(vec![
            collected(
                "",
                vec![
                    call("r1", "read", serde_json::json!({"path": "src/callee.rs"})),
                    call("g1", "grep", serde_json::json!({"pattern": "fn add"})),
                ],
            ),
            collected(PASS, vec![]),
        ]);
        let r = runner(turn.clone(), vec![], 4);
        let out = r.run_turn(ctx(dir.path(), BRIEF)).await.unwrap();
        assert!(out.done);
        assert_eq!(out.tool_calls, 2);
        assert_eq!(out.data.as_ref().unwrap()["verdict_text"], PASS);
        let seen = turn.seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        // The brief is the spawned brief verbatim, plus the tools note.
        let brief_msg = seen[0].0[0].content.first_text().unwrap().to_string();
        assert!(brief_msg.starts_with(BRIEF), "{brief_msg}");
        assert!(brief_msg.contains("REVIEWER TOOLS:"), "{brief_msg}");
        assert!(brief_msg.contains("read, grep, glob"), "{brief_msg}");
        assert_eq!(seen[0].1, ["read", "grep", "glob"]);
        let results = fed(&seen[1].0);
        assert_eq!(results.len(), 2);
        assert!(results[0].contains("pub fn add(a: i32)"), "{results:?}");
        // No hashline tag: the reviewer never edits.
        assert!(!results[0].contains("\"tag\""), "{results:?}");
        assert!(results[1].contains("callee.rs"), "{results:?}");
        let reports = r.reports();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].calls, 2);
        assert_eq!(reports[0].tool_rounds, 1);
        assert_eq!(reports[0].inspected, ["src/callee.rs"]);
        assert!(!reports[0].forced_final);
        assert_eq!(out.data.as_ref().unwrap()["review_tools"]["calls"], 2);
    }

    #[tokio::test]
    async fn mutating_tools_refused_even_when_operator_allows_them() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}\n").unwrap();
        let turn = ScriptTurn::new(vec![
            collected(
                "",
                vec![
                    call(
                        "w",
                        "write",
                        serde_json::json!({"path": "a.rs", "content": "x"}),
                    ),
                    call(
                        "e",
                        "edit",
                        serde_json::json!({"patch": "[a.rs#0000]\nPUT 1:\n+x\n"}),
                    ),
                    call("b", "bash", serde_json::json!({"cmd": "echo hi > b.txt"})),
                    call("m", "mcp__probe__t", serde_json::json!({})),
                    call(
                        "u",
                        "read",
                        serde_json::json!({"path": "web://example.com"}),
                    ),
                    call("o", "read", serde_json::json!({"path": "../outside.txt"})),
                ],
            ),
            collected(PASS, vec![]),
        ]);
        // Operator config allows everything (empty allow) — irrelevant.
        let mut r = runner(turn.clone(), vec![], 4);
        r.gate = reviewer_gate(&polymorph_core::config::Permissions {
            allow: Vec::new(),
            deny: Vec::new(),
        });
        let out = r.run_turn(ctx(dir.path(), BRIEF)).await.unwrap();
        assert_eq!(verdict_of(&out).verdict, polymorph_core::VerdictKind::Pass);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.rs")).unwrap(),
            "fn a() {}\n"
        );
        assert!(!dir.path().join("b.txt").exists());
        let report = &r.reports()[0];
        assert_eq!(report.calls, 6);
        assert_eq!(report.refused, 6);
        let outcomes: Vec<&str> = report
            .ledger
            .iter()
            .map(|e| e["outcome"].as_str().unwrap())
            .collect();
        assert_eq!(
            outcomes,
            [
                "refused-readonly",
                "refused-readonly",
                "refused-readonly",
                "refused-readonly",
                "refused-scope",
                "refused-scope"
            ]
        );
        let results = fed(&turn.seen.lock().unwrap()[1].0);
        assert!(results[0].contains("read-only"), "{results:?}");
        assert!(results[4].contains("URL"), "{results:?}");
        assert!(results[5].contains("outside the workdir"), "{results:?}");
        assert!(report.inspected.is_empty());
    }

    #[tokio::test]
    async fn operator_deny_still_applies() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("secret.txt"), "k=v\n").unwrap();
        let turn = ScriptTurn::new(vec![
            collected(
                "",
                vec![call("r", "read", serde_json::json!({"path": "secret.txt"}))],
            ),
            collected(PASS, vec![]),
        ]);
        let r = runner(turn.clone(), vec!["read:secret.txt".into()], 4);
        r.run_turn(ctx(dir.path(), BRIEF)).await.unwrap();
        let report = &r.reports()[0];
        assert_eq!(report.ledger[0]["outcome"], "denied");
        assert!(report.inspected.is_empty());
        let results = fed(&turn.seen.lock().unwrap()[1].0);
        assert!(results[0].contains("permission denied"), "{results:?}");
        assert!(!results[0].contains("k=v"), "{results:?}");
    }

    #[tokio::test]
    async fn spent_budget_forces_a_tool_free_final_verdict() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}\n").unwrap();
        let read = |id: &str| {
            collected(
                "let me look again",
                vec![call(id, "read", serde_json::json!({"path": "a.rs"}))],
            )
        };
        let turn = ScriptTurn::new(vec![
            read("r1"),
            read("r2"),
            read("r3"),
            // The forced final turn: wrapped JSON, extracted as usual.
            collected(&format!("Verdict:\n```json\n{PASS}\n```"), vec![]),
        ]);
        let r = runner(turn.clone(), vec![], 2);
        let out = r.run_turn(ctx(dir.path(), BRIEF)).await.unwrap();
        // Raw text kept (nothing dropped); the loop's extraction parses it.
        let text = out.data.as_ref().unwrap()["verdict_text"].as_str().unwrap();
        assert_eq!(
            extract_verdict(text).unwrap().verdict,
            polymorph_core::VerdictKind::Pass
        );
        let report = &r.reports()[0];
        assert_eq!(report.tool_rounds, 2);
        assert_eq!(report.calls, 2);
        assert_eq!(report.dropped_calls, 1);
        assert!(report.forced_final);
        let offered = turn.offered();
        assert_eq!(offered.len(), 4);
        assert!(offered[..3].iter().all(|t| !t.is_empty()));
        assert!(offered[3].is_empty(), "final turn must be tool-free");
        let seen = turn.seen.lock().unwrap();
        let last = seen[3].0.last().unwrap().content.first_text().unwrap();
        assert!(last.contains("ONLY the JSON verdict"), "{last}");
    }

    #[tokio::test]
    async fn empty_text_after_tools_forces_final_verdict_and_tool_free_fallback() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(dir.path().join("b.rs"), "fn b() {}\n").unwrap();
        for closing in ["  ", "Looks fine overall."] {
            let turn = ScriptTurn::new(vec![
                collected(
                    "",
                    vec![
                        call("r", "read", serde_json::json!({"path": "a.rs"})),
                        call(
                            "h",
                            "lsp_hover",
                            serde_json::json!({"path": "b.rs", "line": 0, "character": 3}),
                        ),
                    ],
                ),
                collected(closing, vec![]),
                collected(PASS, vec![]),
            ]);
            let mut r = runner(turn.clone(), vec![], 4);
            r.tools = reviewer_tool_names(&r.registry, true);
            let out = r.run_turn(ctx(dir.path(), BRIEF)).await.unwrap();
            assert_eq!(out.data.as_ref().unwrap()["verdict_text"], PASS);
            let report = &r.reports()[0];
            assert!(report.forced_final, "{closing:?}");
            // The serverless hover answered a note, not b.rs: not inspected.
            assert_eq!(report.ledger[1]["outcome"], "ok");
            assert_eq!(report.inspected, ["a.rs"]);
            assert!(turn.offered()[2].is_empty());
        }
        // A model that rejects tool definitions: one tool-free retry.
        let turn = Arc::new(ScriptTurn {
            script: std::sync::Mutex::new(vec![collected(PASS, vec![])].into()),
            seen: std::sync::Mutex::new(Vec::new()),
            fail_first: true,
        });
        let r = runner(turn.clone(), vec![], 4);
        let out = r.run_turn(ctx(dir.path(), BRIEF)).await.unwrap();
        assert_eq!(out.data.as_ref().unwrap()["verdict_text"], PASS);
        assert!(r.reports()[0].tool_free_fallback);
        let offered = turn.offered();
        assert_eq!(offered.len(), 2);
        assert!(!offered[0].is_empty() && offered[1].is_empty());
    }

    #[tokio::test]
    async fn cancelled_review_fails_without_spending() {
        let dir = tempfile::tempdir().unwrap();
        let turn = ScriptTurn::new(vec![]);
        let r = runner(turn.clone(), vec![], 4);
        let c = ctx(dir.path(), BRIEF);
        c.cancel.cancel();
        let err = r.run_turn(c).await.err().expect("cancelled");
        assert!(err.to_string().contains("cancelled"), "{err}");
        assert!(turn.offered().is_empty());
        // The failed call still logs its report (round alignment).
        assert_eq!(r.reports().len(), 1);
    }

    #[tokio::test]
    async fn unverified_out_of_scope_finding_dropped_verified_kept() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/callee.rs"),
            "pub fn add(a: i32) -> i32 { a }\n",
        )
        .unwrap();
        // Unverified: cites the callee without reading it.
        let turn = ScriptTurn::new(vec![collected(
            &fail_on("src/callee.rs", 1, "add needs a VkDevice"),
            vec![],
        )]);
        let r = runner(turn, vec![], 4);
        let out = r.run_turn(ctx(dir.path(), BRIEF)).await.unwrap();
        let v = verdict_of(&out);
        assert_eq!(v.verdict, polymorph_core::VerdictKind::Pass);
        assert!(v.findings.is_empty());
        let report = &r.reports()[0];
        assert_eq!(report.dropped_findings.len(), 1);
        assert!(
            report.dropped_findings[0].contains("src/callee.rs:1"),
            "{report:?}"
        );
        assert_eq!(
            out.data.as_ref().unwrap()["review_tools"]["dropped_unverified"],
            1
        );
        // Verified: the same finding after reading the callee stays, and
        // an in-artifact finding stays without any tool call.
        let both = serde_json::json!({
            "verdict": "fail",
            "findings": [
                {"severity": "P1", "file": "src/callee.rs", "line": 1,
                 "issue": "add takes one arg", "proposed_fix": "call add(1)"},
                {"severity": "P2", "file": "src/main.rs:1", "line": 1,
                 "issue": "two args passed", "proposed_fix": "pass one"},
                {"severity": "P2", "file": "src/other.rs", "line": 3,
                 "issue": "guessed", "proposed_fix": "x"}
            ],
            "confidence": 0.8
        })
        .to_string();
        let turn = ScriptTurn::new(vec![
            collected(
                "",
                vec![call(
                    "r",
                    "read",
                    serde_json::json!({"path": "./src/callee.rs"}),
                )],
            ),
            collected(&both, vec![]),
        ]);
        let r = runner(turn, vec![], 4);
        let out = r.run_turn(ctx(dir.path(), BRIEF)).await.unwrap();
        let v = verdict_of(&out);
        assert_eq!(v.verdict, polymorph_core::VerdictKind::Fail);
        let files: Vec<&str> = v.findings.iter().map(|f| f.file.as_str()).collect();
        assert_eq!(files, ["src/callee.rs", "src/main.rs:1"]);
        assert_eq!(r.reports()[0].dropped_findings.len(), 1);
        assert!(r.reports()[0].dropped_findings[0].contains("src/other.rs"));
    }

    #[test]
    fn cite_or_drop_scope_rules() {
        let wd = Path::new("/work/repo");
        let (text, dropped) = cite_or_drop(PASS, BRIEF, wd, &[]);
        assert_eq!(text, PASS);
        assert!(dropped.is_empty());
        // Unparseable text passes through untouched for the loop to judge.
        let (text, dropped) = cite_or_drop("not json", BRIEF, wd, &[]);
        assert_eq!(text, "not json");
        assert!(dropped.is_empty());
        let keeps = |file: &str, inspected: &[&str]| {
            let inspected: Vec<String> = inspected.iter().map(|s| s.to_string()).collect();
            let (_, dropped) = cite_or_drop(&fail_on(file, 1, "x"), BRIEF, wd, &inspected);
            dropped.is_empty()
        };
        // In the artifact path, any spelling of it.
        assert!(keeps("src/main.rs", &[]));
        assert!(keeps("main.rs", &[]));
        assert!(keeps("./src/main.rs:1:4", &[]));
        assert!(keeps("src\\main.rs", &[]));
        assert!(keeps("src/main.rs (fn main)", &[]));
        // Named only in GOAL (content never shown): not evidence.
        assert!(!keeps("src/callee.rs", &[]));
        // Inspected: relative, absolute, or suffix spellings.
        assert!(keeps("src/callee.rs", &["src/callee.rs"]));
        assert!(keeps("/work/repo/src/callee.rs", &["src/callee.rs"]));
        assert!(keeps("callee.rs", &["src/callee.rs"]));
        // A longer name sharing a suffix is a different file.
        assert!(!keeps("xmain.rs", &[]));
        assert!(!keeps("src/main.rsx", &[]));
    }

    #[tokio::test]
    async fn large_read_is_paged_and_old_results_compacted() {
        let dir = tempfile::tempdir().unwrap();
        let big: String = (1..=5000)
            .map(|i| format!("line {i} of a large file, padded to a realistic width\n"))
            .collect();
        std::fs::write(dir.path().join("big.rs"), &big).unwrap();
        let long_line = "x".repeat(5000);
        std::fs::write(dir.path().join("min.js"), format!("{long_line}\n")).unwrap();
        let turn = ScriptTurn::new(vec![
            collected(
                "",
                vec![
                    call("r1", "read", serde_json::json!({"path": "big.rs"})),
                    call("m1", "read", serde_json::json!({"path": "min.js"})),
                ],
            ),
            collected(
                "",
                vec![call(
                    "r2",
                    "read",
                    serde_json::json!({"path": "big.rs", "offset": 401}),
                )],
            ),
            collected(
                "",
                vec![call(
                    "r3",
                    "read",
                    serde_json::json!({"path": "big.rs", "offset": 801}),
                )],
            ),
            collected(PASS, vec![]),
        ]);
        let mut r = runner(turn.clone(), vec![], 4);
        r.history = ToolHistoryBudget {
            budget_bytes: 16 * 1024,
            keep_rounds: 1,
        };
        r.run_turn(ctx(dir.path(), BRIEF)).await.unwrap();
        let seen = turn.seen.lock().unwrap();
        // Round 1 as fed: a 400-line page naming the next offset, and the
        // megabyte-style line cut with its length named.
        let first = fed(&seen[1].0);
        let page: serde_json::Value = serde_json::from_str(&first[0]).unwrap();
        assert_eq!(
            page["lines"].as_array().unwrap().len(),
            REVIEW_READ_MAX_LINES
        );
        assert_eq!(page["truncated_after"], true);
        assert!(
            page["note"].as_str().unwrap().contains("offset=401"),
            "{page}"
        );
        assert!(first[1].contains("line cut: 5000 chars"), "{}", first[1]);
        assert!(first[1].len() < 2000, "{}", first[1].len());
        // By the last call, the oldest rounds are stubs pointing at the file.
        let last = fed(&seen[3].0);
        assert!(last[0].contains("compacted"), "{}", last[0]);
        assert!(last[0].contains("big.rs"), "{}", last[0]);
        assert!(last[3].contains("line 801 of"), "latest round intact");
    }

    #[test]
    fn report_board_lines_and_json_shape() {
        let report = ReviewToolReport {
            tool_rounds: 2,
            calls: 3,
            refused: 1,
            dropped_calls: 0,
            inspected: (1..=7).map(|i| format!("f{i}.rs")).collect(),
            dropped_findings: vec!["P1 g.rs:1 guessed".into()],
            forced_final: true,
            tool_free_fallback: false,
            ledger: Vec::new(),
        };
        assert_eq!(
            report.board_lines(),
            [
                "  reviewer tools: 3 calls in 2 rounds, 1 refused; final verdict forced; 1 unverified finding dropped",
                "  reviewer inspected: f1.rs, f2.rs, f3.rs, f4.rs, f5.rs (+2 more)",
                "  dropped unverified finding: P1 g.rs:1 guessed",
            ]
        );
        let json = report.to_json();
        assert_eq!(json["dropped_unverified"], 1);
        assert_eq!(json["inspected"].as_array().unwrap().len(), 7);
        assert_eq!(json["forced_final"], true);
    }

    #[test]
    fn confine_keeps_reads_in_the_workdir() {
        let dir = tempfile::tempdir().unwrap();
        let wd = dir.path();
        assert_eq!(confine(wd, "src/a.rs").unwrap(), "src/a.rs");
        assert_eq!(confine(wd, "./src/../src/a.rs").unwrap(), "src/a.rs");
        assert_eq!(confine(wd, ".").unwrap(), "");
        let abs = wd.join("b.rs");
        assert_eq!(confine(wd, &abs.to_string_lossy()).unwrap(), "b.rs");
        assert!(confine(wd, "../x.rs").is_err());
        assert!(confine(wd, "gh://o/r").is_err());
        let outside = wd.parent().unwrap().join("x.rs");
        assert!(confine(wd, &outside.to_string_lossy()).is_err());
    }

    /// Tools off (golden): the reviewer brief the default text-only path spawns, byte for byte —
    /// adds nothing to it.
    #[tokio::test]
    async fn tools_off_review_brief_is_byte_identical() {
        struct Capture(std::sync::Mutex<Vec<String>>);
        #[async_trait]
        impl TurnRunner for Capture {
            async fn run_turn(
                &self,
                ctx: TurnContext,
            ) -> anyhow::Result<polymorph_delegate::TurnOutput> {
                self.0.lock().unwrap().push(ctx.brief.clone());
                Ok(polymorph_delegate::TurnOutput {
                    text: PASS.to_string(),
                    summary: "stub".to_string(),
                    tool_calls: 0,
                    done: true,
                    data: Some(serde_json::json!({ "verdict_text": PASS })),
                })
            }
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/a.rs"), "fn a() {}\n").unwrap();
        let capture = Arc::new(Capture(std::sync::Mutex::new(Vec::new())));
        let reviewer = crate::BusReviewer {
            bus: Arc::new(polymorph_delegate::Bus::flat()),
            runner: capture.clone(),
            workdir: polymorph_delegate::WorkdirSpec::InPlace(dir.path().to_path_buf()),
            artifact_path: Some("src/a.rs".into()),
        };
        use crate::Reviewer;
        reviewer
            .review(crate::ReviewerInput {
                goal: "g".into(),
                criteria: "c".into(),
                artifact: "fn a() {}\n".into(),
                mode: crate::LoopMode::Act,
            })
            .await
            .unwrap();
        let golden = "MODE:\nact\n\nGOAL:\ng\n\nACCEPTANCE CRITERIA:\nc\n\nARTIFACT UNDER REVIEW:\nfn a() {}\n\n\nARTIFACT PATH:\nsrc/a.rs\n\nVERDICT DISCIPLINE:\nEvery finding MUST cite the accused file and line (`line` is a 1-based integer, or null) and quote the accused code in its issue; the quoted content must appear in ARTIFACT UNDER REVIEW above. A finding that cites no file, cites a file whose content is not in the artifact text, or describes a type's fields or signatures without having that type's definition in evidence, is a hypothesis — drop it, do not emit it. When prior-round context is absent, say what was checked, not what was assumed.";
        assert_eq!(capture.0.lock().unwrap().as_slice(), [golden]);
    }

    /// A gate still vetoes a pass the tool reviewer gave (the decorator
    /// order is unchanged: gates wrap whatever reviewer runs).
    #[tokio::test]
    async fn harness_gate_vetoes_a_tool_reviewer_pass() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}\n").unwrap();
        let turn = ScriptTurn::new(vec![
            collected(
                "",
                vec![call("r", "read", serde_json::json!({"path": "a.rs"}))],
            ),
            collected(PASS, vec![]),
        ]);
        let tool_runner = Arc::new(runner(turn, vec![], 4));
        let gate = Arc::new(crate::HarnessGate::default());
        gate.record(vec![polymorph_core::Finding {
            severity: polymorph_core::Severity::P1,
            file: crate::binding::VERIFY_FINDING_FILE.to_string(),
            line: None,
            issue: "Verify gate (harness-observed): cargo test FAILED exit 101".into(),
            proposed_fix: "make it pass".into(),
        }]);
        let reviewer = crate::HarnessGateReviewer {
            inner: crate::BusReviewer {
                bus: Arc::new(polymorph_delegate::Bus::flat()),
                runner: tool_runner.clone(),
                workdir: polymorph_delegate::WorkdirSpec::InPlace(dir.path().to_path_buf()),
                artifact_path: Some("a.rs".into()),
            },
            gates: vec![gate],
        };
        use crate::Reviewer;
        let verdict = reviewer
            .review(crate::ReviewerInput {
                goal: "g".into(),
                criteria: String::new(),
                artifact: "fn a() {}\n".into(),
                mode: crate::LoopMode::Act,
            })
            .await
            .unwrap();
        assert_eq!(verdict.verdict, polymorph_core::VerdictKind::Fail);
        assert_eq!(
            verdict.findings[0].file,
            crate::binding::VERIFY_FINDING_FILE
        );
        assert_eq!(tool_runner.reports()[0].calls, 1);
    }

    /// Fixer stand-in: rewrites the caller when a finding names the
    /// callee's real signature, else returns the artifact unchanged.
    struct FixCaller(std::path::PathBuf);

    #[async_trait]
    impl crate::Executor for FixCaller {
        async fn execute(&self, input: crate::ExecutorInput) -> anyhow::Result<String> {
            let fixed = if input
                .findings
                .iter()
                .any(|f| f.issue.contains("add(a, b, c)"))
            {
                input.artifact.replace("add(1, 2);", "add(1, 2, 3);")
            } else {
                input.artifact.clone()
            };
            std::fs::write(self.0.join("src/main.rs"), &fixed)?;
            Ok(fixed)
        }
    }

    /// Reviewer model stand-in that only knows what it sees: it reads the
    /// callee when it has tools and has not yet, and fails the call site
    /// only when the callee's real signature is in a tool result.
    struct CalleeAware;

    #[async_trait]
    impl AgenticTurnFn for CalleeAware {
        async fn turn(
            &self,
            messages: Vec<genai::chat::ChatMessage>,
            tools: Vec<genai::chat::Tool>,
            _cancel: &tokio_util::sync::CancellationToken,
        ) -> anyhow::Result<polymorph_llm::CollectedTurn> {
            let brief = messages[0].content.first_text().unwrap_or("").to_string();
            let results = fed(&messages);
            let text = match results.iter().find(|r| r.contains("pub fn add(")) {
                None if !tools.is_empty() => {
                    let read = call("r", "read", serde_json::json!({"path": "src/callee.rs"}));
                    return Ok(collected("", vec![read]));
                }
                None => PASS.to_string(),
                Some(callee) if callee.contains("c: i32") && brief.contains("add(1, 2);") => {
                    fail_on(
                        "src/callee.rs",
                        1,
                        "src/main.rs calls add(1, 2) but src/callee.rs:1 declares add(a, b, c)",
                    )
                }
                Some(_) => PASS.to_string(),
            };
            Ok(collected(&text, vec![]))
        }
    }

    async fn callee_drive(tools: bool) -> (crate::LoopReport, String, Vec<ReviewToolReport>) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/callee.rs"),
            "pub fn add(a: i32, b: i32, c: i32) -> i32 { a + b + c }\n",
        )
        .unwrap();
        let main = "fn main() { add(1, 2); }\n";
        std::fs::write(dir.path().join("src/main.rs"), main).unwrap();
        let registry = Arc::new(polymorph_tools::registry::v1_registry());
        let tool_runner = Arc::new(ToolReviewTurnRunner {
            turn: Arc::new(CalleeAware),
            tools: if tools {
                reviewer_tool_names(&registry, false)
            } else {
                Vec::new()
            },
            registry,
            gate: reviewer_gate(&polymorph_core::config::Permissions::default()),
            max_tool_rounds: DEFAULT_REVIEWER_TOOL_ROUNDS,
            history: ToolHistoryBudget::default(),
            log: Default::default(),
        });
        let reviewer = crate::BusReviewer {
            bus: Arc::new(polymorph_delegate::Bus::flat()),
            runner: tool_runner.clone(),
            workdir: polymorph_delegate::WorkdirSpec::InPlace(dir.path().to_path_buf()),
            artifact_path: Some("src/main.rs".into()),
        };
        let cfg = polymorph_core::LoopConfig {
            max_iterations: 3,
            surviving_finding_escalation: 3,
        };
        let report = crate::run_loop(
            &cfg,
            "call add correctly",
            "",
            main,
            crate::LoopMode::Act,
            &reviewer,
            &FixCaller(dir.path().to_path_buf()),
        )
        .await;
        let main_after = std::fs::read_to_string(dir.path().join("src/main.rs")).unwrap();
        (report, main_after, tool_runner.reports())
    }

    /// End to end: a call site passes two arguments to a callee in another file that takes three.
    /// Only the reviewer that read the callee catches it; its finding cites the inspected callee,
    /// survives cite-or-drop, reaches the fixer, and the drive passes once the caller is fixed.
    /// Without tools the same reviewer passes the broken call in round 1.
    #[tokio::test]
    async fn drive_catches_cross_file_signature_mismatch_only_with_tools() {
        let (report, main_after, reports) = callee_drive(true).await;
        assert_eq!(report.outcome, crate::Outcome::Passed { rounds: 2 });
        assert_eq!(report.rounds_log[0].verdict, crate::RoundVerdict::Fail);
        assert!(report.rounds_log[0].issues[0].contains("src/callee.rs:1"));
        assert_eq!(main_after, "fn main() { add(1, 2, 3); }\n");
        assert_eq!(reports.len(), 2);
        assert_eq!(reports[0].inspected, ["src/callee.rs"]);
        assert!(reports[0].dropped_findings.is_empty());
        let (report, main_after, _) = callee_drive(false).await;
        assert_eq!(report.outcome, crate::Outcome::Passed { rounds: 1 });
        assert_eq!(main_after, "fn main() { add(1, 2); }\n");
    }

    /// 's seeded CREATE round still skips the reviewer entirely: the tool reviewer runs from round
    /// 2 only.
    #[tokio::test]
    async fn seeded_round_one_skips_the_tool_reviewer() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        let turn = ScriptTurn::new(vec![collected(PASS, vec![])]);
        let tool_runner = Arc::new(runner(turn.clone(), vec![], 4));
        let reviewer = crate::BusReviewer {
            bus: Arc::new(polymorph_delegate::Bus::flat()),
            runner: tool_runner.clone(),
            workdir: polymorph_delegate::WorkdirSpec::InPlace(dir.path().to_path_buf()),
            artifact_path: Some("src/main.rs".into()),
        };
        let seed = crate::RoundSeed {
            kind: crate::SeedKind::Create,
            findings: vec![polymorph_core::Finding {
                severity: polymorph_core::Severity::P1,
                file: "src/main.rs".into(),
                line: None,
                issue: "create it".into(),
                proposed_fix: "create it".into(),
            }],
        };
        let cfg = polymorph_core::LoopConfig {
            max_iterations: 3,
            surviving_finding_escalation: 3,
        };
        let report = crate::run_loop_seeded(
            &cfg,
            "create src/main.rs",
            "",
            "",
            crate::LoopMode::Act,
            &reviewer,
            &FixCaller(dir.path().to_path_buf()),
            Some(seed),
        )
        .await;
        assert_eq!(report.outcome, crate::Outcome::Passed { rounds: 2 });
        assert_eq!(
            report.rounds_log[0].synthesized,
            Some(crate::SeedKind::Create)
        );
        assert_eq!(turn.offered().len(), 1, "one reviewer call, round 2 only");
        assert_eq!(tool_runner.reports().len(), 1);
    }
}
