//! LSP client substrate.
//!
//! D2: stdio + `Content-Length` framing, client-only v1. Zero new deps —
//! tokio (child + io + time) and serde_json are resident; the initialize
//! handshake is minimal hand-rolled JSON (`lsp-types` adoption rides L1b's
//! op surface, when the 9-op vocabulary actually needs canonical types).
//! Enforcement and `[[lsp_servers]]` ride this client; the module proves
//! the transport against a fixture and stops there (stated, not hidden).

use anyhow::Context as _;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

/// Per-call ceiling (spawn + handshake + requests): fail naming the
/// timeout, never hang a loop on a sick server (D2 fail-soft direction).
const CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Push-listen budget after an empty pull: rust-analyzer publishes asynchronously (empty push
/// first, items right after), taplo ONLY pushes. Clean files usually settle on the first matching
/// push + a 500ms grace linger; silent servers cost the budget.
const PUSH_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);
const PUSH_GRACE: std::time::Duration = std::time::Duration::from_millis(500);

/// One recorded diagnostic: message plus the position and severity the wire already carried — the
/// reviewer evidence names `file:line: severity` and the opt-in error gate needs severity. `line`
/// is 1-based (LSP `range.start.line` + 1); `severity` is the raw LSP code (1 error, 2 warning, 3
/// information, 4 hint; absent = the server left it to the client — never assumed to be an error).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspDiagnostic {
    pub line: Option<u32>,
    pub severity: Option<u8>,
    pub message: String,
}

impl LspDiagnostic {
    /// Parse one wire `Diagnostic` object; `None` without a string
    /// `message` (the pre-#3 extraction skipped those items too).
    pub fn from_item(item: &serde_json::Value) -> Option<Self> {
        let message = item.get("message")?.as_str()?.to_string();
        let line = item
            .pointer("/range/start/line")
            .and_then(serde_json::Value::as_u64)
            .and_then(|l| u32::try_from(l).ok())
            .map(|l| l.saturating_add(1));
        let severity = item
            .get("severity")
            .and_then(serde_json::Value::as_u64)
            .and_then(|s| u8::try_from(s).ok());
        Some(Self {
            line,
            severity,
            message,
        })
    }

    /// Message-only entry (the pre-#3 test seeds: no position, no severity).
    pub fn message_only(message: String) -> Self {
        Self {
            line: None,
            severity: None,
            message,
        }
    }

    /// Error severity (LSP code 1) — the only class the gate counts.
    pub fn is_error(&self) -> bool {
        self.severity == Some(1)
    }
}

/// Merge one diagnostic into `found` unless an identical one (message,
/// line, and severity) is already there: pull + push overlap collapses,
/// distinct sites sharing a message stay distinct.
fn push_unique(found: &mut Vec<LspDiagnostic>, d: LspDiagnostic) {
    if !found.contains(&d) {
        found.push(d);
    }
}

/// Encode one LSP frame: `Content-Length` header + body (D2 transport).
pub fn encode_message(body: &serde_json::Value) -> Vec<u8> {
    let text = body.to_string();
    let mut frame = format!("Content-Length: {}\r\n\r\n", text.len()).into_bytes();
    frame.extend_from_slice(text.as_bytes());
    frame
}

/// Read one LSP frame: headers until the blank line, strict
/// `Content-Length`, then exactly that many body bytes as JSON.
/// Anything else bails naming the condition (never a silent stall).
pub async fn read_message<R>(reader: &mut R) -> anyhow::Result<serde_json::Value>
where
    R: AsyncBufReadExt + Unpin,
{
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let n = reader
            .read_line(&mut line)
            .await
            .context("lsp read header")?;
        if n == 0 {
            anyhow::bail!("lsp eof waiting for header");
        }
        let line = line.trim();
        if line.is_empty() {
            break;
        }
        if let Some(rest) = line.strip_prefix("Content-Length:") {
            let len: usize = rest
                .trim()
                .parse()
                .with_context(|| format!("lsp bad Content-Length {rest:?}"))?;
            content_length = Some(len);
        }
    }
    let len = content_length.context("lsp frame without Content-Length")?;
    let mut body = vec![0u8; len];
    reader
        .read_exact(&mut body)
        .await
        .context("lsp read body")?;
    serde_json::from_slice(&body).context("lsp body is not JSON")
}

/// Cancel-safe bounded read for polling loops: waits up to `wait` for a
/// frame to START (`fill_buf` consumes nothing, so an expiring wait loses
/// no bytes), then reads that frame whole under [`CALL_TIMEOUT`]. Timing
/// out [`read_message`] itself is NOT safe: rust-analyzer's line-buffered
/// stdout flushes the header before the body, and a slice expiring
/// between the two dropped the consumed header, desyncing every later
/// read (the likely `live_rust_sees_broken_file` flake). `Ok(None)` = nothing
/// arrived within `wait`; EOF and read errors are `Err`.
pub async fn read_message_within<R>(
    reader: &mut R,
    wait: std::time::Duration,
) -> anyhow::Result<Option<serde_json::Value>>
where
    R: AsyncBufReadExt + Unpin,
{
    match tokio::time::timeout(wait, reader.fill_buf()).await {
        Err(_) => return Ok(None),
        Ok(Err(err)) => return Err(err).context("lsp read header"),
        Ok(Ok([])) => anyhow::bail!("lsp eof waiting for header"),
        Ok(Ok(_)) => {}
    }
    tokio::time::timeout(CALL_TIMEOUT, read_message(reader))
        .await
        .context("lsp read timeout")?
        .map(Some)
}

