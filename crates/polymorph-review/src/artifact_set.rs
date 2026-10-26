//! Multi-file and worktree-diff review scopes.
//!
//! `review run|drive` grade one `--artifact-file` by default. An
//! [`ArtifactSet`] grades a changeset instead: declared files
//! (`--artifact-file` first, then each `--artifact-files` path or glob)
//! and, in `--diff` mode, every file the drive workdir changed against a
//! git base. The set is re-read from disk for every review, so files a fix
//! turn creates or changes join it. Rows reuse the attachment
//! renderer: same budgets, row/hunk-boundary cuts, every omission named.
//!
//! Rules:
//! - Git runs as `git <args>` with no shell (`std::process::Command`),
//!   in the workdir, scoped to it (`--relative`; untracked files honor
//!   `.gitignore`). Renames show as a deletion plus an addition.
//! - Pure functions of disk + git state: an unchanged tree renders
//!   byte-identically, so the loop's stall check keeps working.

use std::path::{Path, PathBuf};

use crate::binding::{
    attach_identity, read_attached, render_attachment_rows, text_of, AttachWords, AttachedFile,
    AttachedState, ATTACH_FILE_MAX_ROWS, ATTACH_MAX_FILES, ATTACH_MAX_READ_BYTES,
};

/// Section header of a rendered set; also the marker the review brief
/// keys its in-scope sentence on (text starting with `"{header} ("`).
pub const ARTIFACT_SET_HEADER: &str = "ARTIFACT FILES UNDER REVIEW";

/// Finding file of the harness CREATE seed when no file is declared and
/// the diff is empty (no path to name). Never stat'ed as a file.
pub const DIFF_SET_FINDING_FILE: &str = "(worktree diff)";

/// Manifest lines shown before `(N more not listed)` (diff mode).
const MAX_MANIFEST_LINES: usize = ATTACH_MAX_FILES * 5;

/// What to grade: declared members, plus the worktree diff when
/// `diff_base` is set. An empty `declared` with no diff is never built
/// (the CLI requires one of them).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArtifactSet {
    /// Paths and globs, in operator order (`--artifact-file` first). A
    /// value naming an existing path is a path even if it has glob
    /// characters (`[id].tsx`); otherwise one with `*?[{` is a glob,
    /// matched against workdir-relative paths (gitignore-aware, files
    /// only; `*` also crosses `/`, as in the `glob` tool).
    pub declared: Vec<String>,
    /// Git base for worktree-diff mode (`--diff [<base>]`).
    pub diff_base: Option<String>,
}

/// How a diff-mode member differs from the base.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Modified,
    /// Tracked (staged) but absent at the base.
    Added,
    Deleted,
    /// Untracked and not ignored.
    Untracked,
    /// A submodule pointer change (its own tree is not expanded).
    Submodule,
    /// An untracked nested git repository (not expanded).
    NestedRepo,
}

#[derive(Debug, Clone)]
struct Member {
    id: String,
    /// Spelling shown (declared as given; git members workdir-relative).
    path: String,
    declared: bool,
    glob: bool,
    change: Option<Change>,
    /// Workdir-relative `/` spelling git knows the file by (diff members).
    git_path: Option<String>,
}

/// One rendering of the set (the reviewed artifact text plus the facts
/// the caller needs: membership for peripheral dedupe, the CREATE rule).
#[derive(Debug, Clone)]
pub struct RenderedSet {
    pub text: String,
    /// Globs that matched no file (the CLI refuses these pre-spend).
    pub empty_globs: Vec<String>,
    base: PathBuf,
    ids: Vec<String>,
    /// Declared literal paths absent on disk, in order.
    absent_declared: Vec<String>,
    /// The CREATE rule held for this rendering (see [`Self::create_seed`]).
    create: bool,
    diff_base: Option<String>,
}

