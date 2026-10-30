//! Drive restore ledger (docket #8): a failed in-place drive leaves the
//! tree as it found it.
//!
//! `review drive` runs [`WorkdirSpec::InPlace`](polymorph_delegate::WorkdirSpec)
//! by default, so a halted attempt used to leave partial edits that later
//! fixers discovered and adopted (traps #57, #65). The ledger records, on the
//! FIRST `write`/`edit` of each path in the drive, the file's exact original
//! bytes (or that it did not exist), and fingerprints the drive's own writes.
//! On a non-passing outcome [`RestoreLedger::rollback`] puts every original
//! back (deleting files and now-empty directories the drive created), most
//! recent first, after copying the drive's version into
//! `.polymorph/attempts/<stamp>/` so nothing is lost.
//!
//! Safety rules, in order:
//! - A file whose bytes differ from the drive's last recorded state (a human
//!   or another session edited it since) is a CONFLICT: never overwritten,
//!   reported.
//! - Only paths the fixer's `write`/`edit` calls named, or that harness
//!   change detection saw a fixer `bash`/`exec` command change
//!   ([`crate::command_changes`]), are touched — and a command-changed file
//!   only where its drive-start original is provable:
//!   - in a git repo, a path `git status` did not list at drive start was
//!     clean, so its original is the drive-start `HEAD` blob in the form
//!     whose length is the file's drive-start size (the checkout form
//!     through the work tree's filters, `git cat-file --filters`, else the
//!     stored blob: an `autocrlf` LF file is clean too), or — for
//!     a file the command created, absent from that `HEAD` — absence: it
//!     is deleted only while untracked and not ignored;
//!   - outside git, only absence is provable: a file a command created
//!     that the drive-start stamp walk did not see is deleted.
//!
//!   Everything else a command changed (dirty at drive start, modified
//!   outside git, past the caps) is reported NOT RESTORABLE with the
//!   reason. Files a verify command or an undetected change created are
//!   never deleted; the guard reports them from `git status` instead.
//! - Files over the per-file or drive-wide byte cap get no snapshot and are
//!   reported NOT RESTORABLE (left as the drive wrote them). Binary files are
//!   restored byte-exactly like text (CRLF, BOM and encoding included).

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::command_changes::{ChangeKind, TreeDiff};

/// Largest single original the ledger keeps (bigger files are reported
/// not restorable).
pub const RESTORE_MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// Drive-wide bound on kept originals (past it, newly touched files are
/// reported not restorable — memory stays bounded on runaway drives).
pub const RESTORE_MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;

/// Command-changed files per drive the ledger tracks for restore (each
/// may cost a few `git` calls at rollback); past it they are reported
/// not restorable.
pub const RESTORE_MAX_COMMAND_FILES: usize = 200;

/// Archive of rolled-back attempts, under the harness dir that fixer scans
/// never surface (`polymorph_tools::fs::HARNESS_DIR`).
pub const ATTEMPTS_DIR: &str = "attempts";

/// What a non-passing in-place drive does with its edits (`--on-fail`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnFail {
    /// Restore every file the drive's `write`/`edit` calls changed.
    Rollback,
    /// Leave the tree as the drive left it (the pre-#8 behavior).
    Keep,
}

impl OnFail {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "rollback" => Some(Self::Rollback),
            "keep" => Some(Self::Keep),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rollback => "rollback",
            Self::Keep => "keep",
        }
    }
}

/// Content identity: `None` = no file. Length plus a process-stable hash
/// (the ledger never outlives the process).
type Fingerprint = Option<(u64, u64)>;

fn fingerprint_bytes(bytes: &[u8]) -> (u64, u64) {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    (bytes.len() as u64, h.finish())
}

/// Current identity of `path`; a directory (or any non-file) is an error.
fn fingerprint(path: &Path) -> std::io::Result<Fingerprint> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(fingerprint_bytes(&bytes))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Cheap identity of an unsnapshotted path: (length, mtime).
type Stamp = Option<(u64, Option<std::time::SystemTime>)>;

fn stamp(path: &Path) -> Stamp {
    std::fs::metadata(path)
        .ok()
        .map(|m| (m.len(), m.modified().ok()))
}

/// What the path held before the drive first touched it.
#[derive(Debug)]
enum Original {
    Absent,
    Bytes(Vec<u8>),
    /// Changed by a fixer command, clean in git at drive start: the
    /// original is the drive-start `HEAD` blob (`spec` = `<commit>:<path>`;
    /// `None` = unborn branch), fetched at rollback — or absence when that
    /// commit has no such path and the command created the file.
    GitStart {
        spec: Option<String>,
        /// Root-relative spelling (pathspec for the tracked/ignored checks).
        rel: String,
        created: bool,
        /// Size right before the first command changed it (the drive-start
        /// size: nothing tracked it earlier). A clean file's bytes need not
        /// be the checkout form — under `autocrlf` an LF file is clean —
        /// so the blob form whose length matches is the proven original.
        before_len: Option<u64>,
    },
    /// No snapshot (cap, not a regular file, unreadable): the reason, and
    /// the metadata stamp that tells whether the drive changed it at all.
    Unrestorable {
        reason: String,
        before: Stamp,
    },
}

#[derive(Debug)]
struct Entry {
    path: PathBuf,
    key: String,
    shown: String,
    original: Original,
    /// Ancestors the drive's write will create (deepest first) — removed
    /// on rollback only while empty.
    created_dirs: Vec<PathBuf>,
    /// The drive's last recorded state of the file (`None` = never
    /// recorded, i.e. no mutation call finished for it).
    last_write: Option<Fingerprint>,
    /// A `write`/`edit` naming it started and has not been recorded yet:
    /// an abort there (Ctrl-C during the LSP post-write await) left the
    /// drive's bytes on disk unrecorded, so rollback adopts them first.
    in_flight: bool,
}

#[derive(Debug, Default)]
struct State {
    entries: Vec<Entry>,
    stored: u64,
    /// What the tree held when the guard armed (command-change originals).
    start: Option<DriveStart>,
    /// Restorable command-changed entries so far.
    command_files: usize,
    /// Directories fixer commands created (removed on rollback, deepest
    /// first, only while empty).
    command_dirs: Vec<PathBuf>,
}

/// Drive-start facts command-changed originals are proven from.
#[derive(Debug)]
enum DriveStart {
    /// Root inside a git work tree: the root's repo-relative prefix, the
    /// commit checked out (`None` = unborn), and every path `git status`
    /// listed (modified, staged, deleted, untracked; repo-relative,
    /// [`fold`]ed).
    Git {
        prefix: String,
        head: Option<String>,
        dirty: std::collections::HashSet<String>,
    },
    /// A work tree whose `git status` failed: nothing is provable.
    GitUnknown,
    /// Not a repo: the root-relative ([`fold`]ed) workspace files that
    /// existed (`None` = over the stamp-walk cap).
    Tree(Option<std::collections::HashSet<String>>),
}

/// Drive-level restore ledger (docket #8). Shared (`Arc`) between the
/// fixer runner, which records, and the CLI guard, which rolls back.
#[derive(Debug)]
pub struct RestoreLedger {
    root: PathBuf,
    archive: Option<PathBuf>,
    max_file: u64,
    max_total: u64,
    /// Stamp-walk cap of the non-git drive-start capture (`0` = command
    /// change detection off: nothing captured).
    change_cap: usize,
    state: Mutex<State>,
}