/// Extension → language map (L2 enforcement routing; L3's `[[lsp_servers]]`
/// table keys these names). Unknown extensions route nowhere (plain writes).
/// Filename routes cover extensionless files (CMakeLists.txt, Dockerfile).
/// Keys are stable: existing 9 never rename; new keys match
/// `docs/lsp-servers.md` roster order (one server per language; alternates
/// like ruff/deno/eslint documented there, not duplicated here).
pub fn language_of(path: &str) -> Option<&'static str> {
    let p = std::path::Path::new(path);
    if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
        let lower = name.to_ascii_lowercase();
        if lower == "cmakelists.txt" || lower == "cmakecache.txt" {
            return Some("cmake");
        }
        if lower == "dockerfile" || lower.starts_with("dockerfile.") {
            return Some("dockerfile");
        }
    }
    let ext = p.extension()?.to_str()?;
    match ext.to_ascii_lowercase().as_str() {
        "rs" => Some("rust"),
        "py" | "pyi" => Some("python"),
        "cs" => Some("csharp"),
        "js" | "mjs" | "cjs" | "jsx" => Some("javascript"),
        "ts" | "mts" | "cts" | "tsx" => Some("typescript"),
        "go" => Some("go"),
        "lua" => Some("lua"),
        "c" | "h" => Some("c"),
        "toml" => Some("toml"),
        "cpp" | "hpp" | "cc" | "hh" | "cxx" | "hxx" => Some("cpp"),
        "zig" => Some("zig"),
        "cmake" => Some("cmake"),
        "tf" | "tfvars" => Some("terraform"),
        "wgsl" => Some("wgsl"),
        "fs" | "fsi" | "fsx" => Some("fsharp"),
        "sh" | "bash" => Some("bash"),
        "yaml" | "yml" => Some("yaml"),
        "css" => Some("css"),
        "html" | "htm" => Some("html"),
        "json" | "jsonc" => Some("json"),
        "md" | "markdown" => Some("markdown"),
        "dockerfile" => Some("dockerfile"),
        "php" => Some("php"),
        "svelte" => Some("svelte"),
        "astro" => Some("astro"),
        "clj" | "cljs" | "cljc" | "bb" | "edn" => Some("clojure"),
        "glsl" | "vert" | "frag" | "geom" | "tesc" | "tese" | "comp" => Some("glsl"),
        "slang" | "slangh" => Some("slang"),
        "kt" | "kts" => Some("kotlin"),
        "java" => Some("java"),
        "xml" | "xsd" | "xsl" | "xslt" => Some("xml"),
        "d" => Some("d"),
        "rkt" | "rktl" => Some("racket"),
        "dart" => Some("dart"),
        "gd" | "gdshader" => Some("gdscript"),
        _ => None,
    }
}

/// Lowercase a leading Windows drive letter (`C:/…` → `c:/…`). rust-analyzer matches workspace/file
/// URLs case-SENSITIVELY and canonicalizes lowercase. The OS treats the letter case-insensitively,
/// so this is safe everywhere.
fn lower_drive(path: &str) -> String {
    let mut chars = path.chars();
    match (chars.next(), chars.next()) {
        (Some(drive), Some(':')) if drive.is_ascii_uppercase() => {
            format!("{}:{}", drive.to_ascii_lowercase(), chars.as_str())
        }
        _ => path.to_string(),
    }
}

/// Pushed-URI equality tolerating server echo-case.
fn uri_eq(a: &str, b: &str) -> bool {
    a == b || a.to_lowercase() == b.to_lowercase()
}

/// File path → `file://` URI for LSP params (backslash-normalized, drive
/// lowercased; the fixture does not care, most real servers accept this).
pub fn uri_for_verbatim(workdir: &std::path::Path, raw: &str) -> String {
    let p = std::path::Path::new(raw);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        workdir.join(p)
    };
    format!(
        "file:///{}",
        abs.to_string_lossy()
            .replace('\\', "/")
            .trim_start_matches('/')
    )
}

/// Verbatim twin of [`uri_for`] (no case folding): taplo-class servers compare didOpen/root URIs
/// case-SENSITIVELY against the OS-canonical (uppercase-drive) form — lowercase input is silently
/// ignored. rust-class servers need the opposite (lowercase drive); the caller picks per
/// `prefer_verbatim_uri`, each entry probe-backed (stated below).
///
/// `true` for languages whose servers are probe-proven to need verbatim (OS-canonical-case) URIs:
/// `toml` (taplo ignores lowercase-drive input entirely). Every other language gets lowercase-drive
/// URIs. Extend ONLY with a passing/failing probe pair per language (stated).
pub fn prefer_verbatim_uri(language: &str) -> bool {
    matches!(language, "toml")
}

/// Verbatim workspace root (twin of [`root_uri_for`], no case folding).
pub fn root_uri_for_verbatim(workdir: &std::path::Path) -> String {
    format!(
        "file:///{}",
        workdir
            .to_string_lossy()
            .replace('\\', "/")
            .trim_start_matches('/')
    )
}

/// Workspace root → `file://` URI (same normalization as [`uri_for`],
/// no trailing dot — initialize roots must be clean directory URIs).
pub fn root_uri_for(workdir: &std::path::Path) -> String {
    format!(
        "file:///{}",
        lower_drive(
            workdir
                .to_string_lossy()
                .replace('\\', "/")
                .trim_start_matches('/')
        )
    )
}

pub fn uri_for(workdir: &std::path::Path, raw: &str) -> String {
    let p = std::path::Path::new(raw);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        workdir.join(p)
    };
    format!(
        "file:///{}",
        lower_drive(
            abs.to_string_lossy()
                .replace('\\', "/")
                .trim_start_matches('/')
        )
    )
}

