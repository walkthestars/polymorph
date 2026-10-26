//! Hashline applier: validate-everything-first, then write (atomic).
//!
//! Layering:
//! - `apply_to_strings` — pure core over in-memory files. All sections are
//!   tag-checked, bounds-checked, and overlap-checked BEFORE any result is
//!   built, so a bad hunk anywhere means zero changes everywhere.
//! - `apply_to_disk` — reads every section file, runs the pure core, then
//!   writes via temp-file + rename (on-disk variant). A missing file, a
//!   stale tag, or a second-section failure leaves every file untouched.
//! - Line numbers always refer to the ORIGINAL file state (stable addressing).
//!
//! Crash-consistency contract (unit form — true kill -9 fuzzing is not
//! covered here): single `fs::write` would suffice for one file, but temp +
//! rename
//! makes even multi-file patches atomic per file and never leaves a torn
//! target: the target path is only ever replaced by a whole rename, and temp
//! files are removed on any failure (asserted by test).

use super::{FileSection, HashlineError, Op, Patch, SnapshotStore};
use std::collections::{BTreeMap, HashMap};
use std::path::{Component, Path, PathBuf};

/// What happened to one file.
#[derive(Debug, Clone, PartialEq)]
pub enum FileAction {
    Edited { noop: bool },
    Deleted,
    Renamed { to: String },
}

/// Per-file result of the pure core. `new_text` is LF-normalized; `None` for
/// Deleted (no content to write).
#[derive(Debug, Clone)]
pub struct ApplyFileResult {
    pub action: FileAction,
    pub new_text: Option<String>,
}

/// Pure core: validate + apply every section against in-memory files.
/// `files` maps section-path string → normalized content (`Normalized.text`:
/// LF, no trailing terminator, BOM stripped). Disk reads must go through
/// `normalize()` so tags, line numbers, and content all share one model.
pub fn apply_to_strings(
    patch: &Patch,
    files: &BTreeMap<String, String>,
    store: &SnapshotStore,
) -> Result<BTreeMap<String, ApplyFileResult>, HashlineError> {
    // Duplicate sections for one path would apply sequentially against
    // shifting state — reject (stable addressing is per-patch, not per-section).
    let mut seen = std::collections::HashSet::new();
    for sec in &patch.sections {
        if !seen.insert(sec.path.as_str()) {
            return Err(HashlineError::Overlap {
                path: sec.path.clone(),
                msg: "duplicate section for one path in a single patch".into(),
            });
        }
    }
    let mut registers: HashMap<String, Vec<String>> = HashMap::new();
    let mut out = BTreeMap::new();
    for sec in &patch.sections {
        let current = files.get(&sec.path).ok_or_else(|| HashlineError::Io {
            path: sec.path.clone(),
            detail: "file not present in input set (create it with write first)".into(),
        })?;
        let (new_text, action) = apply_section_text(sec, current, &mut registers, store)?;
        let new_opt = match &action {
            FileAction::Deleted => None,
            _ => Some(new_text),
        };
        out.insert(
            sec.path.clone(),
            ApplyFileResult {
                action,
                new_text: new_opt,
            },
        );
    }
    Ok(out)
}

