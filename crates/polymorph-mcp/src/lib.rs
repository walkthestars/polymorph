//! MCP stdio server exposing Polymorph's bounded drive as one tool.
//!
//! The `drive` tool runs the CLI's `review drive` in a subprocess of the
//! current binary (this server ships inside the `polymorph` executable) and
//! returns the event ledger. It is deliberately narrow: one goal, one
//! artifact set, one verdict — bounded rounds, bounded wall-clock, a
//! rollback guard on failure, and one drive at a time. Success requires
//! exit code 0 AND a terminal `finished`/`PASSED` event; every other
//! outcome returns the ledger so the findings speak for themselves.
//!
//! Stdout belongs to the protocol: nothing here prints (the child's stdout
//! and stderr are captured, never forwarded) and logging is never
//! initialized.

use std::sync::Arc;
use std::time::Duration;

use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
use rmcp::model::{CallToolResult, ContentBlock, ErrorData, ServerCapabilities, ServerInfo};
use rmcp::{schemars, tool, tool_handler, tool_router, ServerHandler, ServiceExt};
use tokio::sync::Mutex;

/// Input contract of the `drive` tool, exactly as an MCP client states it.
/// Validation happens in [`plan`]; the schema below documents the same
/// bounds the validator enforces.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct DriveRequest {
    /// Goal the drive holds the change to.
    #[schemars(
        description = "Goal the drive holds the change to: trimmed, non-empty, at most 4000 chars."
    )]
    pub goal: String,

    /// Artifact files the drive may change (the first one leads).
    #[schemars(
        description = "Artifact files the drive may change: 1..=32 entries, each a trimmed non-empty path or glob that must not start with '-'. The first entry is passed as --artifact-file, the rest as --artifact-files."
    )]
    pub artifact_files: Vec<String>,

    /// Optional acceptance criteria text.
    #[serde(default)]
    #[schemars(
        description = "Optional acceptance criteria text: when given, trimmed, non-empty, at most 20000 chars, and must not start with '-'. Passed through as --criteria."
    )]
    pub criteria: Option<String>,

    /// Optional verification commands.
    #[serde(default)]
    #[schemars(
        description = "Optional verification commands: at most 8, each non-empty, at most 2000 chars, and none may start with '-'. Each is passed as --verify-cmd."
    )]
    pub verify_cmds: Vec<String>,

    /// Optional repo directory to run the drive inside.
    #[serde(default)]
    #[schemars(
        description = "Optional path of an existing directory to run the drive in (the server's current directory when omitted). Applied as the child's working directory; never forwarded as a flag."
    )]
    pub repo: Option<String>,

    /// Optional drive mode (only `act` is accepted).
    #[serde(default)]
    #[schemars(
        description = "Optional drive mode: only \"act\" is accepted. Plan documents belong to `polymorph review plan-drive`."
    )]
    pub mode: Option<String>,

    /// Optional reviewer-to-executor round cap.
    #[serde(default)]
    #[schemars(description = "Reviewer-to-executor rounds: 1..=5, default 3.")]
    pub max_rounds: Option<u32>,

    /// Optional wall-clock cap, in seconds.
    #[serde(default)]
    #[schemars(
        description = "Wall-clock cap for the whole drive, in seconds: 30..=7200, default 3600."
    )]
    pub timeout_secs: Option<u64>,
}

/// One validated drive: the request reduced to exactly what the subprocess
/// call needs. Private on purpose — `DriveRequest` is the wire surface;
/// this is the checked form no invalid request can inhabit.
#[derive(Debug)]
struct Plan {
    goal: String,
    artifact_files: Vec<String>,
    criteria: Option<String>,
    verify_cmds: Vec<String>,
    repo: Option<std::path::PathBuf>,
    max_rounds: u32,
    timeout: Duration,
}

