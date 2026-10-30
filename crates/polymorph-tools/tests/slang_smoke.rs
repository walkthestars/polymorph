//! Slang supply-chain smoke: `slangc` pinned + working + emitting debug-info SPIR-V (the artifact
//! the future Vulkan pipeline consumes). Probe + loud-skip discipline mirrors `dap_matrix.rs`
//! (roster: `docs/renderdoc.md`). No agent tool consumes `.spv` yet — supply-chain + artifact proof
//! only.

/// Resolve `slangc`: `SLANGC`, then the winget package under
/// `%LOCALAPPDATA%`, then PATH. `None` = loud skip (host without slang;
/// CI without it skips, never reds). NOTE: `slangc` has NO `--version`
/// flag (E00017) — the probe uses `-h target`, and the pinned version
/// lives in the winget id, not the binary.
fn resolve_slangc() -> Option<String> {
    if let Ok(explicit) = std::env::var("SLANGC") {
        if std::path::Path::new(&explicit).is_file() {
            return Some(explicit);
        }
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        let install = std::path::PathBuf::from(local).join(
            "Microsoft/WinGet/Packages/shader-slang.slang_Microsoft.Winget.Source_8wekyb3d8bbwe/bin/slangc.exe",
        );
        if install.is_file() {
            return Some(install.to_string_lossy().into_owned());
        }
    }
    let ok = std::process::Command::new("slangc")
        .args(["-h", "target"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    ok.then_some("slangc".to_string())
}

#[test]
fn slang_compiles_triangle_shaders_with_debug_info() {
    let Some(slangc) = resolve_slangc() else {
        eprintln!("SKIP slang_compiles_triangle_shaders_with_debug_info: no slangc (see docs/renderdoc.md)");
        return;
    };
    let fixture = format!(
        "{}/tests/fixtures/rdoc_shade.slang",
        env!("CARGO_MANIFEST_DIR")
    );
    let dir = tempfile::tempdir().unwrap();
    for (entry, stage, file) in [
        ("vsMain", "vertex", "vs.spv"),
        ("fsMain", "fragment", "fs.spv"),
    ] {
        let out = dir.path().join(file);
        let status = std::process::Command::new(&slangc)
            .args([
                "-target", "spirv", "-g", "-entry", entry, "-stage", stage, "-o",
            ])
            .arg(&out)
            .arg(&fixture)
            .status()
            .expect("slangc spawn failed");
        assert!(status.success(), "{entry} compile failed: {status}");
        let bytes = std::fs::read(&out).expect("spv unreadable");
        assert!(!bytes.is_empty(), "{entry} emitted empty spv");
        // Non-Semantic Shader DebugInfo extended-set name rides embedded
        // in the binary (byte scan —.spv is not UTF-8 text). Absence
        // means `-g` did not do its job: red, not a skip.
        let needle = b"NonSemantic.Shader.DebugInfo.100";
        assert!(
            bytes.windows(needle.len()).any(|w| w == needle),
            "{entry} spv lacks debug info"
        );
    }
}
