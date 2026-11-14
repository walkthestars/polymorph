//! `polymorph clean` — reclaim local harness artifacts older than a cutoff.
//!
//! Four artifact classes live under `ROOT/.polymorph` (default ROOT `.`):
//! `logs/` (rolling file logs), `attempts/` (rolled-back drive archives whose
//! directory names ARE their stamps: `<unix-secs>-<pid>-<n>`), `tool-output/`
//! (spill files) — plus session ROWS in the store DB (the global `--db`, else
//! `ROOT/.polymorph/sessions.db`).
//!
//! Age rule: an entry is old when its age in unix seconds is strictly below
//! `now - days * 86400`; `--older-than-days 0` means "older than now". Logs and
//! tool-output age by mtime; attempts age by the leading unix seconds of their
//! NAME (mtime fallback — stamps survive copies that reset mtimes); sessions
//! age by `updated_at`.
//!
//! Safety rails: `--dry-run` reports without touching anything; session rows
//! are only deleted with `--force`; with `--older-than-days 0` the newest log
//! is always spared (the active log). A deletion error names the path, never
//! aborts the remaining classes, and fails the command (non-zero exit) after
//! every class has run.

use std::path::{Path, PathBuf};

const SECS_PER_DAY: i64 = 86_400;

/// Parsed `polymorph clean` flags; the dispatcher in `main.rs` fills this from
/// the `Commands::Clean` variant and passes it here.
#[derive(Debug, Clone)]
pub struct CleanArgs {
    /// Root whose `.polymorph` state is cleaned.
    pub root: PathBuf,
    /// Entries older than this many days are removed (0 = everything older
    /// than now).
    pub older_than_days: u64,
    /// Report what would go, delete nothing.
    pub dry_run: bool,
    /// Select `.polymorph/logs`.
    pub logs: bool,
    /// Select `.polymorph/attempts`.
    pub attempts: bool,
    /// Select `.polymorph/tool-output`.
    pub tool_output: bool,
    /// Select session rows (only takes effect with `force`).
    pub sessions: bool,
    /// Consent for session deletion — the only class that needs it.
    pub force: bool,
    /// Select every class (sessions still need `force`).
    pub all: bool,
}

/// The four artifact classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Logs,
    Attempts,
    ToolOutput,
    Sessions,
}

/// Running totals across classes; session rows add to `count` (no bytes apply
/// — the DB file itself stays).
#[derive(Default)]
struct Totals {
    count: u64,
    bytes: u64,
}

