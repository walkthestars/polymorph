//! `tools::renderdoc` — RenderDoc capture facade.
//!
//! Graphics debuggers are NOT protocol-shaped (capture-replay, not
//! request/response): there is no session client here, only a `renderdoccmd` CLI facade plus thin headless
//! helpers. Binaries live host-side, never vendored (the lsp-servers.md precedent; roster:
//! `docs/renderdoc.md`).
//!
//! Capture needs a PRESENTING sample (headless/offscreen apps capture nothing — upstream gap, not
//! our deficit) and OpenGL ≥3.2 core. Never gate on `IsFrameCapturing` right after a trigger (reads
//! 0 pre-present); `.rdc` existence is the proof.

use anyhow::Context as _;
use std::path::Path;

/// Ceiling for one headed capture. The fixture renders 120 frames in seconds on this host; headroom
/// is for slow GPUs — stated, not measured.
pub const CAPTURE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Which `renderdoccmd` to spawn. Single binary (the `[renderdoc]`
/// table) — exactly one exists, so no per-language pool like LSP/DAP.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderdocSpec {
    pub cmd: String,
}

impl RenderdocSpec {
    fn command(&self) -> tokio::process::Command {
        let mut c = tokio::process::Command::new(&self.cmd);
        // A dropped capture must not strand a headed app: kill on drop
        // covers the timeout path (the `exec.rs` / DAP precedent).
        c.kill_on_drop(true);
        c
    }
}

/// Validate one `[renderdoc]` table (fail-closed at attach, the
/// `[[hooks]]` precedent): non-empty `cmd` only. No `env` spelling —
/// none needed yet (the debugpy-PYTHONPATH lesson: env stays API-level
/// until a proven consumer).
pub fn validate(cmd: &str) -> anyhow::Result<RenderdocSpec> {
    let cmd = cmd.trim();
    if cmd.is_empty() {
        anyhow::bail!("renderdoc config is missing `cmd`");
    }
    Ok(RenderdocSpec {
        cmd: cmd.to_string(),
    })
}

/// `renderdoccmd version` first line (e.g. `renderdoccmd x64 v1.46 …`).
pub async fn version(spec: &RenderdocSpec) -> anyhow::Result<String> {
    let out = spec
        .command()
        .arg("version")
        .output()
        .await
        .with_context(|| format!("renderdoc version spawn {}", spec.cmd))?;
    if !out.status.success() {
        anyhow::bail!("renderdoc version exited {}", out.status);
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string())
}

/// A completed headed capture: newest `.rdc` under the template stem.
#[derive(Debug, Clone, PartialEq)]
pub struct CaptureReport {
    pub capture: std::path::PathBuf,
    pub bytes: u64,
    pub captures: usize,
}

/// Drive one capture: `renderdoccmd capture --capture-file <template>
/// --wait-for-exit [--working-dir <dir>] <exe>`, then pick the newest
/// `<stem>*.rdc` (RenderDoc appends `_frame<N>.rdc` to the template —
/// never gate on trigger-time state; the file is the proof).
pub async fn capture(
    spec: &RenderdocSpec,
    exe: &std::path::Path,
    template: &std::path::Path,
    workdir: &std::path::Path,
    timeout: std::time::Duration,
) -> anyhow::Result<CaptureReport> {
    if let Some(parent) = template.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("renderdoc capture dir {}", parent.display()))?;
        }
    }
    let mut cmd = spec.command();
    cmd.arg("capture")
        .arg("--capture-file")
        .arg(template)
        .arg("--wait-for-exit")
        .arg("--working-dir")
        .arg(workdir)
        .arg(exe);
    cmd.current_dir(workdir);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let mut child = cmd
        .spawn()
        .with_context(|| format!("renderdoc capture spawn {} {:?}", spec.cmd, exe))?;
    let status = tokio::time::timeout(timeout, child.wait())
        .await
        .with_context(|| format!("renderdoc capture timed out after {timeout:?}"))?
        .with_context(|| "renderdoc capture wait failed")?;
    if !status.success() {
        anyhow::bail!("renderdoc capture exited {status}");
    }
    let (path, count) = newest_rdc(template)?;
    let bytes = std::fs::metadata(&path)
        .with_context(|| format!("renderdoc capture stat {}", path.display()))?
        .len();
    Ok(CaptureReport {
        capture: path,
        bytes,
        captures: count,
    })
}

