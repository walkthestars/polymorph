//! str_replace baseline scorer (Chunk 28, design `plan-chunk27.md`).
//!
//! The rival arm for the TBM-05 delta: anchored old/new substitution
//! against the SAME `EvalCase` / `Scoreboard` / `gate` / `CaseOutcome` as
//! the hashline scorer. The ONLY moving variables between arms are format
//! pack + extractor/applier — setup and byte-compare ride the shared
//! helpers in [`super::eval`] (`write_initial_files`, `check_workspace`),
//! factored, not copied.
//!
//! Contract (normative, spike-proven in chunk 27 then deleted):
//! ```text
//! [edit a.txt]
//! <<<
//! <exact original lines, anchored context>
//! >>>
//! <replacement lines>
//! ```
//! Determinism mirrors hashline's never-guess stance: `old` must occur
//! EXACTLY once (byte-exact substring; missing/ambiguous refuse named);
//! empty `old` refuses (no anchor = whole-file-rewrite smuggling); prose
//! outside sections refuses; unclosed sections refuse; duplicate sections
//! for one file refuse; `../` escapes refuse. Multi-file = multiple
//! sections (one per file). Whole-patch validation precedes ANY write
//! (atomicity parity with hashline).
//!
//! Extraction parity: raw-or-one-fence like `extract_patch` (models fence
//! by default — review-loop runs #2/#3 precedent). Fence policy must not
//! become the measured variable.
//!
//! Honest limitation (documented, not hidden): matching is byte-exact on
//! `\n`-joined text with NO line-ending normalization. When a model
//! normalizes endings, a CRLF file's `\r\n` bytes never equal its `\n`
//! old-block, so the case refuses as `MissingOld` while hashline's
//! terminator-aware applier passes it (live datum: glm-5.3 reproduced CRLF
//! verbatim on first attempt, so the risk is conditional, not certain).
//! That asymmetry IS a measured effect candidate (whitespace battles),
//! not a harness bug.

use std::path::Path;

use super::eval::{check_workspace, write_initial_files, CaseOutcome, CaseReport, EvalCase};

/// One parsed `[edit path]` section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StrSection {
    pub path: String,
    pub old: String,
    pub new: String,
}

/// Why str_replace extraction refused the output. Every variant names the
/// fault (and path/line where applicable) for the scoreboard and for
/// retry feedback verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StrExtractError {
    /// No fenced block and no parseable section (or fenced content with
    /// no sections) — prose, not a patch.
    ProseOnly,
    /// Two or more fenced blocks — the harness must not guess which is live.
    MultipleBlocks { count: usize },
    /// An opening fence with no closing fence.
    UnclosedFence,
    /// A `[...]` line that is not a well-formed `[edit <path>]` header.
    BadHeader { line: usize, text: String },
    /// Non-blank, non-header text outside any section.
    ProseOutside { line: usize },
    /// Header with no `<<<` opener after it (or EOF first).
    MissingOpen { path: String },
    /// No `>>>` closer before EOF or the next header.
    UnclosedSection { path: String },
    /// Empty `old` block (no anchor = whole-file rewrite smuggling).
    EmptyOld { path: String },
    /// Two sections cite the same file (mirrors hashline's
    /// one-section-per-file rule).
    DuplicateFile { path: String },
}

impl std::fmt::Display for StrExtractError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StrExtractError::ProseOnly => write!(f, "no patch sections: prose only"),
            StrExtractError::MultipleBlocks { count } => write!(
                f,
                "ambiguous output: {count} fenced blocks (want exactly 1)"
            ),
            StrExtractError::UnclosedFence => write!(f, "unclosed fenced block"),
            StrExtractError::BadHeader { line, text } => {
                write!(
                    f,
                    "bad section header at line {line}: {text:?} (want `[edit <path>]`)"
                )
            }
            StrExtractError::ProseOutside { line } => {
                write!(f, "prose outside sections at line {line}")
            }
            StrExtractError::MissingOpen { path } => {
                write!(f, "section [{path}]: missing `<<<` opener")
            }
            StrExtractError::UnclosedSection { path } => {
                write!(f, "section [{path}]: missing `>>>` closer")
            }
            StrExtractError::EmptyOld { path } => {
                write!(f, "section [{path}]: empty old block (no anchor)")
            }
            StrExtractError::DuplicateFile { path } => {
                write!(f, "section [{path}]: duplicate section for one file")
            }
        }
    }
}