/// Selector resolution: `--all` = every class; explicit selectors = exactly
/// those; no selector at all = the safe default (logs, attempts, tool-output —
/// session history is the operator's data and never deleted by default).
fn selected_classes(args: &CleanArgs) -> Vec<Class> {
    if args.all {
        return vec![
            Class::Logs,
            Class::Attempts,
            Class::ToolOutput,
            Class::Sessions,
        ];
    }
    let picked: Vec<Class> = [
        (args.logs, Class::Logs),
        (args.attempts, Class::Attempts),
        (args.tool_output, Class::ToolOutput),
        (args.sessions, Class::Sessions),
    ]
    .into_iter()
    .filter(|(on, _)| *on)
    .map(|(_, class)| class)
    .collect();
    if picked.is_empty() {
        vec![Class::Logs, Class::Attempts, Class::ToolOutput]
    } else {
        picked
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Cutoff in unix seconds: an entry is old when its age is strictly below it.
/// `days = 0` → now ("everything older than now").
fn cutoff_unix(days: u64, now: i64) -> i64 {
    let days = days.min(i64::MAX as u64) as i64;
    now.saturating_sub(days.saturating_mul(SECS_PER_DAY))
}

/// mtime as unix seconds; a pre-epoch clock reads as 0 (ancient, so old).
fn mtime_unix(meta: &std::fs::Metadata) -> i64 {
    use std::time::SystemTime;
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Leading unix seconds of an attempt dir name (`1767225600-4242-0` →
/// 1767225600). `None` when the name has no leading integer — the caller
/// falls back to mtime.
fn attempt_stamp_secs(name: &str) -> Option<i64> {
    name.split('-').next()?.parse::<i64>().ok()
}

/// Attempt age: the name stamp when present (it survives copies that reset
/// mtimes), else the directory mtime.
fn attempt_age_secs(name: &str, mtime: i64) -> i64 {
    attempt_stamp_secs(name).unwrap_or(mtime)
}

/// Regular files directly under `dir` whose mtime is older than `cutoff`,
/// with their sizes. A missing dir is an empty selection — nothing to clean
/// is not an error. `spare_newest` drops the newest file from the selection:
/// the `--older-than-days 0` active-log guard (the active log is the newest
/// file, so it is never removed whatever the clock says).
fn old_files(dir: &Path, cutoff: i64, spare_newest: bool) -> std::io::Result<Vec<(PathBuf, u64)>> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut old: Vec<(PathBuf, i64, u64)> = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        if !meta.is_file() {
            continue; // subdirs and symlinks are not this class's business
        }
        let mtime = mtime_unix(&meta);
        if mtime < cutoff {
            old.push((entry.path(), mtime, meta.len()));
        }
    }
    if spare_newest && !old.is_empty() {
        // Newest last (name breaks mtime ties for determinism), then spared.
        old.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        old.pop();
    }
    Ok(old.into_iter().map(|(path, _, len)| (path, len)).collect())
}

/// Attempt directories directly under `dir` whose age (name stamp, mtime
/// fallback) is older than `cutoff`, sized recursively. A missing dir is an
/// empty selection. Plain files (the `.gitignore` the restore ledger parks
/// here) are not attempt archives and stay.
fn old_attempt_dirs(dir: &Path, cutoff: i64) -> std::io::Result<Vec<(PathBuf, u64)>> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut old = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        if !meta.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if attempt_age_secs(&name, mtime_unix(&meta)) < cutoff {
            old.push((entry.path(), dir_size(&entry.path())));
        }
    }
    Ok(old)
}

/// Recursive byte size of a tree, best-effort: unreadable subtrees count as
/// zero (sizes are report material; removal failures are surfaced separately
/// and name the path).
fn dir_size(root: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                total += meta.len();
            }
        }
    }
    total
}

/// Per-class report line, e.g. `logs: would remove 2 file(s), 150 bytes`.
fn report_line(class: &str, dry_run: bool, count: u64, bytes: u64, unit: &str) -> String {
    let verb = if dry_run { "would remove" } else { "removed" };
    format!("{class}: {verb} {count} {unit}, {bytes} bytes")
}

/// Sessions report line — rows, not files, so no byte count applies.
fn sessions_line(dry_run: bool, count: u64) -> String {
    let verb = if dry_run { "would remove" } else { "removed" };
    format!("sessions: {verb} {count} session(s)")
}

/// Record one failed removal: it names the path and the run keeps going — the
/// rest of the batch and every later class still run; `run` fails at the end.
fn note_removal_failure(errors: &mut Vec<String>, path: &Path, err: &std::io::Error) {
    errors.push(format!("cannot remove {}: {err}", path.display()));
}

/// The end-of-run error once any removal failed: joins every path-naming
/// failure so the non-zero exit carries them all.
fn failures_error(errors: &[String]) -> anyhow::Error {
    anyhow::anyhow!(
        "clean finished with {} deletion error(s): {}",
        errors.len(),
        errors.join("; ")
    )
}

/// Delete the scanned entries — or, on `--dry-run`, just count them. `dirs`
/// swaps `remove_file` for `remove_dir_all` (attempts are trees). Failures are
/// recorded, not raised.
fn apply_removals(
    entries: &[(PathBuf, u64)],
    dry_run: bool,
    dirs: bool,
    errors: &mut Vec<String>,
) -> (u64, u64) {
    let mut count = 0;
    let mut bytes = 0;
    for (path, len) in entries {
        if dry_run {
            count += 1;
            bytes += len;
            continue;
        }
        let removed = if dirs {
            std::fs::remove_dir_all(path)
        } else {
            std::fs::remove_file(path)
        };
        match removed {
            Ok(()) => {
                count += 1;
                bytes += len;
            }
            Err(e) => note_removal_failure(errors, path, &e),
        }
    }
    (count, bytes)
}

