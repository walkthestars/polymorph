//! Adapter matrix smoke: scripted breakpoint + watch + evaluate per adapter through the production
//! clients. ONE generic driver over `DapSession` — rows differ only in construction (stdio vs TCP)
//! and launch arguments. Every row loud-skips when its adapter is absent (roster: `docs/dap-
//! adapters.md`); v2's node/csharp rows stay deferred there with evidence, not here as red.

use polymorph_tools::dap::{DapClient, DapSession, DapSpec, DapTcpClient};

/// Headed/TCP gate: the dlv row binds a TCP listener (the firewall-prompt suspect) — default `cargo
/// test` skips. Set `POLYMORPH_HEADED=1` to run; skips return green (counts unchanged).
fn headed_enabled() -> bool {
    std::env::var("POLYMORPH_HEADED")
        .map(|v| v == "1")
        .unwrap_or(false)
}

/// Probe a binary on PATH (`--version` must succeed).
fn have_bin(cmd: &str) -> bool {
    std::process::Command::new(cmd)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn resolve_python() -> Option<(String, String)> {
    let python = if let Ok(p) = std::env::var("PYTHON_BIN") {
        (!p.trim().is_empty()).then_some(p)
    } else if have_bin("python3") {
        Some("python3".to_string())
    } else if have_bin("python") {
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

/// One stopped-at-breakpoint inspection over ANY session: init →
/// held-launch → breakpoints → configured → stopped/breakpoint →
/// threads → stack → watch expression + frameless fallback. Returns
/// the two evaluated texts (the v2 M2 breakpoint+watch+eval triple).
async fn drive_break_eval(
    session: &mut impl DapSession,
    adapter_id: &str,
    launch_args: serde_json::Value,
    program: &str,
    break_line: u32,
    watch_expr: &str,
) -> (String, String) {
    session.initialize(adapter_id).await.unwrap();
    let launch_seq = session.send_request("launch", launch_args).await.unwrap();
    let bp = session
        .set_breakpoints(program, &[break_line])
        .await
        .unwrap();
    assert_eq!(bp["breakpoints"].as_array().map(Vec::len), Some(1));
    session.configuration_done().await.unwrap();
    session.await_response(launch_seq, "launch").await.unwrap();
    let stopped = session
        .await_event("stopped", std::time::Duration::from_secs(30))
        .await
        .unwrap();
    assert_eq!(stopped["body"]["reason"], "breakpoint");
    let thread_id = stopped["body"]["threadId"].as_u64().unwrap();
    let stack = session.stack_trace(thread_id, 0, 20).await.unwrap();
    let frame_id = stack["stackFrames"][0]["id"].as_u64().unwrap();
    let watch = session.evaluate(watch_expr, Some(frame_id)).await.unwrap();
    // Second evaluate stays in-frame: debugpy answers frameless
    // evaluates with an empty result (observed live — success, no
    // value), so the live rows keep frame context; the frameless
    // shape (omitted `frameId`) stays loopback-proven in `core_ops`.
    let plain = session.evaluate("1+2", Some(frame_id)).await.unwrap();
    (
        watch["result"].as_str().unwrap_or("").to_string(),
        plain["result"].as_str().unwrap_or("").to_string(),
    )
}

#[tokio::test]
async fn matrix_python_break_watch_eval() {
    let Some((python, libs)) = resolve_python() else {
        eprintln!("SKIP matrix_python_break_watch_eval: no Python 3 + bundled debugpy libs");
        return;
    };
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("m.py"), "x = 6*7\nprint(x + 1)\n").unwrap();
    let program = d.path().join("m.py").to_string_lossy().into_owned();
    let mut client = DapClient::spawn(&DapSpec {
        cmd: python,
        args: vec!["-m".to_string(), "debugpy.adapter".to_string()],
        env: vec![("PYTHONPATH".to_string(), libs)],
        workdir: None,
    })
    .await
    .unwrap();
    let (watch, plain) = drive_break_eval(
        &mut client,
        "debugpy",
        serde_json::json!({"program": program.clone()}),
        &program,
        2,
        "x",
    )
    .await;
    assert_eq!(watch, "42");
    assert_eq!(plain, "3");
    client.disconnect().await.unwrap();
}

#[tokio::test]
async fn matrix_go_dlv_break_watch_eval() {
    if !headed_enabled() {
        eprintln!(
            "SKIP matrix_go_dlv_break_watch_eval: headed/TCP (set POLYMORPH_HEADED=1 to run)"
        );
        return;
    }
    if !have_bin("dlv") {
        eprintln!("SKIP matrix_go_dlv_break_watch_eval: no dlv (go install github.com/go-delve/delve/cmd/dlv@latest)");
        return;
    }
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("go.mod"), "module probe\n\ngo 1.27\n").unwrap();
    // Break line 7: the stop lands BEFORE Println runs, with `x` bound.
    std::fs::write(
        d.path().join("main.go"),
        "package main\n\nimport \"fmt\"\n\nfunc main() {\n\tx := 6 * 7\n\tfmt.Println(x + 1)\n}\n",
    )
    .unwrap();
    let program = d.path().join("main.go").to_string_lossy().into_owned();
    // Dlv speaks DAP over TCP only: headless on:0, discover, connect. dlv resolves `program`
    // against ITS cwd — the case tempdir, so build debris (`__debug_bin*`) lands there and dies
    // with it.
    let spec = DapSpec {
        cmd: "dlv".to_string(),
        args: vec![
            "dap".to_string(),
            "-l".to_string(),
            "127.0.0.1:0".to_string(),
            "--headless".to_string(),
        ],
        env: vec![],
        workdir: Some(d.path().to_path_buf()),
    };
    let mut client = DapTcpClient::connect_discovered(&spec, &[]).await.unwrap();
    let (watch, plain) = drive_break_eval(
        &mut client,
        "dlv",
        serde_json::json!({"program": program.clone(), "mode": "debug"}),
        &program,
        7,
        "x",
    )
    .await;
    assert_eq!(watch, "42");
    assert_eq!(plain, "3");
    client.disconnect().await.unwrap();
}

#[tokio::test]
async fn matrix_rust_codelldb_break_watch_eval() {
    let exe = std::env::var("DAP_CODELLDB")
        .ok()
        .filter(|p| !p.is_empty())
        .or_else(|| {
            let tmp = std::env::temp_dir().join("dap/codelldb/extension/adapter/codelldb.exe");
            tmp.is_file().then(|| tmp.to_string_lossy().into_owned())
        });
    let Some(exe) = exe else {
        eprintln!("SKIP matrix_rust_codelldb_break_watch_eval: no codelldb (set DAP_CODELLDB; see docs/dap-adapters.md)");
        return;
    };
    if !have_bin("rustc") {
        eprintln!("SKIP matrix_rust_codelldb_break_watch_eval: no rustc");
        return;
    };
    let d = tempfile::tempdir().unwrap();
    // Break line 3: the stop lands BEFORE println runs, with `x` bound.
    std::fs::write(
        d.path().join("dbg.rs"),
        "fn main() {\n    let x = 6 * 7;\n    println!(\"{}\", x + 1);\n}\n",
    )
    .unwrap();
    let src = d.path().join("dbg.rs");
    let out = d.path().join("dbg.exe");
    let build = std::process::Command::new("rustc")
        .arg("-g")
        .arg(&src)
        .arg("-o")
        .arg(&out)
        .output()
        .unwrap();
    assert!(build.status.success(), "rustc -g must build the fixture");
    // codelldb speaks stdio unless `--port` is passed (discovered live:
    // bare spawn answers initialize — same posture as debugpy).
    let mut client = DapClient::spawn(&DapSpec {
        cmd: exe,
        args: vec![],
        env: vec![],
        workdir: None,
    })
    .await
    .unwrap();
    let (watch, plain) = drive_break_eval(
        &mut client,
        "codelldb",
        serde_json::json!({"program": out.to_string_lossy(), "cwd": d.path().to_string_lossy()}),
        &src.to_string_lossy(),
        3,
        "x",
    )
    .await;
    assert_eq!(watch, "42");
    assert_eq!(plain, "3");
    client.disconnect().await.unwrap();
}

fn resolve_netcoredbg() -> Option<String> {
    // Samsung netcoredbg (MIT) replaces vsdbg (VS-only license) as the C# row — same stdio DAP
    // posture, no handshake.
    if let Ok(p) = std::env::var("DAP_NETCOREDBG") {
        if !p.trim().is_empty() {
            return Some(p);
        }
    }
    let tmp = std::env::temp_dir().join("dap/netcoredbg/netcoredbg/netcoredbg.exe");
    tmp.is_file().then(|| tmp.to_string_lossy().into_owned())
}

#[tokio::test]
async fn matrix_csharp_netcoredbg_break_watch_eval() {
    let Some(dbg) = resolve_netcoredbg() else {
        eprintln!("SKIP matrix_csharp_netcoredbg_break_watch_eval: no netcoredbg (set DAP_NETCOREDBG; see docs/dap-adapters.md)");
        return;
    };
    if !have_bin("dotnet") {
        eprintln!("SKIP matrix_csharp_netcoredbg_break_watch_eval: no dotnet SDK");
        return;
    };
    let d = tempfile::tempdir().unwrap();
    // Break line 2: the stop lands BEFORE WriteLine runs, with `x` bound.
    // Plain top-level statements (restore stays inside SDK-bundled packs).
    let new = std::process::Command::new("dotnet")
        .arg("new")
        .arg("console")
        .arg("-o")
        .arg(d.path())
        .arg("-n")
        .arg("probe")
        .arg("--force")
        .output()
        .unwrap();
    assert!(
        new.status.success(),
        "dotnet new console must scaffold offline"
    );
    std::fs::write(
        d.path().join("Program.cs"),
        "int x = 6 * 7;\nSystem.Console.WriteLine(x + 1);\n",
    )
    .unwrap();
    let build = std::process::Command::new("dotnet")
        .arg("build")
        .arg("-c")
        .arg("Debug")
        .arg("--nologo")
        .arg("-v")
        .arg("q")
        .current_dir(d.path())
        .output()
        .unwrap();
    assert!(build.status.success(), "dotnet build must succeed offline");
    // The built dll is the launch target — matched by project name
    // (`-n probe` above), never first-alphabetical (dependency dlls
    // would make `.next()` arbitrary).
    let dll = d
        .path()
        .join("bin/Debug")
        .read_dir()
        .unwrap()
        .filter_map(|e| {
            let e = e.ok()?;
            e.path().is_dir().then(|| e.path())
        })
        .flat_map(|tfm| {
            std::fs::read_dir(tfm)
                .unwrap()
                .filter_map(|e| {
                    let p = e.ok()?.path();
                    (p.file_stem()?.to_string_lossy() == "probe"
                        && p.extension()?.to_string_lossy() == "dll")
                        .then_some(p)
                })
                .collect::<Vec<_>>()
        })
        .next()
        .unwrap();
    // Netcoredbg --interpreter=vscode speaks DAP over stdio with no license handshake.
    let mut client = DapClient::spawn(&DapSpec {
        cmd: dbg,
        args: vec!["--interpreter=vscode".to_string()],
        env: vec![],
        workdir: None,
    })
    .await
    .unwrap();
    let (watch, plain) = drive_break_eval(
        &mut client,
        "netcoredbg",
        serde_json::json!({"program": dll.to_string_lossy()}),
        &d.path().join("Program.cs").to_string_lossy(),
        2,
        "x",
    )
    .await;
    assert_eq!(watch, "42");
    assert_eq!(plain, "3");
    client.disconnect().await.unwrap();
}