/// Pure per-section apply over normalized text: LF, no trailing terminator,
/// BOM stripped (i.e. `Normalized.text` — disk reads must go through
/// `normalize()`; the terminator is restored by `denormalize()` on write).
/// Tag is verified FIRST (staleness before bounds — mirrors upstream's
/// detect-before-mutate).
fn apply_section_text(
    sec: &FileSection,
    normalized_text: &str,
    registers: &mut HashMap<String, Vec<String>>,
    store: &SnapshotStore,
) -> Result<(String, FileAction), HashlineError> {
    store.verify(Path::new(&sec.path), &sec.tag, normalized_text)?;

    // REM / MV stand alone in their section.
    if sec.ops.iter().any(|op| matches!(op, Op::RemoveFile)) {
        if sec.ops.len() != 1 {
            return Err(HashlineError::Overlap {
                path: sec.path.clone(),
                msg: "REM cannot combine with other ops in one section".into(),
            });
        }
        return Ok((normalized_text.to_string(), FileAction::Deleted));
    }
    if let Some(dest) = sec.ops.iter().find_map(|op| match op {
        Op::MoveFile { dest } => Some(dest.clone()),
        _ => None,
    }) {
        if sec.ops.len() != 1 {
            return Err(HashlineError::Overlap {
                path: sec.path.clone(),
                msg: "MV cannot combine with other ops in one section".into(),
            });
        }
        return Ok((
            normalized_text.to_string(),
            FileAction::Renamed { to: dest },
        ));
    }

    let lines: Vec<&str> = split_lines(normalized_text);
    let n = lines.len() as u32;

    // Collect + validate all ops against ORIGINAL numbering.
    let mut ranges: Vec<(u32, u32)> = Vec::new(); // affected [start,end]
                                                  // inserts: point p (0..=n, lines strictly before the point) -> payloads.
                                                  // Register payloads resolve at EMISSION time (walk), not collection time:
                                                  // a CUT earlier in the walk fills the register a later PASTE reads.
                                                  // Resolving at collection time was a real bug (MissingRegister on valid
                                                  // CUT-then-PASTE patches) — pinned by `cut_register_paste_roundtrip`.
    enum InsertPayload {
        Lines(Vec<String>),
        Register(String),
    }
    let mut inserts: BTreeMap<u32, Vec<InsertPayload>> = BTreeMap::new();
    // range start -> RangeKind, preserving patch order for diagnostics
    enum RangeKind {
        Replace(Vec<String>),
        Cut(Option<String>),
    }
    let mut range_ops: BTreeMap<u32, (u32, RangeKind)> = BTreeMap::new();

    for op in &sec.ops {
        match op {
            Op::Replace { start, end, body } => {
                check_bounds(&sec.path, *start, *end, n)?;
                ranges.push((*start, *end));
                if range_ops
                    .insert(*start, (*end, RangeKind::Replace(body.clone())))
                    .is_some()
                {
                    return Err(overlap(&sec.path, "two range ops start on one line"));
                }
            }
            Op::Cut {
                start,
                end,
                register,
            } => {
                check_bounds(&sec.path, *start, *end, n)?;
                ranges.push((*start, *end));
                if range_ops
                    .insert(*start, (*end, RangeKind::Cut(register.clone())))
                    .is_some()
                {
                    return Err(overlap(&sec.path, "two range ops start on one line"));
                }
            }
            Op::InsertBefore { at, body } => {
                if *at == 0 || *at > n + 1 {
                    return Err(HashlineError::OutOfBounds {
                        path: sec.path.clone(),
                        msg: format!("PUT <{at} with {n} lines (want 1..={})", n + 1),
                    });
                }
                inserts
                    .entry(at - 1)
                    .or_default()
                    .push(InsertPayload::Lines(body.clone()));
            }
            Op::InsertAfter { at, body } => {
                if *at == 0 || *at > n {
                    return Err(HashlineError::OutOfBounds {
                        path: sec.path.clone(),
                        msg: format!("PUT >{at} with {n} lines (want 1..={n})"),
                    });
                }
                inserts
                    .entry(*at)
                    .or_default()
                    .push(InsertPayload::Lines(body.clone()));
            }
            Op::Paste { after, register } => {
                if *after == 0 || *after > n {
                    return Err(HashlineError::OutOfBounds {
                        path: sec.path.clone(),
                        msg: format!("PUT >{after} @{register} with {n} lines"),
                    });
                }
                inserts
                    .entry(*after)
                    .or_default()
                    .push(InsertPayload::Register(register.clone()));
            }
            Op::RemoveFile | Op::MoveFile { .. } => unreachable!("handled above"),
        }
    }

    // Overlap: sorted ranges must not intersect; inserts must not fall
    // strictly inside a range (edges are the legal attachment points).
    ranges.sort();
    for w in ranges.windows(2) {
        if w[1].0 <= w[0].1 {
            return Err(overlap(&sec.path, "affected ranges intersect"));
        }
    }
    for p in inserts.keys() {
        for (s, e) in &ranges {
            // Point p (lines strictly before it) is inside [s,e] iff the gap
            // is neither the left edge (p == s-1) nor the right edge (p == e).
            if *p > s - 1 && *p < *e {
                return Err(overlap(
                    &sec.path,
                    "insertion point lies inside a replaced/deleted range",
                ));
            }
        }
    }

    // Single pass over original lines. Inserts attach to gaps BETWEEN
    // original lines: gap p holds lines strictly before the insertion point
    // (InsertBefore(N) -> p=N-1; InsertAfter(N)/Paste -> p=N). Emitting gap p
    // on arrival at line p+1 (and gap n after the loop) keeps every edge —
    // including a range's left edge — exact.
    let emit_gap = |p: u32,
                    new_lines: &mut Vec<String>,
                    registers: &HashMap<String, Vec<String>>|
     -> Result<(), HashlineError> {
        if let Some(payloads) = inserts.get(&p) {
            for payload in payloads {
                match payload {
                    InsertPayload::Lines(body) => new_lines.extend(body.iter().cloned()),
                    InsertPayload::Register(name) => {
                        let content = registers
                            .get(name)
                            .cloned()
                            .ok_or_else(|| HashlineError::MissingRegister(name.clone()))?;
                        new_lines.extend(content);
                    }
                }
            }
        }
        Ok(())
    };
    let mut new_lines: Vec<String> = Vec::new();
    let mut i: u32 = 1;
    while i <= n {
        emit_gap(i - 1, &mut new_lines, registers)?;
        if let Some((end, kind)) = range_ops.remove(&i) {
            match kind {
                RangeKind::Replace(body) => new_lines.extend(body),
                RangeKind::Cut(reg) => {
                    if let Some(name) = reg {
                        registers.insert(
                            name,
                            lines[(i - 1) as usize..end as usize]
                                .iter()
                                .map(|s| s.to_string())
                                .collect(),
                        );
                    }
                }
            }
            i = end + 1;
            continue;
        }
        new_lines.push(lines[(i - 1) as usize].to_string());
        i += 1;
    }
    // Trailing gap: appends (InsertBefore(n+1) / InsertAfter(n)).
    emit_gap(n, &mut new_lines, registers)?;

    let new_text = new_lines.join("\n");
    let noop = new_text == normalized_text;
    Ok((new_text, FileAction::Edited { noop }))
}

