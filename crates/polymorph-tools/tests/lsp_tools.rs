//! Registry-level LSP tool tests (chunk 100, L-actuation 3A).
//!
//! The fixture probe server (`fixtures/lsp_probe_server.py`) is enrolled
//! through the PRODUCTION path (`add_lsp_server` → lazy spawn on first
//! query) and driven through `Registry::call` — gate → route → didOpen +
//! request → wrapped result. Needs Python 3 (`PYTHON_BIN`, else
//! `python3`/`python` probed): absent interpreter = LOUD skip.

use polymorph_tools::lsp::LspSpec;
use std::path::PathBuf;

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("lsp_probe_server.py")
}

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

fn ctx_for(dir: &tempfile::TempDir) -> polymorph_tools::ToolCallContext {
    polymorph_tools::ToolCallContext::new(
        dir.path().to_path_buf(),
        polymorph_tools::Gate::new(vec![], vec![]),
        tokio_util::sync::CancellationToken::new(),
    )
}

/// Hover through the full tool path returns the fixture's canned markdown.
#[tokio::test]
async fn lsp_hover_reaches_fixture_through_registry() {
    let Some(python) = resolve_python() else {
        eprintln!("SKIP lsp_hover_reaches_fixture_through_registry: no Python 3 (set PYTHON_BIN)");
        return;
    };
    let r = polymorph_tools::registry::v1_registry();
    r.add_lsp_server(
        "python",
        LspSpec {
            cmd: python,
            args: vec![fixture_path().to_string_lossy().into_owned()],
        },
    )
    .await
    .unwrap();
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("a.py"), "x = 1\n").unwrap();
    let out = r
        .call(
            "lsp_hover",
            serde_json::json!({"path": "a.py", "line": 0, "character": 0}),
            &ctx_for(&d),
        )
        .await
        .unwrap();
    assert_eq!(out["language"], "python");
    assert!(out["result"].to_string().contains("probe-hover"), "{out}");
}

/// Definition through the same path returns the fixture's canned
/// location (it speaks definition as well as hover): per-op plumbing
/// proven twice over, both shapes wrapped identically.
#[tokio::test]
async fn lsp_definition_returns_fixture_location() {
    let Some(python) = resolve_python() else {
        eprintln!("SKIP lsp_definition_failure_names_the_op: no Python 3 (set PYTHON_BIN)");
        return;
    };
    let r = polymorph_tools::registry::v1_registry();
    r.add_lsp_server(
        "python",
        LspSpec {
            cmd: python,
            args: vec![fixture_path().to_string_lossy().into_owned()],
        },
    )
    .await
    .unwrap();
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("a.py"), "x = 1\n").unwrap();
    let out = r
        .call(
            "lsp_definition",
            serde_json::json!({"path": "a.py", "line": 0, "character": 0}),
            &ctx_for(&d),
        )
        .await
        .unwrap();
    assert_eq!(out["language"], "python");
    assert!(out["result"].to_string().contains("a.py"), "{out}");
}
