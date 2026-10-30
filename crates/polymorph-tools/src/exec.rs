//! Non-interactive shell execution (Epic B1).
//!
//! Scope: `tokio::process` only. Interactive PTY (portable-pty/ConPTY) is
//! Epic B2 and lives in a separate module when it lands — this file must not
//! grow PTY code paths.
//!
//! Guarantees: timeout kills the whole child (no orphans); a cancelled token
//! kills the child and returns `Interrupted`, never partial success.
//! Output is read incrementally into [`crate::output::StreamCapture`]s
//! (docket #9): [`run_bash_streamed`] spills large streams to files instead
//! of holding them in memory.

use std::time::Duration;
use tokio_util::sync::CancellationToken;

use crate::output::{CapturedStream, SpillConfig, SpillTarget, StreamCapture};

/// Outcome of one finished (non-killed) command.
#[derive(Debug)]
pub struct ExecResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

/// Failure modes. `TimedOut` and `Interrupted` both imply the child was
/// killed before returning — there is no partial-result variant by design
/// (only [`run_bash_streamed`] keeps the output read before a timeout,
/// beside the error, never as a success).
#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    #[error("command timed out after {0:?} (child killed)")]
    TimedOut(Duration),
    #[error("execution cancelled (child killed)")]
    Interrupted,
    #[error("spawn failed: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("output was not valid UTF-8 (lossy-decoded): {0}")]
    NonUtf8(String),
}

/// Run `cmd` via the platform shell (`cmd /C` on Windows, `sh -c` elsewhere).
/// Captures stdout/stderr fully (lossy-decoded — byte-exact fidelity is NOT
/// promised here; the PTY path in B2 owns terminal fidelity).
pub async fn run_bash(
    cmd: &str,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<ExecResult, ExecError> {
    run_bash_with_env(cmd, timeout, cancel, &[]).await
}

/// `run_bash` plus per-child env overrides (chunk 95): values ride the
/// spawned child's environment ONLY — never the process globals (the
/// `set_var` race: parallel hook runs crossed each other's rows).
/// Empty `env` behaves exactly like `run_bash`.
pub async fn run_bash_with_env(
    cmd: &str,
    timeout: Duration,
    cancel: &CancellationToken,
    env: &[(&str, &str)],
) -> Result<ExecResult, ExecError> {
    run_shell(cmd, timeout, cancel, env, None).await
}

/// `run_bash_with_env` in an explicit working directory (docket #2 verify
/// gate: operator commands run in the drive's workdir, not the process
/// cwd). Same shell, quoting, timeout and tree-kill guarantees; a missing
/// `cwd` surfaces as [`ExecError::Spawn`].
pub async fn run_bash_in(
    cmd: &str,
    timeout: Duration,
    cancel: &CancellationToken,
    env: &[(&str, &str)],
    cwd: &std::path::Path,
) -> Result<ExecResult, ExecError> {
    run_shell(cmd, timeout, cancel, env, Some(cwd)).await
}

async fn run_shell(
    cmd: &str,
    timeout: Duration,
    cancel: &CancellationToken,
    env: &[(&str, &str)],
    cwd: Option<&std::path::Path>,
) -> Result<ExecResult, ExecError> {
    let mut stdout = StreamCapture::new(None);
    let mut stderr = StreamCapture::new(None);
    let exit_code = run_captured(cmd, timeout, cancel, env, cwd, &mut stdout, &mut stderr).await?;
    Ok(ExecResult {
        stdout: stdout.finish().into_text(),
        stderr: stderr.finish().into_text(),
        exit_code,
    })
}

/// One `bash` run with bounded capture (docket #9): each stream over
/// `spill.inline_bytes` is written in full to its own file under
/// `<cwd>/.polymorph/tool-output/` WHILE it is read, and comes back as a
/// digest (see [`crate::output`]); streams under it come back exactly as
/// [`run_bash_in`] returns them. Same shell, timeout and tree-kill
/// guarantees as [`run_bash_in`].
#[derive(Debug)]
pub struct StreamedRun {
    /// Exit code, or why there is none. On [`ExecError::TimedOut`] the
    /// streams hold what was read before the kill; on `Interrupted` or a
    /// spawn failure they are empty (cancel never yields partial results).
    pub status: Result<i32, ExecError>,
    pub stdout: CapturedStream,
    pub stderr: CapturedStream,
}

/// [`StreamedRun`] for `cmd` in `cwd`. `spill: None` (or an
/// `inline_bytes` of 0) captures fully in memory, like [`run_bash_in`].
pub async fn run_bash_streamed(
    cmd: &str,
    timeout: Duration,
    cancel: &CancellationToken,
    cwd: &std::path::Path,
    spill: Option<&SpillConfig>,
) -> StreamedRun {
    let target = |label: &'static str, stem: &str| {
        spill.filter(|s| s.enabled()).map(|s| SpillTarget {
            workdir: cwd.to_path_buf(),
            stem: stem.to_string(),
            label,
            inline_bytes: s.inline_bytes,
            keep_files: s.keep_files,
        })
    };
    let stem = crate::output::next_stem();
    let mut stdout = StreamCapture::new(target("stdout", &stem));
    let mut stderr = StreamCapture::new(target("stderr", &stem));
    let status = run_captured(
        cmd,
        timeout,
        cancel,
        &[],
        Some(cwd),
        &mut stdout,
        &mut stderr,
    )
    .await;
    let empty = || CapturedStream::Inline(String::new());
    match status {
        Err(e @ (ExecError::Interrupted | ExecError::Spawn(_))) => StreamedRun {
            status: Err(e),
            stdout: empty(),
            stderr: empty(),
        },
        status => StreamedRun {
            status,
            stdout: stdout.finish(),
            stderr: stderr.finish(),
        },
    }
}