fn check_bounds(path: &str, start: u32, end: u32, n: u32) -> Result<(), HashlineError> {
    if start == 0 || end == 0 || start > end || end > n {
        return Err(HashlineError::OutOfBounds {
            path: path.into(),
            msg: format!("range {start}.={end} with {n} lines"),
        });
    }
    Ok(())
}

fn overlap(path: &str, msg: &str) -> HashlineError {
    HashlineError::Overlap {
        path: path.into(),
        msg: msg.into(),
    }
}

/// Join `root` + section path with traversal protection: absolute paths,
/// `..` escaping the root, and Windows prefixes are rejected. Lexical only
/// (no FS probing) — safe to call before existence checks. A depth counter
/// (not string prefixes) decides containment, so `root="a"`, `out="ab"`
/// traps are impossible by construction.
pub fn contain(root: &Path, section_path: &str) -> Result<PathBuf, HashlineError> {
    let root_depth = root.components().count();
    let mut out = root.to_path_buf();
    let mut depth = root_depth;
    for comp in Path::new(section_path).components() {
        match comp {
            Component::Normal(c) => {
                out.push(c);
                depth += 1;
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if depth <= root_depth {
                    return Err(HashlineError::Traversal(section_path.into()));
                }
                out.pop();
                depth -= 1;
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(HashlineError::Traversal(section_path.into()));
            }
        }
    }
    // `sub/..` resolves to root itself — a directory, not a file target.
    // (The parser already rejects empty paths; this closes the `..` alias.)
    if depth <= root_depth {
        return Err(HashlineError::Traversal(section_path.into()));
    }
    Ok(out)
}

/// THE line model (shared conceptually by hashline + fs read paths): text is
/// split on `'\n'`, and ONE trailing newline is a terminator, not a line —
/// so `"a\n"` is 1 line and `""` is 0 lines (matching `str::lines`). The
/// terminator is tracked separately and restored on write, so a file that
/// ended with a newline still does after editing (and one that didn't,
/// doesn't). Getting this wrong forks line numbers between read and edit —
/// the exact class of bug hashline exists to kill.
///
/// Byte-level normalization: BOM flag, CRLF flag, LF-normalized text
/// (trailing newline tracked separately — see the line model above).
pub struct Normalized {
    pub bom: bool,
    pub crlf: bool,
    pub trailing_newline: bool,
    pub text: String,
}

pub fn split_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut v: Vec<&str> = text.split('\n').collect();
    if text.ends_with('\n') {
        v.pop();
    }
    v
}

