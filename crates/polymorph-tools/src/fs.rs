//! File tools (Epic B3): read / write / grep / glob.
//!
//! - `read_text`: paginated line reads with truncation flags; NUL-byte sniff
//!   refuses binary files (the model must use `bash` + a hex tool for those).
//! - `write_text`: creates parent dirs; single atomic-ish write via
//!   `std::fs::write` (crash-consistency of multi-block edits is B4b, not here).
//! - `grep_files`: ripgrep internals as libraries (`grep-regex` matcher +
//!   `grep-searcher` driver) over an `ignore` gitignore-aware walker.
//! - `glob_files`: `globset` matcher over the same walker.
//!
//! Done-criteria trace (v2 B3): property test asserts grep skips exactly the
//! files a `.gitignore` excludes — see `grep_respects_gitignore` below.
//!
//! Harness state (docket #8) never surfaces in a scan: every walker here
//! skips `.polymorph/` directories and drive event ledgers
//! ([`is_harness_state`]) plus any caller-named paths (`*_hiding`
//! variants), so a later fixer cannot discover a failed drive's ledger by
//! `glob`/`grep`. A `read` of an exact path is not filtered (naming a file
//! is not discovery).

use std::path::{Path, PathBuf};

/// Directory name of the project-local harness state (config, logs,
/// sessions, archived failed attempts) — never workspace content.
pub const HARNESS_DIR: &str = ".polymorph";

/// File-name suffix of drive event ledgers (`review drive --events-json`
/// convention, trap #57). Only files whose head also looks like a ledger
/// are hidden, so a project's own `*-events.json` fixture stays visible.
pub const EVENTS_LEDGER_SUFFIX: &str = "-events.json";

/// `type` values a drive ledger's events carry (the `events_json_value`
/// shape in the CLI).
const LEDGER_EVENT_TYPES: [&str; 8] = [
    "round_started",
    "verdict",
    "finding",
    "diff",
    "plan",
    "budget",
    "halted",
    "finished",
];

/// Whether a walked entry is harness state no fixer scan may surface
/// (docket #8): a `.polymorph` directory, or a `*-events.json` file whose
/// first 4 KiB is a JSON array of drive events. Unreadable files are not
/// hidden (fail-open to the pre-#8 behavior, never to data loss).
pub fn is_harness_state(path: &Path, is_dir: bool) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if is_dir {
        return name == HARNESS_DIR;
    }
    if !name.to_ascii_lowercase().ends_with(EVENTS_LEDGER_SUFFIX) {
        return false;
    }
    use std::io::Read;
    let mut head = Vec::with_capacity(4096);
    let read = std::fs::File::open(path).and_then(|f| f.take(4096).read_to_end(&mut head));
    if read.is_err() {
        return false;
    }
    let head = String::from_utf8_lossy(&head);
    head.trim_start().starts_with('[')
        && LEDGER_EVENT_TYPES.iter().any(|t| {
            head.contains(&format!("\"type\": \"{t}\""))
                || head.contains(&format!("\"type\":\"{t}\""))
        })
}

/// Identity of a caller-hidden path: same file name, then same file (by
/// canonical path when both exist, else lexically).
fn same_file(entry: &Path, hidden: &Path) -> bool {
    let names_match = match (entry.file_name(), hidden.file_name()) {
        (Some(a), Some(b)) if cfg!(windows) => a.eq_ignore_ascii_case(b),
        (Some(a), Some(b)) => a == b,
        _ => false,
    };
    if !names_match {
        return false;
    }
    match (std::fs::canonicalize(entry), std::fs::canonicalize(hidden)) {
        (Ok(a), Ok(b)) => a == b,
        _ => entry == hidden,
    }
}

