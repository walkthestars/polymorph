//! Model-output eval skeleton (offline).
//!
//! The B6 corpus scores FIXED patches; this scores RAW MODEL OUTPUT:
//! extract → apply → byte-compare, with recorded or hand-written outputs
//! (hand-written ones say so — never present a fixture as a model run).
//! Live-model generation is F5b; the scorer + gate land here, LLM-free.
//!
//! Extraction contract (documented choice, revisit if F-measurements say
//! otherwise): the `edit.md` pack shows the patch inside a fenced block
//! but never pins raw-only output, and live runs prove models fence by
//! default (review loop runs #2/#3) — so one fenced block OR raw text both
//! count, while prose-only / multi-block / unclosed-fence fail NAMED.
//! (Contrast the verdict parser, which stays strict-JSON: the reviewer
//! contract demands machine parsing, while this consumer is ours to define.)

use std::path::{Path, PathBuf};

use super::hashline::{apply::normalize, apply_text_to_disk, SnapshotStore};

/// Eval format arm: which
/// extractor/applier scores a case. Recordings stamp it; ABSENCE means
/// hashline — every pre-format recording replays unchanged (pinned).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EvalFormat {
    #[default]
    Hashline,
    StrReplace,
}

impl EvalFormat {
    /// Parse a `--format` flag or recording stamp. Bogus values bail
    /// naming the input (operator error, never a score).
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "hashline" => Ok(EvalFormat::Hashline),
            "str-replace" => Ok(EvalFormat::StrReplace),
            other => Err(format!(
                "unknown format {other:?} (want `hashline` or `str-replace`)"
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            EvalFormat::Hashline => "hashline",
            EvalFormat::StrReplace => "str-replace",
        }
    }
}

impl std::fmt::Display for EvalFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Why extraction refused the output. All variants name the fault for the
/// scoreboard (and for the model in F5b retries).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtractError {
    /// No fenced block and the text doesn't open a `[PATH#TAG]` section.
    ProseOnly,
    /// Two or more fenced blocks — the harness must not guess which is live.
    MultipleBlocks { count: usize },
    /// An opening fence with no closing fence.
    UnclosedFence,
}

impl std::fmt::Display for ExtractError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExtractError::ProseOnly => write!(f, "no patch block: prose only"),
            ExtractError::MultipleBlocks { count } => {
                write!(
                    f,
                    "ambiguous output: {count} fenced blocks (want exactly 1)"
                )
            }
            ExtractError::UnclosedFence => write!(f, "unclosed fenced block"),
        }
    }
}

impl std::error::Error for ExtractError {}

/// Pull the hashline patch out of raw model output: exactly one fenced
/// block wins, else the whole trimmed text when it opens a section.
pub fn extract_patch(text: &str) -> Result<String, ExtractError> {
    let mut blocks: Vec<String> = Vec::new();
    let mut current: Option<Vec<String>> = None;
    for line in text.lines() {
        let t = line.trim_start();
        if t.starts_with("```") {
            if current.is_some() {
                blocks.push(current.take().unwrap().join("\n"));
            } else {
                current = Some(Vec::new());
            }
        } else if let Some(buf) = current.as_mut() {
            buf.push(line.to_string());
        }
    }
    if current.is_some() {
        return Err(ExtractError::UnclosedFence);
    }
    match blocks.len() {
        0 => {
            let raw = text.trim().to_string();
            if raw.starts_with('[') {
                Ok(raw)
            } else {
                Err(ExtractError::ProseOnly)
            }
        }
        1 => Ok(blocks.pop().unwrap().trim().to_string()),
        n => Err(ExtractError::MultipleBlocks { count: n }),
    }
}

/// One scored unit: initial files + the model's raw output + the expected
/// end state. `model_output` may be a live recording or hand-written
/// (fixture — always labeled at the call site).
#[derive(Debug, Clone)]
pub struct EvalCase {
    pub id: String,
    /// (relative path, initial bytes as text).
    pub files: Vec<(String, String)>,
    /// (relative path, expected bytes as text after a correct patch).
    pub expected: Vec<(String, String)>,
    pub model_output: String,
}

