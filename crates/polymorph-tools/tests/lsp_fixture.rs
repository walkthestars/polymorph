//! Fixture-server LSP tests (spawn-positive interop).
//!
//! A REAL child speaks REAL LSP here: `fixtures/lsp_probe_server.py`
//! (stdlib-only) is spawned through the production `LspClient::spawn`
//! path — initialize handshake, hover round-trip, shutdown sequence —
//! while duplex unit tests covered the framing shape in `lsp.rs`.
//! Needs a Python 3 interpreter: `PYTHON_BIN`, else `python3`/`python` from
//! `PATH` probed with `--version` (strict `Python 3` match — the broken
//! WindowsApps alias fails the probe). Absent interpreter: LOUD skip
//! (`eprintln!` + early return — reported as skip, never a silent pass).

use polymorph_tools::lsp::{LspClient, LspSpec};
use std::path::PathBuf;

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("lsp_probe_server.py")
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

fn fixture_spec(python: &str) -> LspSpec {
    LspSpec {
        cmd: python.to_string(),
        args: vec![fixture_path().to_string_lossy().into_owned()],
    }
}

/// Spawn-positive interop: real child → real initialize (capabilities +
/// serverInfo) → real hover (canned markdown) → clean shutdown.
#[tokio::test]
async fn spawn_initializes_hovers_and_shuts_down() {
    let Some(python) = resolve_python() else {
        eprintln!("SKIP spawn_initializes_hovers_and_shuts_down: no Python 3 (set PYTHON_BIN)");
        return;
    };
    let mut client = LspClient::spawn(&fixture_spec(&python)).await.unwrap();
    let init = client.initialize(Some("file:///probe")).await.unwrap();
    assert_eq!(
        init.pointer("/serverInfo/name").and_then(|v| v.as_str()),
        Some("polymorph-probe-lsp"),
        "{init}"
    );
    assert_eq!(
        init.pointer("/capabilities/hoverProvider"),
        Some(&serde_json::Value::Bool(true)),
        "{init}"
    );
    let hover = client
        .request(
            "textDocument/hover",
            serde_json::json!({"textDocument": {"uri": "file:///probe/a.rs"}, "position": {"line": 0, "character": 0}}),
        )
        .await
        .unwrap();
    assert!(hover.to_string().contains("probe-hover"), "{hover}");
    client.shutdown().await.unwrap();
}

/// Unknown methods surface as typed errors naming the method (never a
/// hang waiting for a response that will not come).
#[tokio::test]
async fn unknown_method_errors_loudly() {
    let Some(python) = resolve_python() else {
        eprintln!("SKIP unknown_method_errors_loudly: no Python 3 (set PYTHON_BIN)");
        return;
    };
    let mut client = LspClient::spawn(&fixture_spec(&python)).await.unwrap();
    client.initialize(Some("file:///probe")).await.unwrap();
    let err = client
        .request("workspace/neverHeardOfIt", serde_json::Value::Null)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("neverHeardOfIt"), "{err}");
    client.shutdown().await.unwrap();
}

/// A bogus command fails at spawn with the command named (never a hang,
/// never a downstream handshake error).
#[tokio::test]
async fn bogus_command_fails_at_spawn() {
    let err = LspClient::spawn(&LspSpec {
        cmd: "polymorph-definitely-not-a-binary".to_string(),
        args: vec![],
    })
    .await
    .unwrap_err();
    assert!(
        err.to_string()
            .contains("polymorph-definitely-not-a-binary"),
        "{err}"
    );
}