/// Validate one request into a [`Plan`], or name the first fault. Every
/// bound the tool promises is enforced here — before any lock is taken or
/// subprocess spawned — so a bad request can never spend anything.
fn plan(req: DriveRequest) -> Result<Plan, String> {
    validated_mode(req.mode.as_deref())?;
    Ok(Plan {
        goal: validated_goal(&req.goal)?,
        artifact_files: validated_artifacts(&req.artifact_files)?,
        criteria: validated_criteria(req.criteria.as_deref())?,
        verify_cmds: validated_verify_cmds(&req.verify_cmds)?,
        repo: validated_repo(req.repo.as_deref())?,
        max_rounds: validated_max_rounds(req.max_rounds)?,
        timeout: Duration::from_secs(validated_timeout_secs(req.timeout_secs)?),
    })
}

/// Goal: trimmed, non-empty, at most 4000 chars.
fn validated_goal(raw: &str) -> Result<String, String> {
    let goal = raw.trim();
    if goal.is_empty() {
        return Err("goal must be non-empty after trimming".into());
    }
    let n = goal.chars().count();
    if n > 4000 {
        return Err(format!("goal is {n} chars after trimming; the cap is 4000"));
    }
    Ok(goal.to_string())
}

/// Artifact set: 1..=32 entries, each trimmed non-empty and not flag-like.
fn validated_artifacts(raw: &[String]) -> Result<Vec<String>, String> {
    if raw.is_empty() {
        return Err("artifact_files must list at least 1 file".into());
    }
    if raw.len() > 32 {
        return Err(format!(
            "artifact_files lists {} files; the cap is 32",
            raw.len()
        ));
    }
    raw.iter()
        .enumerate()
        .map(|(i, entry)| {
            let file = entry.trim();
            if file.is_empty() {
                Err(format!("artifact_files[{i}] is empty after trimming"))
            } else if file.starts_with('-') {
                Err(format!(
                    "artifact_files[{i}] must not start with '-' (flag-like): {file}"
                ))
            } else {
                Ok(file.to_string())
            }
        })
        .collect()
}

/// Criteria: absent, or trimmed non-empty, at most 20000 chars, not
/// flag-like.
fn validated_criteria(raw: Option<&str>) -> Result<Option<String>, String> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let criteria = raw.trim();
    if criteria.is_empty() {
        return Err("criteria must be non-empty after trimming when given".into());
    }
    let n = criteria.chars().count();
    if n > 20_000 {
        return Err(format!(
            "criteria is {n} chars after trimming; the cap is 20000"
        ));
    }
    if criteria.starts_with('-') {
        return Err(format!(
            "criteria must not start with '-' (flag-like): {criteria}"
        ));
    }
    Ok(Some(criteria.to_string()))
}

/// Verify commands: at most 8, each non-empty, at most 2000 chars, none
/// flag-like. Kept verbatim — a command's own spacing is its business.
fn validated_verify_cmds(raw: &[String]) -> Result<Vec<String>, String> {
    if raw.len() > 8 {
        return Err(format!(
            "verify_cmds lists {} commands; the cap is 8",
            raw.len()
        ));
    }
    for (i, cmd) in raw.iter().enumerate() {
        if cmd.is_empty() {
            return Err(format!("verify_cmds[{i}] must not be empty"));
        }
        let n = cmd.chars().count();
        if n > 2000 {
            return Err(format!("verify_cmds[{i}] is {n} chars; the cap is 2000"));
        }
        if cmd.starts_with('-') {
            return Err(format!(
                "verify_cmds[{i}] must not start with '-' (flag-like): {cmd}"
            ));
        }
    }
    Ok(raw.to_vec())
}

/// Mode: absent or `act`. `plan` is refused with the command that owns it.
fn validated_mode(raw: Option<&str>) -> Result<(), String> {
    match raw {
        None | Some("act") => Ok(()),
        Some("plan") => Err(
            "mode \"plan\" is not supported here; plan drives belong to `polymorph review plan-drive`"
                .into(),
        ),
        Some(other) => Err(format!(
            "mode must be \"act\" (got {other:?}); plan drives belong to `polymorph review plan-drive`"
        )),
    }
}