/// Typed scoring outcome. `ApplyFail` carries the `HashlineError` Display
/// (variant-named by construction); `Mismatch` names the first divergence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaseOutcome {
    Pass,
    ExtractFail(String),
    ApplyFail(String),
    Mismatch(String),
}

/// The scored case plus its outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaseReport {
    pub id: String,
    pub outcome: CaseOutcome,
}

impl CaseReport {
    pub fn passed(&self) -> bool {
        matches!(self.outcome, CaseOutcome::Pass)
    }
}

/// Write initial workspace files (shared setup for both scorers:
/// `score_case` mints hashline tags after this; the str_replace scorer
/// needs no tags). Caller-owned `root`; setup faults are `ApplyFail`.
pub(crate) fn write_initial_files(root: &Path, files: &[(String, String)]) -> Result<(), String> {
    for (name, content) in files {
        let path = root.join(name);
        if let Some(parent) = path.parent() {
            if std::fs::create_dir_all(parent).is_err() {
                return Err(format!("setup: cannot create dir for {name}"));
            }
        }
        if std::fs::write(&path, content.as_bytes()).is_err() {
            return Err(format!("setup: cannot write {name}"));
        }
    }
    Ok(())
}

/// Byte-exact workspace comparison shared by both scorers
/// factored, not copied — the ONLY thing that may differ between arms is
/// extract+apply). Includes the no-temp-files check (corpus-driver
/// precedent). `Ok` = Pass; `Err(msg)` = the `Mismatch` detail.
pub(crate) fn check_workspace(root: &Path, expected: &[(String, String)]) -> Result<(), String> {
    // No temp files may survive a scored run (corpus-driver precedent).
    let mut got: Vec<(String, Vec<u8>)> = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) => {
                return Err(format!("cannot list workspace: {e}"));
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    return Err(format!("cannot read entry: {e}"));
                }
            };
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                let rel = p
                    .strip_prefix(root)
                    .unwrap_or(&p)
                    .to_string_lossy()
                    .replace('\\', "/");
                if PathBuf::from(&rel).extension().and_then(|x| x.to_str()) == Some("polymorph-tmp")
                {
                    return Err(format!("temp file left behind: {rel}"));
                }
                match std::fs::read(&p) {
                    Ok(b) => got.push((rel, b)),
                    Err(e) => {
                        return Err(format!("cannot read {rel}: {e}"));
                    }
                }
            }
        }
    }
    got.sort_by(|a, b| a.0.cmp(&b.0));
    let mut want: Vec<(String, Vec<u8>)> = expected
        .iter()
        .map(|(n, c)| (n.clone(), c.as_bytes().to_vec()))
        .collect();
    want.sort_by(|a, b| a.0.cmp(&b.0));
    if got.len() != want.len() {
        return Err(format!("file count: got {} want {}", got.len(), want.len()));
    }
    for ((gn, gb), (wn, wb)) in got.iter().zip(want.iter()) {
        if gn != wn {
            return Err(format!("file set: got {gn} want {wn}"));
        }
        if gb != wb {
            return Err(format!("content differs: {gn}"));
        }
    }
    Ok(())
}

/// Score one case inside `root` (caller-owned dir manages lifetimes).
/// Setup mirrors the B6 driver: write initials, mint tags over NORMALIZED
/// text (the same convention the applier verifies — minting raw bytes
/// would fork CRLF/BOM), extract, apply, byte-compare, temp check.
pub fn score_case(case: &EvalCase, root: &Path) -> CaseReport {
    let fail = |o| CaseReport {
        id: case.id.clone(),
        outcome: o,
    };
    if let Err(e) = write_initial_files(root, &case.files) {
        return fail(CaseOutcome::ApplyFail(e));
    }
    let mut store = SnapshotStore::new();
    for (name, content) in &case.files {
        // Same normalization the applier checks (corpus-driver precedent).
        let norm = normalize(content.as_bytes());
        store.mint(Path::new(name), &norm.text);
    }
    let patch = match extract_patch(&case.model_output) {
        Ok(p) => p,
        Err(e) => return fail(CaseOutcome::ExtractFail(e.to_string())),
    };
    if let Err(e) = apply_text_to_disk(&patch, root, &store) {
        return fail(CaseOutcome::ApplyFail(e.to_string()));
    }
    match check_workspace(root, &case.expected) {
        Ok(()) => fail(CaseOutcome::Pass),
        Err(m) => fail(CaseOutcome::Mismatch(m)),
    }
}