/// The gitignore-aware walker every scan shares, with the harness-state
/// filter (docket #8) and `hidden` paths pruned. The walk root itself is
/// never filtered: an explicit `dir: .polymorph` names it.
fn scan_walker(dir: &Path, hidden: &[PathBuf]) -> ignore::Walk {
    let hidden = hidden.to_vec();
    // A walk rooted inside the harness dir (docket #9: `grep` over a
    // `.polymorph/tool-output/` spill file) is an exact address: ignore
    // rules do not apply there (that dir's own `*` `.gitignore`, or a
    // project's `*.log` rule, would otherwise hide every spill file).
    let in_harness = dir.components().any(|c| c.as_os_str() == HARNESS_DIR);
    // require_git(false): agent tools walk arbitrary dirs, not just repos.
    // With the default (true), gitignore rules are silently skipped outside
    // a git repo — exactly the trap the gitignore test below guards.
    ignore::WalkBuilder::new(dir)
        .standard_filters(!in_harness)
        .hidden(true)
        .require_git(false)
        .filter_entry(move |e| {
            let is_dir = e.file_type().is_some_and(|t| t.is_dir());
            !is_harness_state(e.path(), is_dir) && !hidden.iter().any(|h| same_file(e.path(), h))
        })
        .build()
}

/// One page of a text file.
#[derive(Debug)]
pub struct ReadPage {
    pub lines: Vec<String>,
    /// 1-based number of the first returned line.
    pub start_line: usize,
    pub total_lines: usize,
    pub truncated_before: bool,
    pub truncated_after: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum FsError {
    #[error("I/O error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "refused directory {path} (directories cannot be read as text files; use glob instead)"
    )]
    Directory { path: PathBuf },
    #[error("refused binary file {path} (NUL byte in first 8 KiB)")]
    Binary { path: PathBuf },
    #[error("invalid regex {pattern}: {detail}")]
    BadPattern { pattern: String, detail: String },
    #[error("walk error under {dir}: {detail}")]
    Walk { dir: PathBuf, detail: String },
}

fn io_err(path: &Path, source: std::io::Error) -> FsError {
    FsError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Read `path` as UTF-8 (lossy), lines `offset..offset+limit` (1-based offset).
/// `limit == 0` means "to end of file".
pub fn read_text(path: &Path, offset: usize, limit: usize) -> Result<ReadPage, FsError> {
    if path.is_dir() {
        return Err(FsError::Directory {
            path: path.to_path_buf(),
        });
    }
    let bytes = std::fs::read(path).map_err(|e| io_err(path, e))?;
    if bytes.iter().take(8192).any(|&b| b == 0) {
        return Err(FsError::Binary {
            path: path.to_path_buf(),
        });
    }
    let text = String::from_utf8_lossy(&bytes);
    let all: Vec<&str> = text.lines().collect();
    let total = all.len();
    let start = offset.max(1).min(total.saturating_add(1));
    let end = if limit == 0 {
        total + 1
    } else {
        (start + limit).min(total + 1)
    };
    Ok(ReadPage {
        lines: all[start - 1..end - 1]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        start_line: start,
        total_lines: total,
        truncated_before: start > 1,
        truncated_after: end <= total,
    })
}

/// Write `content` to `path`, creating parent directories as needed.
pub fn write_text(path: &Path, content: &str) -> Result<(), FsError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| io_err(path, e))?;
        }
    }
    std::fs::write(path, content).map_err(|e| io_err(path, e))
}

/// One grep hit.
#[derive(Debug, PartialEq)]
pub struct GrepHit {
    pub path: PathBuf,
    /// 1-based; 0 when the searcher reports none (should not happen with
    /// line numbers enabled — treated as data, asserted in tests).
    pub line_number: u64,
    pub line: String,
}

/// Regex search over `dir` (recursive, gitignore-aware). Caps at
/// `max_hits` hits; `capped: true` tells the caller results are partial.
pub fn grep_files(
    pattern: &str,
    dir: &Path,
    max_hits: usize,
) -> Result<(Vec<GrepHit>, bool), FsError> {
    grep_files_hiding(pattern, dir, max_hits, &[])
}

