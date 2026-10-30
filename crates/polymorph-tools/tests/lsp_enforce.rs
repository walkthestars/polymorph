//! Write-through enforcement tests.
//!
//! Writes and edits route through the language server where one runs
//! (diagnostic pull, inform-only); without a server the write stands with
//! a once-per-language fallback notice; breakage evicts and never fails
//! the write (D4). Needs a Python 3 interpreter for the live-server test
//! (same LOUD-skip resolver as the fixture suites); the fallback/skip
//! tests run anywhere.

use polymorph_tools::lsp::LspSpec;
use polymorph_tools::{Gate, Registry, ToolCallContext};
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

fn open_registry() -> (tempfile::TempDir, Registry, ToolCallContext) {
    let d = tempfile::TempDir::new().unwrap();
    let r = polymorph_tools::registry::v1_registry();
    let ctx = ToolCallContext::new(
        d.path().to_path_buf(),
        Gate::new(vec![], vec![]),
        tokio_util::sync::CancellationToken::new(),
    );
    (d, r, ctx)
}

/// Live server: a `.rs` write consults the fixture (always 1 diagnostic)
/// and records the notice; the write itself is unaffected.
#[tokio::test]
async fn post_write_check_records_diagnostics() {
    let Some(python) = resolve_python() else {
        eprintln!("SKIP post_write_check_records_diagnostics: no Python 3 (set PYTHON_BIN)");
        return;
    };
    let (_d, r, ctx) = open_registry();
    r.add_lsp_server(
        "rust",
        LspSpec {
            cmd: python,
            args: vec![fixture_path().to_string_lossy().into_owned()],
        },
    )
    .await
    .unwrap();
    r.call(
        "write",
        serde_json::json!({"path": "a.rs", "content": "fn main() {}\n"}),
        &ctx,
    )
    .await
    .unwrap();
    assert_eq!(
        r.lsp_take_notice().await.as_deref(),
        Some("lsp-diagnostics:rust:1")
    );
}

/// Pool records diagnostic messages per URI (L4 gutter feed): one write
/// through the fixture leaves exactly its message under the file's URI.
#[tokio::test]
async fn pool_records_messages_per_uri() {
    let Some(python) = resolve_python() else {
        eprintln!("SKIP pool_records_messages_per_uri: no Python 3 (set PYTHON_BIN)");
        return;
    };
    let (_d, r, ctx) = open_registry();
    r.add_lsp_server(
        "rust",
        LspSpec {
            cmd: python,
            args: vec![fixture_path().to_string_lossy().into_owned()],
        },
    )
    .await
    .unwrap();
    r.call(
        "write",
        serde_json::json!({"path": "a.rs", "content": "fn main() {}\n"}),
        &ctx,
    )
    .await
    .unwrap();
    let recorded = r.lsp_diagnostics().await;
    assert_eq!(recorded.len(), 1);
    let (uri, messages) = recorded.iter().next().unwrap();
    assert!(uri.ends_with("a.rs"), "{uri}");
    assert_eq!(messages, &vec!["probe-diagnostic".to_string()]);
    // The same record keeps the wire line (1-based) + severity.
    let details = r.lsp_diagnostic_details().await;
    assert_eq!(
        details[uri],
        vec![polymorph_tools::lsp::LspDiagnostic {
            line: Some(1),
            severity: Some(2),
            message: "probe-diagnostic".into(),
        }]
    );
}

/// No server for the language: the write stands, exactly one fallback
/// notice fires, the second write is silent (warn-once discipline).
#[tokio::test]
async fn fallback_warns_once_without_server() {
    let (_d, r, ctx) = open_registry();
    for _ in 0..2 {
        r.call(
            "write",
            serde_json::json!({"path": "a.py", "content": "x = 1\n"}),
            &ctx,
        )
        .await
        .unwrap();
    }
    // One notice total across both writes (the second take is empty).
    assert_eq!(
        r.lsp_take_notice().await.as_deref(),
        Some("lsp-fallback:python")
    );
    assert_eq!(r.lsp_take_notice().await, None);
}

/// Breakage never fails the write: a bogus command evicts to fallback
/// (spawn fails → no client → fallback path, warn-once per language).
#[tokio::test]
async fn breakage_never_fails_the_write() {
    let (_d, r, ctx) = open_registry();
    r.add_lsp_server(
        "go",
        LspSpec {
            cmd: "polymorph-definitely-not-a-binary".to_string(),
            args: vec![],
        },
    )
    .await
    .unwrap();
    r.call(
        "write",
        serde_json::json!({"path": "a.go", "content": "package main\n"}),
        &ctx,
    )
    .await
    .unwrap();
    assert_eq!(
        r.lsp_take_notice().await.as_deref(),
        Some("lsp-fallback:go")
    );
}

