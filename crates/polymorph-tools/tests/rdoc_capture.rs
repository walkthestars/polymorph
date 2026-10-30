//! Tool-level RenderDoc contracts: `rdoc_capture` driven through `Registry::call` — gate-first,
//! fail-closed spec, headed capture to a real `.rdc` — over a REAL install. Probe + loud-skip
//! discipline mirrors `dap_matrix.rs` (roster: `docs/renderdoc.md`).

/// Resolve `renderdoccmd`: absolute install path first, else PATH probe.
/// `None` = loud skip (host without RenderDoc — build-time acquisition
/// in the N1a plan; CI without it skips, never reds).
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
    ok.then(|| "renderdoccmd".to_string())
}

/// Resolve ucrt64 gcc.
fn resolve_gcc() -> Option<String> {
    const GCC: &str = "C:/msys64/ucrt64/bin/gcc.exe";
    std::path::Path::new(GCC).is_file().then(|| GCC.to_string())
}

/// Serializes headed GL work in-process. Cross-binary contention stays possible (cargo runs targets
/// as processes); if it ever bites, the escalation is a lockfile, not a bigger timeout.
static HEADED_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Headed gate: the triangle sample opens a real window and `renderdoccmd capture` injects into it
/// — default `cargo test` skips. Set `POLYMORPH_HEADED=1` to run; skips return green (counts
/// unchanged).
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

/// Compile the triangle fixture into `dir` (binary stays in temp, never
/// the repo). Returns the exe path. `None` = toolchain absent.
fn build_triangle(dir: &tempfile::TempDir, renderdoc_cmd: &str) -> Option<std::path::PathBuf> {
    let gcc = resolve_gcc()?;
    // Include dir derives from the binary (renderdoc_app.h ships in the
    // install — no hardcoded second path).
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

#[tokio::test]
async fn rdoc_version_shape() {
    let Some(cmd) = resolve_renderdoc() else {
        eprintln!("SKIP rdoc_version_shape: no renderdoccmd (see docs/renderdoc.md)");
        return;
    };
    let v = polymorph_tools::renderdoc::version(&polymorph_tools::renderdoc::RenderdocSpec { cmd })
        .await
        .unwrap();
    // Prefix-pinned, not exact (upgrade-proof: 1.46 today, drift allowed).
    assert!(v.starts_with("renderdoccmd"), "{v}");
}

#[tokio::test]
async fn rdoc_capture_triangle_via_registry() {
    if !headed_enabled() {
        eprintln!(
            "SKIP rdoc_capture_triangle_via_registry: headed (set POLYMORPH_HEADED=1 to run)"
        );
        return;
    }
    let _headed = HEADED_LOCK.lock().await;
    let d = tempfile::tempdir().unwrap();
    let Some(cmd) = resolve_renderdoc() else {
        eprintln!("SKIP rdoc_capture_triangle_via_registry: no renderdoccmd");
        return;
    };
    let Some(exe) = build_triangle(&d, &cmd) else {
        eprintln!("SKIP rdoc_capture_triangle_via_registry: no ucrt64 gcc");
        return;
    };
    let r = polymorph_tools::registry::v1_registry();
    r.set_renderdoc_spec(polymorph_tools::renderdoc::RenderdocSpec { cmd })
        .await;
    let ctx = ctx_for(&d);
    // Exercise THROUGH the call path (absolute program), so the test
    // proves resolution + gating + capture, not just the facade.
    let out = r
        .call(
            "rdoc_capture",
            serde_json::json!({ "program": exe.to_string_lossy() }),
            &ctx,
        )
        .await
        .unwrap();
    let path = out["capture"].as_str().unwrap().to_string();
    assert!(path.ends_with(".rdc"), "{out}");
    assert!(out["bytes"].as_u64().unwrap_or(0) > 0, "{out}");
    assert!(std::path::Path::new(&path).is_file(), "{out}");
}

#[tokio::test]
async fn rdoc_capture_gate_first_on_missing_file() {
    let d = tempfile::tempdir().unwrap();
    let r = polymorph_tools::registry::v1_registry();
    let ctx = ctx_for(&d);
    let err = r
        .call(
            "rdoc_capture",
            serde_json::json!({ "program": "ghost.exe" }),
            &ctx,
        )
        .await
        .unwrap_err();
    assert!(format!("{err}").contains("is not a file"), "{err}");
    // Zero side effects: no capture template touched.
    assert!(std::fs::read_dir(d.path()).unwrap().count() == 0);
}

#[tokio::test]
async fn rdoc_capture_deny_preserves() {
    let d = tempfile::tempdir().unwrap();
    let Some(cmd) = resolve_renderdoc() else {
        eprintln!("SKIP rdoc_capture_deny_preserves: no renderdoccmd");
        return;
    };
    let Some(exe) = build_triangle(&d, &cmd) else {
        eprintln!("SKIP rdoc_capture_deny_preserves: no ucrt64 gcc");
        return;
    };
    let r = polymorph_tools::registry::v1_registry();
    r.set_renderdoc_spec(polymorph_tools::renderdoc::RenderdocSpec { cmd })
        .await;
    let ctx = polymorph_tools::ToolCallContext::new(
        d.path().to_path_buf(),
        polymorph_tools::Gate::new(vec![], vec!["rdoc_capture".to_string()]),
        tokio_util::sync::CancellationToken::new(),
    );
    let err = r
        .call(
            "rdoc_capture",
            serde_json::json!({ "program": exe.to_string_lossy() }),
            &ctx,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, polymorph_tools::CallError::Denied(_)),
        "{err}"
    );
    assert!(std::fs::read_dir(d.path()).unwrap().all(|e| !e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .ends_with(".rdc")));
}

#[tokio::test]
async fn rdoc_capture_no_spec_fail_closed() {
    let d = tempfile::tempdir().unwrap();
    let exe = d.path().join("triangle.exe");
    std::fs::write(&exe, b"not-a-real-binary").unwrap();
    let r = polymorph_tools::registry::v1_registry();
    let ctx = ctx_for(&d);
    let err = r
        .call(
            "rdoc_capture",
            serde_json::json!({ "program": exe.to_string_lossy() }),
            &ctx,
        )
        .await
        .unwrap_err();
    assert!(format!("{err}").contains("no renderdoc binary"), "{err}");
}