/// Lazy per-language pool (L2, D2 shape borrowed from `McpPool`): spawn on
/// first write for a registered language, evict on breakage. Enforcement
/// NEVER fails a write (D4) — every fault degrades to a notice.
pub struct LspPool {
    specs: std::collections::BTreeMap<String, LspSpec>,
    clients: std::collections::BTreeMap<String, LspClient>,
    warned_fallback: std::collections::HashSet<String>,
    last_notice: Option<String>,
    /// Languages proven push-only (`textDocument/diagnostic` → `-32601` or
    /// unsupported operation): the pull is skipped thereafter — sending
    /// it can suppress pushes for the session (taplo, jdtls).
    push_only: std::collections::HashSet<String>,
    /// didOpen versions per URI (monotonic per session; re-sends must
    /// advance or servers may ignore them as stale).
    open_versions: std::collections::BTreeMap<String, i32>,
    /// Last pulled diagnostics per URI (L4 gutter feed): replaced on every pull, never blocking;
    /// dropped when a write's consult could not refresh it.
    diagnostics: std::collections::BTreeMap<String, Vec<LspDiagnostic>>,
}

impl Default for LspPool {
    fn default() -> Self {
        let mut push_only = std::collections::HashSet::new();
        // Eclipse JDT.LS (Java) uses push diagnostics only; textDocument/diagnostic
        // throws java.lang.UnsupportedOperationException wrapped in JSON-RPC -32603.
        // Taplo (TOML) is also push-only (pull requests suppress push notifications).
        push_only.insert("java".to_string());
        push_only.insert("toml".to_string());
        Self {
            specs: Default::default(),
            clients: Default::default(),
            warned_fallback: Default::default(),
            last_notice: None,
            push_only,
            open_versions: Default::default(),
            diagnostics: Default::default(),
        }
    }
}

impl std::fmt::Debug for LspPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LspPool")
            .field("specs", &self.specs.len())
            .field("connected", &self.clients.keys().collect::<Vec<_>>())
            .field("warned_fallback", &self.warned_fallback.len())
            .finish()
    }
}

impl LspPool {
    /// Whether a language is configured or learned as push-only diagnostics.
    pub fn is_push_only(&self, language: &str) -> bool {
        self.push_only.contains(language)
    }

    /// Register one language server (duplicate languages refused, not
    /// overwritten — the `register` precedent). `[[lsp_servers]]` maps
    /// onto this at L3; L2 callers register explicitly.
    pub fn add_server(&mut self, language: &str, spec: LspSpec) -> anyhow::Result<()> {
        if self.specs.contains_key(language) {
            anyhow::bail!("duplicate lsp server for language: {language}");
        }
        self.specs.insert(language.to_string(), spec);
        Ok(())
    }

    /// Connected languages (observability).
    pub fn connected(&self) -> Vec<String> {
        self.clients.keys().cloned().collect()
    }

    /// Take the pending notice, if any (tests + future ledger surfaces).
    pub fn take_notice(&mut self) -> Option<String> {
        self.last_notice.take()
    }

    /// Next didOpen version for one URI (starts at 1, advances per send).
    fn next_version(&mut self, uri: &str) -> i32 {
        let entry = self.open_versions.entry(uri.to_string()).or_insert(0);
        *entry += 1;
        *entry
    }

    /// Last recorded diagnostic messages per URI (clone for the gutter
    /// snapshot; message text only — the pre-#3 shape every reader takes).
    pub fn diagnostics(&self) -> std::collections::BTreeMap<String, Vec<String>> {
        self.diagnostics
            .iter()
            .map(|(uri, items)| {
                let messages = items.iter().map(|d| d.message.clone()).collect();
                (uri.clone(), messages)
            })
            .collect()
    }

    /// Last recorded diagnostics per URI with line + severity.
    pub fn diagnostic_details(&self) -> std::collections::BTreeMap<String, Vec<LspDiagnostic>> {
        self.diagnostics.clone()
    }

    /// Test-only seed for the brief feed (no server, no IO).
    pub fn test_seed_diagnostics(&mut self, map: std::collections::BTreeMap<String, Vec<String>>) {
        self.diagnostics = map
            .into_iter()
            .map(|(uri, items)| {
                let items = items.into_iter().map(LspDiagnostic::message_only).collect();
                (uri, items)
            })
            .collect();
    }

    /// Test-only structured seed.
    pub fn test_seed_diagnostic_details(
        &mut self,
        map: std::collections::BTreeMap<String, Vec<LspDiagnostic>>,
    ) {
        self.diagnostics = map;
    }

    /// Warn-once bookkeeping shared with the write-through fallback: true on first call per
    /// language (the caller warns), false once warned.
    pub fn warned_missing(&mut self, language: &str) -> bool {
        self.warned_fallback.insert(language.to_string())
    }