impl RestoreLedger {
    /// Ledger for an in-place drive rooted at `root`, archiving rolled-back
    /// files under `root/.polymorph/attempts/<unix-secs>-<pid>-<n>/` (`n`
    /// counts drives in this process: TUI drives share one pid).
    pub fn new(root: PathBuf) -> Self {
        static DRIVES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let stamp = format!(
            "{}-{}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            std::process::id(),
            DRIVES.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let archive = root
            .join(polymorph_tools::fs::HARNESS_DIR)
            .join(ATTEMPTS_DIR)
            .join(stamp);
        Self::with_limits(
            root,
            Some(archive),
            RESTORE_MAX_FILE_BYTES,
            RESTORE_MAX_TOTAL_BYTES,
        )
    }

    /// Explicit archive dir (`None` = no archive) and caps (tests).
    pub fn with_limits(
        root: PathBuf,
        archive: Option<PathBuf>,
        max_file: u64,
        max_total: u64,
    ) -> Self {
        Self {
            root,
            archive,
            max_file,
            max_total,
            change_cap: crate::command_changes::DEFAULT_COMMAND_CHANGE_MAX_FILES,
            state: Mutex::new(State::default()),
        }
    }

    /// The fixer's `[agent] command_change_max_files` (`0` = detection off,
    /// so [`Self::capture_start`] captures nothing).
    pub fn with_command_change_cap(mut self, cap: usize) -> Self {
        self.change_cap = cap;
        self
    }

    /// Record the drive-start facts command-changed originals are proven
    /// from (git: `HEAD` + `git status`; otherwise a stamp walk). Called
    /// when the guard arms, before the drive's first mutation; later calls
    /// are no-ops.
    pub fn capture_start(&self) {
        if self.change_cap == 0 || self.lock().start.is_some() {
            return;
        }
        let start = capture_drive_start(&self.root, self.change_cap);
        self.lock().start.get_or_insert(start);
    }

    /// After a fixer command: track every path the detector saw it change
    /// that no write/edit (or earlier command) already tracks, with the
    /// original the drive-start facts prove (else not restorable, with the
    /// reason), and record the drive's state of each; remember the
    /// directories it created. Paths outside the root are never tracked.
    pub fn after_command(&self, workdir: &Path, diff: &TreeDiff) {
        if diff.changes.is_empty() && diff.created_dirs.is_empty() {
            return;
        }
        let mut state = self.lock();
        for d in &diff.created_dirs {
            let dir = workdir.join(d);
            if dir.strip_prefix(&self.root).is_ok() && !state.command_dirs.contains(&dir) {
                state.command_dirs.push(dir);
            }
        }
        for c in &diff.changes {
            let path = workdir.join(&c.rel);
            let key = path_key(&path);
            if state.entries.iter().any(|e| e.key == key) {
                continue; // tracked: `refresh` adopts the command's bytes
            }
            if path.strip_prefix(&self.root).is_err() {
                continue;
            }
            let rel = self.shown(&path);
            let unrestorable = |reason: &str| Original::Unrestorable {
                reason: reason.to_string(),
                before: c.before,
            };
            let created = c.kind == ChangeKind::Created;
            let original = if state.command_files >= RESTORE_MAX_COMMAND_FILES {
                unrestorable(&format!(
                    "changed by a fixer command past the drive's {RESTORE_MAX_COMMAND_FILES}-file command-change restore cap"
                ))
            } else {
                match &state.start {
                    None => unrestorable(
                        "changed by a fixer command; the drive-start state was not captured",
                    ),
                    Some(DriveStart::GitUnknown) => unrestorable(
                        "changed by a fixer command; `git status` failed at drive start, so the original is unknown",
                    ),
                    Some(DriveStart::Git {
                        prefix,
                        head,
                        dirty,
                    }) => {
                        let repo_rel = format!("{prefix}{rel}");
                        if dirty.contains(&fold(&repo_rel)) {
                            unrestorable(
                                "changed by a fixer command; it had uncommitted changes (or was untracked) at drive start, so its original content was never captured",
                            )
                        } else {
                            Original::GitStart {
                                spec: head.as_ref().map(|h| format!("{h}:{repo_rel}")),
                                rel: rel.clone(),
                                created,
                                before_len: c.before.map(|(len, _)| len),
                            }
                        }
                    }
                    Some(DriveStart::Tree(files)) => match files {
                        Some(files) if created && !files.contains(&fold(&rel)) => Original::Absent,
                        None if created => unrestorable(
                            "created by a fixer command outside git; the drive-start tree was over the change-detection cap, so it may have existed",
                        ),
                        _ if created => unrestorable(
                            "re-created by a fixer command outside git; it existed at drive start and its original content was never captured",
                        ),
                        _ => unrestorable(
                            "changed by a fixer command outside git; its original content was never captured",
                        ),
                    },
                }
            };
            if !matches!(original, Original::Unrestorable { .. }) {
                state.command_files += 1;
            }
            let mut entry = Entry {
                shown: rel,
                path,
                key,
                original,
                // Command-made dirs unwind once, after every file.
                created_dirs: Vec::new(),
                last_write: None,
                in_flight: false,
            };
            record(&mut entry);
            state.entries.push(entry);
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Paths recorded so far.
    pub fn len(&self) -> usize {
        self.lock().entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        // A panicked recorder leaves consistent entries (each push is
        // whole), so a poisoned lock is still safe to use for rollback.
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Before a fixer tool call: snapshot each `write`/`edit` target the
    /// drive has not touched yet. Other tools record nothing.
    pub fn before_mutation(&self, workdir: &Path, tool: &str, args: &serde_json::Value) {
        let targets = mutation_targets(workdir, tool, args);
        if targets.is_empty() {
            return;
        }
        let mut state = self.lock();
        for path in targets {
            let key = path_key(&path);
            if let Some(entry) = state.entries.iter_mut().find(|e| e.key == key) {
                entry.in_flight = true;
                continue;
            }
            let unrestorable = |reason: String| Original::Unrestorable {
                reason,
                before: stamp(&path),
            };
            let (original, created_dirs) = match std::fs::metadata(&path) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    (Original::Absent, missing_ancestors(&path))
                }
                Err(e) => (unrestorable(format!("unreadable: {e}")), Vec::new()),
                Ok(m) if !m.is_file() => (unrestorable("not a regular file".into()), Vec::new()),
                Ok(m) if m.len() > self.max_file => (
                    unrestorable(format!(
                        "{} bytes, over the {}-byte per-file cap",
                        m.len(),
                        self.max_file
                    )),
                    Vec::new(),
                ),
                Ok(m) if state.stored + m.len() > self.max_total => (
                    unrestorable(format!(
                        "drive-wide snapshot budget ({} bytes) exhausted",
                        self.max_total
                    )),
                    Vec::new(),
                ),
                Ok(_) => match std::fs::read(&path) {
                    Ok(bytes) => {
                        state.stored += bytes.len() as u64;
                        (Original::Bytes(bytes), Vec::new())
                    }
                    Err(e) => (unrestorable(format!("unreadable: {e}")), Vec::new()),
                },
            };
            let shown = self.shown(&path);
            state.entries.push(Entry {
                path,
                key,
                shown,
                original,
                created_dirs,
                last_write: None,
                in_flight: true,
            });
        }
    }

    /// After a fixer `write`/`edit` call (any outcome — a failed multi-file
    /// edit may still have landed part of its sections): record the drive's
    /// state of each target.
    pub fn after_mutation(&self, workdir: &Path, tool: &str, args: &serde_json::Value) {
        let targets = mutation_targets(workdir, tool, args);
        if targets.is_empty() {
            return;
        }
        let mut state = self.lock();
        for path in targets {
            let key = path_key(&path);
            if let Some(entry) = state.entries.iter_mut().find(|e| e.key == key) {
                record(entry);
            }
        }
    }

    /// After a fixer command or a whole fix turn (verify commands
    /// included): adopt the current state of every tracked file as the
    /// drive's own — a formatter the fixer ran is the drive's write, not a
    /// conflict. Also adopts any edit someone else made since the last
    /// record (the documented window; the archive still keeps it).
    pub fn refresh(&self) {
        let mut state = self.lock();
        for entry in state.entries.iter_mut() {
            record(entry);
        }
    }

    /// Restore every tracked file, newest first touch first (so aliases of
    /// one file unwind in order), and empty the ledger: a second call is a
    /// no-op. Never touches a conflicted file.
    pub fn rollback(&self) -> RollbackReport {
        let (entries, mut dirs) = {
            let mut state = self.lock();
            (
                std::mem::take(&mut state.entries),
                std::mem::take(&mut state.command_dirs),
            )
        };
        let mut report = RollbackReport::default();
        for mut entry in entries.into_iter().rev() {
            if entry.in_flight {
                record(&mut entry);
            }
            self.restore_one(entry, &mut report);
        }
        // Deepest first; `remove_dir` refuses non-empty dirs, so anything
        // left in one (kept output, a human's file) keeps it.
        dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
        for dir in dirs {
            if std::fs::remove_dir(&dir).is_ok() {
                report.removed_dirs.push(self.shown(&dir));
            }
        }
        report
    }

    fn restore_one(&self, entry: Entry, report: &mut RollbackReport) {
        let fetched: Option<Vec<u8>>;
        let original: Option<&[u8]> = match &entry.original {
            Original::Absent => None,
            Original::Bytes(bytes) => Some(bytes),
            Original::Unrestorable { reason, before } => {
                // Only worth reporting when the file changed at all.
                if stamp(&entry.path) != *before {
                    report
                        .unrestorable
                        .push((entry.shown.clone(), reason.clone()));
                }
                return;
            }
            Original::GitStart {
                spec,
                created,
                before_len,
                ..
            } => match self.git_original(spec.as_deref(), *created, *before_len) {
                Ok(bytes) => {
                    fetched = bytes;
                    fetched.as_deref()
                }
                Err(reason) => {
                    report.unrestorable.push((entry.shown.clone(), reason));
                    return;
                }
            },
        };
        let original_fp = original.map(fingerprint_bytes);
        let current = match fingerprint(&entry.path) {
            Ok(fp) => fp,
            Err(e) => {
                report
                    .failed
                    .push((entry.shown.clone(), format!("cannot read: {e}")));
                return;
            }
        };
        if current == original_fp {
            return; // unchanged (never written, or written back)
        }
        if entry.last_write != Some(current) {
            report.conflicts.push(entry.shown.clone());
            return;
        }
        // A command-created file goes only while git still calls it
        // untracked and not ignored (a staged or ignored file is not ours).
        if let (None, Original::GitStart { rel, .. }) = (original, &entry.original) {
            if let Some(reason) = self.git_keeps(rel) {
                report.unrestorable.push((entry.shown.clone(), reason));
                return;
            }
        }
        if current.is_some() {
            if let Err(e) = self.archive_file(&entry.path) {
                report.failed.push((
                    entry.shown.clone(),
                    format!("not archived ({e}); left in place"),
                ));
                return;
            }
        }
        match original {
            None => {
                if let Err(e) = std::fs::remove_file(&entry.path) {
                    report
                        .failed
                        .push((entry.shown.clone(), format!("cannot delete: {e}")));
                    return;
                }
                report.removed.push(entry.shown.clone());
                for dir in &entry.created_dirs {
                    // `remove_dir` refuses non-empty dirs: anything else
                    // in there (bash output, a human's file) keeps it.
                    if std::fs::remove_dir(dir).is_ok() {
                        report.removed_dirs.push(self.shown(dir));
                    }
                }
            }
            Some(bytes) => {
                if let Some(parent) = entry.path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                match std::fs::write(&entry.path, bytes) {
                    Ok(()) => report.restored.push(entry.shown.clone()),
                    Err(e) => report
                        .failed
                        .push((entry.shown.clone(), format!("cannot restore: {e}"))),
                }
            }
        }
        if current.is_some() && report.archive.is_none() {
            report.archive = self.archive.as_ref().map(|a| self.shown(a));
        }
    }

    /// `git` in the root.
    fn git(&self, args: &[&str]) -> std::io::Result<std::process::Output> {
        std::process::Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(args)
            .output()
    }

    /// A [`Original::GitStart`] original: the blob's work-tree bytes, or
    /// `None` (absence) when the drive-start commit has no such path and
    /// the command created the file. An error is the not-restorable reason.
    ///
    /// "Clean in `git status`" proves the file's CONTENT, not its bytes:
    /// with `autocrlf` (or an `eol`/filter attribute) both the checkout
    /// form and the stored form are clean. `before_len` (the drive-start
    /// size) picks the form: the checkout form (`--filters`) when its
    /// length matches, else the stored blob when its length matches, else
    /// nothing is proven. For line-ending conversion the length is a
    /// proof: a clean file whose CRLF count is neither 0 nor every line
    /// has a length neither form has.
    fn git_original(
        &self,
        spec: Option<&str>,
        created: bool,
        before_len: Option<u64>,
    ) -> Result<Option<Vec<u8>>, String> {
        let gone = |what: &str| {
            if created {
                Ok(None)
            } else {
                // Changed, not created, yet not in the commit: the path
                // spelling and git disagree (case?) — prove nothing.
                Err(format!(
                    "changed by a fixer command; {what}, so its original is not provable"
                ))
            }
        };
        let Some(spec) = spec else {
            return gone("the branch had no commit at drive start");
        };
        let unavailable = |e: std::io::Error| format!("git unavailable at rollback ({e})");
        if !self
            .git(&["cat-file", "-e", spec])
            .map_err(unavailable)?
            .status
            .success()
        {
            return gone("the drive-start commit has no such file");
        }
        let size = self.git(&["cat-file", "-s", spec]).map_err(unavailable)?;
        let size: u64 = String::from_utf8_lossy(&size.stdout)
            .trim()
            .parse()
            .map_err(|_| "git cat-file -s gave no size".to_string())?;
        if size > self.max_file {
            return Err(format!(
                "{size} bytes at drive start, over the {}-byte per-file cap",
                self.max_file
            ));
        }
        let out = self
            .git(&["cat-file", "--filters", spec])
            .map_err(unavailable)?;
        if !out.status.success() {
            return Err(format!(
                "git cat-file --filters failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let Some(want) = before_len else {
            return Ok(Some(out.stdout));
        };
        let checkout = out.stdout.len() as u64;
        if checkout == want {
            return Ok(Some(out.stdout));
        }
        if size == want {
            let raw = self.git(&["cat-file", "blob", spec]).map_err(unavailable)?;
            if raw.status.success() && raw.stdout.len() as u64 == want {
                return Ok(Some(raw.stdout));
            }
        }
        Err(format!(
            "changed by a fixer command; its drive-start size ({want} bytes) matches neither the drive-start commit's checkout form ({checkout} bytes) nor its stored form ({size} bytes), so its original bytes are not provable (line endings or a filter differ from a checkout)"
        ))
    }

    /// Why a command-created file must stay (`None` = delete it): tracked
    /// now (a command staged it), ignored, or git cannot tell.
    fn git_keeps(&self, rel: &str) -> Option<String> {
        // Literal pathspec: no glob magic in file names (`check-ignore`
        // takes plain paths and refuses pathspec magic).
        match self.git(&[
            "--literal-pathspecs",
            "ls-files",
            "--error-unmatch",
            "--",
            rel,
        ]) {
            Ok(o) if o.status.success() => {
                return Some(
                    "created by a fixer command and tracked now (staged); left in place".into(),
                )
            }
            Ok(_) => {}
            Err(e) => return Some(format!("git unavailable at rollback ({e}); left in place")),
        }
        match self
            .git(&["check-ignore", "-q", "--", rel])
            .map(|o| o.status.code())
        {
            Ok(Some(1)) => None,
            Ok(Some(0)) => {
                Some("created by a fixer command and ignored by git; left in place".into())
            }
            _ => Some("git check-ignore failed; left in place".into()),
        }
    }

    /// Copy the drive's version of `path` into the attempt archive (no-op
    /// without one). Paths outside the root land under `_outside/`.
    fn archive_file(&self, path: &Path) -> std::io::Result<()> {
        let Some(archive) = &self.archive else {
            return Ok(());
        };
        let rel: PathBuf = match path.strip_prefix(&self.root) {
            Ok(rel) => rel.to_path_buf(),
            Err(_) => std::iter::once(std::ffi::OsStr::new("_outside"))
                .chain(path.components().filter_map(|c| match c {
                    Component::Normal(n) => Some(n),
                    _ => None,
                }))
                .collect(),
        };
        let dest = archive.join(rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Keep the archive out of `git status` and every gitignore-aware
        // tool, whatever the project's own ignore rules say.
        if let Some(attempts) = archive.parent() {
            let ignore = attempts.join(".gitignore");
            if !ignore.exists() {
                let _ = std::fs::write(&ignore, "*\n");
            }
        }
        std::fs::copy(path, &dest).map(|_| ())
    }

    /// Root-relative `/`-separated spelling when under the root.
    fn shown(&self, path: &Path) -> String {
        match path.strip_prefix(&self.root) {
            Ok(rel) => rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/"),
            Err(_) => path.display().to_string(),
        }
    }
}

/// Case-folded on Windows (the [`path_key`] rule) for set lookups.
fn fold(rel: &str) -> String {
    if cfg!(windows) {
        rel.to_lowercase()
    } else {
        rel.to_string()
    }
}

/// The drive-start facts of `root` ([`DriveStart`]): `git status` and
/// `HEAD` in a work tree, else a stamp walk capped at `max_files`.
fn capture_drive_start(root: &Path, max_files: usize) -> DriveStart {
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
    };
    let line = |o: std::process::Output| {
        String::from_utf8_lossy(&o.stdout)
            .trim_end_matches(['\r', '\n'])
            .to_string()
    };
    let in_work_tree = git(&["rev-parse", "--is-inside-work-tree"]).map(line);
    if in_work_tree.as_deref() != Some("true") {
        return DriveStart::Tree(
            polymorph_tools::fs::stamp_tree(root, &[], max_files)
                .map(|t| t.files.keys().map(|k| fold(k)).collect()),
        );
    }
    let (Some(prefix), Some(status)) = (
        git(&["rev-parse", "--show-prefix"]).map(line),
        git(&[
            "--no-optional-locks",
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--no-renames",
            "--",
            ".",
        ]),
    ) else {
        return DriveStart::GitUnknown;
    };
    let head = git(&["rev-parse", "--verify", "-q", "HEAD"])
        .map(line)
        .filter(|h| !h.is_empty());
    // `-z` records: `XY <path>` NUL, paths repo-relative and unquoted.
    let dirty = status
        .stdout
        .split(|&b| b == 0)
        .filter_map(|rec| rec.get(3..))
        .filter(|p| !p.is_empty())
        .map(|p| fold(&String::from_utf8_lossy(p)))
        .collect();
    DriveStart::Git {
        prefix,
        head,
        dirty,
    }
}

/// Adopt the file's current bytes as the drive's last write. Unsnapshotted
/// files are skipped (no restore possible; no need to hash a file over the
/// cap after every command).
fn record(entry: &mut Entry) {
    entry.in_flight = false;
    if matches!(entry.original, Original::Unrestorable { .. }) {
        return;
    }
    if let Ok(fp) = fingerprint(&entry.path) {
        entry.last_write = Some(fp);
    }
}

/// The files a `write`/`edit` call is about to touch, resolved exactly as
/// the tools resolve them: `write`'s path, each edit section path and every
/// `MV` destination (an unparseable patch touches nothing).
fn mutation_targets(workdir: &Path, tool: &str, args: &serde_json::Value) -> Vec<PathBuf> {
    match tool {
        "write" => args
            .get("path")
            .and_then(|p| p.as_str())
            .map(|p| vec![polymorph_tools::resolve_tool_path(workdir, p)])
            .unwrap_or_default(),
        "edit" => {
            let Some(patch) = args
                .get("patch")
                .and_then(|p| p.as_str())
                .and_then(|p| polymorph_tools::hashline::parse(p).ok())
            else {
                return Vec::new();
            };
            let mut out = Vec::new();
            for sec in &patch.sections {
                let dests = sec.ops.iter().filter_map(|op| match op {
                    polymorph_tools::hashline::Op::MoveFile { dest } => Some(dest.as_str()),
                    _ => None,
                });
                for raw in std::iter::once(sec.path.as_str()).chain(dests) {
                    if let Ok(p) = polymorph_tools::hashline::apply::contain(workdir, raw) {
                        out.push(p);
                    }
                }
            }
            out
        }
        _ => Vec::new(),
    }
}

/// Lexical identity of a path (`.`/`..` folded; case-folded on Windows,
/// where `Foo.rs` and `foo.rs` are one file).
fn path_key(path: &Path) -> String {
    let mut parts: Vec<String> = Vec::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop();
            }
            other => parts.push(other.as_os_str().to_string_lossy().into_owned()),
        }
    }
    let key = parts.join("/");
    if cfg!(windows) {
        key.to_lowercase()
    } else {
        key
    }
}

/// Ancestors of `path` that do not exist yet, deepest first.
fn missing_ancestors(path: &Path) -> Vec<PathBuf> {
    path.ancestors()
        .skip(1)
        .take_while(|a| !a.as_os_str().is_empty() && !a.exists())
        .map(Path::to_path_buf)
        .collect()
}

/// What a rollback did (docket #8): printed as a summary and attached to
/// the drive's terminal event.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RollbackReport {
    /// Modified files put back to their original bytes.
    pub restored: Vec<String>,
    /// Files the drive created, deleted.
    pub removed: Vec<String>,
    /// Directories the drive created, removed once empty.
    pub removed_dirs: Vec<String>,
    /// Changed since the drive's last write: left as-is.
    pub conflicts: Vec<String>,
    /// Written by the drive but never snapshotted: (path, reason).
    pub unrestorable: Vec<(String, String)>,
    /// I/O failures: (path, error); the file is left as found.
    pub failed: Vec<(String, String)>,
    /// Where the drive's versions of rolled-back files were copied.
    pub archive: Option<String>,
    /// `git status` entries that appeared during the drive and survived
    /// the rollback (bash/verify output); `None` outside a git repo.
    pub left_behind: Option<Vec<String>>,
}