impl ArtifactSet {
    /// Read the set from `workdir` and render it. Errors (never partial
    /// text) when diff mode cannot ask git: git missing, not a work tree,
    /// a base that names no commit; or when a glob is malformed.
    pub fn render(&self, workdir: &Path) -> anyhow::Result<RenderedSet> {
        let (members, empty_globs) = self.collect(workdir)?;
        let base_name = self.diff_base.as_deref().unwrap_or("");
        let baseline = format!("its {base_name} version");
        let no_baseline = if self.diff_base.is_some() {
            format!("no {base_name} version to diff")
        } else {
            "no diff base (only --diff mode diffs)".to_string()
        };
        let created = format!("NEW vs {base_name}");
        let words = AttachWords {
            baseline: &baseline,
            no_baseline: &no_baseline,
            created: &created,
            deleted: "ABSENT (not on disk: not yet created, or deleted; no content)",
            now: "current",
        };
        let changed = members.iter().filter(|m| m.change.is_some()).count();
        let declared = members.iter().filter(|m| m.declared || m.glob).count();
        let scope = match &self.diff_base {
            None => "declared".to_string(),
            Some(b) if declared == 0 => format!("worktree diff vs {b}: {changed} changed"),
            Some(b) => format!("declared + worktree diff vs {b}: {changed} changed"),
        };
        let mut text = format!(
            "{ARTIFACT_SET_HEADER} ({} files, {scope}; harness-read from disk; `N | ` prefixes are 1-based file line numbers, not file content — cite findings as path:line):\n",
            members.len()
        );
        if let Some(b) = &self.diff_base {
            text.push_str(&render_manifest(&members, b));
        }
        let files: Vec<AttachedFile> = members
            .iter()
            .enumerate()
            .map(|(i, m)| self.attached(workdir, m, i < ATTACH_MAX_FILES))
            .collect();
        if members.is_empty() && self.diff_base.is_some() {
            text.push_str(&format!(
                "(no files: nothing declared, and the worktree has no changes vs {base_name})\n"
            ));
        } else if members.is_empty() {
            // Every declared value was a glob that now matches nothing
            // (named below; the CLI refuses this before round 1).
            text.push_str("(no files)\n");
        } else {
            text.push_str(&render_attachment_rows(&files, &words));
        }
        for g in &empty_globs {
            text.push_str(&format!("(glob {g:?} matched no file)\n"));
        }
        let absent_declared: Vec<String> = members
            .iter()
            .filter(|m| m.declared && m.change.is_none() && is_absent(workdir, &m.path))
            .map(|m| m.path.clone())
            .collect();
        // CREATE rule: nothing to grade yet — every member is a declared
        // path that does not exist, and the diff (if any) is empty. One
        // glob match, any changed file (a deletion too), or one existing
        // declared file (even empty) is review-first.
        let create = absent_declared.len() == members.len();
        if create {
            text.push_str(&create_status(&absent_declared, self.diff_base.as_deref()));
        }
        Ok(RenderedSet {
            text,
            empty_globs,
            base: workdir.to_path_buf(),
            ids: members.into_iter().map(|m| m.id).collect(),
            absent_declared,
            create,
            diff_base: self.diff_base.clone(),
        })
    }

    /// Members in render order, deduped by [`attach_identity`] (first
    /// spelling kept; a declared file the diff also lists keeps its
    /// declared spelling and gains the change).
    fn collect(&self, workdir: &Path) -> anyhow::Result<(Vec<Member>, Vec<String>)> {
        let mut members: Vec<Member> = Vec::new();
        let mut empty_globs = Vec::new();
        for raw in &self.declared {
            if is_glob(workdir, raw) {
                let hits = polymorph_tools::fs::glob_relative(raw, workdir)
                    .map_err(|e| anyhow::anyhow!("--artifact-files glob {raw:?}: {e}"))?;
                if hits.is_empty() {
                    empty_globs.push(raw.clone());
                }
                for hit in hits {
                    let m = member(workdir, hit, false, true);
                    add(&mut members, m);
                }
            } else {
                let m = member(workdir, raw.clone(), true, false);
                add(&mut members, m);
            }
        }
        if let Some(base) = &self.diff_base {
            for (path, change) in git_changes(workdir, base)? {
                if is_harness_change(workdir, &path) {
                    continue;
                }
                let mut m = member(workdir, path.clone(), false, false);
                m.change = Some(change);
                m.git_path = Some(path);
                if let Some(i) = add(&mut members, m.clone()) {
                    members[i].change = m.change;
                    members[i].git_path = m.git_path;
                }
            }
        }
        Ok((members, empty_globs))
    }

    /// Disk state of one member (`read` false past the file cap: named
    /// only, never read) plus, for large modified files, the base version
    /// the renderer diffs against.
    fn attached(&self, workdir: &Path, m: &Member, read: bool) -> AttachedFile {
        let state = match m.change {
            _ if !read => AttachedState::Skipped("over the file cap"),
            Some(Change::Submodule) => {
                AttachedState::Skipped("submodule: its own changeset is not expanded")
            }
            Some(Change::NestedRepo) => {
                AttachedState::Skipped("nested git repository: not expanded")
            }
            _ => read_attached(Some(workdir), &m.path),
        };
        let baseline = match (m.change, &state, &self.diff_base, &m.git_path) {
            (Some(Change::Added | Change::Untracked), _, _, _) => Some(None),
            (Some(Change::Modified), AttachedState::Text(t), Some(base), Some(gp))
                if t.lines().count() > ATTACH_FILE_MAX_ROWS =>
            {
                git_show(workdir, base, gp).map(Some)
            }
            _ => None,
        };
        AttachedFile {
            path: m.path.clone(),
            state,
            baseline,
            note: None,
        }
    }
}

