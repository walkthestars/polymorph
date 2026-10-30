//! Fixture-server MCP tests.
//!
//! A REAL child speaks REAL MCP here: `fixtures/mcp_probe_server.py` (stdlib-only) is spawned
//! through `TokioChildProcess` — the production path — while loopback duplexes covered the protocol
//! shape in. Needs a Python 3 interpreter: `PYTHON_BIN`, else `python3`/`python` from `PATH` probed
//! with `--version` (strict `Python 3` match — the broken WindowsApps alias fails the probe).
//! Absent interpreter: LOUD skip (`eprintln!` + early return — reported as skip, never a silent
//! pass).

use polymorph_tools::mcp::{call_mcp_tool, wire_mcp_servers, McpPool, McpServerSpec};
use polymorph_tools::{Gate, Registry, ToolCallContext};
use std::path::PathBuf;

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("mcp_probe_server.py")
}

/// Resolve a working Python 3 or `None` (loud skip at each call site).
fn resolve_python() -> Option<String> {
    if let Ok(p) = std::env::var("PYTHON_BIN") {
        if !p.trim().is_empty() {
            return Some(p);
        }
    }
    for candidate in ["python3", "python"] {
        let probe = std::process::Command::new(candidate)
            .arg("--version")
            .output();
        if let Ok(out) = probe {
            if out.status.success() && String::from_utf8_lossy(&out.stdout).contains("Python 3") {
                return Some(candidate.to_string());
            }
        }
    }
    None
}

fn fixture_entry(python: &str) -> polymorph_core::config::McpServerEntry {
    polymorph_core::config::McpServerEntry {
        id: "probe".into(),
        cmd: python.into(),
        args: vec![fixture_path().to_string_lossy().into_owned()],
    }
}

fn open_ctx() -> (tempfile::TempDir, ToolCallContext) {
    let d = tempfile::TempDir::new().unwrap();
    let ctx = ToolCallContext::new(
        d.path().to_path_buf(),
        Gate::new(vec![], vec![]),
        tokio_util::sync::CancellationToken::new(),
    );
    (d, ctx)
}

/// Spawn-positive interop: real child → real handshake → real list →
/// enroll → real calls through the registry (echo Ok, fail Failed-naming).
#[tokio::test]
async fn spawn_lists_calls_over_stdio() {
    let Some(python) = resolve_python() else {
        eprintln!("SKIP spawn_lists_calls_over_stdio: no Python 3 (set PYTHON_BIN)");
        return;
    };
    let mut r = Registry::new();
    let names = wire_mcp_servers(&mut r, &[fixture_entry(&python)]).await;
    assert_eq!(
        names,
        vec![
            "mcp__probe__probe_echo".to_string(),
            "mcp__probe__probe_fail".to_string()
        ],
        "wire enrolls exactly the fixture tools"
    );
    let (_d, ctx) = open_ctx();
    let out = r
        .call("mcp__probe__probe_echo", serde_json::json!({}), &ctx)
        .await
        .unwrap();
    assert!(out.to_string().contains("echo"), "{out}");
    let err = r
        .call("mcp__probe__probe_fail", serde_json::json!({}), &ctx)
        .await
        .unwrap_err();
    assert!(
        matches!(err, polymorph_tools::CallError::Failed(_)),
        "{err:?}"
    );
    assert!(err.to_string().contains("boom"), "{err}");
}

/// Reconnect heals after a break: a loopback client
/// is injected, its server is dropped pre-call (immediate EOF — the same
/// prompt-close the unit break test relies on), the call fails AND evicts,
/// and the NEXT call transparently respawns the fixture from spec.
#[tokio::test]
async fn reconnect_heals_after_break() {
    use rmcp::ServiceExt;

    let Some(python) = resolve_python() else {
        eprintln!("SKIP reconnect_heals_after_break: no Python 3 (set PYTHON_BIN)");
        return;
    };
    #[derive(Clone, Default)]
    struct BreakProbe;

    #[rmcp::tool_router(server_handler)]
    impl BreakProbe {
        #[rmcp::tool(description = "break probe")]
        async fn probe_echo(&self) -> String {
            "echo".to_string()
        }
    }

    // Serve a loopback pair, then drop the server: the injected client is
    // dead on arrival (no hang — EOF is immediate).
    let (a, b) = tokio::io::duplex(65536);
    let (ar, aw) = tokio::io::split(a);
    let (br, bw) = tokio::io::split(b);
    let server_t =
        rmcp::transport::async_rw::AsyncRwTransport::<rmcp::RoleServer, _, _>::new(ar, aw);
    let client_t =
        rmcp::transport::async_rw::AsyncRwTransport::<rmcp::RoleClient, _, _>::new(br, bw);
    let (server, client) =
        tokio::join!(rmcp::serve_server(BreakProbe, server_t), ().serve(client_t));
    let _server = server.unwrap();
    let client = client.unwrap();
    drop(_server);

    let pool = tokio::sync::Mutex::new(McpPool::default());
    {
        let mut p = pool.lock().await;
        p.add_server(McpServerSpec {
            id: "probe".into(),
            cmd: python.clone(),
            args: vec![fixture_path().to_string_lossy().into_owned()],
        })
        .unwrap();
        p.insert_client("probe", std::sync::Arc::new(client));
    }
    let gate = Gate::new(vec![], vec![]);
    // Break: fails Failed (never Denied) and evicts.
    let err = call_mcp_tool(
        &pool,
        "mcp__probe__probe_echo",
        &serde_json::json!({}),
        &gate,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, polymorph_tools::CallError::Failed(_)),
        "{err:?}"
    );
    assert!(pool.lock().await.connected().is_empty());
    // Heal: the next call respawns the fixture from spec and succeeds.
    let out = call_mcp_tool(
        &pool,
        "mcp__probe__probe_echo",
        &serde_json::json!({}),
        &gate,
    )
    .await
    .unwrap();
    assert!(out.to_string().contains("echo"), "{out}");
    assert_eq!(pool.lock().await.connected(), vec!["probe".to_string()]);
}