/// Round cap: 1..=5, default 3.
fn validated_max_rounds(raw: Option<u32>) -> Result<u32, String> {
    match raw {
        None => Ok(3),
        Some(n) if (1..=5).contains(&n) => Ok(n),
        Some(n) => Err(format!("max_rounds must be between 1 and 5 (got {n})")),
    }
}

/// Wall-clock cap: 30..=7200 seconds, default 3600.
fn validated_timeout_secs(raw: Option<u64>) -> Result<u64, String> {
    match raw {
        None => Ok(3600),
        Some(secs) if (30..=7200).contains(&secs) => Ok(secs),
        Some(secs) => Err(format!(
            "timeout_secs must be between 30 and 7200 (got {secs})"
        )),
    }
}

/// Repo: absent, or an existing directory. It becomes the child's working
/// directory — never a forwarded flag.
fn validated_repo(raw: Option<&str>) -> Result<Option<std::path::PathBuf>, String> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    let repo = std::path::PathBuf::from(trimmed);
    if !repo.is_dir() {
        return Err(format!("repo {trimmed:?} is not an existing directory"));
    }
    Ok(Some(repo))
}

/// The exact `review drive` invocation for one plan. The vector is pinned
/// by tests: the first artifact leads as `--artifact-file`, the rest repeat
/// `--artifact-files`, criteria and verify commands ride their own flags,
/// and the drive is always bounded (`--tools`, `--max-rounds`,
/// `--on-fail rollback`) and always observed (`--events-json`). `--repo`
/// is deliberately absent — the repo is applied as the child's working
/// directory, never passed through as a flag.
fn drive_args(plan: &Plan, events: &std::path::Path) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "review".into(),
        "drive".into(),
        "--goal".into(),
        plan.goal.clone(),
        "--artifact-file".into(),
        plan.artifact_files[0].clone(),
    ];
    for file in &plan.artifact_files[1..] {
        args.push("--artifact-files".into());
        args.push(file.clone());
    }
    if let Some(criteria) = &plan.criteria {
        args.push("--criteria".into());
        args.push(criteria.clone());
    }
    for cmd in &plan.verify_cmds {
        args.push("--verify-cmd".into());
        args.push(cmd.clone());
    }
    args.extend([
        "--tools".into(),
        "--max-rounds".into(),
        plan.max_rounds.to_string(),
        "--on-fail".into(),
        "rollback".into(),
        "--events-json".into(),
        events.to_string_lossy().into_owned(),
    ]);
    args
}

/// True when the ledger carries a terminal `finished`/`PASSED` event.
fn finished_passed(events: &serde_json::Value) -> bool {
    events.as_array().is_some_and(|evs| {
        evs.iter().any(|event| {
            event.get("type").and_then(serde_json::Value::as_str) == Some("finished")
                && event.get("verdict").and_then(serde_json::Value::as_str) == Some("PASSED")
        })
    })
}

/// `rounds_used`/`rounds_max` off the ledger's budget event (the last one,
/// were there several); both null when no budget event exists.
fn budget_rounds(events: &serde_json::Value) -> (Option<u64>, Option<u64>) {
    let Some(evs) = events.as_array() else {
        return (None, None);
    };
    let mut found = (None, None);
    for event in evs {
        if event.get("type").and_then(serde_json::Value::as_str) == Some("budget") {
            found = (
                event.get("rounds_used").and_then(serde_json::Value::as_u64),
                event.get("rounds_max").and_then(serde_json::Value::as_u64),
            );
        }
    }
    found
}

/// Last 4096 chars of the child's stderr: enough to name a failure, never
/// enough to drown the ledger.
fn stderr_tail(stderr: &str) -> String {
    const CAP: usize = 4096;
    let len = stderr.chars().count();
    if len <= CAP {
        stderr.to_string()
    } else {
        stderr.chars().skip(len - CAP).collect()
    }
}

