//! Debugpy integration: launch a debuggee through the
//! extension-bundled debugpy adapter, stop at a breakpoint, walk threads → stack → scopes →
//! variables, evaluate an expression, disconnect cleanly.
//!
//! Interpreter probe order: `PYTHON_BIN`, else `python3`/`python` from `PATH`, else the well-known
//! per-user install root (`%LOCALAPPDATA%\\Programs\\Python\\Python3*\\python.exe`, newest first).
//! Adapter libs probe: `$HOME/.vscode/extensions/` `ms-python.debugpy-*/bundled/libs` (first match)
//! → `PYTHONPATH` for the adapter child only (per-child env, never process-global). Absent
//! interpreter OR libs: LOUD skip (reported, never silent).

use polymorph_tools::dap::{DapClient, DapSpec};

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
            let text = String::from_utf8_lossy(&out.stdout) + String::from_utf8_lossy(&out.stderr);
            if out.status.success() && text.contains("Python 3") {
                return Some(candidate.to_string());
            }
        }
    }
    // Well-known per-user install root.
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        let mut hits: Vec<_> = std::fs::read_dir(format!("{local}\\Programs\\Python"))
            .map(|rd| {
                rd.filter_map(|e| {
                    let p = e.ok()?.path().join("python.exe");
                    p.is_file().then(|| p.to_string_lossy().into_owned())
                })
                .collect()
            })
            .unwrap_or_default();
        hits.sort();
        if let Some(last) = hits.pop() {
            return Some(last);
        }
    }
    None
}

/// Resolve extension-bundled debugpy libs for `PYTHONPATH`, or `None`.
fn resolve_debugpy_libs() -> Option<String> {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()?;
    let ext = std::fs::read_dir(format!("{home}/.vscode/extensions")).ok()?;
    let mut hits: Vec<_> = ext
        .filter_map(|e| {
            let name = e.ok()?.file_name().to_string_lossy().into_owned();
            name.starts_with("ms-python.debugpy-")
                .then(|| format!("{home}/.vscode/extensions/{name}/bundled/libs"))
        })
        .collect();
    hits.sort();
    hits.pop().filter(|p| std::path::Path::new(p).is_dir())
}

#[tokio::test]
async fn debugpy_launch_stops_evaluates_and_disconnects() {
    let Some(python) = resolve_python() else {
        eprintln!(
            "SKIP debugpy_launch_stops_evaluates_and_disconnects: no Python 3 (set PYTHON_BIN)"
        );
        return;
    };
    let Some(libs) = resolve_debugpy_libs() else {
        eprintln!("SKIP debugpy_launch_stops_evaluates_and_disconnects: no bundled debugpy libs (install ms-python.debugpy)");
        return;
    };
    let d = tempfile::tempdir().unwrap();
    // Breakpoint line 2: the stop lands BEFORE `print` runs, with `x`
    // bound (a line-1 stop would leave `x` unassigned — asserted below).
    std::fs::write(d.path().join("dbg.py"), "x = 6*7\nprint(x + 1)\n").unwrap();
    let program = d.path().join("dbg.py").to_string_lossy().into_owned();
    let mut client = DapClient::spawn(&DapSpec {
        cmd: python,
        args: vec!["-m".to_string(), "debugpy.adapter".to_string()],
        env: vec![("PYTHONPATH".to_string(), libs)],
        workdir: None,
    })
    .await
    .unwrap();
    client.initialize("debugpy").await.unwrap();
    // debugpy holds the `launch` response until `configurationDone`
    // (standard lifecycle — a single `request()` would deadlock it),
    // so the send stays outstanding across the configure calls.
    let launch_seq = client
        .send_request("launch", serde_json::json!({"program": program}))
        .await
        .unwrap();
    let bp = client.set_breakpoints(&program, &[2]).await.unwrap();
    assert_eq!(bp["breakpoints"].as_array().map(Vec::len), Some(1));
    client.configuration_done().await.unwrap();
    client.await_response(launch_seq, "launch").await.unwrap();
    let stopped = client
        .await_event("stopped", std::time::Duration::from_secs(30))
        .await
        .unwrap();
    assert_eq!(stopped["body"]["reason"], "breakpoint");
    let thread_id = stopped["body"]["threadId"].as_u64().unwrap();
    let stack = client.stack_trace(thread_id, 0, 20).await.unwrap();
    let frame_id = stack["stackFrames"][0]["id"].as_u64().unwrap();
    let scopes = client.scopes(frame_id).await.unwrap();
    let var_ref = scopes["scopes"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|s| s["variablesReference"].as_u64())
        .filter(|r| *r > 0)
        .unwrap();
    let vars = client.variables(var_ref).await.unwrap();
    let x = vars["variables"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["name"] == "x")
        .unwrap();
    assert_eq!(x["value"], "42");
    let evaled = client.evaluate("x", Some(frame_id)).await.unwrap();
    assert_eq!(evaled["result"], "42");
    client.disconnect().await.unwrap();
}