/// One file class (logs / tool-output): scan, remove (or count on dry-run),
/// print the class line, fold into the totals.
fn clean_file_class(
    name: &str,
    dir: &Path,
    cutoff: i64,
    spare_newest: bool,
    dry_run: bool,
    errors: &mut Vec<String>,
    totals: &mut Totals,
) {
    match old_files(dir, cutoff, spare_newest) {
        Ok(entries) => {
            let (count, bytes) = apply_removals(&entries, dry_run, false, errors);
            println!("{}", report_line(name, dry_run, count, bytes, "file(s)"));
            totals.count += count;
            totals.bytes += bytes;
        }
        Err(e) => errors.push(format!("{name}: cannot list {}: {e}", dir.display())),
    }
}

/// The attempts class: like a file class, but directories, aged by name stamp
/// and removed recursively.
fn clean_attempts_class(
    dir: &Path,
    cutoff: i64,
    dry_run: bool,
    errors: &mut Vec<String>,
    totals: &mut Totals,
) {
    match old_attempt_dirs(dir, cutoff) {
        Ok(entries) => {
            let (count, bytes) = apply_removals(&entries, dry_run, true, errors);
            println!(
                "{}",
                report_line("attempts", dry_run, count, bytes, "dir(s)")
            );
            totals.count += count;
            totals.bytes += bytes;
        }
        Err(e) => errors.push(format!("attempts: cannot list {}: {e}", dir.display())),
    }
}

/// The sessions class: delete rows with `updated_at < cutoff` (messages, reads
/// and plans cascade via the store's foreign keys) — or, on `--dry-run`, just
/// count them. Without `--force` the class is skipped with a note (it is the
/// only class that needs consent). A missing DB file is a no-op note: clean
/// must never create a store.
async fn clean_sessions_class(
    db: &Path,
    cutoff: i64,
    force: bool,
    dry_run: bool,
    errors: &mut Vec<String>,
    totals: &mut Totals,
) {
    if !force {
        println!("sessions: skipped (--sessions only deletes session rows with --force)");
        return;
    }
    if !db.is_file() {
        println!("sessions: no database at {} — nothing to do", db.display());
        return;
    }
    use polymorph_core::sessions::Store;
    let store = match Store::open(db).await {
        Ok(store) => store,
        Err(e) => {
            errors.push(format!("sessions: cannot open {}: {e}", db.display()));
            return;
        }
    };
    let count = if dry_run {
        // Count without deleting: dry-run removes nothing, rows included.
        store
            .list_sessions()
            .await
            .map(|all| all.iter().filter(|s| s.updated_at < cutoff).count() as u64)
    } else {
        store
            .delete_sessions_older_than(cutoff)
            .await
            .map(|n| n as u64)
    };
    match count {
        Ok(n) => {
            println!("{}", sessions_line(dry_run, n));
            totals.count += n; // rows are items; no bytes apply
        }
        Err(e) => errors.push(format!("sessions: {}: {e}", db.display())),
    }
}