/// Op-surface proof (the v2 9): every wrapper round-trips against
/// the fixture with shape asserts; the rename echoes `newName` (request
/// fidelity, not just connectivity); raw passthrough (`request` direct)
/// agrees with its wrapper on the same method.
#[tokio::test]
async fn op_surface_round_trips() {
    let Some(python) = resolve_python() else {
        eprintln!("SKIP op_surface_round_trips: no Python 3 (set PYTHON_BIN)");
        return;
    };
    let mut client = LspClient::spawn(&fixture_spec(&python)).await.unwrap();
    client.initialize(Some("file:///probe")).await.unwrap();
    let uri = "file:///probe/a.rs";
    let hover = client.hover(uri, 0, 0).await.unwrap();
    assert!(hover.to_string().contains("probe-hover"), "{hover}");
    let raw_hover = client
        .request(
            "textDocument/hover",
            serde_json::json!({"textDocument": {"uri": uri}, "position": {"line": 0, "character": 0}}),
        )
        .await
        .unwrap();
    assert_eq!(raw_hover, hover, "raw passthrough agrees with wrapper");
    let def = client.definition(uri, 0, 0).await.unwrap();
    assert_eq!(def.as_array().map(Vec::len), Some(1), "{def}");
    let refs = client.references(uri, 0, 0, true).await.unwrap();
    assert_eq!(refs.as_array().map(Vec::len), Some(2), "{refs}");
    let diag = client.diagnostic(uri).await.unwrap();
    assert!(diag.to_string().contains("probe-diagnostic"), "{diag}");
    let rename = client.rename(uri, 0, 0, "renamed").await.unwrap();
    assert!(rename.to_string().contains("renamed"), "{rename}");
    let actions = client.code_action(uri, 0, 0).await.unwrap();
    assert!(actions.to_string().contains("probe-action"), "{actions}");
    let symbols = client.document_symbol(uri).await.unwrap();
    assert!(symbols.to_string().contains("probe-symbol"), "{symbols}");
    client.shutdown().await.unwrap();
}

/// Observe-only incoming queue: the pushes the server emits around a
/// rename land in `drain_incoming` (pipes are FIFO — a follow-up request
/// pumps everything the rename emitted before its own response arrives).
#[tokio::test]
async fn incoming_observes_pushes() {
    let Some(python) = resolve_python() else {
        eprintln!("SKIP incoming_observes_pushes: no Python 3 (set PYTHON_BIN)");
        return;
    };
    let mut client = LspClient::spawn(&fixture_spec(&python)).await.unwrap();
    client.initialize(Some("file:///probe")).await.unwrap();
    let uri = "file:///probe/a.rs";
    client.rename(uri, 0, 0, "renamed").await.unwrap();
    client.document_symbol(uri).await.unwrap();
    let methods: Vec<String> = client
        .drain_incoming()
        .iter()
        .filter_map(|m| m.get("method").and_then(|v| v.as_str()).map(str::to_string))
        .collect();
    assert!(
        methods.contains(&"textDocument/publishDiagnostics".to_string()),
        "diagnostics observed, got {methods:?}"
    );
    assert!(
        methods.contains(&"workspace/willRenameFiles".to_string()),
        "rename-files observed, got {methods:?}"
    );
    assert!(client.drain_incoming().is_empty(), "drain takes all");
    client.shutdown().await.unwrap();
}

/// Shape-golden transcript (`tests/cassettes/lsp_probe.json`, committed):
/// the file is READ-ONLY input (no test writes it — the mcp key-order
/// churn lesson) and the live surface's method set equals its spine set
/// (order-insensitive parsed comparison, never byte compare).
#[tokio::test]
async fn golden_spine_covers_live_surface() {
    let golden: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests")
                .join("cassettes")
                .join("lsp_probe.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let spine: std::collections::BTreeSet<String> = golden["spine"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    let Some(python) = resolve_python() else {
        eprintln!("SKIP golden_spine_covers_live_surface: no Python 3 (set PYTHON_BIN)");
        return;
    };
    let mut client = LspClient::spawn(&fixture_spec(&python)).await.unwrap();
    let init = client.initialize(Some("file:///probe")).await.unwrap();
    assert_eq!(
        init.pointer("/serverInfo/name").and_then(|v| v.as_str()),
        golden["server"].as_str(),
        "golden names this server"
    );
    let uri = "file:///probe/a.rs";
    let mut live = std::collections::BTreeSet::from(["initialize".to_string()]);
    client.hover(uri, 0, 0).await.unwrap();
    live.insert("textDocument/hover".to_string());
    client.definition(uri, 0, 0).await.unwrap();
    live.insert("textDocument/definition".to_string());
    client.references(uri, 0, 0, false).await.unwrap();
    live.insert("textDocument/references".to_string());
    client.diagnostic(uri).await.unwrap();
    live.insert("textDocument/diagnostic".to_string());
    client.rename(uri, 0, 0, "x").await.unwrap();
    live.insert("textDocument/rename".to_string());
    client.code_action(uri, 0, 0).await.unwrap();
    live.insert("textDocument/codeAction".to_string());
    client.document_symbol(uri).await.unwrap();
    live.insert("textDocument/documentSymbol".to_string());
    client.shutdown().await.unwrap();
    live.insert("shutdown".to_string());
    assert_eq!(live, spine, "live surface == golden spine");
}