/// Fold a completed drive into one JSON answer: the exit code, whether the
/// drive passed (exit 0 AND a terminal PASSED event — neither alone is
/// success), the round budget off the ledger, the full event ledger, and a
/// bounded stderr tail.
fn summarize(exit: Option<i32>, events: &serde_json::Value, stderr: &str) -> serde_json::Value {
    let passed = exit == Some(0) && finished_passed(events);
    let (rounds_used, rounds_max) = budget_rounds(events);
    serde_json::json!({
        "exit_code": exit,
        "passed": passed,
        "rounds_used": rounds_used,
        "rounds_max": rounds_max,
        "events": events,
        "stderr_tail": stderr_tail(stderr),
    })
}

/// Unique scratch path for one drive's event ledger, under the OS temp dir
/// (pid + nanos + in-process counter: unique within and across processes,
/// with no temp-file dependency in the lib).
fn temp_events_path() -> std::path::PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since_epoch| since_epoch.as_nanos())
        .unwrap_or_default();
    std::env::temp_dir().join(format!(
        "polymorph-mcp-drive-{}-{nanos}-{seq}.events.json",
        std::process::id()
    ))
}

/// Error tool result for a drive that never produced a ledger: names the
/// cause and the timeout cap (the one bound every invocation shares).
fn drive_broken(cause: String, plan: &Plan) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(format!(
        "drive failed: {cause} (timeout cap: {}s)",
        plan.timeout.as_secs()
    ))])
}

/// Run one planned drive as a subprocess of the current binary and fold the
/// outcome into a tool result. The child is fully fenced: null stdin,
/// captured stdout/stderr (never forwarded — this process's stdout is the
/// protocol), killed if dropped, and capped by `plan.timeout`. Every
/// failure on the way (resolve, spawn, wait/read, timeout, ledger
/// read/parse) becomes an error tool result naming the cause and the cap.
/// A drive that completes returns its summarized ledger either way: success
/// on a PASSED verdict, error with the same text otherwise.
async fn run_drive(plan: &Plan) -> Result<CallToolResult, ErrorData> {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            return Ok(drive_broken(
                format!("cannot resolve the polymorph binary (current_exe): {e}"),
                plan,
            ));
        }
    };
    let events_path = temp_events_path();
    let mut cmd = tokio::process::Command::new(&exe);
    cmd.args(drive_args(plan, &events_path));
    if let Some(repo) = &plan.repo {
        cmd.current_dir(repo);
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            return Ok(drive_broken(
                format!("cannot spawn {}: {e}", exe.display()),
                plan,
            ));
        }
    };
    let output = match tokio::time::timeout(plan.timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => {
            return Ok(drive_broken(
                format!("drive subprocess wait failed: {e}"),
                plan,
            ));
        }
        Err(_) => {
            return Ok(drive_broken(
                format!("timed out after {}s", plan.timeout.as_secs()),
                plan,
            ));
        }
    };
    let exit = output.status.code();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let ledger = match std::fs::read_to_string(&events_path) {
        Ok(ledger) => ledger,
        Err(e) => {
            return Ok(drive_broken(
                format!("events ledger {} unreadable: {e}", events_path.display()),
                plan,
            ));
        }
    };
    let events: serde_json::Value = match serde_json::from_str(&ledger) {
        Ok(events) => events,
        Err(e) => {
            return Ok(drive_broken(
                format!(
                    "events ledger {} is not valid JSON: {e}",
                    events_path.display()
                ),
                plan,
            ));
        }
    };
    let summary = summarize(exit, &events, &stderr);
    // The ledger is folded into the result; the temp copy goes away.
    let _ = std::fs::remove_file(&events_path);
    let text = serde_json::to_string_pretty(&summary).map_err(|e| {
        ErrorData::internal_error(format!("drive ledger serialization failed: {e}"), None)
    })?;
    let passed = exit == Some(0) && finished_passed(&events);
    if passed {
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    } else {
        Ok(CallToolResult::error(vec![ContentBlock::text(text)]))
    }
}