/// Newest `<template-stem>*.rdc` sibling + match count. Errors when
/// nothing matches (a silent zero-capture run must never read green).
fn newest_rdc(template: &std::path::Path) -> anyhow::Result<(std::path::PathBuf, usize)> {
    let parent = template.parent().filter(|p| !p.as_os_str().is_empty());
    let stem = template
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    // Template carries no extension by contract (`_frame<N>.rdc` is
    // appended); a caller-passed extension would never prefix-match —
    // refuse loudly instead of globbing the wrong stem.
    if template.extension().is_some() {
        anyhow::bail!("renderdoc capture template {template:?} must carry no extension");
    }
    let dir = parent.map_or_else(|| std::path::PathBuf::from("."), Path::to_path_buf);
    let mut hits: Vec<(std::time::SystemTime, std::path::PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(&dir)
        .with_context(|| format!("renderdoc capture scan {}", dir.display()))?
    {
        let entry = entry.with_context(|| "renderdoc capture dir entry failed")?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(stem) || !name.ends_with(".rdc") {
            continue;
        }
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        hits.push((mtime, entry.path()));
    }
    let count = hits.len();
    hits.into_iter()
        .max_by_key(|(t, _)| *t)
        .map(|(_, p)| (p, count))
        .ok_or_else(|| anyhow::anyhow!("renderdoc capture produced no .rdc under {template:?}"))
}

/// Headless replay proof: `thumb --out <png> <rdc>` replays without a viewer; exit 0 + output
/// exists. Facade-only in N1a.
pub async fn thumb(
    spec: &RenderdocSpec,
    rdc: &std::path::Path,
    out: &std::path::Path,
) -> anyhow::Result<()> {
    let mut cmd = spec.command();
    cmd.arg("thumb").arg("--out").arg(out).arg(rdc);
    cmd.stdin(std::process::Stdio::null());
    let status = tokio::time::timeout(CAPTURE_TIMEOUT, cmd.status())
        .await
        .with_context(|| format!("renderdoc thumb timed out on {}", rdc.display()))?
        .with_context(|| "renderdoc thumb spawn failed")?;
    if !status.success() {
        anyhow::bail!("renderdoc thumb exited {status}");
    }
    if !out.is_file() {
        anyhow::bail!("renderdoc thumb wrote no {}", out.display());
    }
    Ok(())
}

/// Shipped query driver: embedded at compile time so installed binaries never resolve script paths
/// at runtime.
const RDOC_QUERY_PY: &str = include_str!("renderdoc_query.py");

/// Ceiling for one headed query (open + replay + walk on a tiny capture
/// takes seconds here; headroom for slow GPUs — stated, not measured).
pub const SCRIPT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Cap for echoed-back draw lists.
pub const MAX_DRAW_EVENTS: usize = 2000;

/// Cap for user-script output echo (read-page precedent, stated).
pub const MAX_SCRIPT_ECHO: usize = 32 * 1024;

/// Resolve `qrenderdoc` from the enrolled `renderdoccmd`: sibling binary
/// when `cmd` carries a parent dir, else `qrenderdoc` on PATH. Pure for
/// unit tests (no probing — absence fails at spawn with the path named).
pub fn qrenderdoc_for_spec(spec: &RenderdocSpec) -> String {
    let cmd_path = Path::new(&spec.cmd);
    match cmd_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        Some(dir) => dir.join("qrenderdoc.exe").to_string_lossy().into_owned(),
        None => "qrenderdoc".to_string(),
    }
}