/// Record a wire transcript: the rmcp client talks to the fixture through a line-logging proxy, and
/// every frame lands in `tests/cassettes/mcp_probe.json` (committed). The player replays this file;
/// here we assert its method spine so the cassette can never silently rot (missing/extra frames
/// fail loudly). The file is rewritten only with `POLYMORPH_RECORD_CASSETTE=1`.
#[tokio::test]
async fn record_transcript_to_cassette() {
    use rmcp::ServiceExt;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    let Some(python) = resolve_python() else {
        eprintln!("SKIP record_transcript_to_cassette: no Python 3 (set PYTHON_BIN)");
        return;
    };
    let mut child = tokio::process::Command::new(&python)
        .arg(fixture_path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .unwrap();
    let mut child_in = child.stdin.take().unwrap();
    let child_out = child.stdout.take().unwrap();
    // rmcp-side halves face the client transport; proxy halves face the
    // logging forwarders (one task per direction, both concurrent with
    // the handshake — sequential awaits would deadlock, lesson).
    let (rmcp_w, proxy_r) = tokio::io::duplex(65536);
    let (proxy_w, rmcp_r) = tokio::io::duplex(65536);
    let frames: std::sync::Arc<tokio::sync::Mutex<Vec<serde_json::Value>>> = Default::default();
    let f1 = frames.clone();
    let out_pump = tokio::spawn(async move {
        let mut lines = tokio::io::BufReader::new(proxy_r).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let v: serde_json::Value =
                serde_json::from_str(&line).unwrap_or(serde_json::Value::Null);
            f1.lock()
                .await
                .push(serde_json::json!({"dir": "out", "line": v}));
            let mut line = line;
            line.push('\n');
            if child_in.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }
    });
    let f2 = frames.clone();
    let in_pump = tokio::spawn(async move {
        let mut lines = tokio::io::BufReader::new(child_out).lines();
        let mut w = proxy_w;
        while let Ok(Some(line)) = lines.next_line().await {
            let v: serde_json::Value =
                serde_json::from_str(&line).unwrap_or(serde_json::Value::Null);
            f2.lock()
                .await
                .push(serde_json::json!({"dir": "in", "line": v}));
            let mut line = line;
            line.push('\n');
            if w.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }
    });
    let client_t =
        rmcp::transport::async_rw::AsyncRwTransport::<rmcp::RoleClient, _, _>::new(rmcp_r, rmcp_w);
    let client = ().serve(client_t).await.unwrap();
    let listed = client.list_tools(None).await.unwrap();
    assert_eq!(listed.tools.len(), 2);
    for tool in ["probe_echo", "probe_fail"] {
        let mut params = rmcp::model::CallToolRequestParams::new(tool.to_string());
        params.arguments = Some(serde_json::Map::new());
        let _ = client.call_tool(params).await.unwrap();
    }
    drop(client);
    let _ = child.kill().await;
    out_pump.abort();
    in_pump.abort();
    // Spine assert: initialize → list → call → call, in order, tolerating
    // interleaved notifications (and any future keep-alive pings).
    let frames = frames.lock().await;
    let mut methods = Vec::new();
    for f in frames.iter() {
        if f["dir"] == "out" {
            if let Some(m) = f["line"]["method"].as_str() {
                methods.push(m.to_string());
            }
        }
    }
    let spine = ["initialize", "tools/list", "tools/call", "tools/call"];
    let mut want = spine.iter();
    let mut next = want.next();
    for m in &methods {
        if next == Some(&m.as_str()) {
            next = want.next();
        }
    }
    assert_eq!(next, None, "spine {spine:?} not found in {methods:?}");
    let wire = frames
        .iter()
        .map(|f| f.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(wire.contains("echo"), "transcript carries echo");
    assert!(wire.contains("boom"), "transcript carries boom");
    let cassette = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("cassettes");
    // Re-record only on request: an unconditional rewrite dirtied the
    // tracked cassette on every run (LF write vs the CRLF checkout under
    // `core.autocrlf=true`). Default runs check the committed tape's
    // outbound method list against the live one instead (parsed,
    // order-insensitive — never a byte compare).
    if std::env::var("POLYMORPH_RECORD_CASSETTE").is_ok_and(|v| v == "1") {
        std::fs::create_dir_all(&cassette).unwrap();
        std::fs::write(
            cassette.join("mcp_probe.json"),
            serde_json::to_string_pretty(&serde_json::json!({"frames": &*frames})).unwrap(),
        )
        .unwrap();
        return;
    }
    let text = std::fs::read_to_string(cassette.join("mcp_probe.json")).unwrap();
    let tape: serde_json::Value = serde_json::from_str(&text).unwrap();
    let mut taped: Vec<String> = tape["frames"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["dir"] == "out")
        .filter_map(|f| f["line"]["method"].as_str().map(str::to_string))
        .collect();
    let mut live = methods;
    taped.sort();
    live.sort();
    assert_eq!(
        taped, live,
        "committed cassette drifted from the live wire; re-record with POLYMORPH_RECORD_CASSETTE=1"
    );
}