impl RollbackReport {
    /// Operator lines (stdout board). Always at least one line.
    pub fn summary_lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.restored.is_empty() && self.removed.is_empty() {
            out.push(
                "ROLLBACK: no file to restore (no write/edit or detected command change survived)"
                    .to_string(),
            );
        } else {
            let mut line = format!(
                "ROLLBACK: restored {} modified file{}, removed {} created file{}",
                self.restored.len(),
                plural(self.restored.len()),
                self.removed.len(),
                plural(self.removed.len()),
            );
            if !self.removed_dirs.is_empty() {
                line.push_str(&format!(
                    " (+{} dir{})",
                    self.removed_dirs.len(),
                    plural(self.removed_dirs.len())
                ));
            }
            if let Some(archive) = &self.archive {
                line.push_str(&format!("; failed attempt archived at {archive}"));
            }
            out.push(line);
        }
        if !self.conflicts.is_empty() {
            out.push(format!(
                "ROLLBACK CONFLICT (changed after the drive's last write; left as-is): {}",
                self.conflicts.join(", ")
            ));
        }
        for (path, reason) in &self.unrestorable {
            out.push(format!(
                "ROLLBACK NOT RESTORABLE (left as the drive wrote it): {path} ({reason})"
            ));
        }
        for (path, error) in &self.failed {
            out.push(format!("ROLLBACK FAILED: {path} ({error})"));
        }
        match &self.left_behind {
            Some(lines) if !lines.is_empty() => out.push(format!(
                "LEFT BEHIND (bash/verify output, not rolled back; review by hand): {}",
                lines.join(", ")
            )),
            Some(_) => {}
            None => out.push(
                "NOTE: not a git repo: files fixer commands (bash) modified are reported, never restored; only files they created are removed"
                    .to_string(),
            ),
        }
        out
    }

    /// The `rollback` object of the drive's terminal event.
    pub fn to_json(&self) -> serde_json::Value {
        let pairs = |v: &[(String, String)], second: &str| {
            v.iter()
                .map(|(p, r)| serde_json::json!({ "path": p, second: r }))
                .collect::<Vec<_>>()
        };
        serde_json::json!({
            "restored": self.restored,
            "removed": self.removed,
            "removed_dirs": self.removed_dirs,
            "conflicts": self.conflicts,
            "unrestorable": pairs(&self.unrestorable, "reason"),
            "failed": pairs(&self.failed, "error"),
            "archive": self.archive,
            "left_behind": self.left_behind,
        })
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// `git status --porcelain` lines under `root` (outside the harness dir),
/// `None` when `root` is not in a work tree or git is unavailable.
pub fn git_status_lines(root: &Path) -> Option<BTreeSet<String>> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "--no-optional-locks",
            "status",
            "--porcelain=v1",
            "--untracked-files=normal",
            "--",
            ".",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    // Porcelain paths are repo-root-relative: the harness dir may sit
    // below the root of the repo.
    let harness = format!("{}/", polymorph_tools::fs::HARNESS_DIR);
    let nested = format!("/{harness}");
    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.trim_end().to_string())
            .filter(|l| {
                let path = l.get(3..).unwrap_or("").trim_matches('"');
                !path.is_empty() && !path.starts_with(&harness) && !path.contains(&nested)
            })
            .collect(),
    )
}

