//! The checker: a sandboxed SMT-LIB2 subprocess and the parsing of its verdict.
//!
//! Trust boundary: the *backend binary, pinned by digest* is trusted;
//! everything else — the proposer, the refiner, this crate's prose — is not.
//! The checker therefore runs the query exactly as written, in a scratch
//! directory it creates, with a scrubbed environment and a wall-clock kill, and
//! reports the solver's own words back verbatim in the transcript digest.
//!
//! One invocation can carry a whole round's obligations: scripts are separated
//! by `(echo "@@ais <index>")` markers and scoped by `push`/`pop`, so a round
//! costs one process instead of one per obligation. The per-obligation script is
//! still the artifact — the certificate carries it verbatim, and a re-check
//! replays it standalone.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::smt::Query;
use crate::types::Probe;

/// A solver's answer to one query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sat {
    /// The negation is satisfiable: a counterexample exists.
    Sat,
    /// The negation is unsatisfiable: the obligation holds.
    Unsat,
    /// The solver could not decide (`unknown`).
    Unknown,
    /// The wall-clock kill fired before the solver answered.
    Timeout,
    /// The solver could not be started, crashed without a verdict, or emitted
    /// something that is not a verdict.
    Error(String),
}

/// One execution of one query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SolverRun {
    /// The answer.
    pub sat: Sat,
    /// Model values, when `sat` and probes were requested.
    pub values: Vec<(String, String)>,
    /// Raw stdout of this query's segment (bounded).
    pub stdout: String,
    /// `sha256:` of the exact segment bytes.
    pub stdout_digest: String,
    /// Wall clock attributed to this query.
    pub elapsed_ms: u64,
    /// Process exit code, when the process exited on its own.
    pub exit_code: Option<i32>,
    /// `(reason-unknown ...)` text when the solver explained itself.
    pub reason_unknown: Option<String>,
}

/// One script in a batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchItem {
    /// Self-contained SMT-LIB2, ending with `check-sat` (and `get-value` probes).
    pub script: String,
    /// Symbols to read from a model if the script is satisfiable.
    pub probes: Vec<Probe>,
}

/// Which solver to run, and how.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SolverSpec {
    /// Command template. `{input}` is replaced by the query file path. Every
    /// other token is passed verbatim.
    pub command: Vec<String>,
    /// Hard wall-clock cap for one invocation.
    pub timeout: Duration,
}

impl SolverSpec {
    /// The default: `z3 -smt2 {input}`, resolved from `PATH`.
    pub fn z3(timeout: Duration) -> Self {
        Self {
            command: vec!["z3".into(), "-smt2".into(), "{input}".into()],
            timeout,
        }
    }

    /// A command line split on whitespace, with `{input}` preserved. Quotes are
    /// honoured so a path with spaces survives.
    pub fn from_command_line(line: &str, timeout: Duration) -> Result<Self, String> {
        let mut command = Vec::new();
        let mut cur = String::new();
        let mut quote: Option<char> = None;
        for c in line.chars() {
            match quote {
                Some(q) if c == q => quote = None,
                Some(_) => cur.push(c),
                None if c == '"' || c == '\'' => quote = Some(c),
                None if c.is_whitespace() => {
                    if !cur.is_empty() {
                        command.push(std::mem::take(&mut cur));
                    }
                }
                None => cur.push(c),
            }
        }
        if quote.is_some() {
            return Err(format!("unbalanced quote in solver command {line:?}"));
        }
        if !cur.is_empty() {
            command.push(cur);
        }
        if command.is_empty() {
            return Err("solver command is empty".into());
        }
        if !command.iter().any(|t| t.contains("{input}")) {
            return Err(format!(
                "solver command {line:?} has no {{input}} placeholder; one is required so the query is passed as a file"
            ));
        }
        Ok(Self { command, timeout })
    }

    /// Resolve the executable the first token names (absolute or `PATH`).
    pub fn executable(&self) -> Result<PathBuf, String> {
        let first = self
            .command
            .first()
            .ok_or_else(|| "solver command is empty".to_string())?;
        let direct = Path::new(first);
        if direct.is_file() {
            return Ok(direct.to_path_buf());
        }
        which(first).ok_or_else(|| not_found_message(first))
    }
}