/// Pass/total aggregation over scored cases.
#[derive(Debug, Clone, Default)]
pub struct Scoreboard {
    pub reports: Vec<CaseReport>,
}

impl Scoreboard {
    pub fn total(&self) -> usize {
        self.reports.len()
    }

    pub fn passed(&self) -> usize {
        self.reports.iter().filter(|r| r.passed()).count()
    }

    pub fn rate(&self) -> f64 {
        if self.reports.is_empty() {
            0.0
        } else {
            self.passed() as f64 / self.reports.len() as f64
        }
    }

    pub fn failed_ids(&self) -> Vec<String> {
        self.reports
            .iter()
            .filter(|r| !r.passed())
            .map(|r| r.id.clone())
            .collect()
    }
}

/// CI gate: pass at or above `min_rate`, else name every failure.
/// Empty boards fail (a gate that passes on zero cases proves nothing).
pub fn gate(sb: &Scoreboard, min_rate: f64) -> Result<(), String> {
    if sb.reports.is_empty() {
        return Err("eval gate: empty scoreboard (scores nothing, proves nothing)".to_string());
    }
    let rate = sb.rate();
    if rate >= min_rate {
        Ok(())
    } else {
        Err(format!(
            "eval gate red: {}/{} passed ({:.0}% < {:.0}%): {}",
            sb.passed(),
            sb.total(),
            rate * 100.0,
            min_rate * 100.0,
            sb.failed_ids().join(", ")
        ))
    }
}

/// Rebuild an `EvalCase` from an `eval run/sweep --record` JSON recording
/// (offline CI replay; v2 shape since F5e). Two shapes, detected
/// on the `files` key: v2 carries `files` + `expected` as `{name, content}`
/// arrays (multi-file, absence, rename all expressible); legacy carries
/// `input` + `expected` strings with the workspace name in `path` (or the
/// legacy `id` when `path` predates it — F5b/F5c records replay unchanged).
/// `model`/`instruction`/`outcome` are provenance, ignored for scoring —
/// the scorer re-derives the outcome, never trusts the recorded one.
///
/// Corrupt recordings are HARD ERRORS naming the field (operator error),
/// never `CaseOutcome` variants (which describe model output) — a bad file
/// must not masquerade as a model failure on the board.
pub fn load_recording(json_text: &str) -> Result<EvalCase, String> {
    let v: serde_json::Value =
        serde_json::from_str(json_text).map_err(|e| format!("recording: invalid JSON: {e}"))?;
    let field = |name: &str| {
        v.get(name)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| format!("recording: missing or non-string `{name}`"))
    };
    let id = field("id")?;
    let output = field("output")?;
    let pair = |entry: &serde_json::Value, key: &str| -> Result<(String, String), String> {
        let name = entry
            .get("name")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| format!("recording: `{key}` entry missing `name`"))?;
        let content = entry
            .get("content")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| format!("recording: `{key}` entry {name:?} missing `content`"))?;
        Ok((name, content))
    };
    let array = |key: &str| -> Result<Vec<(String, String)>, String> {
        v.get(key)
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| format!("recording: missing or non-array `{key}`"))?
            .iter()
            .map(|e| pair(e, key))
            .collect()
    };
    let (files, expected) = match v.get("files") {
        // v2: explicit file arrays (absence = missing entry, rename =
        // differing names — the shapes legacy strings cannot express).
        Some(_) => (array("files")?, array("expected")?),
        // Legacy: single input/expected pair, name in `path` else `id`.
        None => {
            let input = field("input")?;
            let want = field("expected")?;
            let name = v
                .get("path")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| id.clone());
            (vec![(name.clone(), input)], vec![(name, want)])
        }
    };
    Ok(EvalCase {
        files,
        expected,
        model_output: output,
        id,
    })
}