/// Spawn `cmd`, feed both pipes into the captures as bytes arrive, and
/// wait — the shared core of every entry point. The captures are the
/// caller's, so on timeout what was read before the kill stays readable.
async fn run_captured(
    cmd: &str,
    timeout: Duration,
    cancel: &CancellationToken,
    env: &[(&str, &str)],
    cwd: Option<&std::path::Path>,
    stdout: &mut StreamCapture,
    stderr: &mut StreamCapture,
) -> Result<i32, ExecError> {
    if cancel.is_cancelled() {
        return Err(ExecError::Interrupted);
    }
    let mut command = platform_command(cmd);
    for (key, value) in env {
        command.env(key, value);
    }
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    let mut child = command.spawn()?;
    // `kill_on_drop(true)` is configured on the command (see
    // `platform_command`): if our wait future is ever dropped
    // (timeout/cancel paths), the direct child is killed. Grandchildren need
    // `kill_tree_pid` — see its docs.
    let pid = child.id();
    let out_pipe = child.stdout.take();
    let err_pipe = child.stderr.take();

    // Owns the child (dropping the future kills it), reads both pipes to
    // EOF concurrently and waits — what `wait_with_output` did, but into
    // the bounded captures instead of unbounded buffers.
    let wait = async move {
        let (out, err, status) =
            tokio::join!(pump(out_pipe, stdout), pump(err_pipe, stderr), child.wait());
        out?;
        err?;
        status
    };
    tokio::pin!(wait);
    // Unix: if THIS future is dropped mid-wait (an outer select!/timeout),
    // `kill_on_drop` alone strands the group. Declared after `wait` so it
    // drops first, while the leader is still unreaped. Disarmed on success.
    #[cfg(unix)]
    let mut group_guard = GroupKillGuard(pid);
    let status = tokio::select! {
        _ = cancel.cancelled() => {
            kill_tree_pid(pid).await;
            return Err(ExecError::Interrupted);
        }
        out = tokio::time::timeout(timeout, &mut wait) => {
            match out {
                Ok(Ok(s)) => s,
                Ok(Err(e)) => return Err(ExecError::Spawn(e)),
                Err(_) => {
                    // Tree first, then `wait` is dropped → kill_on_drop
                    // finishes the direct child.
                    kill_tree_pid(pid).await;
                    return Err(ExecError::TimedOut(timeout));
                }
            }
        }
    };
    #[cfg(unix)]
    {
        group_guard.0 = None;
    }
    Ok(status.code().unwrap_or(-1))
}

/// Copy one child pipe into its capture until EOF.
async fn pump<R: tokio::io::AsyncRead + Unpin>(
    pipe: Option<R>,
    capture: &mut StreamCapture,
) -> std::io::Result<()> {
    use tokio::io::AsyncReadExt as _;
    let Some(mut pipe) = pipe else {
        return Ok(());
    };
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = pipe.read(&mut buf).await?;
        if n == 0 {
            return Ok(());
        }
        capture.push(&buf[..n]);
    }
}