/// The error for a solver that is neither a file nor on `PATH`. For the
/// default solver it says how to install it, because the release binaries do
/// not bundle one.
fn not_found_message(first: &str) -> String {
    let mut message = format!("solver executable {first:?} was not found.");
    if first == "z3" {
        message.push_str(
            " The proof commands need the z3 SMT solver, which is not bundled.              Install it (Windows: `scoop install z3`; macOS: `brew install z3`;              Debian or Ubuntu: `sudo apt install z3`; or a release from              https://github.com/Z3Prover/z3/releases) and check with `z3 --version`.",
        );
    }
    message.push_str(
        " Where another solver is accepted, set POLYMORPH_AIS_SOLVER to a command          line containing {input} (docs/PROOF.md, section 2).",
    );
    message
}

/// Follow a scoop-style launcher shim to its target.
///
/// Scoop puts one identical launcher (`<name>.exe`) on `PATH` per app, next to
/// a `<name>.shim` text file whose `path = "..."` line names the real binary.
/// Pinning the launcher would give every shimmed solver the same digest — z3
/// and cvc5 would be indistinguishable to the backend check — and killing it
/// on a timeout need not stop the solver it started. So the target is pinned
/// and run directly. A shim that also passes `args` is left alone (its target
/// alone is not the command it runs), as is anything that is not a shim.
pub fn follow_shim(binary: &Path) -> PathBuf {
    let shim = binary.with_extension("shim");
    let Ok(text) = std::fs::read_to_string(&shim) else {
        return binary.to_path_buf();
    };
    let mut target = None;
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "path" => target = Some(value.trim().trim_matches('"').to_string()),
            "args" if !value.trim().is_empty() => return binary.to_path_buf(),
            _ => {}
        }
    }
    match target.map(PathBuf::from) {
        Some(t) if t.is_file() => t,
        _ => binary.to_path_buf(),
    }
}

/// Look up `name` on `PATH` (Windows honours `PATHEXT`).
pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let exts: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT".into())
            .split(';')
            .map(|s| s.to_ascii_lowercase())
            .collect()
    } else {
        vec![String::new()]
    };
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        for ext in &exts {
            let with = dir.join(format!("{name}{ext}"));
            if with.is_file() {
                return Some(with);
            }
        }
    }
    None
}

/// A resolved checker: the executable, its digest and its self-reported version.
#[derive(Debug, Clone)]
pub struct Checker {
    /// The command template.
    pub spec: SolverSpec,
    /// Absolute path of the executable.
    pub binary: PathBuf,
    /// `sha256:` of the executable's bytes.
    pub binary_digest: String,
    /// First line of the version probe output, or `unknown`.
    pub version: String,
}

impl Checker {
    /// Resolve the executable and pin it. A launcher shim is followed to the
    /// binary it starts (see [`follow_shim`]), so the pin names the solver.
    pub fn resolve(spec: SolverSpec) -> Result<Self, String> {
        let binary = follow_shim(&spec.executable()?);
        let bytes = std::fs::read(&binary)
            .map_err(|e| format!("cannot read solver binary {}: {e}", binary.display()))?;
        let binary_digest = crate::cert::sha256_bytes(&bytes);
        let version = probe_version(&binary);
        Ok(Self {
            spec,
            binary,
            binary_digest,
            version,
        })
    }

    /// Run one query.
    pub fn run(&self, query: &Query) -> SolverRun {
        self.run_script(&query.text(), &query.probes)
    }

    /// Run one self-contained SMT-LIB2 script.
    pub fn run_script(&self, script: &str, probes: &[Probe]) -> SolverRun {
        let mut out = self.run_batch(&[BatchItem {
            script: script.to_string(),
            probes: probes.to_vec(),
        }]);
        out.pop().unwrap_or_else(|| SolverRun {
            sat: Sat::Error("the checker produced no output".into()),
            values: Vec::new(),
            stdout: String::new(),
            stdout_digest: crate::cert::sha256_tag(""),
            elapsed_ms: 0,
            exit_code: None,
            reason_unknown: None,
        })
    }