/// Rebuild an `EvalCase` plus its format arm: the `format`
/// stamp selects the scorer (`hashline` → `score_case`, `str-replace` →
/// `score_str_case`). ABSENT stamp defaults hashline — every pre-format
/// recording replays unchanged. UNKNOWN stamps are hard errors naming the
/// value (never a score — scoring under the wrong arm would forge data).
/// Scoring under the non-stamped arm is the cross-format replay the design
/// forbids; `run_eval_check` dispatches per record through this function.
pub fn load_recording_with_format(json_text: &str) -> Result<(EvalCase, EvalFormat), String> {
    let v: serde_json::Value =
        serde_json::from_str(json_text).map_err(|e| format!("recording: invalid JSON: {e}"))?;
    let format = match v.get("format") {
        None => EvalFormat::Hashline,
        Some(s) => {
            let name = s
                .as_str()
                .ok_or_else(|| "recording: `format` is not a string".to_string())?;
            EvalFormat::parse(name).map_err(|e| format!("recording: {e}"))?
        }
    };
    // Single source of truth for case shapes stays `load_recording`
    // (double parse over tiny JSONs; no second shape reader to drift).
    let case = load_recording(json_text)?;
    Ok((case, format))
}

/// Render the user prompt for a live eval turn: the instruction plus
/// each workspace file with its CURRENT tag. The tag flow mirrors the
/// tools-executor read→tag→edit loop: the model patches against tags
/// minted here, never invented ones. Runtime assembly (TOOLS_PREAMBLE
/// precedent) — pack migration, if warranted, is F5's call on measured data.
///
/// ```text
/// TASK: <instruction>
///
/// WORKSPACE (tags are current; cite them verbatim as [PATH#TAG]):
/// --- <name> [<name>#<tag>] ---
/// <content>
/// ---
/// ```
pub fn render_eval_prompt(files: &[(String, String, String)], instruction: &str) -> String {
    let mut out = String::from("TASK:\n");
    out.push_str(instruction);
    out.push_str("\n\nWORKSPACE (tags are current; cite them verbatim as [PATH#TAG]):\n");
    for (name, content, tag) in files {
        out.push_str(&format!("--- {name} [{name}#{tag}] ---\n{content}"));
        if !content.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("---\n");
    }
    out.push_str("Emit ONE hashline patch per the system prompt.\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Hand-written model outputs (NOT model runs — fixtures proving the
    // scorer discriminates brings recordings).
    const GOOD_RAW: &str = "[a.txt#TAG]\nPUT 1:\n+fixed\n";
    const GOOD_FENCED: &str =
        "Here is the patch:\n```text\n[a.txt#TAG]\nPUT 1:\n+fixed\n```\nDone.\n";
    const BARE_FENCE: &str = "```\n[a.txt#TAG]\nPUT 1:\n+fixed\n```";
    const PROSE: &str = "I would change line 1 to fixed, but the tag expired.";
    const TWO_BLOCKS: &str =
        "```\n[a.txt#TAG]\nPUT 1:\n+one\n```\nOr:\n```\n[a.txt#TAG]\nPUT 1:\n+two\n```";
    const UNCLOSED: &str = "```text\n[a.txt#TAG]\nPUT 1:\n+fixed\n";

    fn case(id: &str, output: &str) -> EvalCase {
        EvalCase {
            id: id.to_string(),
            files: vec![("a.txt".to_string(), "orig\n".to_string())],
            expected: vec![("a.txt".to_string(), "fixed\n".to_string())],
            model_output: output.replace("TAG", &fixture_tag()),
        }
    }

    /// Mint over the NORMALIZED fixture content — exactly like score_case's
    /// setup (terminator-stripped per the line model). Minting
    /// the raw `"orig\n"` forks the tag and every fixture goes stale.
    fn fixture_tag() -> String {
        let mut store = SnapshotStore::new();
        let norm = normalize(b"orig\n");
        store.mint(Path::new("a.txt"), &norm.text)
    }

    #[test]
    fn raw_good_output_passes() {
        let d = tempfile::tempdir().unwrap();
        let rep = score_case(&case("raw", GOOD_RAW), d.path());
        assert_eq!(rep.outcome, CaseOutcome::Pass, "{rep:?}");
    }

    #[test]
    fn fenced_blocks_pass() {
        for (id, out) in [("fenced", GOOD_FENCED), ("bare", BARE_FENCE)] {
            let d = tempfile::tempdir().unwrap();
            let rep = score_case(&case(id, out), d.path());
            assert_eq!(rep.outcome, CaseOutcome::Pass, "{id}: {rep:?}");
        }
    }

    #[test]
    fn extraction_failures_name_the_fault() {
        assert_eq!(extract_patch(PROSE), Err(ExtractError::ProseOnly));
        assert_eq!(
            extract_patch(TWO_BLOCKS),
            Err(ExtractError::MultipleBlocks { count: 2 })
        );
        assert_eq!(extract_patch(UNCLOSED), Err(ExtractError::UnclosedFence));
        assert_eq!(extract_patch("   \n  "), Err(ExtractError::ProseOnly));
        for (id, out) in [
            ("prose", PROSE),
            ("two", TWO_BLOCKS),
            ("unclosed", UNCLOSED),
        ] {
            let d = tempfile::tempdir().unwrap();
            let rep = score_case(&case(id, out), d.path());
            assert!(
                matches!(rep.outcome, CaseOutcome::ExtractFail(_)),
                "{id}: {rep:?}"
            );
        }
    }

    #[test]
    fn stale_tag_scores_apply_fail_naming_stale() {
        let d = tempfile::tempdir().unwrap();
        let mut c = case("stale", GOOD_RAW);
        c.model_output = c.model_output.replace(&fixture_tag(), "ffff");
        let rep = score_case(&c, d.path());
        match rep.outcome {
            CaseOutcome::ApplyFail(msg) => assert!(msg.contains("stale tag"), "{msg}"),
            other => panic!("want ApplyFail, got {other:?}"),
        }
        // Atomicity rides along: the file is byte-identical post-rejection.
        let back = std::fs::read(d.path().join("a.txt")).unwrap();
        assert_eq!(back, b"orig\n");
    }

    #[test]
    fn wrong_content_scores_mismatch() {
        let d = tempfile::tempdir().unwrap();
        let mut c = case("wrong", GOOD_RAW);
        c.expected = vec![("a.txt".to_string(), "something-else\n".to_string())];
        let rep = score_case(&c, d.path());
        assert!(matches!(rep.outcome, CaseOutcome::Mismatch(_)), "{rep:?}");
    }

    #[test]
    fn eval_prompt_carries_instruction_tags_and_close() {
        let p = render_eval_prompt(
            &[("a.txt".to_string(), "hi\n".to_string(), "abcd".to_string())],
            "Do the thing.",
        );
        assert!(p.contains("TASK:\nDo the thing."), "{p}");
        assert!(p.contains("--- a.txt [a.txt#abcd] ---"), "{p}");
        assert!(p.contains("hi\n---\n"), "{p}");
        assert!(
            p.ends_with("Emit ONE hashline patch per the system prompt.\n"),
            "{p}"
        );
    }

    #[test]
    fn eval_prompt_terminates_unterminated_content() {
        // Content without a trailing newline still closes cleanly so the
        // `---` fence never glues onto the last content line.
        let p = render_eval_prompt(
            &[("a.txt".to_string(), "hi".to_string(), "abcd".to_string())],
            "Go.",
        );
        assert!(p.contains("hi\n---\n"), "{p}");
    }

    #[test]
    fn gate_greens_and_reds_honestly() {
        let d = tempfile::tempdir().unwrap();
        let good = score_case(&case("g", GOOD_RAW), d.path());
        let bad = score_case(&case("b", PROSE), d.path());
        let all_pass = Scoreboard {
            reports: vec![good.clone()],
        };
        assert!(gate(&all_pass, 1.0).is_ok());
        let mixed = Scoreboard {
            reports: vec![good, bad],
        };
        let err = gate(&mixed, 1.0).expect_err("must go red");
        assert!(err.contains('b'), "{err}");
        assert!(gate(&Scoreboard { reports: vec![] }, 0.0).is_err());
    }

    // Hand-written recordings in the exact `eval run --record` shape
    // (NOT model runs — fixtures proving the loader replays brings
    // live bytes). The `output` tag is minted like `case()` above so the
    // replay hits the Pass path for the right reason (live tag, not luck).
    fn recording(id: &str, output: &str) -> String {
        serde_json::json!({
            "id": "a.txt",
            "model": "fixture-model",
            "instruction": format!("case {id}"),
            "input": "orig\n",
            "expected": "fixed\n",
            "output": output.replace("TAG", &fixture_tag()),
            "outcome": "Pass",
        })
        .to_string()
    }

    #[test]
    fn recording_round_trips_to_pass() {
        let loaded = load_recording(&recording("good", GOOD_RAW)).unwrap();
        assert_eq!(loaded.id, "a.txt");
        assert_eq!(
            loaded.files,
            vec![("a.txt".to_string(), "orig\n".to_string())]
        );
        assert_eq!(
            loaded.expected,
            vec![("a.txt".to_string(), "fixed\n".to_string())]
        );
        let d = tempfile::tempdir().unwrap();
        let rep = score_case(&loaded, d.path());
        assert_eq!(rep.outcome, CaseOutcome::Pass, "{rep:?}");
        // The recorded `outcome` is provenance, never trusted: a recording
        // claiming Pass with prose output still scores ExtractFail.
        let loaded = load_recording(&recording("lying", PROSE)).unwrap();
        let rep = score_case(&loaded, d.path());
        assert!(
            matches!(rep.outcome, CaseOutcome::ExtractFail(_)),
            "{rep:?}"
        );
    }

    #[test]
    fn recording_failures_replay_their_outcomes() {
        let d = tempfile::tempdir().unwrap();
        // Prose output replays to ExtractFail (not a loader error).
        let loaded = load_recording(&recording("prose", PROSE)).unwrap();
        let rep = score_case(&loaded, d.path());
        assert!(
            matches!(rep.outcome, CaseOutcome::ExtractFail(_)),
            "{rep:?}"
        );
        // Stale-tag output replays to ApplyFail naming the stale tag.
        // (`recording()` would mint a live tag, so build this one by hand.)
        let live = GOOD_RAW.replace("TAG", &fixture_tag());
        let stale = live.replace(&fixture_tag(), "ffff");
        let rec = serde_json::json!({
            "id": "a.txt",
            "model": "fixture-model",
            "instruction": "stale",
            "input": "orig\n",
            "expected": "fixed\n",
            "output": stale,
            "outcome": "ApplyFail",
        })
        .to_string();
        let loaded = load_recording(&rec).unwrap();
        let rep = score_case(&loaded, d.path());
        match rep.outcome {
            CaseOutcome::ApplyFail(msg) => assert!(msg.contains("stale tag"), "{msg}"),
            other => panic!("want ApplyFail, got {other:?}"),
        }
    }

    #[test]
    fn recording_corruption_names_the_field() {
        assert!(load_recording("not json{{{")
            .unwrap_err()
            .contains("invalid JSON"));
        for (name, rec) in [
            (
                "id",
                serde_json::json!({"input": "a", "expected": "b", "output": "c"}),
            ),
            (
                "input",
                serde_json::json!({"id": "a", "expected": "b", "output": "c"}),
            ),
            (
                "expected",
                serde_json::json!({"id": "a", "input": "b", "output": "c"}),
            ),
            (
                "output",
                serde_json::json!({"id": "a", "input": "b", "expected": "c"}),
            ),
        ] {
            let err = load_recording(&rec.to_string()).unwrap_err();
            assert!(err.contains(name), "{name}: {err}");
        }
        // Wrong types refuse with the field named (never a score).
        let rec = serde_json::json!({"id": "a", "input": 42, "expected": "b", "output": "c"});
        assert!(load_recording(&rec.to_string())
            .unwrap_err()
            .contains("input"));
        // A wrong-typed `path` is ignored (legacy shape has none) — the
        // filename falls back to `id`, never an error.
        let rec = serde_json::json!({"id": "a", "input": "b", "expected": "c", "output": "d", "path": 42});
        assert_eq!(load_recording(&rec.to_string()).unwrap().files[0].0, "a");
    }

    #[test]
    fn recording_path_separates_identity_from_filename() {
        // F5d sweep shape: the corpus task id is NOT the workspace file.
        // Hand-written (NOT a model run — proves the loader seam).
        let rec = serde_json::json!({
            "id": "replace-single-line",
            "path": "a.txt",
            "model": "fixture-model",
            "instruction": "PUT N replaces one line",
            "input": "orig\n",
            "expected": "fixed\n",
            "output": GOOD_RAW.replace("TAG", &fixture_tag()),
            "outcome": "Pass",
        })
        .to_string();
        let loaded = load_recording(&rec).unwrap();
        assert_eq!(loaded.id, "replace-single-line");
        assert_eq!(
            loaded.files,
            vec![("a.txt".to_string(), "orig\n".to_string())]
        );
        assert_eq!(
            loaded.expected,
            vec![("a.txt".to_string(), "fixed\n".to_string())]
        );
        let d = tempfile::tempdir().unwrap();
        let rep = score_case(&loaded, d.path());
        assert_eq!(rep.outcome, CaseOutcome::Pass, "{rep:?}");
        assert_eq!(rep.id, "replace-single-line");
    }

    // v2 array-shape recordings. Hand-written correct patches with
    // live tags (NOT model runs — proves the loader + scorer cover the
    // shapes legacy strings cannot express: multi-file, rename, absence).
    fn v2_recording(
        id: &str,
        files: Vec<(&str, &str)>,
        expected: Vec<(&str, &str)>,
        patch: &str,
    ) -> String {
        let mut store = SnapshotStore::new();
        let mut live = patch.to_string();
        for (name, content) in &files {
            let norm = normalize(content.as_bytes());
            let tag = store.mint(Path::new(name), &norm.text);
            live = live.replace(&format!("TAG:{name}"), &tag);
        }
        assert!(!live.contains("TAG:"), "unminted tag left: {live}");
        serde_json::json!({
            "id": id,
            "model": "fixture-model",
            "instruction": format!("case {id}"),
            "files": files.iter().map(|(n, c)| serde_json::json!({"name": n, "content": c})).collect::<Vec<_>>(),
            "expected": expected.iter().map(|(n, c)| serde_json::json!({"name": n, "content": c})).collect::<Vec<_>>(),
            "output": live,
            "outcome": "Pass",
        })
        .to_string()
    }

    #[test]
    fn recording_v2_covers_multi_rename_and_absence() {
        for (id, files, expected, patch) in [
            (
                "multi",
                vec![("h1.txt", "x\n"), ("h2.txt", "y\n")],
                vec![("h1.txt", "X\n"), ("h2.txt", "Y\n")],
                "[h1.txt#TAG:h1.txt]\nPUT 1:\n+X\n[h2.txt#TAG:h2.txt]\nPUT 1:\n+Y\n",
            ),
            (
                "mv",
                vec![("n.txt", "data\n")],
                vec![("o.txt", "data\n")],
                "[n.txt#TAG:n.txt]\nMV o.txt\n",
            ),
            (
                "rem",
                vec![("m.txt", "bye\n")],
                vec![],
                "[m.txt#TAG:m.txt]\nREM\n",
            ),
        ] {
            let loaded = load_recording(&v2_recording(id, files, expected, patch)).unwrap();
            assert_eq!(loaded.id, id);
            // Fresh dir per case: score_case scores the whole tree, so
            // leftovers from a previous case would false-fail the count.
            let d = tempfile::tempdir().unwrap();
            let rep = score_case(&loaded, d.path());
            assert_eq!(rep.outcome, CaseOutcome::Pass, "{id}: {rep:?}");
        }
    }

    #[test]
    fn recording_attempts_field_is_provenance_ignored() {
        // The sweep writer stamps per-case attempt counts; the scorer
        // re-derives everything, so the loader must not care (nor fail).
        let mut rec: serde_json::Value = serde_json::from_str(&v2_recording(
            "multi",
            vec![("h1.txt", "x\n"), ("h2.txt", "y\n")],
            vec![("h1.txt", "X\n"), ("h2.txt", "Y\n")],
            "[h1.txt#TAG:h1.txt]\nPUT 1:\n+X\n[h2.txt#TAG:h2.txt]\nPUT 1:\n+Y\n",
        ))
        .unwrap();
        rec["attempts"] = serde_json::json!(3);
        let loaded = load_recording(&rec.to_string()).unwrap();
        assert_eq!(loaded.id, "multi");
        let d = tempfile::tempdir().unwrap();
        let rep = score_case(&loaded, d.path());
        assert_eq!(rep.outcome, CaseOutcome::Pass, "{rep:?}");
    }

    #[test]
    fn recording_v2_malformed_names_the_key() {
        // `files` present but not an array → v2 attempted, v2 fault named.
        let rec = serde_json::json!({"id": "a", "output": "o", "files": "nope", "expected": []});
        assert!(load_recording(&rec.to_string())
            .unwrap_err()
            .contains("files"));
        // Entry missing `content` names the array + file.
        let rec = serde_json::json!({"id": "a", "output": "o",
            "files": [{"name": "a.txt"}], "expected": []});
        let err = load_recording(&rec.to_string()).unwrap_err();
        assert!(err.contains("files") && err.contains("a.txt"), "{err}");
        // Missing `expected` array names it (never falls back to legacy).
        let rec = serde_json::json!({"id": "a", "output": "o",
            "files": [{"name": "a.txt", "content": "x\n"}]});
        assert!(load_recording(&rec.to_string())
            .unwrap_err()
            .contains("expected"));
    }

    #[test]
    fn format_parses_two_arms_and_refuses_bogus() {
        assert_eq!(EvalFormat::parse("hashline"), Ok(EvalFormat::Hashline));
        assert_eq!(EvalFormat::parse("str-replace"), Ok(EvalFormat::StrReplace));
        assert_eq!(EvalFormat::default(), EvalFormat::Hashline);
        assert_eq!(EvalFormat::Hashline.as_str(), "hashline");
        assert_eq!(EvalFormat::StrReplace.as_str(), "str-replace");
        let err = EvalFormat::parse("unified-diff").unwrap_err();
        assert!(
            err.contains("unified-diff") && err.contains("hashline"),
            "{err}"
        );
    }

    #[test]
    fn recording_format_defaults_hashline_and_names_unknown() {
        // Legacy record (no stamp): case loads, arm defaults hashline —
        // every pre-format recording replays under its original scorer.
        let legacy = serde_json::json!({
            "id": "a.txt", "model": "m", "instruction": "i",
            "input": "orig\n", "expected": "fixed\n", "output": "o",
            "outcome": "Pass",
        })
        .to_string();
        let (case, format) = load_recording_with_format(&legacy).unwrap();
        assert_eq!(format, EvalFormat::Hashline);
        assert_eq!(case.id, "a.txt");
        // Stamped record round-trips its arm.
        let mut stamped: serde_json::Value = serde_json::from_str(&legacy).unwrap();
        stamped["format"] = serde_json::json!("str-replace");
        let (_, format) = load_recording_with_format(&stamped.to_string()).unwrap();
        assert_eq!(format, EvalFormat::StrReplace);
        // Unknown stamp is a hard error naming the value (never a score).
        stamped["format"] = serde_json::json!("unified-diff");
        let err = load_recording_with_format(&stamped.to_string()).unwrap_err();
        assert!(err.contains("unified-diff"), "{err}");
        // Non-string stamp likewise refuses.
        stamped["format"] = serde_json::json!(42);
        assert!(load_recording_with_format(&stamped.to_string())
            .unwrap_err()
            .contains("format"));
    }
}