/// Arms a rollback for one drive: [`Self::commit`] on a pass,
/// [`Self::rollback`] otherwise. Dropped while armed (a panic unwinding
/// through the drive, or the drive future dropped mid-turn), it rolls back
/// and prints the summary to stderr — the tree never keeps a half-done
/// attempt silently.
pub struct RollbackGuard {
    ledger: Arc<RestoreLedger>,
    git_start: Option<BTreeSet<String>>,
    armed: bool,
    /// Print the drop-path summary to stderr (off under a TUI, which owns
    /// the screen: the summary then goes to the log only).
    stderr: bool,
}

impl RollbackGuard {
    /// Arm before the drive's first mutation (captures `git status` and
    /// the ledger's drive-start facts).
    pub fn new(ledger: Arc<RestoreLedger>) -> Self {
        ledger.capture_start();
        let git_start = git_status_lines(ledger.root());
        Self {
            ledger,
            git_start,
            armed: true,
            stderr: true,
        }
    }

    /// Log-only drop summary (TUI drives).
    pub fn quiet(mut self) -> Self {
        self.stderr = false;
        self
    }

    /// The drive passed: keep every change.
    pub fn commit(mut self) {
        self.armed = false;
    }

    /// The drive did not pass: restore and report.
    pub fn rollback(mut self) -> RollbackReport {
        self.armed = false;
        self.run()
    }