/// Reap the process TREE rooted at `pid`, best-effort.
///
/// Windows: `taskkill /T /F` kills the whole tree (shell + grandchildren).
/// The direct child additionally dies via `kill_on_drop` when the wait
/// future drops, so a taskkill failure is non-fatal here.
///
/// Unix: the child leads its own process group (`process_group(0)` in
/// `platform_command`), so the group kill in [`kill_tree_sync`] reaches
/// shell-spawned grandchildren (e.g. `sleep` behind `sh -c`) that
/// `kill_on_drop` would miss.
async fn kill_tree_pid(pid: Option<u32>) {
    if let Some(id) = pid {
        let _ = tokio::task::spawn_blocking(move || kill_tree_sync(id)).await;
    }
}

/// Synchronous tree kill for `Drop` paths (see [`kill_tree_pid`]).
///
/// Windows: `taskkill /T /F`. Needed wherever a wrapper spawns the real
/// process as a child that inherits our pipes (e.g. `python jdtls` runs Java
/// via `subprocess.run`): `kill_on_drop` kills only the wrapper, the
/// grandchild keeps the pipe open, and Tokio's blocking pipe reader, and with
/// it runtime shutdown, waits forever.
///
/// Unix: `kill(-pid, SIGKILL)`, i.e. the whole process group `pid` leads.
/// Callers MUST have spawned `pid` with `process_group(0)` (see
/// `platform_command`, `LspSpec::command`); otherwise there is no such group
/// and the call is a harmless `ESRCH`. Errors (`ESRCH` once the group is
/// gone, so double kills are fine) are ignored. `pid` 0/1 and values that
/// overflow `pid_t` are refused: `kill(0, ..)` would signal OUR group and
/// `kill(-1, ..)` every process we may signal.
pub fn kill_tree_sync(pid: u32) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .output();
    }
    #[cfg(unix)]
    {
        if let Ok(pgid) = libc::pid_t::try_from(pid) {
            if pgid > 1 {
                // SAFETY: plain syscall, no memory is passed; a negative
                // pid addresses a process group (POSIX kill(2)).
                unsafe {
                    libc::kill(-pgid, libc::SIGKILL);
                }
            }
        }
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = pid;
    }
}

/// Group-kills on drop unless disarmed (`.0 = None`): the Unix backstop for
/// a `run_bash_with_env` future dropped mid-wait (see [`kill_tree_sync`]).
#[cfg(unix)]
struct GroupKillGuard(Option<u32>);

#[cfg(unix)]
impl Drop for GroupKillGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.0.take() {
            kill_tree_sync(pid);
        }
    }
}

/// Normalize POSIX `./exe` invocations to Windows `.\\exe`.
/// `cmd.exe` does not resolve executables with forward slashes, failing with
/// `'.' is not recognized as an internal or external command`.
pub fn normalize_windows_cmd(cmd: &str) -> String {
    let mut out = String::with_capacity(cmd.len());
    let chars: Vec<char> = cmd.chars().collect();
    let n = chars.len();
    let mut i = 0;
    let mut in_single_quote = false;
    let mut in_double_quote = false;

    while i < n {
        let ch = chars[i];
        if ch == '"' && !in_single_quote {
            in_double_quote = !in_double_quote;
            out.push(ch);
            i += 1;
        } else if ch == '\'' && !in_double_quote {
            in_single_quote = !in_single_quote;
            out.push(ch);
            i += 1;
        } else if !in_single_quote
            && !in_double_quote
            && ch == '.'
            && i + 1 < n
            && chars[i + 1] == '/'
        {
            let at_start = i == 0 || chars[..i].iter().all(|c| c.is_whitespace());
            let preceded_by_sep = if i > 0 {
                let prev_non_ws = chars[..i].iter().rev().find(|c| !c.is_whitespace());
                matches!(prev_non_ws, Some('&' | '|' | ';' | '(' | ')' | '^'))
            } else {
                false
            };
            let preceded_by_ws = i > 0 && chars[i - 1].is_whitespace();

            if at_start || preceded_by_sep || preceded_by_ws {
                out.push('.');
                out.push('\\');
                i += 2;
            } else {
                out.push(ch);
                i += 1;
            }
        } else {
            out.push(ch);
            i += 1;
        }
    }
    out
}