/// Unknown extensions skip silently: plain write, zero notices.
#[tokio::test]
async fn unknown_extension_skips_silently() {
    let (_d, r, ctx) = open_registry();
    r.call(
        "write",
        serde_json::json!({"path": "a.zzz", "content": "???\n"}),
        &ctx,
    )
    .await
    .unwrap();
    assert_eq!(r.lsp_take_notice().await, None);
}

fn wire_entry(
    id: &str,
    cmd: String,
    fixture: &std::path::Path,
    languages: &[&str],
) -> polymorph_core::config::LspServerEntry {
    polymorph_core::config::LspServerEntry {
        id: id.to_string(),
        cmd,
        args: vec![fixture.to_string_lossy().into_owned()],
        languages: languages.iter().map(|s| s.to_string()).collect(),
    }
}

/// Wire matrix (L3): one entry × four fixture languages consults four
/// writes (a diagnostics notice each); an unregistered fifth falls back.
/// Real-server coverage rides host availability (stated gap — no language
/// server is installed here).
#[tokio::test]
async fn wire_matrix_consults_four_and_falls_back_fifth() {
    let Some(python) = resolve_python() else {
        eprintln!(
            "SKIP wire_matrix_consults_four_and_falls_back_fifth: no Python 3 (set PYTHON_BIN)"
        );
        return;
    };
    let (_d, r, ctx) = open_registry();
    let enrolled = polymorph_tools::lsp::wire_lsp_servers(
        &r,
        &[wire_entry(
            "probe",
            python,
            &fixture_path(),
            &["rust", "python", "go", "lua"],
        )],
    )
    .await;
    assert_eq!(enrolled, vec!["rust", "python", "go", "lua"]);
    for (file, language) in [
        ("a.rs", "rust"),
        ("a.py", "python"),
        ("a.go", "go"),
        ("a.lua", "lua"),
    ] {
        r.call(
            "write",
            serde_json::json!({"path": file, "content": "x\n"}),
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(
            r.lsp_take_notice().await,
            Some(format!("lsp-diagnostics:{language}:1")),
            "{file} consulted"
        );
    }
    r.call(
        "write",
        serde_json::json!({"path": "a.ts", "content": "x\n"}),
        &ctx,
    )
    .await
    .unwrap();
    assert_eq!(
        r.lsp_take_notice().await.as_deref(),
        Some("lsp-fallback:typescript")
    );
}

/// Bad entries never poison the table: empty cmd skips with zero
/// enrollments (wire-lenient, MCP precedent).
#[tokio::test]
async fn bad_entry_skips_without_enrollments() {
    let (_d, r, _ctx) = open_registry();
    let enrolled = polymorph_tools::lsp::wire_lsp_servers(
        &r,
        &[wire_entry("bad", String::new(), &fixture_path(), &["rust"])],
    )
    .await;
    assert!(enrolled.is_empty());
}

/// A duplicate language keeps the first registration (warn + skip the
/// second entry's claim — one entry never evicts another's server).
#[tokio::test]
async fn duplicate_language_keeps_first() {
    let Some(python) = resolve_python() else {
        eprintln!("SKIP duplicate_language_keeps_first: no Python 3 (set PYTHON_BIN)");
        return;
    };
    let (_d, r, ctx) = open_registry();
    let enrolled = polymorph_tools::lsp::wire_lsp_servers(
        &r,
        &[
            wire_entry("first", python.clone(), &fixture_path(), &["rust"]),
            wire_entry(
                "second",
                "polymorph-definitely-not-a-binary".to_string(),
                &fixture_path(),
                &["rust"],
            ),
        ],
    )
    .await;
    assert_eq!(enrolled, vec!["rust"]);
    // The FIRST (working) server still answers: a bogus second claim
    // could not displace it.
    r.call(
        "write",
        serde_json::json!({"path": "a.rs", "content": "fn main() {}\n"}),
        &ctx,
    )
    .await
    .unwrap();
    assert_eq!(
        r.lsp_take_notice().await.as_deref(),
        Some("lsp-diagnostics:rust:1")
    );
}

/// Push-only defaults: java and toml are pre-seeded so didOpen never
/// fires textDocument/diagnostic (avoids JDT.LS UnsupportedOperationException
/// eviction and taplo pull-suppression).
#[tokio::test]
async fn default_pool_seeds_push_only_languages() {
    let (_d, r, _ctx) = open_registry();
    assert!(r.lsp_is_push_only("java").await);
    assert!(r.lsp_is_push_only("toml").await);
    assert!(!r.lsp_is_push_only("rust").await);
    assert!(!r.lsp_is_push_only("python").await);
}