    /// Run a whole round in one process. Each item is wrapped in `push`/`pop`
    /// and announced with `(echo "@@ais <index>")`; the verdicts are matched to
    /// items by position.
    pub fn run_batch(&self, items: &[BatchItem]) -> Vec<SolverRun> {
        if items.is_empty() {
            return Vec::new();
        }
        let started = Instant::now();
        let dir = match tempfile::Builder::new().prefix("ais-check-").tempdir() {
            Ok(d) => d,
            Err(e) => {
                return self.failure(
                    &format!("cannot create scratch directory: {e}"),
                    items.len(),
                )
            }
        };
        let mut script = String::new();
        script.push_str("(set-logic ALL)\n");
        script.push_str("(set-option :print-success false)\n");
        for (i, item) in items.iter().enumerate() {
            // The item's own preamble (`set-logic`/`set-option`) is global-only in
            // SMT-LIB2, so it is stripped here and re-applied once above.
            let body = strip_preamble(&item.script);
            script.push_str(&format!(
                "(echo \"@@ais {i}\")\n(push 1)\n{body}\n(pop 1)\n"
            ));
        }
        let input = dir.path().join("round.smt2");
        if let Err(e) = std::fs::write(&input, script.as_bytes()) {
            return self.failure(&format!("cannot write query: {e}"), items.len());
        }

        let mut cmd = Command::new(&self.binary);
        let path_str = input.to_string_lossy().to_string();
        for tok in self.spec.command.iter().skip(1) {
            cmd.arg(tok.replace("{input}", &path_str));
        }
        cmd.current_dir(dir.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        scrub_env(&mut cmd);
        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                return self.failure(
                    &format!("cannot start solver {}: {e}", self.binary.display()),
                    items.len(),
                );
            }
        };

        let (stdout, exit_code, timed_out) = wait_with_deadline(child, self.spec.timeout);
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        if timed_out {
            return items
                .iter()
                .map(|_| SolverRun {
                    sat: Sat::Timeout,
                    values: Vec::new(),
                    stdout: String::new(),
                    stdout_digest: crate::cert::sha256_tag(""),
                    elapsed_ms,
                    exit_code: None,
                    reason_unknown: None,
                })
                .collect();
        }

        let segments = split_segments(&stdout, items.len());
        let per_item = elapsed_ms / u64::try_from(items.len()).unwrap_or(1).max(1);
        items
            .iter()
            .zip(segments)
            .map(|(item, seg)| parse_segment(&seg, &item.probes, per_item, exit_code))
            .collect()
    }

    fn failure(&self, message: &str, n: usize) -> Vec<SolverRun> {
        (0..n)
            .map(|_| SolverRun {
                sat: Sat::Error(message.to_string()),
                values: Vec::new(),
                stdout: String::new(),
                stdout_digest: crate::cert::sha256_tag(""),
                elapsed_ms: 0,
                exit_code: None,
                reason_unknown: None,
            })
            .collect()
    }
}

/// Spawn, poll, and hard-kill on the deadline.
///
/// Both pipes are drained on their own threads while the child runs: a solver
/// that writes more than the OS pipe buffer would otherwise block on write
/// and be misreported as a timeout.
fn wait_with_deadline(
    mut child: std::process::Child,
    timeout: Duration,
) -> (String, Option<i32>, bool) {
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    // A grandchild may still hold the pipes open: the reader
                    // threads are detached rather than joined.
                    return (String::new(), None, true);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return (String::new(), None, true),
        }
    };
    let out = stdout.join().unwrap_or_default();
    let _ = stderr.join();
    (
        String::from_utf8_lossy(&out).to_string(),
        status.code(),
        false,
    )
}

/// Read a pipe to its end on a background thread.
fn drain<R: std::io::Read + Send + 'static>(pipe: Option<R>) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut p) = pipe {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    })
}