    fn run(&self) -> RollbackReport {
        let mut report = self.ledger.rollback();
        report.left_behind = self.git_start.as_ref().and_then(|start| {
            let now = git_status_lines(self.ledger.root())?;
            Some(now.difference(start).cloned().collect())
        });
        report
    }
}

impl Drop for RollbackGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let report = self.run();
        for line in report.summary_lines() {
            tracing::warn!("drive aborted: {line}");
            if self.stderr {
                eprintln!("{line}");
            }
        }
    }
}

/// Turn decorator (docket #8): after every fix turn — its verify commands
/// included when this wraps the verify gate — adopt the tracked files'
/// state as the drive's, so command-made changes to them roll back instead
/// of reading as conflicts.
pub struct RestoreRefreshRunner {
    pub inner: Arc<dyn polymorph_delegate::TurnRunner>,
    pub ledger: Arc<RestoreLedger>,
}

#[async_trait]
impl polymorph_delegate::TurnRunner for RestoreRefreshRunner {
    async fn run_turn(
        &self,
        ctx: polymorph_delegate::TurnContext,
    ) -> anyhow::Result<polymorph_delegate::TurnOutput> {
        let out = self.inner.run_turn(ctx).await;
        self.ledger.refresh();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// Every file under `root` outside the harness dir, by relative path.
    fn tree(root: &Path) -> BTreeMap<String, Vec<u8>> {
        fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                if entry.file_name() == polymorph_tools::fs::HARNESS_DIR {
                    continue;
                }
                if path.is_dir() {
                    walk(root, &path, out);
                } else {
                    let rel = path.strip_prefix(root).unwrap().to_string_lossy();
                    out.insert(rel.replace('\\', "/"), std::fs::read(&path).unwrap());
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(root, root, &mut out);
        out
    }

    fn write_args(path: &str) -> serde_json::Value {
        serde_json::json!({ "path": path, "content": "x" })
    }

    /// Simulate one fixer `write`: record, write, record.
    fn drive_write(ledger: &RestoreLedger, root: &Path, rel: &str, bytes: &[u8]) {
        ledger.before_mutation(root, "write", &write_args(rel));
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
        ledger.after_mutation(root, "write", &write_args(rel));
    }

    fn ledger(root: &Path) -> RestoreLedger {
        RestoreLedger::with_limits(
            root.to_path_buf(),
            Some(root.join(".polymorph").join("attempts").join("t")),
            RESTORE_MAX_FILE_BYTES,
            RESTORE_MAX_TOTAL_BYTES,
        )
    }

    #[test]
    fn rollback_restores_exact_bytes_and_deletes_created_files_and_dirs() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        // CRLF + BOM text and a binary file: restored byte for byte.
        std::fs::write(root.join("a.txt"), b"\xef\xbb\xbfone\r\ntwo\r\n").unwrap();
        std::fs::write(root.join("blob.bin"), [0u8, 1, 2, 255]).unwrap();
        std::fs::create_dir(root.join("keep")).unwrap();
        let before = tree(root);
        let l = ledger(root);
        drive_write(&l, root, "a.txt", b"rewritten\n");
        drive_write(&l, root, "a.txt", b"rewritten twice\n");
        drive_write(&l, root, "blob.bin", b"text now");
        drive_write(&l, root, "new/deep/b.rs", b"fn b() {}\n");
        drive_write(&l, root, "keep/c.rs", b"fn c() {}\n");
        let report = l.rollback();
        assert_eq!(tree(root), before);
        assert!(!root.join("new").exists(), "drive-created dirs removed");
        assert!(root.join("keep").is_dir(), "pre-existing dir kept");
        assert_eq!(report.restored, ["blob.bin", "a.txt"]);
        assert_eq!(report.removed, ["keep/c.rs", "new/deep/b.rs"]);
        assert_eq!(report.removed_dirs, ["new/deep", "new"]);
        assert!(report.conflicts.is_empty() && report.failed.is_empty());
        // The drive's versions are archived, never lost.
        let archive = root.join(".polymorph").join("attempts").join("t");
        assert_eq!(
            std::fs::read(archive.join("a.txt")).unwrap(),
            b"rewritten twice\n"
        );
        assert_eq!(
            std::fs::read(archive.join("new").join("deep").join("b.rs")).unwrap(),
            b"fn b() {}\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.join(".polymorph").join("attempts").join(".gitignore"))
                .unwrap(),
            "*\n"
        );
        assert_eq!(report.archive.as_deref(), Some(".polymorph/attempts/t"));
        // Emptied: a second rollback (the guard's drop) is a no-op.
        assert!(l.is_empty());
        assert_eq!(l.rollback(), RollbackReport::default());
        let lines = report.summary_lines();
        assert!(
            lines[0].starts_with(
                "ROLLBACK: restored 2 modified files, removed 2 created files (+2 dirs)"
            ),
            "{lines:?}"
        );
    }

    #[test]
    fn conflicting_edits_are_never_overwritten() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        std::fs::write(root.join("a.rs"), "orig a").unwrap();
        let l = ledger(root);
        drive_write(&l, root, "a.rs", b"drive a");
        drive_write(&l, root, "b.rs", b"drive b");
        drive_write(&l, root, "c.rs", b"drive c");
        // Someone else edits a.rs and b.rs after the drive's last write.
        std::fs::write(root.join("a.rs"), "human a").unwrap();
        std::fs::write(root.join("b.rs"), "human b").unwrap();
        let report = l.rollback();
        assert_eq!(
            std::fs::read_to_string(root.join("a.rs")).unwrap(),
            "human a"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("b.rs")).unwrap(),
            "human b"
        );
        assert!(!root.join("c.rs").exists());
        assert_eq!(report.conflicts, ["b.rs", "a.rs"]);
        assert_eq!(report.removed, ["c.rs"]);
        assert!(report
            .summary_lines()
            .iter()
            .any(|l| l.starts_with("ROLLBACK CONFLICT") && l.contains("a.rs")));
    }

    #[test]
    fn oversized_files_are_reported_not_restorable() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        std::fs::write(root.join("big.dat"), vec![b'x'; 64]).unwrap();
        std::fs::write(root.join("small.txt"), "s").unwrap();
        std::fs::write(root.join("untouched-big.dat"), vec![b'y'; 64]).unwrap();
        let l = RestoreLedger::with_limits(root.to_path_buf(), None, 32, 1024);
        drive_write(&l, root, "big.dat", b"shrunk");
        drive_write(&l, root, "small.txt", b"changed");
        // Snapshotted for a write that never landed (denied): not reported.
        l.before_mutation(root, "write", &write_args("untouched-big.dat"));
        l.after_mutation(root, "write", &write_args("untouched-big.dat"));
        let report = l.rollback();
        assert_eq!(
            std::fs::read_to_string(root.join("big.dat")).unwrap(),
            "shrunk"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("small.txt")).unwrap(),
            "s"
        );
        assert_eq!(report.unrestorable.len(), 1, "{report:?}");
        assert_eq!(report.unrestorable[0].0, "big.dat");
        assert!(report.unrestorable[0].1.contains("per-file cap"));
        assert!(report.archive.is_none(), "no archive configured");
        // Drive-wide budget: the second file past it is not restorable.
        let d2 = tempfile::tempdir().unwrap();
        let root2 = d2.path();
        std::fs::write(root2.join("one.txt"), "12345").unwrap();
        std::fs::write(root2.join("two.txt"), "12345").unwrap();
        let l2 = RestoreLedger::with_limits(root2.to_path_buf(), None, 32, 8);
        drive_write(&l2, root2, "one.txt", b"a");
        drive_write(&l2, root2, "two.txt", b"b");
        let report = l2.rollback();
        assert_eq!(report.restored, ["one.txt"]);
        assert_eq!(report.unrestorable[0].0, "two.txt");
        assert!(report.unrestorable[0].1.contains("budget"));
        assert!(report
            .summary_lines()
            .iter()
            .any(|l| l.starts_with("ROLLBACK NOT RESTORABLE") && l.contains("two.txt")));
    }

    #[test]
    fn refresh_adopts_command_changes_and_in_flight_writes_roll_back() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        std::fs::write(root.join("fmt.rs"), "orig").unwrap();
        std::fs::write(root.join("aborted.rs"), "orig").unwrap();
        let l = ledger(root);
        drive_write(&l, root, "fmt.rs", b"drive");
        // A formatter the fixer ran rewrites the file: the drive's own.
        std::fs::write(root.join("fmt.rs"), "drive, formatted").unwrap();
        l.refresh();
        // Aborted mid-call: the write landed, the after-record never ran.
        l.before_mutation(root, "write", &write_args("aborted.rs"));
        std::fs::write(root.join("aborted.rs"), "half").unwrap();
        let report = l.rollback();
        assert_eq!(
            std::fs::read_to_string(root.join("fmt.rs")).unwrap(),
            "orig"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("aborted.rs")).unwrap(),
            "orig"
        );
        assert!(report.conflicts.is_empty(), "{report:?}");
    }

    #[test]
    fn edit_targets_include_moves_and_deletes() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        std::fs::write(root.join("old.txt"), "moved\r\n").unwrap();
        std::fs::write(root.join("del.txt"), "deleted").unwrap();
        let before = tree(root);
        let l = ledger(root);
        let patch = serde_json::json!({
            "patch": "[del.txt#abcd]\nREM\n[old.txt#abcd]\nMV sub/new.txt\n"
        });
        l.before_mutation(root, "edit", &patch);
        assert_eq!(l.len(), 3);
        std::fs::remove_file(root.join("del.txt")).unwrap();
        std::fs::create_dir(root.join("sub")).unwrap();
        std::fs::rename(root.join("old.txt"), root.join("sub").join("new.txt")).unwrap();
        l.after_mutation(root, "edit", &patch);
        let report = l.rollback();
        assert_eq!(tree(root), before);
        assert_eq!(report.restored, ["old.txt", "del.txt"]);
        assert_eq!(report.removed, ["sub/new.txt"]);
        // An unparseable patch and read-only tools record nothing.
        l.before_mutation(root, "edit", &serde_json::json!({"patch": "garbage"}));
        l.before_mutation(root, "read", &write_args("old.txt"));
        assert!(l.is_empty());
    }

    #[test]
    fn guard_commit_keeps_and_drop_rolls_back() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        std::fs::write(root.join("a.txt"), "orig").unwrap();
        let l = Arc::new(ledger(root));
        drive_write(&l, root, "a.txt", b"passed");
        RollbackGuard::new(l.clone()).commit();
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "passed"
        );
        // A fresh drive aborts (guard dropped while armed): rolled back.
        let l = Arc::new(ledger(root));
        drive_write(&l, root, "a.txt", b"aborted");
        drop(RollbackGuard::new(l.clone()));
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "passed"
        );
    }

    #[test]
    fn guard_reports_git_leftovers_without_deleting_them() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .output()
        };
        if !git(&["init", "-q"]).is_ok_and(|o| o.status.success()) {
            eprintln!("SKIP guard_reports_git_leftovers_without_deleting_them: no git");
            return;
        }
        std::fs::write(root.join("pre-existing.txt"), "user file").unwrap();
        let l = Arc::new(ledger(root));
        let guard = RollbackGuard::new(l.clone());
        drive_write(&l, root, "made.rs", b"drive");
        // A fixer `bash` command's output: untracked, never ours to delete.
        std::fs::create_dir(root.join("out_check")).unwrap();
        std::fs::write(root.join("out_check").join("x.class"), "junk").unwrap();
        let report = guard.rollback();
        assert_eq!(report.removed, ["made.rs"]);
        assert_eq!(
            report.left_behind,
            Some(vec!["?? out_check/".to_string()]),
            "{report:?}"
        );
        assert!(root.join("out_check").join("x.class").exists());
        assert!(root.join("pre-existing.txt").exists());
        let json = report.to_json();
        assert_eq!(json["removed"][0], "made.rs");
        assert_eq!(json["left_behind"][0], "?? out_check/");
    }

    #[test]
    fn on_fail_parses_both_modes_only() {
        assert_eq!(OnFail::parse("rollback"), Some(OnFail::Rollback));
        assert_eq!(OnFail::parse(" keep "), Some(OnFail::Keep));
        assert_eq!(OnFail::parse("discard"), None);
        assert_eq!(OnFail::Rollback.as_str(), "rollback");
    }
    /// Scripted fixer model: every fix turn writes `a.txt` and a new nested
    /// file, runs a command that leaves stray output, then answers.
    struct ScriptedFixer {
        calls: Mutex<u32>,
    }

    #[async_trait]
    impl crate::AgenticTurnFn for ScriptedFixer {
        async fn turn(
            &self,
            _messages: Vec<genai::chat::ChatMessage>,
            _tools: Vec<genai::chat::Tool>,
            _cancel: &tokio_util::sync::CancellationToken,
        ) -> anyhow::Result<polymorph_llm::CollectedTurn> {
            let n = {
                let mut calls = self.calls.lock().unwrap();
                *calls += 1;
                *calls
            };
            let call = |id: &str, name: &str, args: serde_json::Value| genai::chat::ToolCall {
                call_id: format!("{id}-{n}"),
                fn_name: name.to_string(),
                fn_arguments: args,
                thought_signatures: None,
            };
            // Odd calls mutate; even calls close the turn with text.
            let tool_calls = if n % 2 == 1 {
                vec![
                    call(
                        "w1",
                        "write",
                        serde_json::json!({"path": "a.txt", "content": format!("attempt {n}\n")}),
                    ),
                    call(
                        "w2",
                        "write",
                        serde_json::json!({"path": "gen/deep/b.rs", "content": "fn b() {}\n"}),
                    ),
                    call(
                        "b1",
                        "bash",
                        serde_json::json!({"cmd": "echo junk> stray.log"}),
                    ),
                ]
            } else {
                Vec::new()
            };
            Ok(polymorph_llm::CollectedTurn {
                text: if tool_calls.is_empty() {
                    "done".into()
                } else {
                    String::new()
                },
                tool_calls,
                reasoning_content: None,
                aborted: false,
                rule_hit: None,
            })
        }
    }

    /// Reviewer that fails with a fresh finding each round, or passes.
    struct Scripted {
        pass: bool,
        rounds: Mutex<u32>,
    }

    #[async_trait]
    impl crate::Reviewer for Scripted {
        async fn review(
            &self,
            _input: crate::ReviewerInput,
        ) -> Result<polymorph_core::Verdict, crate::ReviewerError> {
            let n = {
                let mut r = self.rounds.lock().unwrap();
                *r += 1;
                *r
            };
            let pass = self.pass && n > 1;
            Ok(polymorph_core::Verdict {
                verdict: if pass {
                    polymorph_core::VerdictKind::Pass
                } else {
                    polymorph_core::VerdictKind::Fail
                },
                findings: if pass {
                    Vec::new()
                } else {
                    vec![polymorph_core::Finding {
                        severity: polymorph_core::Severity::P1,
                        file: "a.txt".into(),
                        line: None,
                        issue: format!("still wrong (round {n})"),
                        proposed_fix: "fix it".into(),
                    }]
                },
                confidence: 0.9,
            })
        }
    }

    /// Stubbed-model drive over the real executor, tools and loop, with the
    /// CLI's rollback wiring (ledger on the fixer, refresh decorator, guard).
    async fn stubbed_drive(
        root: &Path,
        pass: bool,
        detect: bool,
    ) -> (crate::Outcome, Option<RollbackReport>) {
        let ledger = Arc::new(ledger(root));
        let mut fixer = crate::AgenticFixRunner {
            turn: Arc::new(ScriptedFixer {
                calls: Mutex::new(0),
            }),
            registry: Arc::new(polymorph_tools::registry::v1_registry()),
            gate: polymorph_tools::Gate::new(vec![], vec![]),
            tools: vec!["write".into(), "bash".into()],
            max_tool_rounds: 4,
            max_bonus_tool_rounds: 0,
            advisor: None,
            restore: None,
            history: Default::default(),
            command_changes: 0,
        };
        if detect {
            fixer.command_changes = crate::DEFAULT_COMMAND_CHANGE_MAX_FILES;
        }
        fixer.restore = Some(ledger.clone());
        let executor = crate::BusExecutor {
            bus: Arc::new(polymorph_delegate::Bus::flat()),
            runner: Arc::new(RestoreRefreshRunner {
                inner: Arc::new(fixer),
                ledger: ledger.clone(),
            }),
            workdir: polymorph_delegate::WorkdirSpec::InPlace(root.to_path_buf()),
            tools: vec!["write".into(), "bash".into()],
            tool_outcomes: Arc::new(Mutex::new(Vec::new())),
            merge: None,
            artifact_path: Some(PathBuf::from("a.txt")),
            peripherals: None,
            artifact_set: None,
        };
        let reviewer = Scripted {
            pass,
            rounds: Mutex::new(0),
        };
        let cfg = polymorph_core::LoopConfig {
            max_iterations: 3,
            ..Default::default()
        };
        let guard = RollbackGuard::new(ledger);
        let report = crate::run_loop(
            &cfg,
            "fix a.txt",
            "",
            "original\r\n",
            crate::LoopMode::Act,
            &reviewer,
            &executor,
        )
        .await;
        let rollback = match report.outcome {
            crate::Outcome::Passed { .. } => {
                guard.commit();
                None
            }
            crate::Outcome::Escalated { .. } => Some(guard.rollback()),
        };
        (report.outcome, rollback)
    }

    #[tokio::test]
    async fn escalated_stubbed_drive_leaves_the_tree_as_it_was() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        std::fs::write(root.join("a.txt"), "original\r\n").unwrap();
        std::fs::write(root.join("other.txt"), "untouched").unwrap();
        let before = tree(root);
        let (outcome, rollback) = stubbed_drive(root, false, false).await;
        assert!(
            matches!(outcome, crate::Outcome::Escalated { .. }),
            "{outcome:?}"
        );
        let rollback = rollback.unwrap();
        // Every write/edit change is gone; the command's stray output is
        // reported territory, never deleted.
        let mut after = tree(root);
        assert!(after.remove("stray.log").is_some(), "command output kept");
        assert_eq!(after, before);
        assert!(!root.join("gen").exists());
        assert_eq!(rollback.restored, ["a.txt"]);
        assert_eq!(rollback.removed, ["gen/deep/b.rs"]);
        assert!(rollback.conflicts.is_empty(), "{rollback:?}");
    }

    /// The same escalated drive with command change detection on: the
    /// command's output file is the drive's too (created after the
    /// drive-start walk), so the tree comes back exactly as it was.
    #[tokio::test]
    async fn escalated_drive_with_detection_also_removes_command_output() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        std::fs::write(root.join("a.txt"), "original\r\n").unwrap();
        let before = tree(root);
        let (outcome, rollback) = stubbed_drive(root, false, true).await;
        assert!(
            matches!(outcome, crate::Outcome::Escalated { .. }),
            "{outcome:?}"
        );
        let rollback = rollback.unwrap();
        if rollback.left_behind.is_some() {
            eprintln!("SKIP escalated_drive_with_detection_also_removes_command_output: temp dir in a repo");
            return;
        }
        assert_eq!(tree(root), before);
        assert_eq!(rollback.restored, ["a.txt"]);
        assert_eq!(rollback.removed, ["stray.log", "gen/deep/b.rs"]);
        assert!(rollback.conflicts.is_empty() && rollback.unrestorable.is_empty());
    }

    #[tokio::test]
    async fn passed_stubbed_drive_keeps_its_edits() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        std::fs::write(root.join("a.txt"), "original\r\n").unwrap();
        let (outcome, rollback) = stubbed_drive(root, true, false).await;
        assert!(
            matches!(outcome, crate::Outcome::Passed { .. }),
            "{outcome:?}"
        );
        assert!(rollback.is_none());
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "attempt 1\n"
        );
        assert!(root.join("gen").join("deep").join("b.rs").exists());
    }

    /// Scripted fixer model for command-change rollback: one round running
    /// the given `bash` commands, then a closing text.
    struct Commands(Mutex<Option<Vec<String>>>);

    #[async_trait]
    impl crate::AgenticTurnFn for Commands {
        async fn turn(
            &self,
            _messages: Vec<genai::chat::ChatMessage>,
            _tools: Vec<genai::chat::Tool>,
            _cancel: &tokio_util::sync::CancellationToken,
        ) -> anyhow::Result<polymorph_llm::CollectedTurn> {
            let cmds = self.0.lock().unwrap().take().unwrap_or_default();
            let done = cmds.is_empty();
            Ok(polymorph_llm::CollectedTurn {
                text: if done { "done".into() } else { String::new() },
                tool_calls: cmds
                    .into_iter()
                    .enumerate()
                    .map(|(i, cmd)| genai::chat::ToolCall {
                        call_id: format!("b{i}"),
                        fn_name: "bash".into(),
                        fn_arguments: serde_json::json!({ "cmd": cmd }),
                        thought_signatures: None,
                    })
                    .collect(),
                reasoning_content: None,
                aborted: false,
                rule_hit: None,
            })
        }
    }

    /// One fix turn running `cmds` in `root` with detection on and the
    /// ledger attached (the CLI wiring); the turn's ledger entries.
    async fn command_turn(
        root: &Path,
        ledger: &Arc<RestoreLedger>,
        cmds: &[&str],
    ) -> Vec<serde_json::Value> {
        let fixer = crate::AgenticFixRunner {
            turn: Arc::new(Commands(Mutex::new(Some(
                cmds.iter().map(|c| c.to_string()).collect(),
            )))),
            registry: Arc::new(polymorph_tools::registry::v1_registry()),
            gate: polymorph_tools::Gate::new(vec![], vec![]),
            tools: vec!["bash".into()],
            max_tool_rounds: 2,
            max_bonus_tool_rounds: 0,
            advisor: None,
            restore: Some(ledger.clone()),
            history: Default::default(),
            command_changes: crate::DEFAULT_COMMAND_CHANGE_MAX_FILES,
        };
        let ctx = polymorph_delegate::TurnContext {
            child_id: "c".into(),
            role: polymorph_core::Role::Act,
            brief: "fix".into(),
            workdir: root.to_path_buf(),
            turn: 1,
            steer: Vec::new(),
            cancel: tokio_util::sync::CancellationToken::new(),
        };
        let out = polymorph_delegate::TurnRunner::run_turn(&fixer, ctx)
            .await
            .unwrap();
        out.data.unwrap()["tool_outcomes"]
            .as_array()
            .unwrap()
            .clone()
    }

    /// Git: what a command did to a file clean at drive start is undone
    /// from the drive-start commit (byte-exact, CRLF kept); files it
    /// created are archived and deleted (with the dirs it made) while
    /// untracked and unchanged since; an ignored file is never seen; a file
    /// dirty or untracked at drive start, or edited after the drive's last
    /// look, is left and reported.
    #[tokio::test]
    async fn command_changes_roll_back_where_git_proves_the_original() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .output()
        };
        if !git(&["init", "-q"]).is_ok_and(|o| o.status.success()) {
            eprintln!("SKIP command_changes_roll_back_where_git_proves_the_original: no git");
            return;
        }
        for args in [
            &["config", "user.name", "t"][..],
            &["config", "user.email", "t@example.invalid"],
            &["config", "core.autocrlf", "false"],
            &["config", "commit.gpgsign", "false"],
        ] {
            assert!(git(args).unwrap().status.success());
        }
        std::fs::write(root.join("tracked.txt"), b"orig\r\nline two\r\n").unwrap();
        std::fs::write(root.join("dirty.txt"), "base\n").unwrap();
        std::fs::write(root.join(".gitignore"), "*.log\n").unwrap();
        assert!(git(&["add", "-A"]).unwrap().status.success());
        assert!(git(&["commit", "-q", "-m", "base"])
            .unwrap()
            .status
            .success());
        // Uncommitted work at drive start: never ours to restore.
        std::fs::write(root.join("dirty.txt"), "user wip\n").unwrap();
        std::fs::write(root.join("pre.txt"), "user notes\n").unwrap();
        let l = Arc::new(ledger(root));
        let guard = RollbackGuard::new(l.clone());
        let entries = command_turn(
            root,
            &l,
            &[
                "echo sed-edit> tracked.txt",
                "echo more> dirty.txt",
                "echo again> pre.txt",
                "echo gen> made.txt",
                "mkdir gen",
                "echo deep> gen/deep.txt",
                "echo junk> build.log",
                "echo mine> human.txt",
            ],
        )
        .await;
        let changed = |i: usize| entries[i]["changed_paths"].clone();
        assert_eq!(changed(0), serde_json::json!(["tracked.txt"]));
        assert_eq!(changed(3), serde_json::json!(["made.txt"]));
        assert_eq!(changed(5), serde_json::json!(["gen/deep.txt"]));
        assert!(
            entries[6].get("changed_paths").is_none(),
            "ignored output is not workspace content: {}",
            entries[6]
        );
        // Someone edits a drive-created file after the drive's last look.
        std::fs::write(root.join("human.txt"), "human edit\n").unwrap();
        let report = guard.rollback();
        assert_eq!(
            std::fs::read(root.join("tracked.txt")).unwrap(),
            b"orig\r\nline two\r\n"
        );
        assert!(!root.join("made.txt").exists());
        assert!(!root.join("gen").exists(), "command-made dir removed");
        assert_eq!(report.restored, ["tracked.txt"]);
        assert_eq!(report.removed, ["gen/deep.txt", "made.txt"]);
        assert_eq!(report.removed_dirs, ["gen"]);
        let archive = root.join(".polymorph").join("attempts").join("t");
        assert!(std::fs::read_to_string(archive.join("made.txt"))
            .unwrap()
            .starts_with("gen"));
        assert!(std::fs::read_to_string(archive.join("tracked.txt"))
            .unwrap()
            .starts_with("sed-edit"));
        // Left alone: ignored, dirty/untracked at start, conflicted.
        assert!(root.join("build.log").exists());
        assert!(std::fs::read_to_string(root.join("dirty.txt"))
            .unwrap()
            .starts_with("more"));
        assert!(std::fs::read_to_string(root.join("pre.txt"))
            .unwrap()
            .starts_with("again"));
        assert_eq!(
            std::fs::read_to_string(root.join("human.txt")).unwrap(),
            "human edit\n"
        );
        assert_eq!(report.conflicts, ["human.txt"]);
        let unrestorable: Vec<&str> = report
            .unrestorable
            .iter()
            .map(|(p, _)| p.as_str())
            .collect();
        assert_eq!(unrestorable, ["pre.txt", "dirty.txt"], "{report:?}");
        assert!(report.unrestorable[1].1.contains("uncommitted changes"));
        assert!(report.failed.is_empty(), "{report:?}");
        // The conflicted file is the only new `git status` entry left.
        assert_eq!(report.left_behind, Some(vec!["?? human.txt".to_string()]));
    }

    /// Live-drive regression: under `core.autocrlf=true` a file written
    /// with LF is clean in `git status`, yet its checkout form is CRLF —
    /// the drive-start size picks the form that was on disk (LF stays LF,
    /// a checked-out CRLF file stays CRLF), and a mixed-ending file, which
    /// matches neither form, is reported instead of rewritten.
    #[tokio::test]
    async fn command_changes_restore_the_drive_start_line_endings() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .output()
        };
        if !git(&["init", "-q"]).is_ok_and(|o| o.status.success()) {
            eprintln!("SKIP command_changes_restore_the_drive_start_line_endings: no git");
            return;
        }
        for args in [
            &["config", "user.name", "t"][..],
            &["config", "user.email", "t@example.invalid"],
            &["config", "core.autocrlf", "true"],
            &["config", "commit.gpgsign", "false"],
        ] {
            assert!(git(args).unwrap().status.success());
        }
        std::fs::write(root.join("lf.py"), b"debug = True\nname = 1\n").unwrap();
        std::fs::write(root.join("crlf.py"), b"a = 1\nb = 2\n").unwrap();
        std::fs::write(root.join("mixed.py"), b"x = 1\r\ny = 2\nz = 3\n").unwrap();
        assert!(git(&["add", "-A"]).unwrap().status.success());
        assert!(git(&["commit", "-q", "-m", "base"])
            .unwrap()
            .status
            .success());
        // A real checkout form for crlf.py (CRLF); lf.py and mixed.py keep
        // the bytes they were written with, all three clean.
        std::fs::remove_file(root.join("crlf.py")).unwrap();
        assert!(git(&["checkout", "--", "crlf.py"])
            .unwrap()
            .status
            .success());
        assert_eq!(
            std::fs::read(root.join("crlf.py")).unwrap(),
            b"a = 1\r\nb = 2\r\n"
        );
        let status = git(&["status", "--porcelain"]).unwrap();
        assert!(status.stdout.is_empty(), "{status:?}");
        let l = Arc::new(ledger(root));
        let guard = RollbackGuard::new(l.clone());
        let entries = command_turn(
            root,
            &l,
            &["echo DEBUG = False> lf.py && echo c> crlf.py && echo w> mixed.py"],
        )
        .await;
        assert_eq!(
            entries[0]["changed_paths"],
            serde_json::json!(["crlf.py", "lf.py", "mixed.py"])
        );
        let report = guard.rollback();
        assert_eq!(
            std::fs::read(root.join("lf.py")).unwrap(),
            b"debug = True\nname = 1\n",
            "LF original restored as LF, not the CRLF checkout form"
        );
        assert_eq!(
            std::fs::read(root.join("crlf.py")).unwrap(),
            b"a = 1\r\nb = 2\r\n"
        );
        assert_eq!(report.restored, ["lf.py", "crlf.py"], "{report:?}");
        assert!(std::fs::read_to_string(root.join("mixed.py"))
            .unwrap()
            .starts_with('w'));
        assert_eq!(report.unrestorable.len(), 1, "{report:?}");
        assert_eq!(report.unrestorable[0].0, "mixed.py");
        assert!(report.unrestorable[0].1.contains("not provable"));
    }

    /// Outside git only absence is provable: a command-created file is
    /// archived and deleted, a command-modified one is reported (never
    /// guessed back), an ignored one is never seen.
    #[tokio::test]
    async fn command_changes_outside_git_remove_created_files_only() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        std::fs::write(root.join("existing.txt"), "e\n").unwrap();
        std::fs::write(root.join(".gitignore"), "*.log\n").unwrap();
        let l = Arc::new(ledger(root));
        let guard = RollbackGuard::new(l.clone());
        if guard.git_start.is_some() {
            eprintln!(
                "SKIP command_changes_outside_git_remove_created_files_only: temp dir in a repo"
            );
            return;
        }
        let entries = command_turn(
            root,
            &l,
            &[
                "echo changed> existing.txt",
                "echo new> created.txt",
                "echo junk> out.log",
            ],
        )
        .await;
        assert_eq!(
            entries[1]["changed_paths"],
            serde_json::json!(["created.txt"])
        );
        assert!(entries[2].get("changed_paths").is_none());
        let report = guard.rollback();
        assert!(!root.join("created.txt").exists());
        assert!(root.join("out.log").exists());
        assert!(std::fs::read_to_string(root.join("existing.txt"))
            .unwrap()
            .starts_with("changed"));
        assert_eq!(report.removed, ["created.txt"]);
        assert_eq!(report.unrestorable.len(), 1, "{report:?}");
        assert_eq!(report.unrestorable[0].0, "existing.txt");
        assert!(report.unrestorable[0].1.contains("outside git"));
        let lines = report.summary_lines();
        assert!(lines[0].starts_with("ROLLBACK: restored 0 modified files, removed 1 created file"));
        assert!(lines.iter().any(|l| l.starts_with("NOTE: not a git repo")));
    }

    /// Without a captured drive start (no guard armed), command changes are
    /// tracked but never restored: nothing proves their originals.
    #[test]
    fn command_changes_without_drive_start_are_reported_only() {
        use crate::command_changes::CommandChange;
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        std::fs::write(root.join("new.txt"), "made").unwrap();
        let l = ledger(root);
        l.after_command(
            root,
            &TreeDiff {
                changes: vec![CommandChange {
                    rel: "new.txt".into(),
                    kind: ChangeKind::Created,
                    before: None,
                }],
                created_dirs: Vec::new(),
            },
        );
        let report = l.rollback();
        assert!(root.join("new.txt").exists());
        assert!(report.unrestorable[0].1.contains("not captured"));
    }
}
