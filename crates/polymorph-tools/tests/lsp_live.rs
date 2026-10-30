//! Live-server L2/L3 proofs: real `[[lsp_servers]]` config → wire → write → enforcement against
//! INSTALLED servers (roster: `docs/lsp-servers.md`). Each test loud-skips unless ITS server
//! resolves (PATH probe, else known absolute dirs) — the fixture suites stay the always-green
//! floor; these prove the live ceiling.
//!
//! Asserts are deliberately loose (notice PREFIX + non-empty messages,
//! never exact counts/text): diagnostic volume varies by server version.
//! A zero-diagnostic live run FAILS honestly (no silent green) —
//! investigated, never weakened blind.

use polymorph_tools::{Gate, Registry, ToolCallContext};
use std::path::Path;

/// Resolve a server binary: bare name on PATH first, then known absolute
/// dirs. `None` = loud skip at the call site.
fn probe_server(cmd: &str) -> Option<String> {
    if std::process::Command::new(cmd)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        return Some(cmd.to_string());
    }
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_default();
    for dir in [
        format!("{home}/go/bin"),
        format!("{home}/AppData/Local/Programs/Python/Python313/Scripts"),
        format!("{home}/.cargo/bin"),
    ] {
        let candidate = Path::new(&dir).join(cmd);
        let with_exe = candidate.with_extension("exe");
        for path in [&candidate, &with_exe] {
            if std::process::Command::new(path)
                .arg("--version")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
            {
                return Some(path.to_string_lossy().into_owned());
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

/// Full config path: temp TOML `[[lsp_servers]]` → loader → wire → broken
/// write → diagnostics notice + recorded messages for `language`.
async fn live_proof(
    server_cmd: &str,
    server_args: &[&str],
    language: &str,
    file: &str,
    content: &str,
    prelude: &[(&str, &str)],
) {
    let (d, r, ctx) = open_registry();
    std::fs::write(
        d.path().join("Cargo.toml"),
        "[package]\nname = \"probe\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(d.path().join("src")).unwrap();
    for (name, text) in prelude {
        let target = d.path().join(name);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(target, text).unwrap();
    }
    let cfg_file = d.path().join("polymorph.toml");
    let args = server_args
        .iter()
        .map(|a| format!("\"{a}\""))
        .collect::<Vec<_>>()
        .join(", ");
    std::fs::write(
        &cfg_file,
        format!(
            "[[lsp_servers]]\nid = \"live\"\ncmd = \"{}\"\nargs = [{}]\nlanguages = [\"{}\"]\n",
            server_cmd.replace('\\', "\\\\"),
            args,
            language
        ),
    )
    .unwrap();
    let missing = Path::new("/nonexistent/config.toml");
    let cfg = polymorph_core::config::load_with_cli(
        missing,
        &cfg_file,
        &polymorph_core::config::CliOverrides::default(),
    )
    .unwrap();
    let enrolled = polymorph_tools::lsp::wire_lsp_servers(&r, &cfg.lsp_servers).await;
    assert_eq!(enrolled, vec![language.to_string()]);
    // Warm-up writes: cold servers (workspace load) report nothing on
    // first contact, and a slow load can outlast one consult's push
    // budget — so re-write (bounded) until a consult reports. Their
    // notices are discarded; the asserted write below is a fresh
    // steady-state consult that must report on its own.
    for _ in 0..3 {
        r.call(
            "write",
            serde_json::json!({"path": file, "content": content}),
            &ctx,
        )
        .await
        .unwrap();
        if r.lsp_take_notice().await.is_some() {
            break;
        }
    }
    r.call(
        "write",
        serde_json::json!({"path": file, "content": content}),
        &ctx,
    )
    .await
    .unwrap();
    let notice = r.lsp_take_notice().await.unwrap_or_else(|| {
        panic!("{language}: live server consulted but zero diagnostics recorded")
    });
    assert!(
        notice.starts_with(&format!("lsp-diagnostics:{language}:")),
        "{language}: unexpected notice {notice:?}"
    );
    let recorded = r.lsp_diagnostics().await;
    let messages: Vec<&String> = recorded
        .iter()
        .filter(|(uri, _)| uri.ends_with(file))
        .flat_map(|(_, texts)| texts)
        .collect();
    assert!(!messages.is_empty(), "{language}: notice but no messages");
}

#[tokio::test]
async fn live_rust_sees_broken_file() {
    let Some(cmd) = probe_server("rust-analyzer") else {
        eprintln!("SKIP live_rust_sees_broken_file: no rust-analyzer (see docs/lsp-servers.md)");
        return;
    };
    // Warm-up writes warm the cold server (workspace load), the last
    // consults steady-state. All go through the full config path.
    live_proof(&cmd, &[], "rust", "src/main.rs", "fn broken( {\n", &[]).await;
}

#[tokio::test]
async fn live_go_sees_broken_file() {
    let Some(cmd) = probe_server("gopls") else {
        eprintln!("SKIP live_go_sees_broken_file: no gopls (see docs/lsp-servers.md)");
        return;
    };
    live_proof(
        &cmd,
        &[],
        "go",
        "broken.go",
        "package broken( {\n",
        &[("go.mod", "module probe\n\ngo 1.27\n")],
    )
    .await;
}

#[tokio::test]
async fn live_python_sees_broken_file() {
    let Some(cmd) = probe_server("basedpyright-langserver") else {
        eprintln!("SKIP live_python_sees_broken_file: no basedpyright (see docs/lsp-servers.md)");
        return;
    };
    live_proof(&cmd, &["--stdio"], "python", "broken.py", "x = \n", &[]).await;
}

#[tokio::test]
async fn live_toml_sees_broken_file() {
    let Some(cmd) = probe_server("taplo") else {
        eprintln!("SKIP live_toml_sees_broken_file: no taplo (see docs/lsp-servers.md)");
        return;
    };
    live_proof(
        &cmd,
        &["lsp", "stdio"],
        "toml",
        "broken.toml",
        "[unclosed\n",
        &[],
    )
    .await;
}