/// Drop `set-logic`/`set-option` lines: they are global and already emitted.
fn strip_preamble(script: &str) -> String {
    script
        .lines()
        .filter(|l| {
            let t = l.trim_start();
            !(t.starts_with("(set-logic") || t.starts_with("(set-option"))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Cut the batch stdout into per-item segments using the `@@ais <n>` markers.
fn split_segments(stdout: &str, n: usize) -> Vec<String> {
    let mut segments = vec![String::new(); n];
    let mut cur: Option<usize> = None;
    for line in stdout.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("@@ais ") {
            if let Ok(i) = rest.trim().parse::<usize>() {
                cur = if i < n { Some(i) } else { None };
                continue;
            }
        }
        if let Some(i) = cur {
            segments[i].push_str(line);
            segments[i].push('\n');
        }
    }
    segments
}

fn parse_segment(
    segment: &str,
    probes: &[Probe],
    elapsed_ms: u64,
    exit_code: Option<i32>,
) -> SolverRun {
    let stdout = truncate(segment.to_string(), 64 * 1024);
    let stdout_digest = crate::cert::sha256_tag(&stdout);
    // `success` and `unsupported` are SMT-LIB2's standard replies to commands
    // other than `check-sat` (cvc5 answers `unsupported` to z3's `:timeout`
    // option); they are never a verdict, so the verdict is the first other line.
    let first = stdout
        .lines()
        .map(|l| l.trim())
        .find(|l| !l.is_empty() && *l != "success" && *l != "unsupported")
        .unwrap_or("");
    let reason_unknown = stdout
        .lines()
        .find(|l| l.contains(":reason-unknown"))
        .map(|l| l.trim().to_string());
    let sat = match first {
        "sat" => Sat::Sat,
        "unsat" => Sat::Unsat,
        "unknown" => Sat::Unknown,
        other => {
            return SolverRun {
                sat: Sat::Error(format!("solver produced no verdict (first line {other:?})")),
                values: Vec::new(),
                stdout,
                stdout_digest,
                elapsed_ms,
                exit_code,
                reason_unknown,
            }
        }
    };
    let values = if sat == Sat::Sat {
        parse_values(&stdout, probes)
    } else {
        Vec::new()
    };
    SolverRun {
        sat,
        values,
        stdout,
        stdout_digest,
        elapsed_ms,
        exit_code,
        reason_unknown,
    }
}

/// Minimal environment so a sandboxed solver cannot read secrets: pass only what
/// Windows needs to start a process at all.
fn scrub_env(cmd: &mut Command) {
    cmd.env_clear();
    for key in [
        "PATH",
        "SystemRoot",
        "SystemDrive",
        "windir",
        "TEMP",
        "TMP",
        "USERPROFILE",
        "LOCALAPPDATA",
        "APPDATA",
        "ProgramFiles",
        "ProgramData",
        "HOME",
        "TZ",
    ] {
        if let Some(v) = std::env::var_os(key) {
            cmd.env(key, v);
        }
    }
}

fn probe_version(binary: &Path) -> String {
    // `-version` is Z3's spelling; cvc5 uses `--version`. Try both, quietly.
    for flag in ["-version", "--version"] {
        let mut cmd = Command::new(binary);
        if let Ok(o) = cmd.arg(flag).stdin(Stdio::null()).output() {
            let text = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if let Some(first) = text.lines().next() {
                if !first.is_empty() {
                    return first.to_string();
                }
            }
        }
    }
    "unknown".into()
}

fn truncate(mut s: String, cap: usize) -> String {
    if s.len() > cap {
        let mut cut = cap;
        while cut > 0 && !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
        s.push('…');
    }
    s
}

/// Parse the `get-value` reply: `((i 3) (r (- 6)) (a ((as const (Array Int Int)) 0)))`.
/// Returns `(symbol, rendered-value)` pairs for the probes that appeared.
pub fn parse_values(stdout: &str, probes: &[Probe]) -> Vec<(String, String)> {
    let Some(sexpr) = first_sexpr_after(stdout, "sat") else {
        return Vec::new();
    };
    let Some(items) = sexpr_list(&sexpr) else {
        return Vec::new();
    };
    let wanted: std::collections::BTreeSet<&str> =
        probes.iter().map(|p| p.symbol.as_str()).collect();
    let mut out = Vec::new();
    for item in items {
        let Some(pair) = sexpr_list(&item) else {
            continue;
        };
        if pair.len() < 2 {
            continue;
        }
        let sym = pair[0].trim();
        if !wanted.contains(sym) {
            continue;
        }
        out.push((sym.to_string(), simplify_value(pair[1].trim())));
    }
    out
}

/// Find the first balanced s-expression that starts after the verdict line.
fn first_sexpr_after(stdout: &str, verdict: &str) -> Option<String> {
    let lines: Vec<&str> = stdout.lines().collect();
    let start = lines.iter().position(|l| l.trim() == verdict)?;
    let rest = lines[start + 1..].join("\n");
    let rest = rest.trim_start();
    if !rest.starts_with('(') {
        return None;
    }
    let mut depth = 0usize;
    for (i, c) in rest.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(rest[..=i].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

/// Split one level of a parenthesized list into its elements.
fn sexpr_list(s: &str) -> Option<Vec<String>> {
    let s = s.trim();
    if !s.starts_with('(') || !s.ends_with(')') {
        return None;
    }
    let inner = &s[1..s.len() - 1];
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut cur = String::new();
    for c in inner.chars() {
        match c {
            '(' => {
                depth += 1;
                cur.push(c);
            }
            ')' => {
                depth -= 1;
                cur.push(c);
            }
            c if c.is_whitespace() && depth == 0 => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    Some(out)
}

/// Read a value s-expression as a source-level literal where possible:
/// `3` → `3`, `(- 6)` → `-6`, `true` → `true`, anything else kept verbatim.
fn simplify_value(v: &str) -> String {
    let v = v.trim();
    if let Ok(n) = v.parse::<i64>() {
        return n.to_string();
    }
    if let Some(items) = sexpr_list(v) {
        if items.len() == 2 && items[0] == "-" {
            if let Ok(n) = items[1].parse::<i64>() {
                return (-n).to_string();
            }
        }
    }
    v.to_string()
}

/// Write a script verbatim to a scratch file and run it standalone. Used by the
/// certificate replay, which must send exactly the bytes the certificate carries.
/// The replay gets the same sandbox as a batch: scratch directory, scrubbed
/// environment, wall-clock kill.
pub fn run_verbatim(checker: &Checker, script: &str) -> Option<SolverRun> {
    let dir = tempfile::Builder::new()
        .prefix("ais-replay-")
        .tempdir()
        .ok()?;
    let input = dir.path().join("replay.smt2");
    std::fs::write(&input, script).ok()?;
    let mut cmd = Command::new(&checker.binary);
    let path = input.to_string_lossy().to_string();
    for tok in checker.spec.command.iter().skip(1) {
        cmd.arg(tok.replace("{input}", &path));
    }
    cmd.current_dir(dir.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    scrub_env(&mut cmd);
    let started = Instant::now();
    let child = cmd.spawn().ok()?;
    let (stdout, exit_code, timed_out) = wait_with_deadline(child, checker.spec.timeout);
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    if timed_out {
        return Some(SolverRun {
            sat: Sat::Timeout,
            values: Vec::new(),
            stdout: String::new(),
            stdout_digest: crate::cert::sha256_tag(""),
            elapsed_ms,
            exit_code: None,
            reason_unknown: None,
        });
    }
    Some(parse_segment(&stdout, &[], elapsed_ms, exit_code))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_default_solver_says_how_to_install_it() {
        let message = not_found_message("z3");
        assert!(message.contains("\"z3\" was not found"), "{message}");
        assert!(message.contains("brew install z3"), "{message}");
        assert!(message.contains("z3 --version"), "{message}");
        assert!(message.contains("POLYMORPH_AIS_SOLVER"), "{message}");
    }

    #[test]
    fn a_missing_custom_solver_gets_no_install_advice() {
        let message = not_found_message("cvc5");
        assert!(message.contains("\"cvc5\" was not found"), "{message}");
        assert!(!message.contains("install"), "{message}");
        assert!(message.contains("POLYMORPH_AIS_SOLVER"), "{message}");
    }
}