/// `polymorph clean`. `global_db` is the global `--db` override; `None` means
/// `ROOT/.polymorph/sessions.db`. Prints one line per selected class plus a
/// total. Returns `Err` — non-zero exit — iff any removal failed, after every
/// class has run.
pub async fn run(args: CleanArgs, global_db: Option<&Path>) -> anyhow::Result<()> {
    let cutoff = cutoff_unix(args.older_than_days, now_unix());
    let harness = args.root.join(".polymorph");
    let mut errors: Vec<String> = Vec::new();
    let mut totals = Totals::default();

    for class in selected_classes(&args) {
        match class {
            Class::Logs => clean_file_class(
                "logs",
                &harness.join("logs"),
                cutoff,
                // `--older-than-days 0` must never eat the active log.
                args.older_than_days == 0,
                args.dry_run,
                &mut errors,
                &mut totals,
            ),
            Class::ToolOutput => clean_file_class(
                "tool-output",
                &harness.join("tool-output"),
                cutoff,
                false,
                args.dry_run,
                &mut errors,
                &mut totals,
            ),
            Class::Attempts => clean_attempts_class(
                &harness.join("attempts"),
                cutoff,
                args.dry_run,
                &mut errors,
                &mut totals,
            ),
            Class::Sessions => {
                // The global `--db` wins; else the ROOT-scoped default.
                let db = global_db
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| harness.join("sessions.db"));
                clean_sessions_class(
                    &db,
                    cutoff,
                    args.force,
                    args.dry_run,
                    &mut errors,
                    &mut totals,
                )
                .await;
            }
        }
    }

    let verb = if args.dry_run {
        "would remove"
    } else {
        "removed"
    };
    println!(
        "total: {verb} {} item(s), {} bytes",
        totals.count, totals.bytes
    );
    if errors.is_empty() {
        Ok(())
    } else {
        Err(failures_error(&errors))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;
    use std::fs::File;
    use std::time::{Duration, SystemTime};

    /// Overwrite mtime. Opened with write access — Windows grants the
    /// FILE_WRITE_ATTRIBUTES that SetFileTime needs only through it.
    fn set_mtime(path: &Path, at: SystemTime) {
        let f = File::options().write(true).open(path).unwrap();
        f.set_times(std::fs::FileTimes::new().set_accessed(at).set_modified(at))
            .unwrap();
    }

    /// Default-shaped args rooted at `root` (no selectors, 7-day cutoff).
    fn args(root: &Path) -> CleanArgs {
        CleanArgs {
            root: root.to_path_buf(),
            older_than_days: 7,
            dry_run: false,
            logs: false,
            attempts: false,
            tool_output: false,
            sessions: false,
            force: false,
            all: false,
        }
    }

    #[test]
    fn clean_flags_parse_with_defaults() {
        let cli = crate::Cli::try_parse_from(["polymorph", "clean"]).unwrap();
        match cli.command {
            Some(crate::Commands::Clean {
                root,
                older_than_days,
                dry_run,
                logs,
                attempts,
                tool_output,
                sessions,
                force,
                all,
            }) => {
                assert_eq!(root, PathBuf::from("."));
                assert_eq!(older_than_days, 7);
                assert!(!dry_run);
                assert!(!logs && !attempts && !tool_output && !sessions);
                assert!(!force && !all);
            }
            other => panic!("wrong command: {other:?}"),
        }
        // The full flag surface parses (kebab-case --tool-output included).
        let cli = crate::Cli::try_parse_from([
            "polymorph",
            "clean",
            "--root",
            "some/root",
            "--older-than-days",
            "0",
            "--dry-run",
            "--logs",
            "--attempts",
            "--tool-output",
            "--sessions",
            "--force",
            "--all",
        ])
        .unwrap();
        match cli.command {
            Some(crate::Commands::Clean {
                root,
                older_than_days,
                dry_run,
                logs,
                attempts,
                tool_output,
                sessions,
                force,
                all,
            }) => {
                assert_eq!(root, PathBuf::from("some/root"));
                assert_eq!(older_than_days, 0);
                assert!(dry_run && logs && attempts && tool_output && sessions && force && all);
            }
            other => panic!("wrong command: {other:?}"),
        }
    }

    #[test]
    fn selection_defaults_to_files_all_selects_every_class() {
        let with = |logs, attempts, tool_output, sessions, all| CleanArgs {
            logs,
            attempts,
            tool_output,
            sessions,
            all,
            ..args(Path::new("."))
        };
        // No selector: the safe file default — sessions never default on.
        assert_eq!(
            selected_classes(&with(false, false, false, false, false)),
            vec![Class::Logs, Class::Attempts, Class::ToolOutput]
        );
        // Explicit selectors pick exactly those.
        assert_eq!(
            selected_classes(&with(true, false, false, false, false)),
            vec![Class::Logs]
        );
        assert_eq!(
            selected_classes(&with(false, true, false, false, false)),
            vec![Class::Attempts]
        );
        assert_eq!(
            selected_classes(&with(false, false, true, false, false)),
            vec![Class::ToolOutput]
        );
        assert_eq!(
            selected_classes(&with(false, false, false, true, false)),
            vec![Class::Sessions]
        );
        // --all selects every class (force still gates sessions at run time).
        assert_eq!(
            selected_classes(&with(false, false, false, false, true)),
            vec![
                Class::Logs,
                Class::Attempts,
                Class::ToolOutput,
                Class::Sessions
            ]
        );
    }

    #[test]
    fn cutoff_math_days_and_zero() {
        assert_eq!(cutoff_unix(7, 1_000_000), 1_000_000 - 7 * 86_400);
        assert_eq!(cutoff_unix(0, 1_000_000), 1_000_000, "0 = older than now");
        // Absurd day counts saturate instead of panicking on overflow.
        assert!(cutoff_unix(u64::MAX, 1_000_000) < 0);
    }

    #[test]
    fn attempt_age_uses_the_stamp_with_mtime_fallback() {
        // Leading unix seconds win over any mtime.
        assert_eq!(attempt_stamp_secs("1767225600-4242-0"), Some(1_767_225_600));
        assert_eq!(
            attempt_age_secs("1767225600-4242-0", 999_999),
            1_767_225_600
        );
        // No leading integer → the directory mtime is the age.
        assert_eq!(attempt_stamp_secs("restore-copy"), None);
        assert_eq!(attempt_stamp_secs("-42-0"), None);
        assert_eq!(attempt_stamp_secs("12x-0"), None);
        assert_eq!(attempt_age_secs("restore-copy", 1234), 1234);
    }

    #[test]
    fn report_lines_match_the_shape() {
        assert_eq!(
            report_line("logs", true, 2, 150, "file(s)"),
            "logs: would remove 2 file(s), 150 bytes"
        );
        assert_eq!(
            report_line("tool-output", false, 0, 0, "file(s)"),
            "tool-output: removed 0 file(s), 0 bytes"
        );
        assert_eq!(
            report_line("attempts", true, 1, 4096, "dir(s)"),
            "attempts: would remove 1 dir(s), 4096 bytes"
        );
        assert_eq!(sessions_line(false, 3), "sessions: removed 3 session(s)");
        assert_eq!(
            sessions_line(true, 3),
            "sessions: would remove 3 session(s)"
        );
    }

    #[test]
    fn deletion_failure_names_the_path_and_fails_the_run() {
        let mut errors = Vec::new();
        let path = Path::new("root/.polymorph/logs/polymorph.2020.json");
        note_removal_failure(&mut errors, path, &std::io::Error::other("locked"));
        assert_eq!(errors.len(), 1, "one failure recorded, run continues");
        assert!(
            errors[0].contains(&path.display().to_string()),
            "{}",
            errors[0]
        );
        // The end-of-run error carries the path so the non-zero exit names it.
        let msg = failures_error(&errors).to_string();
        assert!(msg.contains("1 deletion error"), "{msg}");
        assert!(msg.contains(&path.display().to_string()), "{msg}");
    }

    #[tokio::test]
    async fn old_log_removed_new_log_kept() {
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join(".polymorph").join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        let old = logs.join("polymorph.2020-01-01.json");
        std::fs::write(&old, b"stale forensics").unwrap();
        let fresh = logs.join("polymorph.today.json");
        std::fs::write(&fresh, b"active log").unwrap();
        let now = SystemTime::now();
        set_mtime(&old, now - Duration::from_secs(30 * 86_400));
        set_mtime(&fresh, now);

        // No selectors: the default selection (logs, attempts, tool-output).
        run(args(dir.path()), None).await.unwrap();

        assert!(!old.exists(), "log older than the 7-day cutoff is removed");
        assert!(fresh.exists(), "log newer than the cutoff is kept");
    }

    #[tokio::test]
    async fn dry_run_removes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let harness = dir.path().join(".polymorph");
        let logs = harness.join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        let old_log = logs.join("polymorph.2020.json");
        std::fs::write(&old_log, b"stale").unwrap();
        set_mtime(
            &old_log,
            SystemTime::now() - Duration::from_secs(30 * 86_400),
        );
        let attempts = harness.join("attempts");
        let attempt = attempts.join(format!("{}-7-0", now_unix() - 30 * 86_400));
        std::fs::create_dir_all(&attempt).unwrap();
        std::fs::write(attempt.join("f.txt"), b"archived change").unwrap();
        let tool_output = harness.join("tool-output");
        std::fs::create_dir_all(&tool_output).unwrap();
        let spill = tool_output.join("1-2-3.stdout.log");
        std::fs::write(&spill, b"spill").unwrap();
        set_mtime(&spill, SystemTime::now() - Duration::from_secs(30 * 86_400));

        let mut a = args(dir.path());
        a.dry_run = true;
        run(a, None).await.unwrap();

        assert!(old_log.exists(), "dry-run must not remove logs");
        assert!(attempt.exists(), "dry-run must not remove attempts");
        assert!(spill.exists(), "dry-run must not remove tool-output");
    }

    #[tokio::test]
    async fn attempts_age_by_the_stamp_in_the_name() {
        let dir = tempfile::tempdir().unwrap();
        let attempts = dir.path().join(".polymorph").join("attempts");
        std::fs::create_dir_all(&attempts).unwrap();
        // Ancient NAME stamp, fresh mtime (created this second): only the
        // stamp can make this dir old.
        let stale = attempts.join(format!("{}-4242-0", now_unix() - 30 * 86_400));
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(stale.join("rolled-back.txt"), b"archived change").unwrap();
        // Fresh stamp: kept by the 7-day default cutoff.
        let fresh = attempts.join(format!("{}-4242-1", now_unix()));
        std::fs::create_dir_all(&fresh).unwrap();

        run(args(dir.path()), None).await.unwrap();

        assert!(
            !stale.exists(),
            "stamp-old attempt is removed despite fresh mtime (recursively)"
        );
        assert!(fresh.exists(), "stamp-fresh attempt stays");
    }

    #[tokio::test]
    async fn zero_days_spares_the_newest_log() {
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join(".polymorph").join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        let older = logs.join("a.json");
        let newer = logs.join("b.json");
        std::fs::write(&older, b"x").unwrap();
        std::fs::write(&newer, b"x").unwrap();
        // Both strictly before "now": with --older-than-days 0 both are old,
        // but the newest (the active log) must be spared.
        set_mtime(&older, SystemTime::now() - Duration::from_secs(600));
        set_mtime(&newer, SystemTime::now() - Duration::from_secs(60));

        let mut a = args(dir.path());
        a.older_than_days = 0;
        a.logs = true;
        run(a, None).await.unwrap();

        assert!(!older.exists(), "everything older than now goes");
        assert!(newer.exists(), "the newest log (the active one) is spared");
    }

    #[tokio::test]
    async fn sessions_without_force_are_skipped() {
        use polymorph_core::sessions::Store;
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join(".polymorph").join("sessions.db");
        let store = Store::open(&db).await.unwrap();
        let kept = store.create_session("operator history").await.unwrap();
        store.close().await.unwrap();

        let mut a = args(dir.path());
        a.sessions = true; // selected without --force
        run(a, None).await.unwrap(); // skipped with a note, not an error

        let store = Store::open(&db).await.unwrap();
        let list = store.list_sessions().await.unwrap();
        assert_eq!(list.len(), 1, "sessions are never deleted without --force");
        assert_eq!(list[0].id, kept.id);
    }

    #[tokio::test]
    async fn sessions_with_force_removes_old_rows_and_keeps_new() {
        use polymorph_core::sessions::{MessageRole, Store};
        let dir = tempfile::tempdir().unwrap();
        // No global --db passed: the ROOT-scoped default must be used.
        let db = dir.path().join(".polymorph").join("sessions.db");
        let store = Store::open(&db).await.unwrap();
        let old = store.create_session("old").await.unwrap();
        let fresh = store.create_session("fresh").await.unwrap();
        store
            .append_message(&old.id, MessageRole::User, "dies with the session")
            .await
            .unwrap();
        store.close().await.unwrap();
        // Backdate the old row with rusqlite directly (the drift seam).
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute(
                "UPDATE sessions SET updated_at = ?1 WHERE id = ?2",
                rusqlite::params![1, old.id],
            )
            .unwrap();
        }

        let mut a = args(dir.path());
        a.sessions = true;
        a.force = true;
        run(a, None).await.unwrap();

        let store = Store::open(&db).await.unwrap();
        let list = store.list_sessions().await.unwrap();
        assert_eq!(list.len(), 1, "only the old row is removed");
        assert_eq!(list[0].id, fresh.id);
        // The cascade took the old session's messages with it.
        assert!(store.history(&old.id, 0).await.unwrap().is_empty());
    }
}