fn platform_command(cmd: &str) -> tokio::process::Command {
    // kill_on_drop: dropping the wait future (timeout/cancel) kills the
    // direct child. Tree reaping for grandchildren is `kill_tree_pid`; on
    // Unix that needs the child to lead its own process group.
    // Stdio MUST be piped here: the default is inherit, under which
    // wait_with_output returns empty buffers (exit codes still work — a
    // trap this module's own tests caught).
    use std::process::Stdio;
    let mut c = shell_command(cmd);
    c.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    c.kill_on_drop(true);
    #[cfg(unix)]
    c.process_group(0);
    c
}

/// `cmd /S /C "<cmd>"`, passed raw. `Command::args` would escape every
/// embedded `"` as `\"` (MSVCRT argv rules), but `cmd.exe` does its own
/// parsing and passes `\"` through literally, so `gradlew --tests "X"` got a
/// filter with quote characters in it. With `/S`, cmd strips exactly the
/// outer pair of quotes and runs the rest verbatim, including payloads that
/// themselves start with a quoted path.
#[cfg(windows)]
fn shell_command(cmd: &str) -> tokio::process::Command {
    let normalized = normalize_windows_cmd(cmd);
    let mut c = tokio::process::Command::new("cmd");
    c.raw_arg(format!("/S /C \"{normalized}\""));
    c
}

