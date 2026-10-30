//! Tool-level DAP contracts (chunk 107, M3): the five session tools
//! driven through `Registry::call` — table routing, program-scoped
//! gating, isolation, removal, caps — over REAL adapters. Probe +
//! loud-skip discipline mirrors `dap_matrix.rs` (roster:
//! `docs/dap-adapters.md`).

use polymorph_tools::dap::DapSpec;

/// Headed/TCP gate (chunk 117): the dlv row binds a TCP listener (the
/// firewall-prompt suspect) — default `cargo test` skips. Set
/// `POLYMORPH_HEADED=1` to run; skips return green (counts unchanged).
fn headed_enabled() -> bool {
    std::env::var("POLYMORPH_HEADED")
        .map(|v| v == "1")
        .unwrap_or(false)
}

fn ctx_for(dir: &tempfile::TempDir) -> polymorph_tools::ToolCallContext {
    polymorph_tools::ToolCallContext::new(
        dir.path().to_path_buf(),
        polymorph_tools::Gate::new(vec![], vec![]),
        tokio_util::sync::CancellationToken::new(),
    )
}

fn resolve_python() -> Option<(String, String)> {
    let python = if let Ok(p) = std::env::var("PYTHON_BIN") {
        (!p.trim().is_empty()).then_some(p)
    } else if std::process::Command::new("python3")
        .arg("--version")
        .output()
        .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).contains("Python 3"))
        .unwrap_or(false)
    {
        Some("python3".to_string())
    } else if std::process::Command::new("python")
        .arg("--version")
        .output()
        .map(|o| {
            o.status.success()
                && (String::from_utf8_lossy(&o.stdout) + String::from_utf8_lossy(&o.stderr))
                    .contains("Python 3")
        })
        .unwrap_or(false)
    {
        Some("python".to_string())
    } else {
        None
    }?;
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()?;
    let mut hits: Vec<_> = std::fs::read_dir(format!("{home}/.vscode/extensions"))
        .ok()?
        .filter_map(|e| {
            let name = e.ok()?.file_name().to_string_lossy().into_owned();
            name.starts_with("ms-python.debugpy-")
                .then(|| format!("{home}/.vscode/extensions/{name}/bundled/libs"))
        })
        .collect();
    hits.sort();
    let libs = hits.pop().filter(|p| std::path::Path::new(p).is_dir())?;
    Some((python, libs))
}

async fn debugpy_registry(d: &tempfile::TempDir) -> Option<polymorph_tools::Registry> {
    let (python, libs) = resolve_python()?;
    let r = polymorph_tools::registry::v1_registry();
    r.add_dap_adapter(
        "python",
        DapSpec {
            cmd: python,
            args: vec!["-m".to_string(), "debugpy.adapter".to_string()],
            env: vec![("PYTHONPATH".to_string(), libs)],
            workdir: None,
        },
    )
    .await
    .unwrap();
    // Exercise THROUGH the call path (workdir-relative program), so the
    // test proves resolution + gating, not just the adapter.
    std::fs::write(d.path().join("t.py"), "x = 6*7\nprint(x + 1)\n").unwrap();
    Some(r)
}

#[tokio::test]
async fn dap_tools_full_flow_debugpy() {
    let d = tempfile::tempdir().unwrap();
    let Some(r) = debugpy_registry(&d).await else {
        eprintln!("SKIP dap_tools_full_flow_debugpy: no Python 3 + bundled debugpy libs");
        return;
    };
    let ctx = ctx_for(&d);
    // Start with a breakpoint: stopped at line 2 with `x` bound.
    let started = r
        .call(
            "dap_start",
            serde_json::json!({
                "program": "t.py",
                "breakpoints": [{"path": "t.py", "lines": [2]}],
            }),
            &ctx,
        )
        .await
        .unwrap();
    let session = started["session"].as_str().unwrap().to_string();
    assert!(session.starts_with("session-"), "{started}");
    assert_eq!(started["stopped"]["reason"], "breakpoint");
    // Inspect reads threads + frames + locals in one call (`x` visible).
    let seen = r
        .call("dap_inspect", serde_json::json!({"session": session}), &ctx)
        .await
        .unwrap();
    assert!(seen["frames"]
        .as_array()
        .map(|f| !f.is_empty())
        .unwrap_or(false));
    let dump = seen.to_string();
    assert!(dump.contains("\"x\""), "{dump}");
    assert!(dump.contains("42"), "{dump}");
    // Evaluate watches the stopped frame; frameless falls back to it.
    let evaled = r
        .call(
            "dap_evaluate",
            serde_json::json!({"session": session, "expression": "x * 2"}),
            &ctx,
        )
        .await
        .unwrap();
    assert_eq!(evaled["result"]["result"], "84");
    // Continue runs past the last breakpoint: the program ends, the
    // session auto-removes, and disconnecting it afterwards fails loud.
    let cont = r
        .call(
            "dap_continue",
            serde_json::json!({"session": session}),
            &ctx,
        )
        .await
        .unwrap();
    assert_eq!(cont["ended"], true);
    let err = r
        .call(
            "dap_disconnect",
            serde_json::json!({"session": session}),
            &ctx,
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains(&session), "{err}");
}