pub fn normalize(bytes: &[u8]) -> Normalized {
    let (bom, rest) = match bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        Some(r) => (true, r),
        None => (false, bytes),
    };
    let text = String::from_utf8_lossy(rest);
    let crlf = text.contains("\r\n");
    let mut lf = text.replace("\r\n", "\n");
    let trailing_newline = lf.ends_with('\n');
    if trailing_newline {
        lf.pop(); // terminator tracked in the flag, not the text
    }
    Normalized {
        bom,
        crlf,
        trailing_newline,
        text: lf,
    }
}

pub fn denormalize(n: &Normalized, new_text: &str) -> Vec<u8> {
    // new_text carries no trailing terminator (see line model); restore it.
    let mut text = new_text.to_string();
    if n.trailing_newline && !text.is_empty() {
        text.push('\n');
    }
    let mut out = Vec::new();
    if n.bom {
        out.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
    }
    if n.crlf {
        out.extend_from_slice(text.replace('\n', "\r\n").as_bytes());
    } else {
        out.extend_from_slice(text.as_bytes());
    }
    out
}

/// Disk report: path → action (content already written).
#[derive(Debug)]
pub struct DiskReport {
    pub files: Vec<(String, FileAction)>,
}

/// Validate every section (reads + pure core), then write all (temp + rename).
/// Any validation failure → zero files touched. Temp files are removed on any
/// write-phase failure.
pub fn apply_to_disk(
    patch: &Patch,
    root: &Path,
    store: &SnapshotStore,
) -> Result<DiskReport, HashlineError> {
    // Phase 1 — validate everything. No writes below this line until every
    // section has produced its new bytes.
    struct Pending {
        section_path: String,
        target: PathBuf,
        action: FileAction,
        new_bytes: Option<Vec<u8>>,
        rename_dest: Option<PathBuf>,
    }
    let mut pending: Vec<Pending> = Vec::new();
    // Pure-core inputs, built per section from disk reads.
    let mut core_inputs: BTreeMap<String, String> = BTreeMap::new();
    let mut styles: HashMap<String, Normalized> = HashMap::new();
    for sec in &patch.sections {
        let target = contain(root, &sec.path)?;
        let bytes = std::fs::read(&target).map_err(|e| HashlineError::Io {
            path: sec.path.clone(),
            detail: format!("read failed (create the file with write first): {e}"),
        })?;
        let norm = normalize(&bytes);
        core_inputs.insert(sec.path.clone(), norm.text.clone());
        // stash full Normalized for the write phase
        styles.insert(sec.path.clone(), norm);
    }
    let results = apply_to_strings(patch, &core_inputs, store)?;
    for sec in &patch.sections {
        let target = contain(root, &sec.path)?;
        let res = &results[&sec.path];
        let norm = &styles[&sec.path];
        match &res.action {
            FileAction::Edited { noop: true } => pending.push(Pending {
                section_path: sec.path.clone(),
                target,
                action: res.action.clone(),
                new_bytes: None,
                rename_dest: None,
            }),
            FileAction::Edited { noop: false } => {
                let bytes = denormalize(norm, res.new_text.as_deref().unwrap_or(""));
                pending.push(Pending {
                    section_path: sec.path.clone(),
                    target,
                    action: res.action.clone(),
                    new_bytes: Some(bytes),
                    rename_dest: None,
                });
            }
            FileAction::Deleted => pending.push(Pending {
                section_path: sec.path.clone(),
                target,
                action: res.action.clone(),
                new_bytes: None,
                rename_dest: None,
            }),
            FileAction::Renamed { to } => {
                let dest = contain(root, to)?;
                if dest.exists() {
                    return Err(HashlineError::Io {
                        path: to.clone(),
                        detail: "MV destination already exists (no overwrite)".into(),
                    });
                }
                // Validate the destination parent NOW (validation phase), so
                // a missing directory fails before any file is written — not
                // mid-write after earlier sections landed.
                if let Some(parent) = dest.parent() {
                    if !parent.exists() {
                        return Err(HashlineError::Io {
                            path: to.clone(),
                            detail: "MV destination parent directory does not exist".into(),
                        });
                    }
                }
                pending.push(Pending {
                    section_path: sec.path.clone(),
                    target,
                    action: res.action.clone(),
                    new_bytes: None,
                    rename_dest: Some(dest),
                });
            }
        }
    }

    // Phase 2 — write everything. Track temps for cleanup.
    let mut tmps: Vec<PathBuf> = Vec::new();
    let mut report = DiskReport { files: Vec::new() };
    let fail = |tmps: &[PathBuf], e: HashlineError| {
        for t in tmps {
            let _ = std::fs::remove_file(t);
        }
        e
    };
    for p in &pending {
        match (&p.action, &p.new_bytes, &p.rename_dest) {
            (FileAction::Edited { noop: true }, _, _) => {
                report
                    .files
                    .push((p.section_path.clone(), p.action.clone()));
            }
            (FileAction::Edited { .. }, Some(bytes), _) => {
                let tmp = p.target.with_extension("polymorph-tmp");
                if let Err(e) = std::fs::write(&tmp, bytes) {
                    return Err(fail(
                        &tmps,
                        HashlineError::Io {
                            path: p.section_path.clone(),
                            detail: format!("temp write failed: {e}"),
                        },
                    ));
                }
                tmps.push(tmp.clone());
                if let Err(e) = std::fs::rename(&tmp, &p.target) {
                    return Err(fail(
                        &tmps,
                        HashlineError::Io {
                            path: p.section_path.clone(),
                            detail: format!("atomic rename failed: {e}"),
                        },
                    ));
                }
                tmps.retain(|t| t != &tmp);
                report
                    .files
                    .push((p.section_path.clone(), p.action.clone()));
            }
            (FileAction::Deleted, _, _) => {
                if let Err(e) = std::fs::remove_file(&p.target) {
                    return Err(fail(
                        &tmps,
                        HashlineError::Io {
                            path: p.section_path.clone(),
                            detail: format!("delete failed: {e}"),
                        },
                    ));
                }
                report
                    .files
                    .push((p.section_path.clone(), p.action.clone()));
            }
            (FileAction::Renamed { .. }, _, Some(dest)) => {
                if let Err(e) = std::fs::rename(&p.target, dest) {
                    return Err(fail(
                        &tmps,
                        HashlineError::Io {
                            path: p.section_path.clone(),
                            detail: format!("rename failed: {e}"),
                        },
                    ));
                }
                report
                    .files
                    .push((p.section_path.clone(), p.action.clone()));
            }
            _ => {
                return Err(fail(
                    &tmps,
                    HashlineError::Io {
                        path: p.section_path.clone(),
                        detail: "internal: pending entry malformed".into(),
                    },
                ));
            }
        }
    }
    Ok(report)
}