    /// Post-write consult (L2, D4): server running → `didOpen`, then diagnostic pull, findings
    /// recorded (inform, never block); no server → direct write stands + warn ONCE per language;
    /// breakage → evict + notice. The write already succeeded — this cannot fail it. `text` is the
    /// just-written content (best-effort read by the caller; `None` skips the open and pulls bare).
    /// Notify errors are ignored.
    pub async fn post_write(
        &mut self,
        language: &str,
        uri: &str,
        text: Option<&str>,
        root: Option<&str>,
    ) {
        // didOpen version + push-only flag first (borrows end before the
        // client borrow below).
        let version = text.map(|_| self.next_version(uri));
        let skip_pull = self.push_only.contains(language);
        let Some(client) = self.client_for(language, root).await else {
            // No consult happened: a snapshot from an earlier write no longer describes this file.
            self.diagnostics.remove(uri);
            if self.warned_fallback.insert(language.to_string()) {
                tracing::warn!(language, "lsp fallback: no server, direct write stands");
                self.last_notice = Some(format!("lsp-fallback:{language}"));
            }
            return;
        };
        if let Some(text) = text {
            // Best-effort open (ignored on failure — the pull below is
            // the real consult; some servers answer bare pulls anyway).
            let version = version.unwrap_or(1);
            let _ = client
                .notify(
                    "textDocument/didOpen",
                    serde_json::json!({"textDocument": {
                        "uri": uri,
                        "languageId": language,
                        "version": version,
                        "text": text,
                    }}),
                )
                .await;
        }
        // Pull phase. No pull support (taplo et al., `-32601`) is NOT
        // breakage — it means pushes-only (the code match is on the
        // JSON-RPC code in the typed request error, the only signal a
        // bare `anyhow` carries here; documented, not sniffed blind).
        // Skip the pull for proven push-only languages (a first-contact
        // pull can suppress the session's pushes — taplo probe pair).
        // `learned` applies AFTER the last client use below (borrowck).
        let mut learned_push_only = false;
        // -32601 poisons taplo-class sessions (probe-proven: nothing
        // pushes afterwards), so the client is reaped for a fresh
        // pull-free session next write. Applied post-client-use.
        let mut reap_client = false;
        let mut messages: Vec<LspDiagnostic> = if skip_pull {
            Vec::new()
        } else {
            match client.diagnostic(uri).await {
                Err(err)
                    if err.to_string().contains("-32601")
                        || err.to_string().contains("UnsupportedOperationException")
                        || (err.to_string().contains("-32603")
                            && (err.to_string().to_lowercase().contains("unsupported")
                                || err.to_string().to_lowercase().contains("not implemented")
                                || err.to_string().to_lowercase().contains("diagnostic"))) =>
                {
                    learned_push_only = true;
                    reap_client = true;
                    Vec::new()
                }
                Err(err) => {
                    self.clients.remove(language);
                    // Same as the no-server arm: nothing refreshed it.
                    self.diagnostics.remove(uri);
                    tracing::warn!(language, error = %format!("{err:#}"), "lsp server evicted");
                    self.last_notice = Some(format!("lsp-evict:{language}"));
                    return;
                }
                Ok(value) => value
                    .get("items")
                    .and_then(serde_json::Value::as_array)
                    .map(|items| items.iter().filter_map(LspDiagnostic::from_item).collect())
                    .unwrap_or_default(),
            }
        };
        // Push-model servers (taplo et al.) report via `publishDiagnostics` instead of the pull:
        // merge any queued pushes for THIS uri.
        for pushed in client.drain_incoming() {
            let pushed_uri = pushed
                .pointer("/params/uri")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            let is_ours = pushed.get("method").and_then(serde_json::Value::as_str)
                == Some("textDocument/publishDiagnostics")
                && uri_eq(pushed_uri, uri);
            if !is_ours {
                continue;
            }
            if let Some(items) = pushed
                .pointer("/params/diagnostics")
                .and_then(serde_json::Value::as_array)
            {
                for d in items.iter().filter_map(LspDiagnostic::from_item) {
                    push_unique(&mut messages, d);
                }
            }
        }
        // Push-model top-up: only when the pull came back empty — a pull WITH items already
        // answered.
        if messages.is_empty() {
            for d in client.await_publish(uri, PUSH_BUDGET).await {
                push_unique(&mut messages, d);
            }
        }
        if learned_push_only {
            self.push_only.insert(language.to_string());
        }
        if reap_client {
            self.clients.remove(language);
        }
        let n = messages.len();
        self.diagnostics.insert(uri.to_string(), messages);
        if n > 0 {
            tracing::warn!(language, items = n, "lsp diagnostics after write");
            self.last_notice = Some(format!("lsp-diagnostics:{language}:{n}"));
        }
    }

    /// Read-only query for the fixer-allowlist LSP tools: didOpen the caller-supplied text, then
    /// one hover/definition/references request. No server (no spec, spawn/init failure) → `None`
    /// (the caller degrades to a model-readable note, never an error — L fail-soft). Request
    /// failure → evict + notice (the post_write path) + the error (the caller names the op). Never
    /// writes, never gates (the caller gated before any IO).
    #[allow(clippy::too_many_arguments)] // single call site; params map 1:1 onto the three query ops
    pub async fn query(
        &mut self,
        language: &str,
        uri: &str,
        text: &str,
        root: Option<&str>,
        op: &str,
        line: u32,
        ch: u32,
        include_declaration: bool,
    ) -> Option<anyhow::Result<serde_json::Value>> {
        let version = self.next_version(uri);
        let client = self.client_for(language, root).await?;
        let _ = client
            .notify(
                "textDocument/didOpen",
                serde_json::json!({"textDocument": {
                    "uri": uri,
                    "languageId": language,
                    "version": version,
                    "text": text,
                }}),
            )
            .await;
        let out = match op {
            "hover" => client.hover(uri, line, ch).await,
            "definition" => client.definition(uri, line, ch).await,
            "references" => client.references(uri, line, ch, include_declaration).await,
            _ => return Some(Err(anyhow::anyhow!("unknown lsp query op {op:?}"))),
        };
        match out {
            Ok(value) => Some(Ok(value)),
            Err(err) => {
                self.clients.remove(language);
                tracing::warn!(language, op, error = %format!("{err:#}"), "lsp query failed, server evicted");
                self.last_notice = Some(format!("lsp-evict:{language}"));
                Some(Err(err))
            }
        }
    }

    /// Connected AND initialized client, spawning lazily from the spec. Spawn OR initialize failure
    /// = `None` (the caller degrades to fallback — never an error). The initialize was a live gap:
    /// real servers refuse requests before it (the fixture never cared). `root` is the workspace
    /// URI.
    async fn client_for(&mut self, language: &str, root: Option<&str>) -> Option<&mut LspClient> {
        if !self.clients.contains_key(language) {
            let spec = self.specs.get(language)?.clone();
            let mut client = match LspClient::spawn(&spec).await {
                Ok(client) => client,
                Err(err) => {
                    tracing::warn!(language, error = %format!("{err:#}"), "lsp spawn failed");
                    return None;
                }
            };
            if let Err(err) = client.initialize(root).await {
                tracing::warn!(language, error = %format!("{err:#}"), "lsp initialize failed");
                return None;
            }
            self.clients.insert(language.to_string(), client);
        }
        self.clients.get_mut(language)
    }
}