impl std::error::Error for StrExtractError {}

/// Why str_replace application refused a parsed patch. Whole-patch
/// validation precedes any write, so these never half-apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StrApplyError {
    /// Section path escapes the workspace root.
    Traversal { path: String },
    /// Section cites a file absent from the workspace.
    MissingFile { path: String },
    /// `old` occurs zero times (byte-exact).
    MissingOld { path: String },
    /// `old` occurs more than once — the harness must not guess which.
    AmbiguousOld { path: String, count: usize },
    /// Filesystem fault (read/write/create-dir), never a model fault.
    Io { path: String, detail: String },
}

impl std::fmt::Display for StrApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StrApplyError::Traversal { path } => {
                write!(f, "path escapes workspace: {path:?}")
            }
            StrApplyError::MissingFile { path } => write!(f, "no such file: {path:?}"),
            StrApplyError::MissingOld { path } => {
                write!(f, "old block occurs 0 times in {path:?} (want exactly 1)")
            }
            StrApplyError::AmbiguousOld { path, count } => write!(
                f,
                "old block occurs {count} times in {path:?} (want exactly 1)"
            ),
            StrApplyError::Io { path, detail } => write!(f, "io error at {path:?}: {detail}"),
        }
    }
}

impl std::error::Error for StrApplyError {}

/// Unwrap raw-or-one-fence (extraction parity with `extract_patch`):
/// exactly one fenced block wins its inner text, else the whole trimmed
/// text. Fence faults are named, never scores.
fn unwrap_fence(text: &str) -> Result<String, StrExtractError> {
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
        return Err(StrExtractError::UnclosedFence);
    }
    match blocks.len() {
        0 => Ok(text.to_string()),
        1 => Ok(blocks.pop().unwrap()),
        n => Err(StrExtractError::MultipleBlocks { count: n }),
    }
}

/// Parse `[edit path]` sections out of (fence-unwrapped) text.
fn parse_sections(inner: &str) -> Result<Vec<StrSection>, StrExtractError> {
    let lines: Vec<&str> = inner.lines().collect();
    let mut sections: Vec<StrSection> = Vec::new();
    let mut i = 0;
    let n = lines.len();
    // Line numbers are 1-based over the unwrapped text.
    let is_blank = |s: &str| s.trim().is_empty();
    while i < n {
        let line = lines[i];
        let t = line.trim();
        if is_blank(line) {
            i += 1;
            continue;
        }
        // Header?
        if t.starts_with('[') {
            let path = parse_header(t).ok_or_else(|| StrExtractError::BadHeader {
                line: i + 1,
                text: t.to_string(),
            })?;
            i += 1;
            // Skip blanks between header and `<<<`.
            while i < n && is_blank(lines[i]) {
                i += 1;
            }
            if i >= n || lines[i].trim() != "<<<" {
                return Err(StrExtractError::MissingOpen { path });
            }
            i += 1;
            // Collect old until `>>>`, a next header, or EOF.
            let mut old: Vec<&str> = Vec::new();
            let mut closed = false;
            while i < n {
                let l = lines[i];
                if l.trim() == ">>>" {
                    closed = true;
                    i += 1;
                    break;
                }
                if l.trim().starts_with("[edit ") && l.trim().ends_with(']') {
                    break; // next header while open => unclosed below
                }
                // A bare `[...]` line inside old is content, not a header
                // (only `[edit ...]` opens) — keep it verbatim.
                old.push(l);
                i += 1;
            }
            if !closed {
                return Err(StrExtractError::UnclosedSection { path });
            }
            if old.join("\n").is_empty() {
                return Err(StrExtractError::EmptyOld { path });
            }
            // Collect new until the next header or EOF (may be empty =
            // replace-with-nothing; blanks preserved verbatim).
            let mut new: Vec<&str> = Vec::new();
            while i < n {
                let l = lines[i];
                let lt = l.trim();
                if lt.starts_with("[edit ") && lt.ends_with(']') {
                    break;
                }
                if lt.starts_with('[') && lt.ends_with(']') && parse_header(lt).is_none() {
                    return Err(StrExtractError::BadHeader {
                        line: i + 1,
                        text: lt.to_string(),
                    });
                }
                new.push(l);
                i += 1;
            }
            // Trailing blank lines before the next header/EOF are patch
            // formatting, not file content — but interior blanks stay.
            // Keep verbatim EXCEPT strip at most the formatting tail? No:
            // byte-exactness demands verbatim. Models anchor on content;
            // a trailing newline in `new` is a real empty line. Keep all.
            if sections.iter().any(|s: &StrSection| s.path == path) {
                return Err(StrExtractError::DuplicateFile { path });
            }
            sections.push(StrSection {
                path,
                old: old.join("\n"),
                new: new.join("\n"),
            });
        } else {
            return Err(StrExtractError::ProseOutside { line: i + 1 });
        }
    }
    if sections.is_empty() {
        return Err(StrExtractError::ProseOnly);
    }
    Ok(sections)
}