/// The MCP server: one `drive` tool, plus a gate so only one drive runs at
/// a time (a second caller waits — it never runs concurrently).
#[derive(Clone)]
pub struct PolymorphServer {
    tool_router: ToolRouter<Self>,
    gate: Arc<Mutex<()>>,
}

impl Default for PolymorphServer {
    fn default() -> Self {
        Self::new()
    }
}

#[tool_router]
impl PolymorphServer {
    /// A server with its tool router wired and an idle drive gate.
    pub fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
            gate: Arc::new(Mutex::new(())),
        }
    }

    #[tool(
        description = "Drive one bounded change to a verdict. Runs polymorph review drive --tools with a rollback guard and returns the event ledger; success requires exit code 0 and a finished PASSED event."
    )]
    async fn drive(
        &self,
        Parameters(req): Parameters<DriveRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let planned = match plan(req) {
            Ok(planned) => planned,
            Err(msg) => return Err(ErrorData::invalid_params(msg, None)),
        };
        // One drive at a time: hold the gate for the whole subprocess run.
        let _gate = self.gate.lock().await;
        run_drive(&planned).await
    }
}

// `#[tool_handler]` fills in `call_tool`/`list_tools` routed through the
// stored router field; `get_info` stays hand-written.
#[tool_handler(router = self.tool_router)]
impl ServerHandler for PolymorphServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(rmcp::model::Implementation::new(
                "polymorph",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Polymorph drive tool: one goal, one artifact set, one verdict. Success requires exit 0 and a finished PASSED event; on failure the ledger carries the findings.",
            )
    }
}