/// Validate one `[[lsp_servers]]` entry (fail-closed at attach, the
/// `[[hooks]]` precedent): non-empty id/cmd plus at least one language.
/// Returns the languages with their shared spec.
pub fn validate_entry(
    entry: &polymorph_core::config::LspServerEntry,
) -> anyhow::Result<(Vec<String>, LspSpec)> {
    if entry.id.trim().is_empty() {
        anyhow::bail!("lsp server entry without id");
    }
    if entry.cmd.trim().is_empty() {
        anyhow::bail!("lsp server '{}' without command", entry.id);
    }
    if entry.languages.is_empty() {
        anyhow::bail!("lsp server '{}' without languages", entry.id);
    }
    Ok((
        entry.languages.clone(),
        LspSpec {
            cmd: entry.cmd.clone(),
            args: entry.args.clone(),
        },
    ))
}

/// Wire configured servers (the `wire_mcp_servers` shape): validate +
/// register per language, fail-soft per entry (warn + continue — one bad
/// entry never kills the run; validation strictness lives in
/// `validate_entry`, wiring leniency here — the layers are deliberate).
/// Returns enrolled languages for observability. The bench deliberately
/// does NOT call this (apparatus stability, the MCP-stated reason).
pub async fn wire_lsp_servers(
    registry: &crate::Registry,
    servers: &[polymorph_core::config::LspServerEntry],
) -> Vec<String> {
    let mut enrolled = Vec::new();
    for entry in servers {
        let (languages, spec) = match validate_entry(entry) {
            Ok(v) => v,
            Err(err) => {
                tracing::warn!(entry = %entry.id, error = %format!("{err:#}"), "lsp server skipped");
                continue;
            }
        };
        for language in &languages {
            if let Err(err) = registry.add_lsp_server(language, spec.clone()).await {
                tracing::warn!(language = %language, error = %format!("{err:#}"), "lsp language skipped");
                continue;
            }
            enrolled.push(language.clone());
        }
    }
    enrolled
}

/// Server descriptor: which child to spawn.
#[derive(Debug, Clone, PartialEq)]
pub struct LspSpec {
    pub cmd: String,
    pub args: Vec<String>,
}

impl LspSpec {
    fn command(&self) -> tokio::process::Command {
        let mut c = tokio::process::Command::new(&self.cmd);
        c.args(&self.args);
        // A dropped client must not strand a server: kill on drop covers
        // hung processes (the `exec.rs` precedent; well-behaved servers
        // like rust-analyzer also exit on pipe close).
        c.kill_on_drop(true);
        // Unix: lead a fresh process group so `exec::kill_tree_sync` (Drop, shutdown) reaps
        // wrapper-launched grandchildren too.
        #[cfg(unix)]
        c.process_group(0);
        c
    }
}

/// One connected language server: piped stdio child + request ids.
/// Client-only v1 (D2): requests, notifications, shutdown — no serving.
#[derive(Debug)]
pub struct LspClient {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    stdout: tokio::io::BufReader<tokio::process::ChildStdout>,
    next_id: u64,
    /// Server-pushed traffic seen while awaiting responses: notifications
    /// and server→client requests, observed but never answered (client-only
    /// v1 posture). The L1b diagnostics + willRenameFiles proofs read it.
    incoming: Vec<serde_json::Value>,
}

impl LspClient {
    /// Spawn the server child (piped stdio; stderr nulled so a chatty
    /// server can never pollute the terminal or the ledger).
    pub async fn spawn(spec: &LspSpec) -> anyhow::Result<Self> {
        let mut child = spec
            .command()
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .with_context(|| format!("lsp spawn {} {:?}", spec.cmd, spec.args))?;
        let stdin = child.stdin.take().context("lsp stdin not piped")?;
        let stdout = child.stdout.take().context("lsp stdout not piped")?;
        Ok(Self {
            child,
            stdin,
            stdout: tokio::io::BufReader::new(stdout),
            next_id: 0,
            incoming: Vec::new(),
        })
    }