#[tokio::test]
async fn dap_tools_sessions_isolated_and_disconnect_removes() {
    let d = tempfile::tempdir().unwrap();
    let Some(r) = debugpy_registry(&d).await else {
        eprintln!("SKIP dap_tools_sessions_isolated_and_disconnect_removes: no Python 3 + bundled debugpy libs");
        return;
    };
    let ctx = ctx_for(&d);
    let start = |program: &str| {
        r.call(
            "dap_start",
            serde_json::json!({"program": program, "breakpoints": [{"path": program, "lines": [2]}]}),
            &ctx,
        )
    };
    let a = start("t.py").await.unwrap()["session"]
        .as_str()
        .unwrap()
        .to_string();
    std::fs::write(d.path().join("u.py"), "y = 1\nprint(y)\n").unwrap();
    let b = start("u.py").await.unwrap()["session"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(a, b, "ids must differ");
    // Disconnecting one leaves the other fully usable.
    let bye = r
        .call("dap_disconnect", serde_json::json!({"session": a}), &ctx)
        .await
        .unwrap();
    assert_eq!(bye["disconnected"], true);
    let again = r
        .call("dap_disconnect", serde_json::json!({"session": a}), &ctx)
        .await
        .unwrap_err()
        .to_string();
    assert!(again.contains(&a), "{again}");
    let seen = r
        .call("dap_inspect", serde_json::json!({"session": b}), &ctx)
        .await
        .unwrap();
    assert!(seen["frames"]
        .as_array()
        .map(|f| !f.is_empty())
        .unwrap_or(false));
    r.call("dap_disconnect", serde_json::json!({"session": b}), &ctx)
        .await
        .unwrap();
}

#[tokio::test]
async fn dap_tools_session_op_gate_denies_and_preserves() {
    let d = tempfile::tempdir().unwrap();
    let Some(r) = debugpy_registry(&d).await else {
        eprintln!("SKIP dap_tools_session_op_gate_denies_and_preserves: no Python 3 + bundled debugpy libs");
        return;
    };
    let ctx = ctx_for(&d);
    let session = r
        .call(
            "dap_start",
            serde_json::json!({"program": "t.py", "breakpoints": [{"path": "t.py", "lines": [2]}]}),
            &ctx,
        )
        .await
        .unwrap()["session"]
        .as_str()
        .unwrap()
        .to_string();
    // Permissions follow the program: deny the op and the session must
    // survive untouched (denial leaves zero side effects).
    let denied = polymorph_tools::ToolCallContext::new(
        d.path().to_path_buf(),
        polymorph_tools::Gate::new(vec![], vec!["dap_inspect:t.py".to_string()]),
        tokio_util::sync::CancellationToken::new(),
    );
    let err = r
        .call(
            "dap_inspect",
            serde_json::json!({"session": session}),
            &denied,
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("dap_inspect:t.py"), "{err}");
    // Session intact under the open gate.
    let seen = r
        .call("dap_inspect", serde_json::json!({"session": session}), &ctx)
        .await
        .unwrap();
    assert!(seen["frames"]
        .as_array()
        .map(|f| !f.is_empty())
        .unwrap_or(false));
    r.call(
        "dap_disconnect",
        serde_json::json!({"session": session}),
        &ctx,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn dap_tools_dlv_flow_through_table() {
    if !headed_enabled() {
        eprintln!(
            "SKIP dap_tools_dlv_flow_through_table: headed/TCP (set POLYMORPH_HEADED=1 to run)"
        );
        return;
    }
    // TCP transport through the SESSION TABLE path (the matrix dlv row
    // proves the client; this proves table routing + removal over TCP).
    if !std::process::Command::new("dlv")
        .arg("version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        eprintln!("SKIP dap_tools_dlv_flow_through_table: no dlv");
        return;
    }
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("go.mod"), "module probe\n\ngo 1.27\n").unwrap();
    std::fs::write(
        d.path().join("main.go"),
        "package main\n\nimport \"fmt\"\n\nfunc main() {\n\tx := 6 * 7\n\tfmt.Println(x + 1)\n}\n",
    )
    .unwrap();
    let r = polymorph_tools::registry::v1_registry();
    r.add_dap_adapter(
        "go",
        DapSpec {
            cmd: "dlv".to_string(),
            args: vec![],
            env: vec![],
            workdir: None,
        },
    )
    .await
    .unwrap();
    let ctx = ctx_for(&d);
    // NOTE: the table path hardcodes dlv's stdio-less invocation
    // (`dap -l 127.0.0.1:0 --headless` + case-dir cwd) — adapter-specific
    // facts live in ONE place (call_dap_start), not per caller.
    let started = r
        .call(
            "dap_start",
            serde_json::json!({
                "program": "main.go",
                "language": "go",
                "breakpoints": [{"path": "main.go", "lines": [7]}],
            }),
            &ctx,
        )
        .await
        .unwrap();
    let session = started["session"].as_str().unwrap().to_string();
    assert_eq!(started["stopped"]["reason"], "breakpoint");
    let evaled = r
        .call(
            "dap_evaluate",
            serde_json::json!({"session": session, "expression": "x"}),
            &ctx,
        )
        .await
        .unwrap();
    assert_eq!(evaled["result"]["result"], "42");
    r.call(
        "dap_disconnect",
        serde_json::json!({"session": session}),
        &ctx,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn dap_tools_inspect_caps_name_omissions() {
    // 60 locals in scope: the payload caps at MAX_INSPECT_VARS and says
    // how many it dropped (chunk-98 caps precedent, live proof).
    let d = tempfile::tempdir().unwrap();
    let Some(r) = debugpy_registry(&d).await else {
        eprintln!("SKIP dap_tools_inspect_caps_name_omissions: no Python 3 + bundled debugpy libs");
        return;
    };
    let mut prog = String::new();
    for i in 0..60 {
        prog.push_str(&format!("v{i} = {i}\n"));
    }
    prog.push_str("print(v0)\n");
    std::fs::write(d.path().join("big.py"), &prog).unwrap();
    let ctx = ctx_for(&d);
    let session = r
        .call(
            "dap_start",
            serde_json::json!({"program": "big.py", "breakpoints": [{"path": "big.py", "lines": [61]}]}),
            &ctx,
        )
        .await
        .unwrap()["session"]
        .as_str()
        .unwrap()
        .to_string();
    let seen = r
        .call("dap_inspect", serde_json::json!({"session": session}), &ctx)
        .await
        .unwrap();
    let scopes = seen["scopes"].as_array().unwrap();
    // Cap proof without overfitting the adapter's scope layout (debugpy
    // reports Locals AND Globals, both holding the 60): every scope
    // shows at most MAX and names its remainder; at least one scope
    // actually overflows.
    assert!(!scopes.is_empty(), "must report scopes");
    for s in scopes {
        let shown = s["variables"].as_array().map(Vec::len).unwrap_or(0);
        assert!(shown <= 50, "cap breached: {shown}");
    }
    let omitted: usize = scopes
        .iter()
        .map(|s| s["omitted_variables"].as_u64().unwrap_or(0) as usize)
        .sum();
    assert!(omitted > 0, "60 locals must overflow some scope");
    r.call(
        "dap_disconnect",
        serde_json::json!({"session": session}),
        &ctx,
    )
    .await
    .unwrap();
}