/// [`grep_files`] that also never searches the `hidden` files (docket #8:
/// a drive's own `--events-json` path, whatever its name).
pub fn grep_files_hiding(
    pattern: &str,
    dir: &Path,
    max_hits: usize,
    hidden: &[PathBuf],
) -> Result<(Vec<GrepHit>, bool), FsError> {
    use grep_regex::RegexMatcher;
    use grep_searcher::{SearcherBuilder, Sink, SinkMatch};

    let matcher = RegexMatcher::new(pattern).map_err(|e| FsError::BadPattern {
        pattern: pattern.into(),
        detail: e.to_string(),
    })?;
    let mut searcher = SearcherBuilder::new()
        .line_number(true)
        // Binary files yield zero matches (Quit on first NUL) instead of
        // lossy-decoded garbage — same contract as `read_text`'s refusal.
        .binary_detection(grep_searcher::BinaryDetection::quit(b'\x00'))
        .build();

    struct Collector {
        hits: Vec<GrepHit>,
        path: PathBuf,
        cap: usize,
    }
    impl Sink for Collector {
        type Error = std::io::Error;
        fn matched(
            &mut self,
            _searcher: &grep_searcher::Searcher,
            mat: &SinkMatch<'_>,
        ) -> Result<bool, Self::Error> {
            if self.hits.len() >= self.cap {
                return Ok(false); // stop this file
            }
            self.hits.push(GrepHit {
                path: self.path.clone(),
                line_number: mat.line_number().unwrap_or(0),
                line: String::from_utf8_lossy(mat.bytes()).into_owned(),
            });
            Ok(self.hits.len() < self.cap)
        }
    }

    let mut hits = Vec::new();
    for entry in scan_walker(dir, hidden) {
        let entry = entry.map_err(|e| FsError::Walk {
            dir: dir.to_path_buf(),
            detail: e.to_string(),
        })?;
        if hits.len() >= max_hits {
            return Ok((hits, true));
        }
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        // Binary files are skipped by the searcher's Quit(NUL) detection
        // above — no explicit sniff needed here.
        let mut sink = Collector {
            hits: Vec::new(),
            path: entry.path().to_path_buf(),
            cap: max_hits - hits.len(),
        };
        // Unreadable files are skipped, not fatal (agent tools must be robust
        // to permission noise mid-walk).
        let _ = searcher.search_path(&matcher, entry.path(), &mut sink);
        hits.append(&mut sink.hits);
    }
    Ok((hits, false))
}

/// Glob `pattern` (e.g. `**/*.rs`) over `dir`, gitignore-aware.
pub fn glob_files(pattern: &str, dir: &Path) -> Result<Vec<PathBuf>, FsError> {
    glob_files_hiding(pattern, dir, &[])
}

/// [`glob_files`] that also never lists the `hidden` files (docket #8).
pub fn glob_files_hiding(
    pattern: &str,
    dir: &Path,
    hidden: &[PathBuf],
) -> Result<Vec<PathBuf>, FsError> {
    let glob = globset::Glob::new(pattern).map_err(|e| FsError::BadPattern {
        pattern: pattern.into(),
        detail: e.to_string(),
    })?;
    let matcher = glob.compile_matcher();
    let mut out = Vec::new();
    for entry in scan_walker(dir, hidden) {
        let entry = entry.map_err(|e| FsError::Walk {
            dir: dir.to_path_buf(),
            detail: e.to_string(),
        })?;
        if matcher.is_match(entry.path()) {
            out.push(entry.path().to_path_buf());
        }
    }
    out.sort();
    Ok(out)
}

/// Glob `pattern` against paths RELATIVE to `dir` (docket #5 artifact
/// sets: `src/*.rs` names `dir/src/a.rs`, which [`glob_files`] — matching
/// whole walked paths — never does). Same `globset` matcher and
/// gitignore-aware walker; regular files only, as `/`-separated relative
/// spellings, sorted.
pub fn glob_relative(pattern: &str, dir: &Path) -> Result<Vec<String>, FsError> {
    let glob = globset::Glob::new(pattern).map_err(|e| FsError::BadPattern {
        pattern: pattern.into(),
        detail: e.to_string(),
    })?;
    let matcher = glob.compile_matcher();
    let mut out = Vec::new();
    for entry in scan_walker(dir, &[]) {
        let entry = entry.map_err(|e| FsError::Walk {
            dir: dir.to_path_buf(),
            detail: e.to_string(),
        })?;
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(dir) else {
            continue;
        };
        let rel = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        if matcher.is_match(&rel) {
            out.push(rel);
        }
    }
    out.sort();
    Ok(out)
}