/// Serve the MCP protocol on stdin/stdout. Never writes to stdout outside
/// the protocol itself, and never initializes logging.
pub async fn serve_stdio() -> anyhow::Result<()> {
    let service = PolymorphServer::new()
        .serve(rmcp::transport::stdio())
        .await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A valid, fully-omittable request every table case starts from.
    fn base_req() -> DriveRequest {
        DriveRequest {
            goal: "Drive one bounded change to a verdict".into(),
            artifact_files: vec!["crates/polymorph-mcp/src/lib.rs".into()],
            criteria: Some("All findings addressed".into()),
            verify_cmds: vec!["cargo test -p polymorph-mcp".into()],
            repo: None,
            mode: None,
            max_rounds: None,
            timeout_secs: None,
        }
    }

    fn req_with(mutate: impl FnOnce(&mut DriveRequest)) -> DriveRequest {
        let mut req = base_req();
        mutate(&mut req);
        req
    }

    #[test]
    fn plan_rejects_out_of_bounds_requests() {
        let cases: Vec<(&str, DriveRequest, &str)> = vec![
            ("empty goal", req_with(|r| r.goal = "   ".into()), "goal"),
            (
                "overlong goal",
                req_with(|r| r.goal = "g".repeat(4001)),
                "goal",
            ),
            (
                "no artifacts",
                req_with(|r| r.artifact_files = vec![]),
                "artifact_files",
            ),
            (
                "33 artifacts",
                req_with(|r| {
                    r.artifact_files = (1..=33).map(|i| format!("f{i}.rs")).collect();
                }),
                "artifact_files",
            ),
            (
                "flag-like artifact",
                req_with(|r| r.artifact_files = vec!["--other-file".into()]),
                "artifact_files",
            ),
            (
                "blank artifact",
                req_with(|r| r.artifact_files = vec!["   ".into()]),
                "artifact_files",
            ),
            (
                "blank criteria",
                req_with(|r| r.criteria = Some("   ".into())),
                "criteria",
            ),
            (
                "flag-like criteria",
                req_with(|r| r.criteria = Some("- no leading dash".into())),
                "criteria",
            ),
            (
                "flag-like verify cmd",
                req_with(|r| r.verify_cmds = vec!["--evil".into()]),
                "verify_cmds",
            ),
            (
                "empty verify cmd",
                req_with(|r| r.verify_cmds = vec!["".into()]),
                "verify_cmds",
            ),
            (
                "nine verify cmds",
                req_with(|r| r.verify_cmds = (1..=9).map(|i| format!("echo {i}")).collect()),
                "verify_cmds",
            ),
            (
                "plan mode",
                req_with(|r| r.mode = Some("plan".into())),
                "polymorph review plan-drive",
            ),
            (
                "bogus mode",
                req_with(|r| r.mode = Some("report".into())),
                "mode",
            ),
            (
                "max_rounds 0",
                req_with(|r| r.max_rounds = Some(0)),
                "max_rounds",
            ),
            (
                "max_rounds 6",
                req_with(|r| r.max_rounds = Some(6)),
                "max_rounds",
            ),
            (
                "timeout below the floor",
                req_with(|r| r.timeout_secs = Some(29)),
                "timeout_secs",
            ),
            (
                "timeout above the ceiling",
                req_with(|r| r.timeout_secs = Some(7203)),
                "timeout_secs",
            ),
            (
                "missing repo",
                req_with(|r| r.repo = Some("definitely-not-a-directory-xyz".into())),
                "repo",
            ),
        ];
        for (name, req, needle) in cases {
            let err = plan(req).unwrap_err();
            assert!(
                err.contains(needle),
                "{name}: expected the fault to name {needle:?}, got: {err}"
            );
        }
    }

    #[test]
    fn plan_accepts_a_tempdir_repo() {
        let dir = tempfile::tempdir().unwrap();
        let mut req = base_req();
        req.repo = Some(dir.path().to_string_lossy().into_owned());
        req.goal = format!("  {}  ", req.goal);
        let planned = plan(req).unwrap();
        assert_eq!(planned.repo.as_deref(), Some(dir.path()));
        // The goal is stored trimmed.
        assert_eq!(planned.goal, "Drive one bounded change to a verdict");
        // Defaults from omitted fields.
        assert_eq!(planned.max_rounds, 3);
        assert_eq!(planned.timeout, Duration::from_secs(3600));
        assert_eq!(planned.criteria.as_deref(), Some("All findings addressed"));
    }

    #[test]
    fn plan_accepts_the_full_bounds_window() {
        let mut req = base_req();
        req.mode = Some("act".into());
        req.max_rounds = Some(5);
        req.timeout_secs = Some(30);
        req.artifact_files = (1..=32).map(|i| format!("src/file{i}.rs")).collect();
        req.verify_cmds = (1..=8).map(|i| format!("echo step{i}")).collect();
        let planned = plan(req).unwrap();
        assert_eq!(planned.max_rounds, 5);
        assert_eq!(planned.timeout, Duration::from_secs(30));
        assert_eq!(planned.artifact_files.len(), 32);
        assert_eq!(planned.verify_cmds.len(), 8);

        let mut req = base_req();
        req.max_rounds = Some(1);
        req.timeout_secs = Some(7200);
        req.criteria = None;
        let planned = plan(req).unwrap();
        assert_eq!(planned.max_rounds, 1);
        assert_eq!(planned.timeout, Duration::from_secs(7200));
        assert_eq!(planned.criteria, None);
    }

    #[test]
    fn drive_args_is_the_exact_cli_invocation() {
        let planned = Plan {
            goal: "One bounded change".into(),
            artifact_files: vec![
                "src/one.rs".into(),
                "src/two.rs".into(),
                "src/three.rs".into(),
            ],
            criteria: Some("No regressions".into()),
            verify_cmds: vec!["cargo test".into(), "cargo clippy -- -D warnings".into()],
            repo: Some(std::path::PathBuf::from("/repos/demo")),
            max_rounds: 4,
            timeout: Duration::from_secs(600),
        };
        let args = drive_args(&planned, std::path::Path::new("/tmp/ledger/events.json"));
        assert_eq!(
            args,
            vec![
                "review",
                "drive",
                "--goal",
                "One bounded change",
                "--artifact-file",
                "src/one.rs",
                "--artifact-files",
                "src/two.rs",
                "--artifact-files",
                "src/three.rs",
                "--criteria",
                "No regressions",
                "--verify-cmd",
                "cargo test",
                "--verify-cmd",
                "cargo clippy -- -D warnings",
                "--tools",
                "--max-rounds",
                "4",
                "--on-fail",
                "rollback",
                "--events-json",
                "/tmp/ledger/events.json",
            ]
        );
        // The repo rides as a working directory, never as a flag.
        assert!(!args.iter().any(|arg| arg == "--repo"));

        // Minimal plan: single artifact, no criteria, no verify commands.
        let minimal = Plan {
            goal: "g".into(),
            artifact_files: vec!["a.rs".into()],
            criteria: None,
            verify_cmds: vec![],
            repo: None,
            max_rounds: 3,
            timeout: Duration::from_secs(3600),
        };
        let args = drive_args(&minimal, std::path::Path::new("events.json"));
        assert_eq!(
            args,
            vec![
                "review",
                "drive",
                "--goal",
                "g",
                "--artifact-file",
                "a.rs",
                "--tools",
                "--max-rounds",
                "3",
                "--on-fail",
                "rollback",
                "--events-json",
                "events.json",
            ]
        );
    }

    #[test]
    fn summarize_passes_only_on_exit_zero_with_a_passed_finish() {
        let passed_events = serde_json::json!([
            {"type": "round_started", "round": 1, "max_rounds": 3},
            {"type": "verdict", "round": 1, "passed": true, "summary": "clean"},
            {"type": "budget", "rounds_used": 1, "rounds_max": 3, "tools_used": 2, "tools_max": 18},
            {"type": "finished", "verdict": "PASSED"},
        ]);
        let summary = summarize(Some(0), &passed_events, "");
        assert_eq!(summary["exit_code"], serde_json::json!(0));
        assert_eq!(summary["passed"], serde_json::json!(true));
        assert_eq!(summary["rounds_used"], serde_json::json!(1));
        assert_eq!(summary["rounds_max"], serde_json::json!(3));
        assert_eq!(summary["events"], passed_events);
        assert_eq!(summary["stderr_tail"], serde_json::json!(""));

        // Same ledger, non-zero exit: the verdict alone is not success.
        let summary = summarize(Some(1), &passed_events, "boom");
        assert_eq!(summary["exit_code"], serde_json::json!(1));
        assert_eq!(summary["passed"], serde_json::json!(false));
        assert_eq!(summary["stderr_tail"], serde_json::json!("boom"));

        // Exit 0 but a FAILED finish: the exit code alone is not success.
        let failed_events = serde_json::json!([
            {"type": "budget", "rounds_used": 3, "rounds_max": 3, "tools_used": 9, "tools_max": 18},
            {"type": "finished", "verdict": "FAILED"},
        ]);
        let summary = summarize(Some(0), &failed_events, "findings remain");
        assert_eq!(summary["passed"], serde_json::json!(false));
        assert_eq!(summary["rounds_used"], serde_json::json!(3));
        assert_eq!(summary["rounds_max"], serde_json::json!(3));
    }

    #[test]
    fn summarize_nulls_rounds_without_a_budget_event() {
        let no_budget = serde_json::json!([
            {"type": "round_started", "round": 1, "max_rounds": 3},
            {"type": "finished", "verdict": "PASSED"},
        ]);
        let summary = summarize(Some(0), &no_budget, "");
        assert_eq!(summary["passed"], serde_json::json!(true));
        assert_eq!(summary["rounds_used"], serde_json::Value::Null);
        assert_eq!(summary["rounds_max"], serde_json::Value::Null);

        // Not even an array (a child that wrote garbage shape): nothing
        // passes, nothing is reported, nothing panics.
        let summary = summarize(None, &serde_json::json!(null), "");
        assert_eq!(summary["exit_code"], serde_json::Value::Null);
        assert_eq!(summary["passed"], serde_json::json!(false));
        assert_eq!(summary["rounds_used"], serde_json::Value::Null);
        assert_eq!(summary["rounds_max"], serde_json::Value::Null);
    }

    #[test]
    fn summarize_tails_stderr_at_4096_chars() {
        let head = "a".repeat(904);
        let tail = "b".repeat(4096);
        let stderr = format!("{head}{tail}");
        let summary = summarize(Some(0), &serde_json::json!([]), &stderr);
        let got = summary["stderr_tail"].as_str().unwrap();
        assert_eq!(got.chars().count(), 4096);
        assert!(got.chars().all(|c| c == 'b'));

        // Short stderr passes through whole.
        let summary = summarize(Some(0), &serde_json::json!([]), "short");
        assert_eq!(summary["stderr_tail"], serde_json::json!("short"));
    }

    /// Text blocks of a tool result, joined (never empty when a tool
    /// answered with text).
    fn result_text(result: &rmcp::model::CallToolResult) -> String {
        let mut texts = Vec::new();
        for block in &result.content {
            if let rmcp::model::ContentBlock::Text(text) = block {
                texts.push(text.text.clone());
            }
        }
        texts.join("\n")
    }

    /// The server must actually serve: exactly one tool, named `drive`,
    /// schema and description intact, over a real rmcp loopback (production
    /// swaps only the transport for stdio through the same `serve` call).
    /// The invalid-argument call also proves validation answers before any
    /// subprocess exists — a spawn here would re-run this very test binary
    /// as the "CLI", so its absence is the point.
    #[tokio::test]
    async fn serves_exactly_one_drive_tool_over_loopback() {
        let (a, b) = tokio::io::duplex(65536);
        let (ar, aw) = tokio::io::split(a);
        let (br, bw) = tokio::io::split(b);
        let server_t =
            rmcp::transport::async_rw::AsyncRwTransport::<rmcp::RoleServer, _, _>::new(ar, aw);
        let client_t =
            rmcp::transport::async_rw::AsyncRwTransport::<rmcp::RoleClient, _, _>::new(br, bw);
        // Each `serve` awaits the peer's handshake: run both ends together.
        let (server, client) = tokio::join!(
            rmcp::serve_server(PolymorphServer::new(), server_t),
            rmcp::ServiceExt::serve((), client_t)
        );
        let _server = server.unwrap();
        let client = client.unwrap();

        let listed = client.list_tools(None).await.unwrap();
        assert_eq!(listed.tools.len(), 1, "exactly one tool: {listed:?}");
        let drive = &listed.tools[0];
        assert_eq!(drive.name.to_string(), "drive");
        let description = drive.description.as_deref().unwrap_or_default();
        assert!(description.contains("review drive"), "{description}");
        let schema = serde_json::Value::Object(drive.input_schema.as_ref().clone());
        assert!(
            schema["properties"]["goal"].is_object(),
            "goal schema: {schema}"
        );
        assert!(
            schema["properties"]["artifact_files"].is_object(),
            "artifact_files schema: {schema}"
        );

        // Invalid input is refused at the tool boundary, naming the field.
        let mut params = rmcp::model::CallToolRequestParams::new("drive".to_string());
        let arguments = serde_json::json!({
            "goal": "   ",
            "artifact_files": ["crates/polymorph-mcp/src/lib.rs"],
        });
        params.arguments = arguments.as_object().cloned();
        match client.call_tool(params).await {
            Ok(result) => {
                assert_eq!(
                    result.is_error,
                    Some(true),
                    "empty goal must not pass: {result:?}"
                );
                let text = result_text(&result);
                assert!(text.contains("goal"), "must name the field: {text}");
            }
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("goal") || msg.contains("param"),
                    "must name the refusal: {msg}"
                );
            }
        }
    }
}