impl RenderedSet {
    /// Whether `raw` (any spelling) names a member — the executor drops
    /// such files from the peripheral section (never twice).
    pub fn covers(&self, raw: &str) -> bool {
        self.ids.contains(&attach_identity(&self.base, raw))
    }

    /// Harness round-1 finding for a CREATE set (parity): `Some`
    /// exactly when the rendering carries the ARTIFACT SET STATUS note —
    /// every member a missing declared path and no changed file. Names
    /// the first missing declared file, or [`DIFF_SET_FINDING_FILE`] when
    /// nothing is declared (an empty diff). Kind `Start` for a pure
    /// `--diff` set on a clean tree (nothing declared), else `Create`.
    pub fn create_seed(&self) -> Option<crate::RoundSeed> {
        if !self.create {
            return None;
        }
        let file = self
            .absent_declared
            .first()
            .cloned()
            .unwrap_or_else(|| DIFF_SET_FINDING_FILE.to_string());
        let status = create_fact(&self.absent_declared, self.diff_base.as_deref());
        if self.absent_declared.is_empty() && self.diff_base.is_some() {
            // Pure `--diff` on a clean tree: the goal may as well be a
            // change to existing files (live drive: a signature refactor
            // was seeded "files ... must be created") — say implement.
            return Some(crate::RoundSeed {
                kind: crate::SeedKind::Start,
                findings: vec![polymorph_core::Finding {
                severity: polymorph_core::Severity::P1,
                file,
                line: None,
                issue: format!(
                    "Harness-synthesized START finding (no reviewer call): {status} GOAL is not implemented yet: make the changes GOAL and ACCEPTANCE CRITERIA require."
                ),
                proposed_fix: "Implement GOAL: edit the existing files it concerns and create any new files it names, with the contents GOAL and ACCEPTANCE CRITERIA require, then run the verification they name.".to_string(),
                }],
            });
        }
        let targets = if self.absent_declared.is_empty() {
            "the files GOAL names".to_string()
        } else {
            self.absent_declared.join(", ")
        };
        Some(crate::RoundSeed {
            kind: crate::SeedKind::Create,
            findings: vec![polymorph_core::Finding {
            severity: polymorph_core::Severity::P1,
            file,
            line: None,
            issue: format!(
                "Harness-synthesized CREATE finding (no reviewer call): {status} {targets} must be created with the required contents stated in GOAL and ACCEPTANCE CRITERIA."
            ),
            proposed_fix: format!(
                "Create {targets} (and any other files GOAL names) with the full contents GOAL and ACCEPTANCE CRITERIA require, then run the verification they name."
            ),
            }],
        })
    }
}

/// Whether `text` is a rendered artifact set (review-brief marker).
pub fn is_artifact_set(text: &str) -> bool {
    text.starts_with(&format!("{ARTIFACT_SET_HEADER} ("))
}

/// Push `m` unless a member with its identity exists; that member's
/// index when it does.
fn add(members: &mut Vec<Member>, m: Member) -> Option<usize> {
    match members.iter().position(|x| x.id == m.id) {
        Some(i) => Some(i),
        None => {
            members.push(m);
            None
        }
    }
}

fn member(workdir: &Path, path: String, declared: bool, glob: bool) -> Member {
    Member {
        id: attach_identity(workdir, &path),
        path,
        declared,
        glob,
        change: None,
        git_path: None,
    }
}

/// Whether a worktree change is harness state, never the artifact
/// (live drive: an unignored `.polymorph/` put the drive's own
/// config and growing rolling log into ARTIFACT FILES UNDER REVIEW, and
/// the reviewer graded those instead of the code): anything under a
/// `.polymorph` directory, or a drive event ledger (the harness-state scan
/// filter, [`polymorph_tools::fs::is_harness_state`]), or the marker file
/// of a `--repo` drive worktree (untracked by design, never a change).
fn is_harness_change(workdir: &Path, git_path: &str) -> bool {
    let rel = Path::new(git_path);
    rel == Path::new(polymorph_delegate::WORKTREE_MARKER)
        || rel
            .components()
            .any(|c| c.as_os_str() == polymorph_tools::fs::HARNESS_DIR)
        || polymorph_tools::fs::is_harness_state(&workdir.join(rel), false)
}