#[cfg(not(windows))]
fn shell_command(cmd: &str) -> tokio::process::Command {
    let mut c = tokio::process::Command::new("sh");
    c.args(["-c", cmd]);
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn echo_captures_stdout() {
        let cancel = CancellationToken::new();
        let r = run_bash("echo hello-tools", Duration::from_secs(10), &cancel)
            .await
            .unwrap();
        assert!(r.stdout.contains("hello-tools"));
        assert_eq!(r.exit_code, 0);
    }

    #[tokio::test]
    async fn nonzero_exit_is_reported_not_error() {
        let cancel = CancellationToken::new();
        let r = run_bash("exit 7", Duration::from_secs(10), &cancel)
            .await
            .unwrap();
        assert_eq!(r.exit_code, 7);
    }

    /// `run_bash_in` runs in the given directory; a missing one is a
    /// spawn error, never a silent fallback to the process cwd.
    #[tokio::test]
    async fn run_bash_in_uses_the_given_cwd() {
        let dir = std::env::temp_dir().join(format!("pm-exec-cwd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("marker-cwd.txt"), "x").unwrap();
        let cmd = if cfg!(windows) { "dir /b" } else { "ls" };
        let cancel = CancellationToken::new();
        let r = run_bash_in(cmd, Duration::from_secs(10), &cancel, &[], &dir)
            .await
            .unwrap();
        assert!(r.stdout.contains("marker-cwd.txt"), "{}", r.stdout);
        let missing = dir.join("no-such-subdir");
        let err = run_bash_in(cmd, Duration::from_secs(10), &cancel, &[], &missing)
            .await
            .unwrap_err();
        assert!(matches!(err, ExecError::Spawn(_)), "{err:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn stderr_is_captured() {
        let cmd = if cfg!(windows) {
            "echo oops 1>&2"
        } else {
            "echo oops >&2"
        };
        let cancel = CancellationToken::new();
        let r = run_bash(cmd, Duration::from_secs(10), &cancel)
            .await
            .unwrap();
        assert!(r.stderr.contains("oops"));
    }

    #[tokio::test]
    async fn timeout_kills_and_reports() {
        // Blocker chosen per-platform: unambiguous binary names (Git's
        // MSYS bin dir shadows some inbox names, e.g. GNU `timeout` beats
        // timeout.exe) and no nested quotes (cmd /C quote-stripping). Both
        // guaranteed present on their host OS.
        let cmd = if cfg!(windows) {
            "powershell -NoProfile -Command Start-Sleep -Seconds 30"
        } else {
            "sleep 30"
        };
        let cancel = CancellationToken::new();
        let err = run_bash(cmd, Duration::from_millis(500), &cancel)
            .await
            .unwrap_err();
        assert!(matches!(err, ExecError::TimedOut(_)));
    }

    /// Double quotes must reach the program exactly as written. On Windows,
    /// `Command::args` escapes an embedded `"` as `\"` (MSVCRT rules), which
    /// `cmd.exe` does not understand: `gradlew --tests "X"` then received the
    /// filter with literal quote characters and found no tests (Frontier
    /// UNIT-2.1 SSIM drive, 2026-09-27).
    #[tokio::test]
    async fn double_quotes_reach_the_program_unescaped() {
        let cancel = CancellationToken::new();
        let r = run_bash(
            "echo \"a b\" && echo x\"y\"z",
            Duration::from_secs(10),
            &cancel,
        )
        .await
        .unwrap();
        assert_eq!(r.exit_code, 0);
        let lines: Vec<&str> = r.stdout.lines().map(str::trim_end).collect();
        if cfg!(windows) {
            // cmd's echo prints quotes verbatim.
            assert_eq!(lines, vec!["\"a b\"", "x\"y\"z"], "stdout: {:?}", r.stdout);
        } else {
            assert_eq!(lines, vec!["a b", "xyz"], "stdout: {:?}", r.stdout);
        }
        assert!(
            !r.stdout.contains('\\'),
            "no backslash escapes: {:?}",
            r.stdout
        );
    }

    /// A payload that starts with a quoted program path (spaces in it) must
    /// run; plain `cmd /C` would strip the first and last quote of the line.
    #[cfg(windows)]
    #[tokio::test]
    async fn quoted_program_path_at_start_runs() {
        let cancel = CancellationToken::new();
        let r = run_bash(
            "\"C:\\Windows\\System32\\cmd.exe\" /C echo \"x y\"",
            Duration::from_secs(10),
            &cancel,
        )
        .await
        .unwrap();
        assert_eq!(r.exit_code, 0, "stderr: {:?}", r.stderr);
        assert_eq!(r.stdout.trim_end(), "\"x y\"");
    }

    /// Spawns a backgrounded `sleep 30` grandchild behind `sh -c` (it also
    /// inherits, and holds open, our stdout pipe) and records its pid.
    #[cfg(unix)]
    fn grandchild_cmd(pid_file: &std::path::Path) -> String {
        format!("sleep 30 & echo $! > '{}'; wait", pid_file.display())
    }

    /// Dead = gone, or a zombie awaiting its (re)parent's reap. Polls: the
    /// SIGKILL is delivered asynchronously.
    #[cfg(unix)]
    async fn wait_until_dead(pid_file: &std::path::Path) {
        let pid = std::fs::read_to_string(pid_file).unwrap();
        let pid = pid.trim().to_string();
        assert!(!pid.is_empty(), "grandchild pid recorded");
        for _ in 0..50 {
            let out = std::process::Command::new("ps")
                .args(["-o", "stat=", "-p", &pid])
                .output()
                .unwrap();
            let stat = String::from_utf8_lossy(&out.stdout);
            if stat.trim().is_empty() || stat.trim_start().starts_with('Z') {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("grandchild {pid} survived the group kill");
    }

    /// Item #10: the timeout path reaps the whole group, not just `sh`.
    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_kills_grandchildren_too() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("gc.pid");
        let cancel = CancellationToken::new();
        let err = run_bash(&grandchild_cmd(&pid_file), Duration::from_secs(1), &cancel)
            .await
            .unwrap_err();
        assert!(matches!(err, ExecError::TimedOut(_)), "{err:?}");
        wait_until_dead(&pid_file).await;
    }

    /// Item #10: dropping the run future mid-wait (an outer timeout) fires
    /// the group guard; `kill_on_drop` alone would strand the `sleep`.
    #[cfg(unix)]
    #[tokio::test]
    async fn dropped_future_kills_grandchildren() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("gc.pid");
        let cancel = CancellationToken::new();
        let cmd = grandchild_cmd(&pid_file);
        let outer = tokio::time::timeout(
            Duration::from_secs(1),
            run_bash(&cmd, Duration::from_secs(60), &cancel),
        )
        .await;
        assert!(outer.is_err(), "outer timeout must drop the run future");
        wait_until_dead(&pid_file).await;
    }

    /// Docket #9: under the threshold the streamed run returns exactly
    /// what `run_bash_in` returns (byte identity for small outputs).
    #[tokio::test]
    async fn streamed_small_output_matches_run_bash_in() {
        let dir = tempfile::tempdir().unwrap();
        let cmd = if cfg!(windows) {
            "echo out-line && echo err-line 1>&2 && exit 3"
        } else {
            "echo out-line; echo err-line >&2; exit 3"
        };
        let cancel = CancellationToken::new();
        let t = Duration::from_secs(10);
        let full = run_bash_in(cmd, t, &cancel, &[], dir.path()).await.unwrap();
        let spill = SpillConfig::default();
        let run = run_bash_streamed(cmd, t, &cancel, dir.path(), Some(&spill)).await;
        assert_eq!(run.status.unwrap(), 3);
        assert!(matches!(&run.stdout, CapturedStream::Inline(s) if *s == full.stdout));
        assert!(matches!(&run.stderr, CapturedStream::Inline(s) if *s == full.stderr));
        assert!(!dir.path().join(".polymorph").exists());
    }

    /// Docket #9: ~50 MB of output streams to the spill file (every byte)
    /// while the inline digest stays small.
    #[tokio::test]
    async fn streamed_large_output_spills_every_byte() {
        let dir = tempfile::tempdir().unwrap();
        // 51,200 lines of 1,023 `x` + LF = 52,428,800 bytes.
        let cmd = if cfg!(windows) {
            "powershell -NoProfile -Command \"[Console]::Out.Write(('x'*1023+[char]10)*51200)\""
        } else {
            "yes $(printf 'x%.0s' $(seq 1 1023)) | head -n 51200"
        };
        let cancel = CancellationToken::new();
        let spill = SpillConfig::default();
        let run = run_bash_streamed(
            cmd,
            Duration::from_secs(120),
            &cancel,
            dir.path(),
            Some(&spill),
        )
        .await;
        assert_eq!(run.status.unwrap(), 0);
        let CapturedStream::Spilled(s) = &run.stdout else {
            panic!("expected a spill: {:?}", run.stdout.text().len());
        };
        assert_eq!(s.total_bytes, 52_428_800);
        assert_eq!(s.total_lines, 51_200);
        assert!(s.error.is_none(), "{:?}", s.error);
        let meta = std::fs::metadata(s.abs_path.as_ref().unwrap()).unwrap();
        assert_eq!(meta.len(), 52_428_800);
        assert!(
            s.digest.len() < 32 * 1024,
            "digest {} bytes",
            s.digest.len()
        );
        assert!(run.stderr.is_empty());
    }

    /// Docket #9: a timeout keeps what the command printed before the kill.
    #[tokio::test]
    async fn streamed_timeout_keeps_partial_output() {
        let dir = tempfile::tempdir().unwrap();
        let cmd = if cfg!(windows) {
            "echo printed-before-kill && powershell -NoProfile -Command Start-Sleep -Seconds 30"
        } else {
            "echo printed-before-kill; sleep 30"
        };
        let cancel = CancellationToken::new();
        let spill = SpillConfig::default();
        let run = run_bash_streamed(
            cmd,
            Duration::from_secs(3),
            &cancel,
            dir.path(),
            Some(&spill),
        )
        .await;
        assert!(
            matches!(run.status, Err(ExecError::TimedOut(_))),
            "{:?}",
            run.status
        );
        assert!(
            run.stdout.text().contains("printed-before-kill"),
            "{:?}",
            run.stdout
        );
    }

    #[tokio::test]
    async fn pre_cancelled_token_never_spawns() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = run_bash("echo must-not-run", Duration::from_secs(10), &cancel)
            .await
            .unwrap_err();
        assert!(matches!(err, ExecError::Interrupted));
    }

    #[test]
    fn normalize_windows_cmd_transforms_leading_dot_slash() {
        assert_eq!(normalize_windows_cmd("./gradlew check"), ".\\gradlew check");
        assert_eq!(
            normalize_windows_cmd("  ./gradlew.bat build"),
            "  .\\gradlew.bat build"
        );
        assert_eq!(
            normalize_windows_cmd("cd sub && ./gradlew test"),
            "cd sub && .\\gradlew test"
        );
        assert_eq!(
            normalize_windows_cmd("foo || ./bar; ./baz"),
            "foo || .\\bar; .\\baz"
        );
        // Quoted strings preserve literal contents
        assert_eq!(
            normalize_windows_cmd("git grep \"./src/foo\""),
            "git grep \"./src/foo\""
        );
        assert_eq!(
            normalize_windows_cmd("echo 'keep ./intact'"),
            "echo 'keep ./intact'"
        );
    }
}