    /// One request/response round-trip: writes carry the next id; reads
    /// skip notifications (no `id`) and unrelated responses until ours
    /// arrives. Errors (transport AND JSON-RPC `error`) fail with context.
    pub async fn request(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        self.next_id += 1;
        let id = self.next_id;
        let frame = encode_message(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }));
        let write = async {
            self.stdin
                .write_all(&frame)
                .await
                .context("lsp write request")
        };
        tokio::time::timeout(CALL_TIMEOUT, write)
            .await
            .context("lsp write timeout")??;
        loop {
            let read = read_message(&mut self.stdout);
            let msg = tokio::time::timeout(CALL_TIMEOUT, read)
                .await
                .context("lsp read timeout")??;
            let msg_id = msg.get("id").and_then(serde_json::Value::as_u64);
            if msg_id != Some(id) {
                // Not ours: a notification or a server→client request.
                // Answered (config) or queued for observation — the pipe
                // stays pumped and nothing is lost.
                self.note_or_respond(msg).await;
                continue;
            }
            if let Some(err) = msg.get("error") {
                anyhow::bail!("lsp {method} error: {err}");
            }
            return Ok(msg
                .get("result")
                .cloned()
                .unwrap_or(serde_json::Value::Null));
        }
    }

    /// One notification (no response expected).
    pub async fn notify(&mut self, method: &str, params: serde_json::Value) -> anyhow::Result<()> {
        let frame = encode_message(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }));
        self.stdin
            .write_all(&frame)
            .await
            .context("lsp write notify")
    }

    /// Drain server-pushed traffic observed so far (notifications +
    /// server→client requests, oldest-first). Observation only.
    pub fn drain_incoming(&mut self) -> Vec<serde_json::Value> {
        std::mem::take(&mut self.incoming)
    }

    /// Answer one server→client request where a safe null exists, else queue it for observation.
    /// Currently ONLY `workspace/configuration` is answered (`[null]` per requested item = "no
    /// config", which the spec sanctions): taplo-class servers stall their analysis until it
    /// resolves. Anything else stays queued (answering blindly would lie).
    async fn note_or_respond(&mut self, msg: serde_json::Value) {
        let is_config_request = msg.get("method").and_then(serde_json::Value::as_str)
            == Some("workspace/configuration")
            && msg.get("id").is_some()
            && msg.get("result").is_none()
            && msg.get("error").is_none();
        if !is_config_request {
            self.incoming.push(msg);
            return;
        }
        let id = msg.get("id").cloned().unwrap_or(serde_json::Value::Null);
        let count = msg
            .pointer("/params/items")
            .and_then(serde_json::Value::as_array)
            .map(Vec::len)
            .unwrap_or(1);
        let response = encode_message(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": vec![serde_json::Value::Null; count],
        }));
        let _ = self.stdin.write_all(&response).await;
    }

    /// Listen for `publishDiagnostics` for `uri` up to the push budget:
    /// returns the first non-empty message set, or whatever arrived.
    /// Early-outs: non-empty match, or first match + grace linger (clean
    /// files settle fast; only silent servers cost the full budget).
    /// Non-matching traffic is queued to `incoming` (nothing is lost).
    pub async fn await_publish(
        &mut self,
        uri: &str,
        budget: std::time::Duration,
    ) -> Vec<LspDiagnostic> {
        let start = std::time::Instant::now();
        let mut first_match: Option<std::time::Instant> = None;
        let mut found = Vec::new();
        loop {
            let elapsed = start.elapsed();
            if elapsed >= budget {
                break;
            }
            if !found.is_empty() {
                break;
            }
            if let Some(seen) = first_match {
                if seen.elapsed() >= PUSH_GRACE {
                    break;
                }
            }
            let remaining = budget - elapsed;
            let slice = remaining.min(std::time::Duration::from_millis(500));
            let msg = match read_message_within(&mut self.stdout, slice).await {
                Ok(Some(msg)) => msg,
                Ok(None) => continue,
                // EOF / broken frame: nothing more will parse this consult.
                Err(_) => break,
            };
            let pushed_uri = msg
                .pointer("/params/uri")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            let is_push = msg.get("method").and_then(serde_json::Value::as_str)
                == Some("textDocument/publishDiagnostics")
                && uri_eq(pushed_uri, uri);
            if !is_push {
                self.note_or_respond(msg).await;
                continue;
            }
            if first_match.is_none() {
                first_match = Some(std::time::Instant::now());
            }
            if let Some(items) = msg
                .pointer("/params/diagnostics")
                .and_then(serde_json::Value::as_array)
            {
                for d in items.iter().filter_map(LspDiagnostic::from_item) {
                    push_unique(&mut found, d);
                }
            }
        }
        found
    }

    /// LSP startup sequence: `initialize` (see [`initialize_params`]) then the `initialized`
    /// notification (process + root + empty capabilities v1) then the `initialized` notification.
    /// Returns the server's result (capabilities + serverInfo — the fixture asserts both, a real
    /// server's shape is L1b's business). `None` root sends JSON null.
    pub async fn initialize(
        &mut self,
        root_uri: Option<&str>,
    ) -> anyhow::Result<serde_json::Value> {
        let result = self
            .request("initialize", initialize_params(root_uri))
            .await
            .context("lsp initialize")?;
        self.notify("initialized", serde_json::json!({})).await?;
        Ok(result)
    }

    /// Thin op wrappers (L1b: the v2 9, client→server direction). Params
    /// mirror LSP shapes minimally; results pass through as `Value` —
    /// canonical `lsp-types` mapping is L2's business, when enforcement
    /// needs field-level reads. `request` itself stays public as the raw
    /// pass-through (the 9th op).
    fn doc_pos(uri: &str, line: u32, character: u32) -> serde_json::Value {
        serde_json::json!({
            "textDocument": {"uri": uri},
            "position": {"line": line, "character": character},
        })
    }

    pub async fn hover(
        &mut self,
        uri: &str,
        line: u32,
        ch: u32,
    ) -> anyhow::Result<serde_json::Value> {
        self.request("textDocument/hover", Self::doc_pos(uri, line, ch))
            .await
    }

    pub async fn definition(
        &mut self,
        uri: &str,
        line: u32,
        ch: u32,
    ) -> anyhow::Result<serde_json::Value> {
        self.request("textDocument/definition", Self::doc_pos(uri, line, ch))
            .await
    }

    pub async fn references(
        &mut self,
        uri: &str,
        line: u32,
        ch: u32,
        include_declaration: bool,
    ) -> anyhow::Result<serde_json::Value> {
        let mut params = Self::doc_pos(uri, line, ch);
        params["context"] = serde_json::json!({"includeDeclaration": include_declaration});
        self.request("textDocument/references", params).await
    }

    /// Pull-model diagnostics (a plain request — no subscription state).
    pub async fn diagnostic(&mut self, uri: &str) -> anyhow::Result<serde_json::Value> {
        self.request(
            "textDocument/diagnostic",
            serde_json::json!({"textDocument": {"uri": uri}}),
        )
        .await
    }

    pub async fn rename(
        &mut self,
        uri: &str,
        line: u32,
        ch: u32,
        new_name: &str,
    ) -> anyhow::Result<serde_json::Value> {
        let mut params = Self::doc_pos(uri, line, ch);
        params["newName"] = serde_json::Value::String(new_name.to_string());
        self.request("textDocument/rename", params).await
    }

    pub async fn code_action(
        &mut self,
        uri: &str,
        line: u32,
        ch: u32,
    ) -> anyhow::Result<serde_json::Value> {
        self.request("textDocument/codeAction", Self::doc_pos(uri, line, ch))
            .await
    }

    pub async fn document_symbol(&mut self, uri: &str) -> anyhow::Result<serde_json::Value> {
        self.request(
            "textDocument/documentSymbol",
            serde_json::json!({"textDocument": {"uri": uri}}),
        )
        .await
    }

    /// LSP shutdown sequence: `shutdown` request, `exit` notification,
    /// then reap the child (kill on best-effort — a hung server must not
    /// wedge test teardown OR the loop).
    pub async fn shutdown(mut self) -> anyhow::Result<()> {
        let _ = self.request("shutdown", serde_json::Value::Null).await;
        let _ = self.notify("exit", serde_json::Value::Null).await;
        if let Some(pid) = self.child.id() {
            let _ = tokio::task::spawn_blocking(move || crate::exec::kill_tree_sync(pid)).await;
        }
        let _ = self.child.kill().await;
        Ok(())
    }
}

