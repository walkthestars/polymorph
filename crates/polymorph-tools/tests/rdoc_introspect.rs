//! Tool-level RenderDoc introspection contracts:
//! `rdoc_draws/state/diff/script` through `Registry::call` over headed
//! replays of the triangle fixture. Probe + loud-skip discipline mirrors
//! `dap_matrix.rs` (roster: `docs/renderdoc.md`). Headed `qrenderdoc`
//! needs a display; tests skip loud without the binary (headless-CI
//! support is future work, stated in).

fn resolve_renderdoc() -> Option<String> {
    const INSTALL: &str = "C:/Program Files/RenderDoc/renderdoccmd.exe";
    if std::path::Path::new(INSTALL).is_file() {
        return Some(INSTALL.to_string());
    }
    let ok = std::process::Command::new("renderdoccmd")
        .arg("version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    ok.then_some("renderdoccmd".to_string())
}

fn resolve_gcc() -> Option<String> {
    const GCC: &str = "C:/msys64/ucrt64/bin/gcc.exe";
    std::path::Path::new(GCC).is_file().then(|| GCC.to_string())
}

/// Serializes headed GL work in-process (defect — see
/// rdoc_capture.rs for the full note; same lock shape, per-binary).
static HEADED_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Headed gate: captures pop sample windows and `rdoc_script`
/// launches the qrenderdoc GUI — default `cargo test` skips. Set
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

/// Compile the triangle fixture into `dir` (binary stays in temp).
/// `None` = toolchain absent (loud skip at callers).
fn build_triangle(dir: &tempfile::TempDir, renderdoc_cmd: &str) -> Option<std::path::PathBuf> {
    let gcc = resolve_gcc()?;
    let include = std::path::Path::new(renderdoc_cmd)
        .parent()?
        .to_string_lossy()
        .into_owned();
    let fixture = format!(
        "{}/tests/fixtures/rdoc_triangle.c",
        env!("CARGO_MANIFEST_DIR")
    );
    let exe = dir.path().join("triangle.exe");
    let path = format!(
        "C:/msys64/ucrt64/bin;{}",
        std::env::var("PATH").unwrap_or_default()
    );
    let status = std::process::Command::new(&gcc)
        .env("PATH", path)
        .args(["-O2", "-I", &include, "-o"])
        .arg(&exe)
        .arg(&fixture)
        .args(["-lopengl32", "-lgdi32"])
        .status()
        .ok()?;
    status.success().then_some(exe)
}

/// Record one capture through the PRODUCTION `rdoc_capture` path.
/// Returns the `.rdc` path. Panics on failure (a capture that fails
/// here is a red, not a skip — the binary + toolchain resolved above).
async fn record_capture(
    r: &polymorph_tools::Registry,
    ctx: &polymorph_tools::ToolCallContext,
    exe: &std::path::Path,
    template: &std::path::Path,
) -> String {
    // Enroll the spec first (the wire_renderdoc path, awaited inline —
    // tests are async; see wired_registry note above).
    let out = r
        .call(
            "rdoc_capture",
            serde_json::json!({
                "program": exe.to_string_lossy(),
                "capture_file": template.to_string_lossy(),
            }),
            ctx,
        )
        .await
        .unwrap();
    out["capture"].as_str().unwrap().to_string()
}

async fn ready_registry(
    dir: &tempfile::TempDir,
    cmd: &str,
) -> (polymorph_tools::Registry, polymorph_tools::ToolCallContext) {
    let r = polymorph_tools::registry::v1_registry();
    r.set_renderdoc_spec(polymorph_tools::renderdoc::RenderdocSpec {
        cmd: cmd.to_string(),
    })
    .await;
    let ctx = ctx_for(dir);
    (r, ctx)
}

#[tokio::test]
async fn rdoc_draws_lists_triangle_draws() {
    if !headed_enabled() {
        eprintln!("SKIP rdoc_draws_lists_triangle_draws: headed (set POLYMORPH_HEADED=1 to run)");
        return;
    }
    let _headed = HEADED_LOCK.lock().await;
    let d = tempfile::tempdir().unwrap();
    let Some(cmd) = resolve_renderdoc() else {
        eprintln!("SKIP rdoc_draws_lists_triangle_draws: no renderdoccmd");
        return;
    };
    let Some(exe) = build_triangle(&d, &cmd) else {
        eprintln!("SKIP rdoc_draws_lists_triangle_draws: no ucrt64 gcc");
        return;
    };
    let (r, ctx) = ready_registry(&d, &cmd).await;
    let rdc = record_capture(&r, &ctx, &exe, &d.path().join("t")).await;
    let out = r
        .call("rdoc_draws", serde_json::json!({ "capture": rdc }), &ctx)
        .await
        .unwrap();
    assert!(out["count"].as_u64().unwrap_or(0) >= 1, "{out}");
    assert!(out["draws"].as_u64().unwrap_or(0) >= 1, "{out}");
    let first_draw = out["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e.get("draw").and_then(|v| v.as_bool()) == Some(true))
        .expect("a draw event");
    assert!(first_draw["event"].as_u64().unwrap_or(0) > 0, "{out}");
}

#[tokio::test]
async fn rdoc_state_reads_triangle_pipeline() {
    if !headed_enabled() {
        eprintln!(
            "SKIP rdoc_state_reads_triangle_pipeline: headed (set POLYMORPH_HEADED=1 to run)"
        );
        return;
    }
    let _headed = HEADED_LOCK.lock().await;
    let d = tempfile::tempdir().unwrap();
    let Some(cmd) = resolve_renderdoc() else {
        eprintln!("SKIP rdoc_state_reads_triangle_pipeline: no renderdoccmd");
        return;
    };
    let Some(exe) = build_triangle(&d, &cmd) else {
        eprintln!("SKIP rdoc_state_reads_triangle_pipeline: no ucrt64 gcc");
        return;
    };
    let (r, ctx) = ready_registry(&d, &cmd).await;
    let rdc = record_capture(&r, &ctx, &exe, &d.path().join("t")).await;
    let draws = r
        .call("rdoc_draws", serde_json::json!({ "capture": rdc }), &ctx)
        .await
        .unwrap();
    let event = draws["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e.get("draw").and_then(|v| v.as_bool()) == Some(true))
        .and_then(|e| e.get("event").and_then(|v| v.as_u64()))
        .expect("a draw event id");
    let out = r
        .call(
            "rdoc_state",
            serde_json::json!({ "capture": rdc, "event": event }),
            &ctx,
        )
        .await
        .unwrap();
    let topo = out["state"]["topology"].as_str().unwrap_or("");
    assert!(topo.contains("Triangle"), "{out}");
    assert!(
        out["state"]["viewports"].as_u64().unwrap_or(0) >= 1,
        "{out}"
    );
}

#[tokio::test]
async fn rdoc_diff_self_empty_across_recordings() {
    if !headed_enabled() {
        eprintln!(
            "SKIP rdoc_diff_self_empty_across_recordings: headed (set POLYMORPH_HEADED=1 to run)"
        );
        return;
    }
    let _headed = HEADED_LOCK.lock().await;
    let d = tempfile::tempdir().unwrap();
    let Some(cmd) = resolve_renderdoc() else {
        eprintln!("SKIP rdoc_diff_self_empty_across_recordings: no renderdoccmd");
        return;
    };
    let Some(exe) = build_triangle(&d, &cmd) else {
        eprintln!("SKIP rdoc_diff_self_empty_across_recordings: no ucrt64 gcc");
        return;
    };
    let (r, ctx) = ready_registry(&d, &cmd).await;
    let a = record_capture(&r, &ctx, &exe, &d.path().join("a")).await;
    let b = record_capture(&r, &ctx, &exe, &d.path().join("b")).await;
    assert_ne!(a, b, "two recordings must be distinct files");
    let out = r
        .call(
            "rdoc_diff",
            serde_json::json!({ "capture_a": a, "capture_b": b }),
            &ctx,
        )
        .await
        .unwrap();
    assert_eq!(out["added"].as_array().map(Vec::len), Some(0), "{out}");
    assert_eq!(out["removed"].as_array().map(Vec::len), Some(0), "{out}");
    assert!(out["count_a"].as_u64().unwrap_or(0) > 0, "{out}");
    assert_eq!(out["count_a"], out["count_b"], "{out}");
}

#[tokio::test]
async fn rdoc_script_runs_user_echo() {
    if !headed_enabled() {
        eprintln!("SKIP rdoc_script_runs_user_echo: headed (set POLYMORPH_HEADED=1 to run)");
        return;
    }
    let _headed = HEADED_LOCK.lock().await;
    let d = tempfile::tempdir().unwrap();
    let Some(cmd) = resolve_renderdoc() else {
        eprintln!("SKIP rdoc_script_runs_user_echo: no renderdoccmd");
        return;
    };
    // Trivial user script following the RDOC_OUT + quit contract (the
    // shape every rdoc_script caller must follow — schema documents it).
    let script = d.path().join("echo_user.py");
    std::fs::write(
        &script,
        "import json, os\nwith open(os.environ['RDOC_OUT'], 'w') as f:\n    json.dump({'ok': True, 'echo': 42}, f)\nfrom PySide2.QtWidgets import QApplication\napp = QApplication.instance()\nif app is not None:\n    app.quit()\n",
    )
    .unwrap();
    let (r, ctx) = ready_registry(&d, &cmd).await;
    let out = r
        .call(
            "rdoc_script",
            serde_json::json!({ "script": script.to_string_lossy() }),
            &ctx,
        )
        .await
        .unwrap();
    assert_eq!(out["truncated"], false, "{out}");
    assert!(out["output"].as_str().unwrap_or("").contains("42"), "{out}");
}

#[tokio::test]
async fn read_renderdoc_url_lists_draws_headed() {
    // P2 renderdoc:// delegate over the N1b machinery.
    // Headed (qrenderdoc + sample window) — gated like every headed
    // test here; committed unrun this chunk (117 precedent: delegate
    // bodies are live-proven N1b code, only the parse+route is new).
    if !headed_enabled() {
        eprintln!(
            "SKIP read_renderdoc_url_lists_draws_headed: headed (set POLYMORPH_HEADED=1 to run)"
        );
        return;
    }
    let d = tempfile::tempdir().unwrap();
    let Some(cmd) = resolve_renderdoc() else {
        eprintln!("SKIP read_renderdoc_url_lists_draws_headed: no renderdoccmd");
        return;
    };
    let Some(exe) = build_triangle(&d, &cmd) else {
        eprintln!("SKIP read_renderdoc_url_lists_draws_headed: no ucrt64 gcc");
        return;
    };
    let _headed = HEADED_LOCK.lock().await;
    let (r, ctx) = ready_registry(&d, &cmd).await;
    let rdc = record_capture(&r, &ctx, &exe, &d.path().join("t")).await;
    let out = r
        .call(
            "read",
            serde_json::json!({"path": format!("renderdoc://{rdc}")}),
            &ctx,
        )
        .await
        .unwrap();
    assert!(out["count"].as_u64().unwrap_or(0) >= 1, "{out}");
    assert!(out["draws"].as_u64().unwrap_or(0) >= 1, "{out}");
}

#[tokio::test]
async fn read_renderdoc_url_with_event_reads_state_headed() {
    // Same gate + provenance as the draws test above (event form).
    if !headed_enabled() {
        eprintln!("SKIP read_renderdoc_url_with_event_reads_state_headed: headed (set POLYMORPH_HEADED=1 to run)");
        return;
    }
    let d = tempfile::tempdir().unwrap();
    let Some(cmd) = resolve_renderdoc() else {
        eprintln!("SKIP read_renderdoc_url_with_event_reads_state_headed: no renderdoccmd");
        return;
    };
    let Some(exe) = build_triangle(&d, &cmd) else {
        eprintln!("SKIP read_renderdoc_url_with_event_reads_state_headed: no ucrt64 gcc");
        return;
    };
    let _headed = HEADED_LOCK.lock().await;
    let (r, ctx) = ready_registry(&d, &cmd).await;
    let rdc = record_capture(&r, &ctx, &exe, &d.path().join("t")).await;
    let draws = r
        .call("rdoc_draws", serde_json::json!({"capture": rdc}), &ctx)
        .await
        .unwrap();
    let event = draws["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e.get("draw").and_then(|v| v.as_bool()) == Some(true))
        .and_then(|e| e.get("event").and_then(|v| v.as_u64()))
        .expect("a draw event id");
    let out = r
        .call(
            "read",
            serde_json::json!({"path": format!("renderdoc://{rdc}#{event}")}),
            &ctx,
        )
        .await
        .unwrap();
    assert!(
        out["state"]["topology"]
            .as_str()
            .unwrap_or("")
            .contains("Triangle"),
        "{out}"
    );
}

#[tokio::test]
async fn rdoc_draws_deny_preserves() {
    let d = tempfile::tempdir().unwrap();
    let r = polymorph_tools::registry::v1_registry();
    let ctx = polymorph_tools::ToolCallContext::new(
        d.path().to_path_buf(),
        polymorph_tools::Gate::new(vec![], vec!["rdoc_draws".to_string()]),
        tokio_util::sync::CancellationToken::new(),
    );
    let err = r
        .call(
            "rdoc_draws",
            serde_json::json!({ "capture": "ghost.rdc" }),
            &ctx,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, polymorph_tools::CallError::Denied(_)),
        "{err}"
    );
}

#[tokio::test]
async fn rdoc_draws_missing_capture_is_red() {
    let d = tempfile::tempdir().unwrap();
    let r = polymorph_tools::registry::v1_registry();
    let ctx = ctx_for(&d);
    let err = r
        .call(
            "rdoc_draws",
            serde_json::json!({ "capture": "ghost.rdc" }),
            &ctx,
        )
        .await
        .unwrap_err();
    assert!(format!("{err}").contains("is not a file"), "{err}");
}