/// Parse `[edit <path>]`: `Some(path)` or `None` (malformed).
fn parse_header(t: &str) -> Option<String> {
    let inner = t.strip_prefix("[edit ")?.strip_suffix(']')?;
    let path = inner.trim().to_string();
    if path.is_empty() {
        return None;
    }
    Some(path)
}

/// Pull str_replace sections out of raw model output: fence-unwrap, then
/// parse. Fence faults and section faults are both named.
pub fn extract_strreplace(text: &str) -> Result<Vec<StrSection>, StrExtractError> {
    let inner = unwrap_fence(text)?;
    // Fast path: no header at all => prose (avoids line-number noise).
    if !inner.contains("[edit ") {
        // A bare `[...]` line that is not `[edit ...]` still names its
        // shape fault rather than hiding as prose.
        for (idx, line) in inner.lines().enumerate() {
            let t = line.trim();
            if t.starts_with('[') && t.ends_with(']') && !t.trim().is_empty() {
                return Err(StrExtractError::BadHeader {
                    line: idx + 1,
                    text: t.to_string(),
                });
            }
        }
        return Err(StrExtractError::ProseOnly);
    }
    parse_sections(&inner)
}

/// Reject workspace escapes: absolute paths or any `..` component.
fn check_path(path: &str) -> Result<(), StrApplyError> {
    let p = Path::new(path);
    if p.is_absolute()
        || p.components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(StrApplyError::Traversal {
            path: path.to_string(),
        });
    }
    Ok(())
}

/// Apply parsed sections to the workspace at `root`: validate WHOLE patch
/// first (read + count every file), then write all. Never half-applies.
pub fn apply_strreplace_to_disk(sections: &[StrSection], root: &Path) -> Result<(), StrApplyError> {
    // Phase 1: read + check everything.
    let mut planned: Vec<(String, String)> = Vec::new();
    for s in sections {
        check_path(&s.path)?;
        if sections.iter().filter(|o| o.path == s.path).count() > 1 {
            return Err(StrApplyError::Io {
                path: s.path.clone(),
                detail: "duplicate section (extractor must refuse first)".to_string(),
            });
        }
        let disk = root.join(&s.path);
        let content = std::fs::read_to_string(&disk).map_err(|e| {
            if disk.exists() {
                StrApplyError::Io {
                    path: s.path.clone(),
                    detail: e.to_string(),
                }
            } else {
                StrApplyError::MissingFile {
                    path: s.path.clone(),
                }
            }
        })?;
        let count = content.match_indices(&s.old).count();
        if count == 0 {
            return Err(StrApplyError::MissingOld {
                path: s.path.clone(),
            });
        }
        if count > 1 {
            return Err(StrApplyError::AmbiguousOld {
                path: s.path.clone(),
                count,
            });
        }
        planned.push((s.path.clone(), content.replacen(&s.old, &s.new, 1)));
    }
    // Phase 2: write all (all checks passed — nothing validated can fail
    // except IO, which writes complete files only).
    for (path, next) in planned {
        let disk = root.join(&path);
        if let Some(parent) = disk.parent() {
            std::fs::create_dir_all(parent).map_err(|e| StrApplyError::Io {
                path: path.clone(),
                detail: e.to_string(),
            })?;
        }
        std::fs::write(&disk, next).map_err(|e| StrApplyError::Io {
            path: path.clone(),
            detail: e.to_string(),
        })?;
    }
    Ok(())
}