impl Drop for LspClient {
    /// Kill the server's whole process tree, not just the direct child:
    /// `kill_on_drop` alone leaves wrapper-launched servers (`python jdtls`
    /// starts Java with `subprocess.run`) alive and holding our stdout pipe,
    /// which kept `polymorph review drive` from exiting after its verdict.
    /// Runs before the fields drop, so the wrapper is still there to anchor
    /// the tree.
    fn drop(&mut self) {
        if let Some(pid) = self.child.id() {
            crate::exec::kill_tree_sync(pid);
        }
    }
}

/// `initialize` params: process + root + empty capabilities v1. A root is sent both as `rootUri`
/// and as the single `workspaceFolders` entry: pyright-class servers (basedpyright) build their
/// workspace ONLY from `workspaceFolders` — given just `rootUri`, every file lands in the rootless
/// default workspace and in-project imports report as unresolved/"implicitly relative" ERRORS.
/// `None` keeps both null (detached).
pub fn initialize_params(root_uri: Option<&str>) -> serde_json::Value {
    let folders = root_uri.map(|uri| {
        let name = uri
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .filter(|n| !n.is_empty())
            .unwrap_or("workspace");
        serde_json::json!([{ "uri": uri, "name": name }])
    });
    serde_json::json!({
        "processId": std::process::id(),
        "rootUri": root_uri,
        "workspaceFolders": folders,
        "capabilities": {},
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn initialize_params_send_root_as_workspace_folder() {
        let p = super::initialize_params(Some("file:///c:/work/proj"));
        assert_eq!(p["rootUri"], "file:///c:/work/proj");
        assert_eq!(
            p["workspaceFolders"],
            serde_json::json!([{ "uri": "file:///c:/work/proj", "name": "proj" }])
        );
        let detached = super::initialize_params(None);
        assert!(detached["rootUri"].is_null());
        assert!(detached["workspaceFolders"].is_null());
    }

    use super::*;

    #[test]
    fn frame_round_trips_through_duplex() {
        let body = serde_json::json!({"jsonrpc": "2.0", "id": 7, "method": "x"});
        let frame = encode_message(&body);
        let text = String::from_utf8(frame).unwrap();
        assert!(text.starts_with("Content-Length: "), "{text}");
        let (head, tail) = text.split_once("\r\n\r\n").unwrap();
        let len: usize = head["Content-Length: ".len()..].parse().unwrap();
        assert_eq!(len, tail.len());
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(tail).unwrap(),
            body
        );
    }

    #[tokio::test]
    async fn read_skips_extra_headers_and_parses_body() {
        // Extra headers (e.g. Content-Type) are tolerated; the blank line
        // ends the section; the body is exactly Content-Length bytes even
        // when the pipe splits mid-frame (duplex write in two halves).
        let body = serde_json::json!({"ok": true});
        let text = body.to_string();
        let (mut w, r) = tokio::io::duplex(1024);
        let head = format!(
            "Content-Type: application/vscode-jsonrpc; charset=utf-8\r\nContent-Length: {}\r\n\r\n",
            text.len()
        );
        w.write_all(head.as_bytes()).await.unwrap();
        w.write_all(&text.as_bytes()[..2]).await.unwrap();
        w.write_all(&text.as_bytes()[2..]).await.unwrap();
        drop(w);
        let mut r = tokio::io::BufReader::new(r);
        assert_eq!(read_message(&mut r).await.unwrap(), body);
    }

    #[tokio::test]
    async fn missing_length_is_a_named_error_not_a_stall() {
        let (mut w, r) = tokio::io::duplex(1024);
        w.write_all(b"Content-Type: application/json\r\n\r\n{}")
            .await
            .unwrap();
        drop(w);
        let mut r = tokio::io::BufReader::new(r);
        let err = read_message(&mut r).await.unwrap_err();
        assert!(err.to_string().contains("without Content-Length"), "{err}");
    }

    #[tokio::test]
    async fn polling_read_survives_a_wait_expiring_mid_frame() {
        // The header lands, the body lags past several polling waits
        // (rust-analyzer flushes them separately): no byte may be lost,
        // and the frame after it must still parse (no desync).
        let first = serde_json::json!({"method": "a"});
        let second = serde_json::json!({"method": "b"});
        let (mut w, r) = tokio::io::duplex(1024);
        let writer = tokio::spawn(async move {
            let frame = encode_message(&first);
            let split = frame.windows(4).position(|x| x == b"\r\n\r\n").unwrap() + 4;
            w.write_all(&frame[..split]).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(120)).await;
            w.write_all(&frame[split..]).await.unwrap();
            w.write_all(&encode_message(&second)).await.unwrap();
            w
        });
        let mut r = tokio::io::BufReader::new(r);
        let mut got = Vec::new();
        while got.len() < 2 {
            let wait = std::time::Duration::from_millis(20);
            if let Some(msg) = read_message_within(&mut r, wait).await.unwrap() {
                got.push(msg["method"].clone());
            }
        }
        assert_eq!(got, vec!["a", "b"]);
        drop(writer.await.unwrap());
        let err = read_message_within(&mut r, std::time::Duration::from_millis(20))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("eof"), "{err}");
    }

    #[tokio::test]
    async fn eof_is_a_named_error() {
        let (w, r) = tokio::io::duplex(1024);
        drop(w);
        let mut r = tokio::io::BufReader::new(r);
        let err = read_message(&mut r).await.unwrap_err();
        assert!(err.to_string().contains("eof"), "{err}");
    }

    #[test]
    fn diagnostic_item_parses_line_and_severity() {
        // 0-based wire line -> 1-based; raw severity code kept; absent fields stay absent (never
        // guessed); no message -> skipped.
        let full = serde_json::json!({
            "message": "mismatched types",
            "severity": 1,
            "range": {"start": {"line": 11, "character": 4}, "end": {"line": 11, "character": 9}},
        });
        assert_eq!(
            LspDiagnostic::from_item(&full),
            Some(LspDiagnostic {
                line: Some(12),
                severity: Some(1),
                message: "mismatched types".into(),
            })
        );
        assert!(LspDiagnostic::from_item(&full).unwrap().is_error());
        let bare = LspDiagnostic::from_item(&serde_json::json!({"message": "m"})).unwrap();
        assert_eq!(bare, LspDiagnostic::message_only("m".into()));
        assert!(!bare.is_error());
        assert_eq!(
            LspDiagnostic::from_item(&serde_json::json!({"severity": 1})),
            None
        );
    }

    #[test]
    fn identical_diagnostics_merge_distinct_sites_stay() {
        let at = |line| LspDiagnostic {
            line: Some(line),
            severity: Some(1),
            message: "cannot find symbol".into(),
        };
        let mut found = Vec::new();
        push_unique(&mut found, at(3));
        push_unique(&mut found, at(3));
        push_unique(&mut found, at(9));
        assert_eq!(found, vec![at(3), at(9)]);
    }

    #[tokio::test]
    async fn unconsulted_write_drops_the_stale_snapshot() {
        // A write whose consult could not run (no server here) leaves NO snapshot for the URI,
        // never the previous write's.
        let mut pool = LspPool::default();
        let mut seed = std::collections::BTreeMap::new();
        seed.insert(
            "file:///w/a.rs".to_string(),
            vec![LspDiagnostic::message_only("old".into())],
        );
        seed.insert(
            "file:///w/b.rs".to_string(),
            vec![LspDiagnostic::message_only("kept".into())],
        );
        pool.test_seed_diagnostic_details(seed);
        pool.post_write("rust", "file:///w/a.rs", Some("fn main() {}\n"), None)
            .await;
        let left = pool.diagnostic_details();
        assert!(!left.contains_key("file:///w/a.rs"), "{left:?}");
        assert_eq!(
            pool.diagnostics()["file:///w/b.rs"],
            vec!["kept".to_string()]
        );
    }

    #[test]
    fn drive_letter_lowercases_for_server_matching() {
        // Decisive probe: rust-analyzer matches file URLs case-sensitively (uppercase drive = file
        // never attaches).
        assert_eq!(super::lower_drive("C:/x/y"), "c:/x/y");
        assert_eq!(super::lower_drive("c:/x"), "c:/x");
        assert_eq!(super::lower_drive("/unix/y"), "/unix/y");
        assert_eq!(super::lower_drive("relative"), "relative");
        assert!(super::uri_for(std::path::Path::new("/w"), "a.rs").starts_with("file://"));
    }

    #[test]
    fn language_of_routes_full_roster() {
        // Installed-servers routing: every roster language resolves;
        // unknown stays unknown (a.zzz/a.xyz precedent in lsp_enforce).
        for (path, want) in [
            ("a.rs", "rust"),
            ("a.py", "python"),
            ("a.cs", "csharp"),
            ("a.js", "javascript"),
            ("a.jsx", "javascript"),
            ("a.ts", "typescript"),
            ("a.tsx", "typescript"),
            ("a.go", "go"),
            ("a.lua", "lua"),
            ("a.c", "c"),
            ("a.h", "c"),
            ("a.toml", "toml"),
            ("a.cpp", "cpp"),
            ("a.hpp", "cpp"),
            ("a.zig", "zig"),
            ("CMakeLists.txt", "cmake"),
            ("a.cmake", "cmake"),
            ("a.tf", "terraform"),
            ("a.wgsl", "wgsl"),
            ("a.fs", "fsharp"),
            ("a.sh", "bash"),
            ("a.yaml", "yaml"),
            ("a.yml", "yaml"),
            ("a.css", "css"),
            ("a.html", "html"),
            ("a.json", "json"),
            ("a.md", "markdown"),
            ("Dockerfile", "dockerfile"),
            ("a.php", "php"),
            ("a.svelte", "svelte"),
            ("a.astro", "astro"),
            ("a.clj", "clojure"),
            ("a.glsl", "glsl"),
            ("a.vert", "glsl"),
            ("a.slang", "slang"),
            ("a.kt", "kotlin"),
            ("a.java", "java"),
            ("a.xml", "xml"),
            ("a.d", "d"),
            ("a.rkt", "racket"),
            ("a.dart", "dart"),
            ("a.gd", "gdscript"),
        ] {
            assert_eq!(super::language_of(path), Some(want), "{path}");
        }
        assert_eq!(super::language_of("a.zzz"), None);
        assert_eq!(super::language_of("a.xyz"), None);
    }
}