/// One headed query run: write `script` to a pid-scoped temp file
/// (deleted after — installed-binary-safe), run `qrenderdoc --script`
/// with `env` (+ RDOC_OUT pointing at a temp result file), parse the
/// result file. The GUI exit code is unreliable — the FILE contract
/// governs (`ok:true` wins even on odd exits; missing/unparseable
/// files bail naming the exit).
pub async fn run_script(
    spec: &RenderdocSpec,
    script: &str,
    env: &[(String, String)],
) -> anyhow::Result<serde_json::Value> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "polymorph-rdoc-{}",
        std::process::id() + seq as u32
    ));
    std::fs::create_dir_all(&dir).with_context(|| "renderdoc script scratch failed")?;
    struct RemoveOnDrop(std::path::PathBuf);
    impl Drop for RemoveOnDrop {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _guard = RemoveOnDrop(dir.clone());
    let script_path = dir.join("query.py");
    let out_path = dir.join("out.json");
    std::fs::write(&script_path, script).with_context(|| "renderdoc script write failed")?;
    let mut cmd = tokio::process::Command::new(qrenderdoc_for_spec(spec));
    cmd.arg("--script").arg(&script_path);
    cmd.envs(env.iter().map(|(k, v)| (k, v)));
    cmd.env("RDOC_OUT", &out_path);
    cmd.stdin(std::process::Stdio::null());
    cmd.kill_on_drop(true);
    let status = tokio::time::timeout(SCRIPT_TIMEOUT, cmd.status())
        .await
        .with_context(|| "renderdoc script timed out (script must quit the UI)")?
        .with_context(|| "renderdoc script spawn failed")?;
    let text = std::fs::read_to_string(&out_path)
        .with_context(|| format!("renderdoc script wrote no result (exit {status})"))?;
    let value: serde_json::Value =
        serde_json::from_str(&text).with_context(|| "renderdoc script result is not JSON")?;
    if value.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        anyhow::bail!(
            "renderdoc script reported: {}",
            value
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("(no error)")
        );
    }
    Ok(value)
}

/// Run the shipped driver in `mode` over `capture` (+ optional extras).
/// Thin envelope over [`run_script`] — one open/replay path for all
/// first-class tools (a mode per tool, not a script per tool).
pub async fn run_query(
    spec: &RenderdocSpec,
    mode: &str,
    capture: &std::path::Path,
    extra: &[(String, String)],
) -> anyhow::Result<serde_json::Value> {
    let mut env: Vec<(String, String)> = vec![
        ("RDOC_MODE".to_string(), mode.to_string()),
        (
            "RDOC_FILE".to_string(),
            capture.to_string_lossy().into_owned(),
        ),
    ];
    env.extend(extra.iter().cloned());
    run_script(spec, RDOC_QUERY_PY, &env).await
}

/// Shape a draws result for the model (pure: cap + summarize for unit
/// tests with hand JSON).
pub fn shape_draws(value: &serde_json::Value) -> serde_json::Value {
    let events = value
        .get("events")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let truncated = events.len() > MAX_DRAW_EVENTS;
    let kept: Vec<serde_json::Value> = events.into_iter().take(MAX_DRAW_EVENTS).collect();
    let draws = kept
        .iter()
        .filter(|e| e.get("draw").and_then(|v| v.as_bool()) == Some(true))
        .count();
    serde_json::json!({
        "count": kept.len(),
        "draws": draws,
        "truncated": truncated,
        "events": kept,
    })
}