fn is_glob(workdir: &Path, raw: &str) -> bool {
    raw.contains(['*', '?', '[', '{']) && !polymorph_tools::resolve_tool_path(workdir, raw).exists()
}

fn is_absent(workdir: &Path, raw: &str) -> bool {
    let full = polymorph_tools::resolve_tool_path(workdir, raw);
    matches!(std::fs::symlink_metadata(full), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
}

/// The CREATE fact the note and the seed both state.
fn create_fact(absent: &[String], diff_base: Option<&str>) -> String {
    match (absent.is_empty(), diff_base) {
        (true, None) => {
            "No file of the artifact set exists on disk (every glob matched nothing).".to_string()
        }
        (true, Some(b)) => format!(
            "No file is declared and the worktree has no changes vs {b} (nothing to grade yet)."
        ),
        (false, None) => "No declared file of the artifact set exists on disk (CREATE job or deleted paths).".to_string(),
        (false, Some(b)) => format!(
            "No declared file of the artifact set exists on disk and the worktree has no changes vs {b} (CREATE job or deleted paths)."
        ),
    }
}

/// Reviewer note for a CREATE set (`create_job_note` parity).
fn create_status(absent: &[String], diff_base: Option<&str>) -> String {
    format!(
        "\nARTIFACT SET STATUS:\n{} Judge goal/criteria fit against the empty set; do NOT emit findings merely claiming files are missing, and do NOT pass a goal that requires files. If files should be created, emit exactly ONE finding that names a target file, states the required contents from the goal and criteria (that file citation satisfies VERDICT DISCIPLINE), and proposes the creation — a failing verdict with zero findings is schema-invalid and wastes the round.\n",
        create_fact(absent, diff_base)
    )
}

/// One line per member with its diff status (diff mode), capped.
fn render_manifest(members: &[Member], base: &str) -> String {
    let mut out = String::new();
    for m in members.iter().take(MAX_MANIFEST_LINES) {
        let status = match m.change {
            Some(Change::Modified) => format!("MODIFIED vs {base}"),
            Some(Change::Added) => format!("ADDED vs {base} (tracked, new)"),
            Some(Change::Deleted) => format!("DELETED vs {base}"),
            Some(Change::Untracked) => "UNTRACKED (new, not ignored)".to_string(),
            Some(Change::Submodule) => format!("SUBMODULE changed vs {base}"),
            Some(Change::NestedRepo) => "NESTED GIT REPOSITORY (untracked)".to_string(),
            None => format!("declared; unchanged vs {base} or outside the diff"),
        };
        out.push_str(&format!(
            "- {}: {status}\n",
            m.path.replace(['\r', '\n'], " ")
        ));
    }
    if members.len() > MAX_MANIFEST_LINES {
        out.push_str(&format!(
            "({} more files not listed)\n",
            members.len() - MAX_MANIFEST_LINES
        ));
    }
    out
}

/// Run git (no shell) in `dir`; stdout on success, the trimmed stderr in
/// the error otherwise.
fn git(dir: &Path, args: &[&str]) -> anyhow::Result<Vec<u8>> {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => {
                anyhow::anyhow!("--diff needs git on PATH (spawn failed: {e})")
            }
            _ => anyhow::anyhow!("--diff: cannot run git in {}: {e}", dir.display()),
        })?;
    if !out.status.success() {
        anyhow::bail!(
            "git {} failed in {}: {}",
            args.join(" "),
            dir.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

/// Files under `dir` that differ from `base` (tracked: modified, added,
/// deleted, submodule; plus untracked-not-ignored), sorted by path.
fn git_changes(dir: &Path, base: &str) -> anyhow::Result<Vec<(String, Change)>> {
    if base.is_empty() || base.starts_with('-') {
        anyhow::bail!("--diff base {base:?} is not a revision");
    }
    let inside = git(dir, &["rev-parse", "--is-inside-work-tree"]);
    if !inside.is_ok_and(|out| out.trim_ascii() == b"true") {
        // Surface the spawn error when git itself is missing.
        git(dir, &["--version"])?;
        anyhow::bail!(
            "--diff: {} is not inside a git work tree (worktree-diff review needs a repo)",
            dir.display()
        );
    }
    let commit = format!("{base}^{{commit}}");
    if git(dir, &["rev-parse", "--verify", "--quiet", &commit]).is_err() {
        anyhow::bail!(
            "--diff base {base:?} does not name a commit (a repo with no commits has no HEAD: commit first, or pass a base)"
        );
    }
    let mut changes: std::collections::BTreeMap<String, Change> = Default::default();
    let raw = git(
        dir,
        &[
            "diff",
            "--raw",
            "-z",
            "--no-renames",
            "--no-ext-diff",
            "--relative",
            base,
            "--",
        ],
    )?;
    let mut fields = raw.split(|&b| b == 0).filter(|f| !f.is_empty());
    while let Some(meta) = fields.next() {
        let Some(path) = fields.next() else {
            break;
        };
        let meta = String::from_utf8_lossy(meta);
        let path = String::from_utf8_lossy(path).into_owned();
        // `:<src mode> <dst mode> <src sha> <dst sha> <status>`.
        let parts: Vec<&str> = meta.trim_start_matches(':').split(' ').collect();
        let submodule = parts.iter().take(2).any(|mode| *mode == "160000");
        let change = match parts.get(4).and_then(|s| s.chars().next()) {
            _ if submodule => Change::Submodule,
            Some('A') => Change::Added,
            Some('D') => Change::Deleted,
            _ => Change::Modified,
        };
        changes.insert(path, change);
    }
    let untracked = git(dir, &["ls-files", "-z", "--others", "--exclude-standard"])?;
    for path in untracked.split(|&b| b == 0).filter(|f| !f.is_empty()) {
        let path = String::from_utf8_lossy(path);
        let (path, change) = match path.strip_suffix('/') {
            Some(dir_path) => (dir_path.to_string(), Change::NestedRepo),
            None => (path.into_owned(), Change::Untracked),
        };
        changes.entry(path).or_insert(change);
    }
    Ok(changes.into_iter().collect())
}

/// The base's version of a file for the large-file diff; `None` when git
/// cannot show it, or it is binary or over the read cap (full-content
/// fallback, named by the renderer).
fn git_show(dir: &Path, base: &str, git_path: &str) -> Option<String> {
    // Plumbing, not `show`: the raw blob, no textconv filters.
    let blob = format!("{base}:./{git_path}");
    let bytes = git(dir, &["cat-file", "blob", &blob]).ok()?;
    if bytes.len() as u64 > ATTACH_MAX_READ_BYTES {
        return None;
    }
    text_of(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A temp git repo with a local identity (never the real repo's).
    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for args in [
            &["init", "-q"][..],
            &["config", "user.name", "t"],
            &["config", "user.email", "t@example.invalid"],
            &["config", "core.autocrlf", "false"],
            &["config", "commit.gpgsign", "false"],
        ] {
            git(dir.path(), args).unwrap();
        }
        dir
    }

    fn commit_all(dir: &Path) {
        git(dir, &["add", "-A"]).unwrap();
        git(dir, &["commit", "-q", "-m", "base"]).unwrap();
    }

    fn write(dir: &Path, path: &str, body: &str) {
        let full = dir.join(path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(full, body).unwrap();
    }

    fn declared(paths: &[&str]) -> ArtifactSet {
        ArtifactSet {
            declared: paths.iter().map(|p| p.to_string()).collect(),
            diff_base: None,
        }
    }

    fn diff(base: &str) -> ArtifactSet {
        ArtifactSet {
            declared: Vec::new(),
            diff_base: Some(base.to_string()),
        }
    }

    /// Declared files render together, each under a path
    /// header with numbered lines a finding can cite as `path:line`;
    /// spellings of one file collapse; a missing one is ABSENT.
    #[test]
    fn declared_files_render_with_citable_headers() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.rs", "fn a() {}\n");
        write(dir.path(), "src/b.rs", "fn b() {\n}\n");
        let out = declared(&["a.rs", "src/b.rs", "./a.rs", "gone.rs"])
            .render(dir.path())
            .unwrap();
        assert_eq!(
            out.text,
            "ARTIFACT FILES UNDER REVIEW (3 files, declared; harness-read from disk; `N | ` prefixes are 1-based file line numbers, not file content — cite findings as path:line):\n--- a.rs: FULL (1 lines)\n1 | fn a() {}\n--- src/b.rs: FULL (2 lines)\n1 | fn b() {\n2 | }\n--- gone.rs: ABSENT (not on disk: not yet created, or deleted; no content)\n"
        );
        assert!(is_artifact_set(&out.text));
        assert!(out.covers("./src/b.rs"));
        assert!(out.covers(&dir.path().join("a.rs").to_string_lossy()));
        assert!(!out.covers("c.rs"));
        // One existing member: review-first, no CREATE note or seed.
        assert!(out.create_seed().is_none());
    }

    /// Globs expand against workdir-relative paths (sorted,
    /// gitignore-aware), dedupe against declared paths, and a zero-match
    /// glob is reported (the CLI refuses it pre-spend). A literal path
    /// with glob characters that exists stays a path.
    #[test]
    fn globs_expand_relative_and_dedupe() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "src/z.rs", "z\n");
        write(dir.path(), "src/a.rs", "a\n");
        write(dir.path(), "src/notes.txt", "n\n");
        write(dir.path(), "pages/[id].tsx", "p\n");
        let out = declared(&["src/z.rs", "src/*.rs", "pages/[id].tsx", "none/*.rs"])
            .render(dir.path())
            .unwrap();
        let order: Vec<&str> = out
            .text
            .lines()
            .filter_map(|l| l.strip_prefix("--- "))
            .collect();
        assert_eq!(
            order,
            [
                "src/z.rs: FULL (1 lines)",
                "src/a.rs: FULL (1 lines)",
                "pages/[id].tsx: FULL (1 lines)"
            ]
        );
        assert_eq!(out.empty_globs, ["none/*.rs"]);
        assert!(
            out.text.ends_with("(glob \"none/*.rs\" matched no file)\n"),
            "{}",
            out.text
        );
        assert!(declared(&["src/[.rs"]).render(dir.path()).is_err());
    }

    /// Diff mode picks up modified, added (staged), deleted and
    /// untracked files — not ignored ones — and lists each with its status.
    #[test]
    fn diff_mode_picks_up_modified_added_deleted_untracked() {
        let dir = repo();
        let d = dir.path();
        write(d, "keep.rs", "same\n");
        write(d, "mod.rs", "old\n");
        write(d, "del.rs", "bye\n");
        write(d, "ren_old.rs", "r\n");
        write(d, ".gitignore", "*.log\n");
        commit_all(d);
        write(d, "mod.rs", "new\n");
        std::fs::remove_file(d.join("del.rs")).unwrap();
        write(d, "staged.rs", "s\n");
        git(d, &["add", "staged.rs"]).unwrap();
        write(d, "dir with space/ünï.rs", "u\n");
        write(d, "noise.log", "ignored\n");
        std::fs::write(d.join("img.bin"), [0x89u8, 0, 1]).unwrap();
        // A rename is a deletion plus an addition (`--no-renames`).
        git(d, &["mv", "ren_old.rs", "ren_new.rs"]).unwrap();
        let out = diff("HEAD").render(d).unwrap();
        assert!(
            out.text.starts_with(
                "ARTIFACT FILES UNDER REVIEW (7 files, worktree diff vs HEAD: 7 changed; "
            ),
            "{}",
            out.text
        );
        for want in [
            "- del.rs: DELETED vs HEAD\n",
            "- dir with space/ünï.rs: UNTRACKED (new, not ignored)\n",
            "- mod.rs: MODIFIED vs HEAD\n",
            "- staged.rs: ADDED vs HEAD (tracked, new)\n",
            "- ren_new.rs: ADDED vs HEAD (tracked, new)\n- ren_old.rs: DELETED vs HEAD\n",
            "--- img.bin: BINARY or non-UTF-8 (3 bytes; content not shown)\n",
            "--- del.rs: ABSENT (not on disk",
            "--- dir with space/ünï.rs: FULL (NEW vs HEAD, 1 lines)\n1 | u\n",
            "--- mod.rs: FULL (1 lines)\n1 | new\n",
        ] {
            assert!(out.text.contains(want), "missing {want:?} in {}", out.text);
        }
        assert!(!out.text.contains("keep.rs"), "{}", out.text);
        assert!(!out.text.contains("noise.log"), "{}", out.text);
        assert!(out.create_seed().is_none());
    }

    /// Live drive regression: an unignored `.polymorph/` (config, rolling
    /// log, attempts) and a drive ledger are harness state, never diff
    /// members — the set stays byte-stable while the log grows, and a tree
    /// changed only by harness state is an empty diff.
    #[test]
    fn diff_mode_skips_harness_state() {
        let dir = repo();
        let d = dir.path();
        write(d, "a.rs", "a\n");
        commit_all(d);
        write(d, ".polymorph/config.toml", "[permissions]\n");
        write(d, ".polymorph/logs/polymorph.2026-10-08", "{}\n");
        write(
            d,
            "drive-events.json",
            "[\n {\n  \"type\": \"round_started\"\n }\n]\n",
        );
        write(d, "fixture-events.json", "{\"not\": \"a ledger\"}\n");
        // A `--repo` drive tree's marker (`.polymorph-child`) too.
        write(d, polymorph_delegate::WORKTREE_MARKER, "drive-0\n");
        let set = diff("HEAD");
        let first = set.render(d).unwrap();
        assert!(!first.text.contains(".polymorph"), "{}", first.text);
        assert!(!first.text.contains("drive-events.json"), "{}", first.text);
        assert!(first.text.contains("fixture-events.json"), "{}", first.text);
        write(d, ".polymorph/logs/polymorph.2026-10-08", "{}\n{}\n");
        assert_eq!(first.text, set.render(d).unwrap().text);
        // Only harness state left: an empty diff (the CREATE seed fires).
        std::fs::remove_file(d.join("fixture-events.json")).unwrap();
        let seed = set.render(d).unwrap().create_seed().expect("seed");
        assert_eq!(seed.kind, crate::SeedKind::Start);
        let seed = seed.findings;
        assert_eq!(seed[0].file, DIFF_SET_FINDING_FILE);
        assert!(
            seed[0]
                .issue
                .contains("(nothing to grade yet). GOAL is not implemented yet"),
            "{}",
            seed[0].issue
        );
        assert!(
            !seed[0].issue.contains("must be created"),
            "{}",
            seed[0].issue
        );
    }

    /// The set is re-read every time — a file written after
    /// the first rendering (a fix turn) joins it — and an unchanged tree
    /// renders byte-identically (the loop's stall check depends on it).
    #[test]
    fn diff_set_grows_when_a_file_is_written() {
        let dir = repo();
        let d = dir.path();
        write(d, "a.rs", "a\n");
        commit_all(d);
        write(d, "a.rs", "a2\n");
        let set = diff("HEAD");
        let first = set.render(d).unwrap();
        assert_eq!(first.text, set.render(d).unwrap().text);
        assert!(!first.text.contains("b.rs"));
        write(d, "src/b.rs", "b\n");
        let second = set.render(d).unwrap();
        assert!(
            second
                .text
                .contains("--- src/b.rs: FULL (NEW vs HEAD, 1 lines)"),
            "{}",
            second.text
        );
        assert!(second.covers("src/b.rs"));
    }

    /// A workdir below the repo root grades only its own
    /// subtree, with workdir-relative paths (tools resolve them there),
    /// and diffs large files against the base from there too.
    #[test]
    fn diff_mode_is_scoped_to_a_subdirectory_workdir() {
        let dir = repo();
        let d = dir.path();
        let old: String = (1..=500)
            .map(|i| {
                format!(
                    "{i}
"
                )
            })
            .collect();
        write(d, "sub/big.rs", &old);
        write(
            d, "top.rs", "t
",
        );
        commit_all(d);
        write(
            d,
            "sub/big.rs",
            &old.replace(
                "250
", "250 x
",
            ),
        );
        write(
            d,
            "sub/new.rs",
            "n
",
        );
        write(
            d, "top.rs", "t2
",
        );
        let out = diff("HEAD").render(&d.join("sub")).unwrap();
        assert!(
            out.text.contains(
                "- big.rs: MODIFIED vs HEAD
- new.rs: UNTRACKED"
            ),
            "{}",
            out.text
        );
        assert!(
            out.text
                .contains("--- big.rs: DIFF vs its HEAD version (500 lines now"),
            "{}",
            out.text
        );
        assert!(!out.text.contains("top.rs"), "{}", out.text);
    }

    /// Diff mode outside a git work tree, or with a base that
    /// is not a commit, errors clearly instead of grading nothing.
    #[test]
    fn diff_mode_errors_outside_git_and_on_bad_base() {
        let plain = tempfile::tempdir().unwrap();
        let err = diff("HEAD").render(plain.path()).unwrap_err().to_string();
        assert!(err.contains("is not inside a git work tree"), "{err}");
        let dir = repo();
        let err = diff("HEAD").render(dir.path()).unwrap_err().to_string();
        assert!(err.contains("does not name a commit"), "{err}");
        write(dir.path(), "a.rs", "a\n");
        commit_all(dir.path());
        let err = diff("no-such-ref")
            .render(dir.path())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("\"no-such-ref\" does not name a commit"),
            "{err}"
        );
        let err = diff("--output=x")
            .render(dir.path())
            .unwrap_err()
            .to_string();
        assert!(err.contains("is not a revision"), "{err}");
    }

    /// A large modified file renders as a numbered diff
    /// against the base's version (CRLF-insensitive); a large untracked
    /// one has no base version and renders partially, cut named.
    #[test]
    fn diff_mode_large_file_falls_back_to_unified_diff() {
        let dir = repo();
        let d = dir.path();
        let old: String = (1..=600).map(|i| format!("line {i}\n")).collect();
        write(d, "big.rs", &old);
        commit_all(d);
        let now = old
            .replace("line 300\n", "line 300 changed\n")
            .replace('\n', "\r\n");
        write(d, "big.rs", &now);
        write(d, "huge_new.rs", &old);
        let out = diff("HEAD").render(d).unwrap();
        assert!(
            out.text.contains("--- big.rs: DIFF vs its HEAD version (600 lines now, over the 400-line full-content cap; 1 hunks, 3 context lines; lines outside the hunks are unchanged and not shown)\n@@ current lines 297-303 @@\n297   | line 297\n"),
            "{}",
            out.text
        );
        assert!(
            out.text
                .contains("    - | line 300\n300 + | line 300 changed\n"),
            "{}",
            out.text
        );
        assert!(
            out.text.contains("--- huge_new.rs: PARTIAL (NEW vs HEAD, 600 lines; lines 1-400 shown, lines 401-600 OMITTED: per-file cap of 400 lines, no HEAD version to diff)\n"),
            "{}",
            out.text
        );
    }

    /// Past the 20-file cap members are named, never read;
    /// rows stop at the shared budget with the cut named.
    #[test]
    fn set_budget_overflow_names_omissions() {
        let dir = tempfile::tempdir().unwrap();
        let mut paths = Vec::new();
        for i in 0..ATTACH_MAX_FILES + 2 {
            let p = format!("f{i:02}.rs");
            let body: String = (1..=100).map(|n| format!("{n}\n")).collect();
            write(dir.path(), &p, &body);
            paths.push(p);
        }
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        let out = declared(&refs).render(dir.path()).unwrap();
        // 1500-row budget: 15 files in full, the rest OMITTED by budget.
        assert!(
            out.text.contains("--- f14.rs: FULL (100 lines)\n"),
            "{}",
            out.text
        );
        assert!(
            out.text.contains(
                "--- f15.rs: OMITTED (100 lines; attachment budget exhausted; content not shown)\n"
            ),
            "{}",
            out.text
        );
        assert!(
            out.text
                .contains("(2 more files OMITTED, over the 20-file cap: f20.rs, f21.rs)\n"),
            "{}",
            out.text
        );
    }

    /// Seed only when every member is a missing
    /// declared path and the diff is empty; any existing (even empty)
    /// file, glob match, or changed file is review-first.
    #[test]
    fn create_rule_needs_every_member_absent_and_no_changes() {
        let dir = repo();
        let d = dir.path();
        write(d, "base.rs", "b\n");
        commit_all(d);
        let set = ArtifactSet {
            declared: vec!["new/a.rs".into(), "new/b.rs".into()],
            diff_base: Some("HEAD".into()),
        };
        let out = set.render(d).unwrap();
        assert!(out.text.contains("\nARTIFACT SET STATUS:\nNo declared file of the artifact set exists on disk and the worktree has no changes vs HEAD (CREATE job or deleted paths)."), "{}", out.text);
        let seed = out.create_seed().expect("seed");
        assert_eq!(seed.kind, crate::SeedKind::Create);
        let seed = seed.findings;
        assert_eq!(seed.len(), 1);
        assert_eq!(seed[0].file, "new/a.rs");
        assert_eq!(seed[0].line, None);
        assert!(
            seed[0].issue.contains("new/a.rs, new/b.rs must be created"),
            "{}",
            seed[0].issue
        );
        assert!(seed[0]
            .proposed_fix
            .starts_with("Create new/a.rs, new/b.rs "));
        // Empty diff, nothing declared: the pseudo file.
        let empty = diff("HEAD").render(d).unwrap();
        assert!(
            empty.text.contains(
                "(no files: nothing declared, and the worktree has no changes vs HEAD)\n"
            ),
            "{}",
            empty.text
        );
        assert_eq!(
            empty.create_seed().unwrap().findings[0].file,
            DIFF_SET_FINDING_FILE
        );
        // One existing (empty) declared file: review-first.
        write(d, "new/a.rs", "");
        assert!(set.render(d).unwrap().create_seed().is_none());
        std::fs::remove_file(d.join("new/a.rs")).unwrap();
        // A tracked deletion is a change: review-first.
        std::fs::remove_file(d.join("base.rs")).unwrap();
        let deleted = set.render(d).unwrap();
        assert!(
            !deleted.text.contains("ARTIFACT SET STATUS"),
            "{}",
            deleted.text
        );
        assert!(deleted.create_seed().is_none());
        // Declared-only, all missing: seeds without diff wording.
        let plain = declared(&["x.rs"]).render(d).unwrap();
        assert!(plain.create_seed().unwrap().findings[0].issue.contains("No declared file of the artifact set exists on disk (CREATE job or deleted paths). x.rs must be created"));
    }
}