/// Cheap content identity of one file for change detection: (length,
/// mtime). Equal stamps read as unchanged — a rewrite that keeps both the
/// size and the mtime (coarse-mtime filesystems, `touch -r`) is invisible.
pub type FileStamp = (u64, Option<std::time::SystemTime>);

/// Stamped view of a tree (command change detection): every regular file
/// and every directory under the root, keyed by `/`-separated root-relative
/// spelling.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TreeStamps {
    pub files: std::collections::HashMap<String, FileStamp>,
    pub dirs: std::collections::HashSet<String>,
}

/// Version-control metadata directories a stamp walk never enters.
const VCS_DIRS: [&str; 4] = [".git", ".hg", ".svn", ".jj"];

/// Stamp every file under `dir` the way a workspace scan sees it:
/// gitignore-aware (ignored build output is not workspace content), harness
/// state and `hidden` paths pruned, VCS metadata skipped, dotfiles INCLUDED
/// (a command that edits `.env` or `.github/` changed the workspace too),
/// symlinks not followed (a symlink is neither stamped nor walked through).
/// Metadata only — no file is opened. `None` once more than `max_files`
/// files are seen (the walk stops there, so the cost stays bounded).
/// Unreadable entries are skipped (identically on every walk).
pub fn stamp_tree(dir: &Path, hidden: &[PathBuf], max_files: usize) -> Option<TreeStamps> {
    let hidden = hidden.to_vec();
    let walker = ignore::WalkBuilder::new(dir)
        .hidden(false)
        .require_git(false)
        .filter_entry(move |e| {
            let is_dir = e.file_type().is_some_and(|t| t.is_dir());
            if is_dir && VCS_DIRS.iter().any(|v| e.file_name() == *v) {
                return false;
            }
            !is_harness_state(e.path(), is_dir) && !hidden.iter().any(|h| same_file(e.path(), h))
        })
        .build();
    let mut out = TreeStamps::default();
    for entry in walker.flatten() {
        let Some(kind) = entry.file_type() else {
            continue;
        };
        if !kind.is_file() && !kind.is_dir() {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(dir) else {
            continue;
        };
        if rel.as_os_str().is_empty() {
            continue;
        }
        let rel = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        if kind.is_dir() {
            out.dirs.insert(rel);
            continue;
        }
        if out.files.len() == max_files {
            return None;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        out.files.insert(rel, (meta.len(), meta.modified().ok()));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn fixture() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        let mut f = std::fs::File::create(root.join("src").join("main.rs")).unwrap();
        writeln!(f, "fn main() {{\n    println!(\"needle here\");\n}}").unwrap();
        let mut f = std::fs::File::create(root.join("notes.txt")).unwrap();
        writeln!(f, "line one\nline two needle\nline three").unwrap();
        // gitignored decoy containing the same needle
        let mut f = std::fs::File::create(root.join("ignored.log")).unwrap();
        writeln!(f, "needle in ignored file").unwrap();
        let mut f = std::fs::File::create(root.join(".gitignore")).unwrap();
        writeln!(f, "*.log").unwrap();
        d
    }

    #[test]
    fn write_read_roundtrip_with_pagination() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("sub").join("f.txt");
        write_text(&p, "a\nb\nc\nd").unwrap();
        let page = read_text(&p, 2, 2).unwrap();
        assert_eq!(page.lines, vec!["b", "c"]);
        assert_eq!(page.start_line, 2);
        assert_eq!(page.total_lines, 4);
        assert!(page.truncated_before && page.truncated_after);
        let full = read_text(&p, 1, 0).unwrap();
        assert_eq!(full.lines.len(), 4);
        assert!(!full.truncated_before && !full.truncated_after);
    }

    #[test]
    fn binary_refused() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("b.bin");
        std::fs::write(&p, [0x89, 0x50, 0x4E, 0x47, 0x00, 0x01]).unwrap();
        assert!(matches!(read_text(&p, 1, 0), Err(FsError::Binary { .. })));
    }

    #[test]
    fn directory_refused() {
        let d = tempfile::tempdir().unwrap();
        assert!(matches!(
            read_text(d.path(), 1, 0),
            Err(FsError::Directory { .. })
        ));
    }

    #[test]
    fn grep_respects_gitignore() {
        // B3 done-criteria, unit form: the walker must skip exactly what
        // `.gitignore` excludes — the decoy `ignored.log` holds the needle
        // but must never appear in hits.
        let d = fixture();
        let (hits, capped) = grep_files("needle", d.path(), 100).unwrap();
        assert!(!capped);
        assert_eq!(hits.len(), 2);
        for h in &hits {
            assert_ne!(h.path.extension().and_then(|e| e.to_str()), Some("log"));
            assert!(h.line_number >= 1, "searcher must report line numbers");
        }
    }

    #[test]
    fn grep_skips_binary_files() {
        // NUL-containing file holding the needle text: must yield no hits
        // (Quit(NUL)), never lossy-decoded garbage.
        let d = fixture();
        std::fs::write(d.path().join("blob.bin"), b"needle\x00more").unwrap();
        let (hits, _) = grep_files("needle", d.path(), 100).unwrap();
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn grep_bad_pattern_is_typed_error() {
        let d = fixture();
        assert!(matches!(
            grep_files("([", d.path(), 10),
            Err(FsError::BadPattern { .. })
        ));
    }

    #[test]
    fn grep_hit_cap_reported() {
        let d = fixture();
        let (hits, capped) = grep_files("needle|line|fn", d.path(), 1).unwrap();
        assert!(capped);
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn glob_finds_by_extension() {
        let d = fixture();
        let rs = glob_files("**/*.rs", d.path()).unwrap();
        assert_eq!(rs.len(), 1);
        assert!(rs[0].ends_with(Path::new("src").join("main.rs")));
        let logs = glob_files("**/*.log", d.path()).unwrap();
        assert!(logs.is_empty(), "gitignored files stay invisible to glob");
    }

    #[test]
    fn glob_relative_matches_dir_relative_files_only() {
        let d = fixture();
        assert_eq!(
            glob_relative("src/*.rs", d.path()).unwrap(),
            ["src/main.rs"]
        );
        assert_eq!(glob_relative("*.txt", d.path()).unwrap(), ["notes.txt"]);
        // Directories never match; gitignored files stay invisible.
        assert!(glob_relative("src", d.path()).unwrap().is_empty());
        assert!(glob_relative("*.log", d.path()).unwrap().is_empty());
        assert!(glob_relative("[", d.path()).is_err());
    }

    /// Fixture plus harness state (docket #8): a `.polymorph/` tree, a
    /// drive ledger, and a project fixture that only shares the suffix.
    fn polluted() -> tempfile::TempDir {
        let d = fixture();
        let root = d.path();
        std::fs::create_dir_all(root.join(".polymorph").join("attempts")).unwrap();
        std::fs::write(
            root.join(".polymorph").join("attempts").join("old.rs"),
            "needle from a failed drive",
        )
        .unwrap();
        std::fs::write(
            root.join("unit-events.json"),
            "[\n  {\n    \"max_rounds\": 3,\n    \"round\": 1,\n    \"type\": \"round_started\"\n  },\n  {\n    \"message\": \"needle finding\",\n    \"type\": \"finding\"\n  }\n]",
        )
        .unwrap();
        std::fs::write(
            root.join("user-events.json"),
            "{\"events\": [\"needle fixture\"]}",
        )
        .unwrap();
        d
    }

    #[test]
    fn scans_never_surface_harness_state() {
        let d = polluted();
        let (hits, _) = grep_files("needle", d.path(), 100).unwrap();
        let names: Vec<String> = hits
            .iter()
            .map(|h| h.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        // The project's own `*-events.json` (not a ledger) stays visible.
        assert!(names.contains(&"user-events.json".to_string()), "{names:?}");
        assert!(
            !names.contains(&"unit-events.json".to_string()),
            "{names:?}"
        );
        assert!(!names.contains(&"old.rs".to_string()), "{names:?}");
        let all = glob_files("**/*", d.path()).unwrap();
        assert!(
            all.iter()
                .all(|p| !p.components().any(|c| c.as_os_str() == HARNESS_DIR)),
            "{all:?}"
        );
        assert!(
            !all.iter().any(|p| p.ends_with("unit-events.json")),
            "{all:?}"
        );
        assert!(
            all.iter().any(|p| p.ends_with("user-events.json")),
            "{all:?}"
        );
        let rel = glob_relative("**/*.json", d.path()).unwrap();
        assert_eq!(rel, ["user-events.json"]);
        // An explicit walk root inside the harness dir is a named target,
        // not discovery: it is searched.
        let (inside, _) =
            grep_files("needle", &d.path().join(".polymorph").join("attempts"), 10).unwrap();
        assert_eq!(inside.len(), 1);
    }

    #[test]
    fn hiding_variants_drop_caller_named_paths() {
        let d = polluted();
        std::fs::write(d.path().join("ev.json"), "needle in a custom ledger").unwrap();
        let hidden = vec![d.path().join("ev.json")];
        let (hits, _) = grep_files_hiding("needle", d.path(), 100, &hidden).unwrap();
        assert!(
            hits.iter().all(|h| !h.path.ends_with("ev.json")),
            "{hits:?}"
        );
        let (plain, _) = grep_files("needle", d.path(), 100).unwrap();
        assert!(plain.iter().any(|h| h.path.ends_with("ev.json")));
        let globbed = glob_files_hiding("**/*.json", d.path(), &hidden).unwrap();
        assert!(
            globbed.iter().all(|p| !p.ends_with("ev.json")),
            "{globbed:?}"
        );
    }

    #[test]
    fn harness_state_rule_is_name_and_shape() {
        let d = polluted();
        assert!(is_harness_state(&d.path().join(".polymorph"), true));
        assert!(!is_harness_state(&d.path().join(".polymorph"), false));
        assert!(is_harness_state(&d.path().join("unit-events.json"), false));
        assert!(!is_harness_state(&d.path().join("user-events.json"), false));
        // Missing files are not hidden (fail-open).
        assert!(!is_harness_state(&d.path().join("gone-events.json"), false));
    }

    #[test]
    fn stamp_tree_sees_workspace_files_only_and_caps() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        std::fs::write(root.join(".gitignore"), "out/\n").unwrap();
        std::fs::create_dir_all(root.join("src/deep")).unwrap();
        std::fs::write(root.join("src/deep/a.rs"), "fn a() {}").unwrap();
        std::fs::write(root.join(".env"), "K=V").unwrap();
        std::fs::create_dir_all(root.join("out")).unwrap();
        std::fs::write(root.join("out/gen.bin"), "x").unwrap();
        std::fs::create_dir_all(root.join(".git/objects")).unwrap();
        std::fs::write(root.join(".git/HEAD"), "ref").unwrap();
        std::fs::create_dir_all(root.join(HARNESS_DIR).join("tool-output")).unwrap();
        std::fs::write(root.join(HARNESS_DIR).join("tool-output/x.log"), "l").unwrap();
        std::fs::write(root.join("hidden.json"), "h").unwrap();
        let st = stamp_tree(root, &[root.join("hidden.json")], 100).unwrap();
        let mut files: Vec<&str> = st.files.keys().map(String::as_str).collect();
        files.sort();
        // Dotfiles count; ignored output, VCS metadata, harness state and
        // caller-hidden files never do.
        assert_eq!(files, [".env", ".gitignore", "src/deep/a.rs"]);
        let mut dirs: Vec<&str> = st.dirs.iter().map(String::as_str).collect();
        dirs.sort();
        assert_eq!(dirs, ["src", "src/deep"]);
        assert_eq!(st.files["src/deep/a.rs"].0, 9);
        // One file over the cap (4 unhidden files): no partial answer.
        assert!(stamp_tree(root, &[], 3).is_none());
        assert!(stamp_tree(root, &[], 4).is_some());
    }
}