/// Wire one `[renderdoc]` table (the `wire_dap_adapters` shape):
/// validate + enroll, fail-soft per entry (warn — one bad path never
/// kills the run). Returns enrolled or not.
pub async fn wire_renderdoc(registry: &crate::Registry, cmd: &str) -> bool {
    match validate(cmd) {
        Ok(spec) => {
            registry.set_renderdoc_spec(spec).await;
            true
        }
        Err(err) => {
            tracing::warn!(error = %format!("{err:#}"), "renderdoc skipped");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_refuses_empty_cmd() {
        assert!(validate("").is_err());
        assert!(validate("   ").is_err());
        assert_eq!(
            validate("renderdoccmd").unwrap(),
            RenderdocSpec {
                cmd: "renderdoccmd".to_string()
            }
        );
    }

    #[test]
    fn newest_rdc_picks_single_match_and_ignores_decoys() {
        let dir = tempfile::tempdir().unwrap();
        let tmpl = dir.path().join("probe");
        std::fs::write(dir.path().join("probe_frame6.rdc"), b"rdc").unwrap();
        std::fs::write(dir.path().join("probe.png"), b"png").unwrap();
        std::fs::write(dir.path().join("other_frame6.rdc"), b"other").unwrap();
        let (path, count) = newest_rdc(&tmpl).unwrap();
        assert_eq!(path, dir.path().join("probe_frame6.rdc"));
        assert_eq!(count, 1);
    }

    #[test]
    fn newest_rdc_empty_dir_is_red_not_green() {
        let dir = tempfile::tempdir().unwrap();
        assert!(newest_rdc(&dir.path().join("probe")).is_err());
    }

    #[test]
    fn newest_rdc_template_extension_refused() {
        let dir = tempfile::tempdir().unwrap();
        assert!(newest_rdc(&dir.path().join("probe.rdc")).is_err());
    }

    #[test]
    fn qrenderdoc_resolves_as_sibling_or_path() {
        // Compare as paths: join renders OS-native separators (the
        // contract is sibling resolution, not display format).
        let abs = RenderdocSpec {
            cmd: "C:/Program Files/RenderDoc/renderdoccmd.exe".to_string(),
        };
        assert_eq!(
            std::path::PathBuf::from(qrenderdoc_for_spec(&abs)),
            std::path::Path::new("C:/Program Files/RenderDoc").join("qrenderdoc.exe")
        );
        let bare = RenderdocSpec {
            cmd: "renderdoccmd".to_string(),
        };
        assert_eq!(qrenderdoc_for_spec(&bare), "qrenderdoc");
    }

    #[test]
    fn embedded_driver_is_loaded() {
        // Guards against empty-file drift: the contract keywords must
        // survive embedding (the script itself is live-proven in
        // tests/rdoc_introspect.rs).
        for token in ["RDOC_MODE", "RDOC_OUT", "GetRootActions", "SetFrameEvent"] {
            assert!(RDOC_QUERY_PY.contains(token), "missing {token}");
        }
    }

    #[test]
    fn shape_draws_counts_filters_and_truncates() {
        let ev =
            |event: u64, draw: bool| serde_json::json!({"event": event, "draw": draw, "flags": 2});
        let v = serde_json::json!({"events": [ev(3, false), ev(5, true), ev(7, true)]});
        let shaped = shape_draws(&v);
        assert_eq!(shaped["count"], 3);
        assert_eq!(shaped["draws"], 2);
        assert_eq!(shaped["truncated"], false);
        assert_eq!(shaped["events"].as_array().map(Vec::len), Some(3));
        // Missing events key degrades to zeros (never a shape error).
        let empty = shape_draws(&serde_json::json!({"ok": true}));
        assert_eq!(empty["count"], 0);
        assert_eq!(empty["draws"], 0);
        // Truncation bound holds on large lists.
        let big: Vec<serde_json::Value> = (0..MAX_DRAW_EVENTS as u64 + 5)
            .map(|i| ev(i, true))
            .collect();
        let shaped = shape_draws(&serde_json::json!({"events": big}));
        assert_eq!(shaped["truncated"], true);
        assert_eq!(
            shaped["events"].as_array().map(Vec::len),
            Some(MAX_DRAW_EVENTS)
        );
    }
}