/// Score one case with the str_replace arm inside `root` (caller-owned
/// dir; same lifetime contract as `score_case`). Setup + compare ride the
/// shared helpers — extract+apply are the only arm-specific code.
pub fn score_str_case(case: &EvalCase, root: &Path) -> CaseReport {
    let fail = |o| CaseReport {
        id: case.id.clone(),
        outcome: o,
    };
    if let Err(e) = write_initial_files(root, &case.files) {
        return fail(CaseOutcome::ApplyFail(e));
    }
    let sections = match extract_strreplace(&case.model_output) {
        Ok(s) => s,
        Err(e) => return fail(CaseOutcome::ExtractFail(e.to_string())),
    };
    if let Err(e) = apply_strreplace_to_disk(&sections, root) {
        return fail(CaseOutcome::ApplyFail(e.to_string()));
    }
    match check_workspace(root, &case.expected) {
        Ok(()) => fail(CaseOutcome::Pass),
        Err(m) => fail(CaseOutcome::Mismatch(m)),
    }
}

/// Render the user prompt for a str_replace eval turn (chunk 28): the
/// instruction plus each workspace file WITHOUT tags (tags are meaningless
/// without snapshots — showing them would leak hashline concepts into the
/// baseline, design §1). Shape mirrors `render_eval_prompt`.
///
/// ```text
/// TASK: <instruction>
///
/// WORKSPACE (plain files, no tags — cite paths as `[edit PATH]`):
/// --- <name> ---
/// <content>
/// ---
/// ```
pub fn render_str_prompt(files: &[(String, String)], instruction: &str) -> String {
    let mut out = String::from("TASK:\n");
    out.push_str(instruction);
    out.push_str("\n\nWORKSPACE (plain files, no tags — cite paths as `[edit PATH]`):\n");
    for (name, content) in files {
        out.push_str(&format!("--- {name} ---\n{content}"));
        if !content.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("---\n");
    }
    out.push_str("Emit ONE str_replace patch per the system prompt.\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn str_case(
        id: &str,
        files: Vec<(&str, &str)>,
        expected: Vec<(&str, &str)>,
        output: &str,
    ) -> EvalCase {
        EvalCase {
            id: id.to_string(),
            files: files
                .into_iter()
                .map(|(n, c)| (n.to_string(), c.to_string()))
                .collect(),
            expected: expected
                .into_iter()
                .map(|(n, c)| (n.to_string(), c.to_string()))
                .collect(),
            model_output: output.to_string(),
        }
    }

    // Spike case 1 (permanent): anchored replace, byte-exact.
    #[test]
    fn spike_replace_is_byte_exact() {
        let out = "[edit a.txt]\n<<<\nb\n>>>\nB\n";
        let sections = extract_strreplace(out).unwrap();
        assert_eq!(
            sections,
            vec![StrSection {
                path: "a.txt".to_string(),
                old: "b".to_string(),
                new: "B".to_string(),
            }]
        );
        let d = tempfile::tempdir().unwrap();
        let rep = score_str_case(
            &str_case(
                "spike-replace",
                vec![("a.txt", "a\nb\nc\n")],
                vec![("a.txt", "a\nB\nc\n")],
                out,
            ),
            d.path(),
        );
        assert_eq!(rep.outcome, CaseOutcome::Pass, "{rep:?}");
    }

    // Spike case 2 (permanent): insert via trailing-context anchor.
    #[test]
    fn spike_anchored_insert_is_byte_exact() {
        let out = "[edit a.txt]\n<<<\nb\nc\n>>>\nb\nc\nbottom\n";
        let d = tempfile::tempdir().unwrap();
        let rep = score_str_case(
            &str_case(
                "spike-insert",
                vec![("a.txt", "a\nb\nc\n")],
                vec![("a.txt", "a\nb\nc\nbottom\n")],
                out,
            ),
            d.path(),
        );
        assert_eq!(rep.outcome, CaseOutcome::Pass, "{rep:?}");
    }

    #[test]
    fn multi_file_two_sections_pass() {
        let out = "[edit h1.txt]\n<<<\nx\n>>>\nX\n[edit h2.txt]\n<<<\ny\n>>>\nY\n";
        let d = tempfile::tempdir().unwrap();
        let rep = score_str_case(
            &str_case(
                "multi",
                vec![("h1.txt", "x\n"), ("h2.txt", "y\n")],
                vec![("h1.txt", "X\n"), ("h2.txt", "Y\n")],
                out,
            ),
            d.path(),
        );
        assert_eq!(rep.outcome, CaseOutcome::Pass, "{rep:?}");
    }

    #[test]
    fn single_fenced_block_parses() {
        let out = "Here is the patch:\n```text\n[edit a.txt]\n<<<\nb\n>>>\nB\n```\nDone.\n";
        let d = tempfile::tempdir().unwrap();
        let rep = score_str_case(
            &str_case(
                "fenced",
                vec![("a.txt", "a\nb\nc\n")],
                vec![("a.txt", "a\nB\nc\n")],
                out,
            ),
            d.path(),
        );
        assert_eq!(rep.outcome, CaseOutcome::Pass, "{rep:?}");
    }

    #[test]
    fn missing_old_names_the_file() {
        let d = tempfile::tempdir().unwrap();
        let rep = score_str_case(
            &str_case(
                "missing",
                vec![("a.txt", "a\nb\nc\n")],
                vec![("a.txt", "a\nB\nc\n")],
                "[edit a.txt]\n<<<\nzzz\n>>>\nB\n",
            ),
            d.path(),
        );
        match rep.outcome {
            CaseOutcome::ApplyFail(msg) => {
                assert!(msg.contains("0 times") && msg.contains("a.txt"), "{msg}")
            }
            other => panic!("want ApplyFail, got {other:?}"),
        }
    }

    #[test]
    fn ambiguous_old_names_the_count() {
        let d = tempfile::tempdir().unwrap();
        let rep = score_str_case(
            &str_case(
                "ambiguous",
                vec![("a.txt", "x\nx\n")],
                vec![("a.txt", "y\nx\n")],
                "[edit a.txt]\n<<<\nx\n>>>\ny\n",
            ),
            d.path(),
        );
        match rep.outcome {
            CaseOutcome::ApplyFail(msg) => assert!(msg.contains("2 times"), "{msg}"),
            other => panic!("want ApplyFail, got {other:?}"),
        }
    }

    #[test]
    fn empty_old_refuses() {
        assert!(matches!(
            extract_strreplace("[edit a.txt]\n<<<\n>>>\nB\n"),
            Err(StrExtractError::EmptyOld { .. })
        ));
        let d = tempfile::tempdir().unwrap();
        let rep = score_str_case(
            &str_case(
                "empty",
                vec![("a.txt", "a\n")],
                vec![("a.txt", "B\n")],
                "[edit a.txt]\n<<<\n>>>\nB\n",
            ),
            d.path(),
        );
        assert!(
            matches!(rep.outcome, CaseOutcome::ExtractFail(_)),
            "{rep:?}"
        );
    }

    #[test]
    fn unclosed_section_refuses() {
        assert!(matches!(
            extract_strreplace("[edit a.txt]\n<<<\nb\n"),
            Err(StrExtractError::UnclosedSection { .. })
        ));
    }

    #[test]
    fn prose_outside_refuses() {
        assert!(matches!(
            extract_strreplace("I changed it:\n[edit a.txt]\n<<<\nb\n>>>\nB\n"),
            Err(StrExtractError::ProseOutside { .. })
        ));
        assert_eq!(
            extract_strreplace("just some prose"),
            Err(StrExtractError::ProseOnly)
        );
    }

    #[test]
    fn fence_faults_mirror_hashline() {
        assert_eq!(
            extract_strreplace("```\n[edit a.txt]\n<<<\nb\n>>>\nB\n"),
            Err(StrExtractError::UnclosedFence)
        );
        assert!(matches!(
            extract_strreplace(
                "```\n[edit a.txt]\n<<<\na\n>>>\nb\n```\n```\n[edit a.txt]\n<<<\na\n>>>\nc\n```"
            ),
            Err(StrExtractError::MultipleBlocks { count: 2 })
        ));
    }

    #[test]
    fn duplicate_file_and_traversal_refuse() {
        assert!(matches!(
            extract_strreplace("[edit a.txt]\n<<<\na\n>>>\nb\n[edit a.txt]\n<<<\na\n>>>\nc\n"),
            Err(StrExtractError::DuplicateFile { .. })
        ));
        let d = tempfile::tempdir().unwrap();
        let rep = score_str_case(
            &str_case(
                "traversal",
                vec![("a.txt", "a\n")],
                vec![("a.txt", "b\n")],
                "[edit ../evil.txt]\n<<<\na\n>>>\nb\n",
            ),
            d.path(),
        );
        match rep.outcome {
            CaseOutcome::ApplyFail(msg) => assert!(msg.contains("escapes"), "{msg}"),
            other => panic!("want ApplyFail, got {other:?}"),
        }
    }

    #[test]
    fn failed_patch_leaves_files_untouched() {
        // Atomicity parity: two sections, second stale => first untouched.
        let d = tempfile::tempdir().unwrap();
        let rep = score_str_case(
            &str_case(
                "atomic",
                vec![("h1.txt", "x\n"), ("h2.txt", "y\n")],
                vec![("h1.txt", "X\n"), ("h2.txt", "Y\n")],
                "[edit h1.txt]\n<<<\nx\n>>>\nX\n[edit h2.txt]\n<<<\nzzz\n>>>\nY\n",
            ),
            d.path(),
        );
        assert!(matches!(rep.outcome, CaseOutcome::ApplyFail(_)), "{rep:?}");
        assert_eq!(std::fs::read(d.path().join("h1.txt")).unwrap(), b"x\n");
        assert_eq!(std::fs::read(d.path().join("h2.txt")).unwrap(), b"y\n");
    }

    #[test]
    fn wrong_content_scores_mismatch() {
        let d = tempfile::tempdir().unwrap();
        let rep = score_str_case(
            &str_case(
                "wrong",
                vec![("a.txt", "a\nb\nc\n")],
                vec![("a.txt", "something-else\n")],
                "[edit a.txt]\n<<<\nb\n>>>\nB\n",
            ),
            d.path(),
        );
        assert!(matches!(rep.outcome, CaseOutcome::Mismatch(_)), "{rep:?}");
    }

    #[test]
    fn str_prompt_is_untagged_with_close() {
        let p = render_str_prompt(
            &[("a.txt".to_string(), "hi\n".to_string())],
            "Do the thing.",
        );
        assert!(p.contains("TASK:\nDo the thing."), "{p}");
        assert!(p.contains("--- a.txt ---\n"), "{p}");
        assert!(!p.contains('#'), "{p}");
        assert!(
            p.ends_with("Emit ONE str_replace patch per the system prompt.\n"),
            "{p}"
        );
    }
}