/// Parse + apply to disk in one call (the tool-call entry point for the corpus).
pub fn apply_text_to_disk(
    patch_text: &str,
    root: &Path,
    store: &SnapshotStore,
) -> Result<DiskReport, HashlineError> {
    let patch = super::parse(patch_text)?;
    apply_to_disk(&patch, root, store)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with(files: &BTreeMap<String, String>) -> SnapshotStore {
        let mut s = SnapshotStore::new();
        for (p, c) in files {
            s.mint(Path::new(p), c);
        }
        s
    }

    fn files(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(p, c)| (p.to_string(), c.to_string()))
            .collect()
    }

    #[test]
    fn replace_and_insert_original_numbering() {
        // Two hunks in one patch both address ORIGINAL lines (stable
        // addressing): replacing lines 1-2 does not shift the insert
        // anchored at old line 4.
        let input = files(&[("a", "l1\nl2\nl3\nl4")]);
        let mut store = SnapshotStore::new();
        let tag = store.mint(Path::new("a"), "l1\nl2\nl3\nl4");
        let patch =
            super::super::parse(&format!("[a#{tag}]\nPUT 1.=2:\n+X\nPUT >4:\n+Z\n")).unwrap();
        let out = apply_to_strings(&patch, &input, &store).unwrap();
        assert_eq!(out["a"].new_text.as_deref().unwrap(), "X\nl3\nl4\nZ");
    }

    #[test]
    fn insert_at_range_left_edge_survives() {
        // Regression pin: an insert attached to a range's LEFT edge
        // (gap p == s-1) must emit — an earlier draft swallowed it.
        let input = files(&[("a", "1\n2\n3")]);
        let mut store = SnapshotStore::new();
        let tag = store.mint(Path::new("a"), "1\n2\n3");
        let patch =
            super::super::parse(&format!("[a#{tag}]\nPUT 2.=3:\n+X\nPUT <2:\n+Y\n")).unwrap();
        let out = apply_to_strings(&patch, &input, &store).unwrap();
        assert_eq!(out["a"].new_text.as_deref().unwrap(), "1\nY\nX");
    }

    #[test]
    fn stale_tag_rejects_before_bounds() {
        let input = files(&[("a", "l1\nl2")]);
        let patch = super::super::parse("[a#ffff]\nPUT 99:\n+x\n").unwrap();
        let store = store_with(&input);
        // Tag wrong AND line out of bounds: StaleTag must win (detect first).
        assert!(matches!(
            apply_to_strings(&patch, &input, &store),
            Err(HashlineError::StaleTag { .. })
        ));
    }

    #[test]
    fn bounds_checked() {
        let input = files(&[("a", "l1")]);
        let mut store = SnapshotStore::new();
        let tag = store.mint(Path::new("a"), "l1");
        let patch = super::super::parse(&format!("[a#{tag}]\nPUT 2:\n+x\n")).unwrap();
        assert!(matches!(
            apply_to_strings(&patch, &input, &store),
            Err(HashlineError::OutOfBounds { .. })
        ));
    }

    #[test]
    fn overlap_rejected_zero_applied() {
        let input = files(&[("a", "1\n2\n3\n4")]);
        let mut store = SnapshotStore::new();
        let tag = store.mint(Path::new("a"), "1\n2\n3\n4");
        let patch = super::super::parse(&format!("[a#{tag}]\nPUT 1.=2:\n+X\nCUT 2.=3\n")).unwrap();
        let before = input.clone();
        let err = apply_to_strings(&patch, &input, &store).unwrap_err();
        assert!(matches!(err, HashlineError::Overlap { .. }));
        assert_eq!(input, before);
    }

    #[test]
    fn insert_inside_range_rejected() {
        let input = files(&[("a", "1\n2\n3")]);
        let mut store = SnapshotStore::new();
        let tag = store.mint(Path::new("a"), "1\n2\n3");
        let patch =
            super::super::parse(&format!("[a#{tag}]\nPUT 1.=3:\n+X\nPUT <2:\n+Y\n")).unwrap();
        assert!(matches!(
            apply_to_strings(&patch, &input, &store),
            Err(HashlineError::Overlap { .. })
        ));
    }

    #[test]
    fn cut_register_paste_roundtrip() {
        let input = files(&[("a", "keep\nmove1\nmove2\ntail")]);
        let mut store = SnapshotStore::new();
        let tag = store.mint(Path::new("a"), "keep\nmove1\nmove2\ntail");
        let patch =
            super::super::parse(&format!("[a#{tag}]\nCUT 2.=3 @blk\nPUT >4 @blk\n")).unwrap();
        let out = apply_to_strings(&patch, &input, &store).unwrap();
        // CUT removes 2-3, PASTE appends after old line 4 (the tail).
        assert_eq!(
            out["a"].new_text.as_deref().unwrap(),
            "keep\ntail\nmove1\nmove2"
        );
    }

    #[test]
    fn noop_detected_not_written() {
        // Replacing lines with identical content: Ok, noop=true.
        let input = files(&[("a", "same\nlines")]);
        let mut store = SnapshotStore::new();
        let tag = store.mint(Path::new("a"), "same\nlines");
        let patch = super::super::parse(&format!("[a#{tag}]\nPUT 1:\n+same\n")).unwrap();
        let out = apply_to_strings(&patch, &input, &store).unwrap();
        assert!(matches!(out["a"].action, FileAction::Edited { noop: true }));
    }

    #[test]
    fn reapply_is_stale_not_idempotent() {
        // THE key metamorphic property: applying twice must fail the second
        // time (content moved), never silently double-apply.
        let input = files(&[("a", "1\n2")]);
        let mut store = SnapshotStore::new();
        let tag = store.mint(Path::new("a"), "1\n2");
        let patch = super::super::parse(&format!("[a#{tag}]\nPUT 1:\n+X\n")).unwrap();
        let out = apply_to_strings(&patch, &input, &store).unwrap();
        let changed = out["a"].new_text.clone().unwrap();
        assert_eq!(changed, "X\n2");
        // New content, same tag → stale.
        let mut input2 = input.clone();
        input2.insert("a".into(), changed);
        assert!(matches!(
            apply_to_strings(&patch, &input2, &store),
            Err(HashlineError::StaleTag { .. })
        ));
    }

    #[test]
    fn traversal_rejected() {
        let root = Path::new("/ws");
        assert!(contain(root, "../evil.rs").is_err());
        assert!(contain(root, "sub/../../evil.rs").is_err());
        assert!(contain(root, "/absolute.rs").is_err());
        assert!(contain(root, "sub/../ok.rs").is_ok());
        assert!(contain(root, "ok.rs").is_ok());
    }

    #[test]
    fn line_model_pins_terminator_semantics() {
        // `"a\n"` is ONE line (terminator, not content) — matches str::lines
        // and fs::read_text, so read↔edit line numbers can never fork.
        assert_eq!(split_lines(""), Vec::<&str>::new());
        assert_eq!(split_lines("a"), vec!["a"]);
        assert_eq!(split_lines("a\n"), vec!["a"]);
        assert_eq!(split_lines("a\nb\n"), vec!["a", "b"]);
        assert_eq!(split_lines("\n"), vec![""]);
        // normalize/denormalize preserve style + terminator + BOM.
        let n = normalize(b"a\r\n");
        assert!(n.crlf && n.trailing_newline && !n.bom);
        assert_eq!(n.text, "a");
        assert_eq!(denormalize(&n, "b"), b"b\r\n".to_vec());
        // No terminator in -> none out.
        let n = normalize(b"a");
        assert!(!n.trailing_newline);
        assert_eq!(denormalize(&n, "b"), b"b".to_vec());
    }

    #[test]
    fn crlf_and_bom_roundtrip() {
        let bytes = b"\xef\xbb\xbfline1\r\nline2\r\n".to_vec();
        let n = normalize(&bytes);
        assert!(n.bom && n.crlf && n.trailing_newline);
        assert_eq!(n.text, "line1\nline2");
        // new_text carries no terminator (line model); denormalize restores.
        let back = denormalize(&n, "line1\nCHANGED");
        assert_eq!(back, b"\xef\xbb\xbfline1\r\nCHANGED\r\n".to_vec());
    }

    #[test]
    fn disk_preserves_trailing_newline_style() {
        // Terminated stays terminated; unterminated stays unterminated.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("t.txt"), "a\nb\n").unwrap();
        std::fs::write(dir.path().join("u.txt"), "a\nb").unwrap();
        let mut store = SnapshotStore::new();
        let tt = store.mint(Path::new("t.txt"), "a\nb");
        let tu = store.mint(Path::new("u.txt"), "a\nb");
        let text = format!("[t.txt#{tt}]\nPUT 1:\n+X\n[u.txt#{tu}]\nPUT 1:\n+X\n");
        apply_text_to_disk(&text, dir.path(), &store).unwrap();
        assert_eq!(std::fs::read(dir.path().join("t.txt")).unwrap(), b"X\nb\n");
        assert_eq!(std::fs::read(dir.path().join("u.txt")).unwrap(), b"X\nb");
    }

    #[test]
    fn disk_atomicity_second_file_missing() {
        // Two sections, second file absent: validation must fail BEFORE the
        // first file is touched, and no temp files may remain.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "aaa\n").unwrap();
        let mut store = SnapshotStore::new();
        let ta = store.mint(Path::new("a.txt"), "aaa");
        let text = format!("[a.txt#{ta}]\nPUT 1:\n+AAA\n[missing.txt#0000]\nPUT 1:\n+x\n");
        let patch = super::super::parse(&text).unwrap();
        // missing.txt tag can't verify — but the MISSING FILE read fails first
        // either way; what matters: a.txt untouched + no tmps.
        apply_to_disk(&patch, dir.path(), &store).unwrap_err();
        assert_eq!(std::fs::read(dir.path().join("a.txt")).unwrap(), b"aaa\n");
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("polymorph-tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn disk_rem_and_mv() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("old.txt"), "data\n").unwrap();
        let mut store = SnapshotStore::new();
        let tdel = store.mint(Path::new("del.txt"), "x");
        std::fs::write(dir.path().join("del.txt"), "x\n").unwrap();
        let tmv = store.mint(Path::new("old.txt"), "data");
        let text = format!("[del.txt#{tdel}]\nREM\n[old.txt#{tmv}]\nMV new.txt\n");
        let patch = super::super::parse(&text).unwrap();
        let rep = apply_to_disk(&patch, dir.path(), &store).unwrap();
        assert_eq!(rep.files.len(), 2);
        assert!(!dir.path().join("del.txt").exists());
        assert!(!dir.path().join("old.txt").exists());
        assert_eq!(
            std::fs::read(dir.path().join("new.txt")).unwrap(),
            b"data\n"
        );
    }
}
