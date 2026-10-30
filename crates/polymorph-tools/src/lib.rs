//! `polymorph-tools` — tool implementations + self-registering registry.
//!
//! CONVENTION (Epic A, per execution plan §4): tools self-register in
//! `polymorph-tools/src/registry.rs`; nothing registers from `core`.
//! Chunk 3: exec/read/write/grep/glob land here; edit (hashline) is B4.

pub mod dap;
pub mod eval;
pub mod eval_str;
pub mod evolve;
pub mod exec;
pub mod fs;
pub mod gate;
pub mod hashline;
pub mod hooks;
pub mod lsp;
pub mod mcp;
pub mod output;
pub mod registry;
pub mod renderdoc;
pub mod script_hooks;
pub mod split;
pub mod web;

pub use eval::{
    extract_patch, gate, load_recording, load_recording_with_format, render_eval_prompt,
    score_case, CaseOutcome, CaseReport, EvalCase, EvalFormat, ExtractError, Scoreboard,
};
pub use eval_str::{
    apply_strreplace_to_disk, extract_strreplace, render_str_prompt, score_str_case, StrApplyError,
    StrExtractError, StrSection,
};
pub use exec::{ExecError, ExecResult};
pub use fs::{FsError, GrepHit, ReadPage};
pub use gate::{Denied, Gate};
pub use hooks::{
    AskRegistry, HookDecision, HookEvent, HookRegistry, PickOption, PickOutcome, PickRequest,
    PromptRequest, Prompter, MAX_ASK_OPTIONS,
};
pub use script_hooks::{ScriptHook, ScriptOn};

/// Typed tool-call fault (bench forensics): denials stay distinguishable
/// from execution failures WITHOUT string-sniffing — match the variant,
/// never message text. Display forwards inner text verbatim, so every
/// existing message assertion keeps passing unchanged.
#[derive(Debug)]
pub enum CallError {
    Denied(Denied),
    Failed(anyhow::Error),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::Denied(d) => write!(f, "{d}"),
            CallError::Failed(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for CallError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CallError::Denied(d) => Some(d),
            CallError::Failed(e) => Some(e.as_ref()),
        }
    }
}

impl From<anyhow::Error> for CallError {
    fn from(e: anyhow::Error) -> Self {
        CallError::Failed(e)
    }
}

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context as _;

/// Tool execution context (placeholder; Epic B fills in session/permissions).
#[derive(Debug, Clone, Default)]
pub struct ToolContext {
    pub session_id: String,
}

/// A registered tool descriptor. Input schemas live in
/// [`Registry::schema`], execution in [`Registry::call`] (Epic F3a) — the
/// descriptor itself stays name + description by convention.
#[derive(Clone)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
}

impl std::fmt::Debug for ToolDef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolDef")
            .field("name", &self.name)
            .field("description", &self.description)
            .finish()
    }
}

/// Execution context for one tool call (Epic F3a): the child workdir that
/// relative paths resolve against, the permission gate, and the
/// cancellation token (observed by `bash` only — fs/hashline calls are fast
/// and sync). All fields clone; build one per call site.
#[derive(Debug, Clone)]
pub struct ToolCallContext {
    pub workdir: PathBuf,
    pub gate: Gate,
    pub cancel: tokio_util::sync::CancellationToken,
}

impl ToolCallContext {
    pub fn new(workdir: PathBuf, gate: Gate, cancel: tokio_util::sync::CancellationToken) -> Self {
        Self {
            workdir,
            gate,
            cancel,
        }
    }
}

/// Post-hook detail for ok outcomes: sorted top-level keys (shape only —
/// values NEVER enter hook events, same invariant as the tracing layers).
fn summarized(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&str> = map.keys().map(String::as_str).collect();
            keys.sort_unstable();
            format!("keys:{}", keys.join(","))
        }
        serde_json::Value::Array(items) => format!("array:{}", items.len()),
        serde_json::Value::String(_) => "string".to_string(),
        serde_json::Value::Number(_) => "number".to_string(),
        serde_json::Value::Bool(_) => "bool".to_string(),
        serde_json::Value::Null => "null".to_string(),
    }
}

/// Registry: name → definition. Populated by `inventory`-style explicit
/// registration calls (no magic re-export from core). Owns the hashline
/// snapshot store (Epic F3a): tags mint and verify against ONE store per
/// registry, so an `edit` sees tags minted through the same registry.
#[derive(Debug, Default)]
pub struct Registry {
    tools: BTreeMap<String, ToolDef>,
    snapshots: tokio::sync::Mutex<hashline::SnapshotStore>,
    mcp: tokio::sync::Mutex<mcp::McpPool>,
    lsp: tokio::sync::Mutex<lsp::LspPool>,
    dap: tokio::sync::Mutex<dap::DapPool>,
    dap_sessions: tokio::sync::Mutex<dap::DapSessions>,
    renderdoc: tokio::sync::Mutex<Option<renderdoc::RenderdocSpec>>,
    web: tokio::sync::Mutex<Option<web::WebSpec>>,
    mcp_schemas: BTreeMap<String, serde_json::Value>,
    hooks: HookRegistry,
    scripts: Vec<ScriptHook>,
    ask: AskRegistry,
    /// Files `grep`/`glob` never surface (docket #8: a drive's own
    /// `--events-json` path), on top of the always-on harness-state rule.
    scan_hidden: Vec<PathBuf>,
    /// `bash` output bounding (docket #9): streams over the threshold
    /// spill to `.polymorph/tool-output/` and reach the model as digests.
    output_spill: output::SpillConfig,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register one tool. Duplicate names are rejected (not overwritten).
    pub fn register(&mut self, def: ToolDef) -> anyhow::Result<()> {
        if self.tools.contains_key(&def.name) {
            anyhow::bail!("duplicate tool registration: {}", def.name);
        }
        self.tools.insert(def.name.clone(), def);
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&ToolDef> {
        self.tools.get(name)
    }

    /// Register one MCP server spec (connect stays lazy on first call).
    pub fn add_mcp_server(&mut self, spec: mcp::McpServerSpec) -> anyhow::Result<()> {
        self.mcp.get_mut().add_server(spec)
    }

    /// Test + cassette seam: inject an already-connected MCP client.
    pub async fn inject_mcp_client(&self, id: &str, client: mcp::McpClient) {
        self.mcp.lock().await.insert_client(id, client);
    }

    /// Connected MCP server ids (observability).
    pub async fn mcp_connected(&self) -> Vec<String> {
        self.mcp.lock().await.connected()
    }

    /// Schema for one tool: enrolled server schemas first, then the V1
    /// static table (chunk 54 — the model loop reads through here).
    pub fn schema_of(&self, name: &str) -> Option<serde_json::Value> {
        self.mcp_schemas
            .get(name)
            .cloned()
            .or_else(|| Self::schema(name))
    }

    pub fn names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Mint a hashline tag for content shown to the model (Epic F3b read→
    /// tag flow): the caller passes the FULL current normalized text (tags
    /// bind content; callers holding paginated views re-read the file).
    /// Thin lock+mint — the store stays encapsulated across crates.
    pub async fn mint_tag(&self, path: &Path, normalized_content: &str) -> String {
        self.snapshots.lock().await.mint(path, normalized_content)
    }

    /// JSON-Schema (draft-2020-12 subset) for one tool's args, handed to
    /// the model verbatim by F3b's loop. Hand-written per tool; the gate
    /// test below pins every tool's required keys against drift.
    pub fn schema(name: &str) -> Option<serde_json::Value> {
        use serde_json::json;
        let s = match name {
            "read" => json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path (workdir-relative or absolute) or web:// URL (extracted as markdown; offset/limit are files-only)"},
                    "offset": {"type": "integer", "description": "1-based first line (default 1)"},
                    "limit": {"type": "integer", "description": "Max lines, 0 = to end (default 0)"},
                },
                "required": ["path"],
            }),
            "write" => json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path, workdir-relative or absolute; parents created"},
                    "content": {"type": "string", "description": "Full file content"},
                },
                "required": ["path", "content"],
            }),
            "edit" => json!({
                "type": "object",
                "properties": {
                    "patch": {"type": "string", "description": "Hashline patch text ([PATH#TAG] headers with PUT/CUT/REM/MV hunks)"},
                },
                "required": ["patch"],
            }),
            "grep" => json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Regex pattern"},
                    "dir": {"type": "string", "description": "Search dir (default: workdir)"},
                    "max_hits": {"type": "integer", "description": "Hit cap (default 50)"},
                },
                "required": ["pattern"],
            }),
            "glob" => json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Glob pattern, e.g. **/*.rs"},
                    "dir": {"type": "string", "description": "Search dir (default: workdir)"},
                },
                "required": ["pattern"],
            }),
            "bash" => json!({
                "type": "object",
                "properties": {
                    "cmd": {"type": "string", "description": "Shell command (non-interactive)"},
                    "timeout_secs": {"type": "integer", "description": "Kill after N seconds (default 120, max 600)"},
                },
                "required": ["cmd"],
            }),
            "lsp_hover" => json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path, workdir-relative or absolute"},
                    "line": {"type": "integer", "description": "0-based line (per LSP)"},
                    "character": {"type": "integer", "description": "0-based character (per LSP)"},
                },
                "required": ["path", "line", "character"],
            }),
            "lsp_definition" => json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path, workdir-relative or absolute"},
                    "line": {"type": "integer", "description": "0-based line (per LSP)"},
                    "character": {"type": "integer", "description": "0-based character (per LSP)"},
                },
                "required": ["path", "line", "character"],
            }),
            "lsp_references" => json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path, workdir-relative or absolute"},
                    "line": {"type": "integer", "description": "0-based line (per LSP)"},
                    "character": {"type": "integer", "description": "0-based character (per LSP)"},
                    "include_declaration": {"type": "boolean", "description": "Include the declaration site (default true)"},
                },
                "required": ["path", "line", "character"],
            }),
            "dap_start" => json!({
                "type": "object",
                "properties": {
                    "program": {"type": "string", "description": "Debuggee path, workdir-relative or absolute (source file or built binary)"},
                    "language": {"type": "string", "description": "Adapter language: python, go, rust, csharp (default: inferred from extension; REQUIRED for built binaries like .exe/.dll)"},
                    "breakpoints": {"type": "array", "description": "Stops: [{path, lines}] with 1-based lines (default: none — the program runs to end)"},
                    "timeout_secs": {"type": "integer", "description": "Wait cap for the first stop (default 60, max 600)"},
                },
                "required": ["program"],
            }),
            "dap_continue" => json!({
                "type": "object",
                "properties": {
                    "session": {"type": "string", "description": "Session id from dap_start"},
                    "timeout_secs": {"type": "integer", "description": "Wait cap for the next stop (default 60, max 600)"},
                },
                "required": ["session"],
            }),
            "dap_inspect" => json!({
                "type": "object",
                "properties": {
                    "session": {"type": "string", "description": "Session id from dap_start (must be stopped — continue first)"},
                },
                "required": ["session"],
            }),
            "dap_evaluate" => json!({
                "type": "object",
                "properties": {
                    "session": {"type": "string", "description": "Session id from dap_start"},
                    "expression": {"type": "string", "description": "Expression in the debuggee language (a watch: variable names, arithmetic, calls with side effects DISCOURAGED — they mutate the debuggee)"},
                    "frame": {"type": "integer", "description": "Stack frame id (default: last inspected frame)"},
                },
                "required": ["session", "expression"],
            }),
            "dap_disconnect" => json!({
                "type": "object",
                "properties": {
                    "session": {"type": "string", "description": "Session id from dap_start (always disconnect when done — sessions never outlive the task)"},
                },
                "required": ["session"],
            }),
            "rdoc_capture" => json!({
                "type": "object",
                "properties": {
                    "program": {"type": "string", "description": "Graphics app path, workdir-relative or absolute (must present frames; OpenGL needs a 3.2+ core context — legacy is refused)"},
                    "capture_file": {"type": "string", "description": "Capture template path, frame number appended (default: <workdir>/rdoc_capture)"},
                    "timeout_secs": {"type": "integer", "description": "Wait cap for app exit (default 120, max 600)"},
                },
                "required": ["program"],
            }),
            "rdoc_draws" => json!({
                "type": "object",
                "properties": {
                    "capture": {"type": "string", "description": "Capture path (.rdc), workdir-relative or absolute"},
                },
                "required": ["capture"],
            }),
            "rdoc_state" => json!({
                "type": "object",
                "properties": {
                    "capture": {"type": "string", "description": "Capture path (.rdc), workdir-relative or absolute"},
                    "event": {"type": "integer", "description": "Frame event id (from rdoc_draws)"},
                },
                "required": ["capture", "event"],
            }),
            "rdoc_diff" => json!({
                "type": "object",
                "properties": {
                    "capture_a": {"type": "string", "description": "First capture path (.rdc)"},
                    "capture_b": {"type": "string", "description": "Second capture path (.rdc)"},
                },
                "required": ["capture_a", "capture_b"],
            }),
            "rdoc_script" => json!({
                "type": "object",
                "properties": {
                    "script": {"type": "string", "description": "Python script path (runs under RenderDoc's embedded runtime, NOT host Python). MUST write JSON {ok, ...} to the RDOC_OUT env path and quit the UI (PySide2 QApplication.instance().quit()) or the call times out after 120s"},
                },
                "required": ["script"],
            }),
            "web_search" => json!({
                "type": "object",
                "properties": {
                    "q": {"type": "string", "description": "Search query (1-500 chars)"},
                    "page": {"type": "integer", "description": "Page number, starts at 1 (default 1)"},
                    "safe": {"type": "integer", "description": "Safe search: 0 off, 1 moderate, 2 strict (default 1)"},
                    "lang": {"type": "string", "description": "One of en, de, fr, es (default: server decides)"},
                },
                "required": ["q"],
            }),
            "web_extract" => json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string", "description": "Page URL to extract as markdown (https)"},
                },
                "required": ["url"],
            }),
            "ask" => json!({
                "type": "object",
                "properties": {
                    "question": {"type": "string", "description": "The ambiguity, phrased so the options answer it"},
                    "options": {"type": "array", "description": "1–8 options: {label (returned verbatim), hint?, recommended? (at most one)}", "items": {"type": "object"}},
                    "multi": {"type": "boolean", "description": "Allow several labels (default false)"},
                    "allow_other": {"type": "boolean", "description": "Admit free-text Other (default true)"},
                },
                "required": ["question", "options"],
            }),
            "split_plan" => json!({
                "type": "object",
                "properties": {
                    "repo": {"type": "string", "description": "Repo path (workdir-relative or absolute; \".\" = workdir)"},
                    "deps": {"type": "array", "description": "Ordering edges [{before, after}] on file paths (default [])", "items": {"type": "object"}},
                },
                "required": ["repo"],
            }),
            _ => return None,
        };
        Some(s)
    }

    /// Dispatch one JSON tool call (Epic F3a): resolve → gate → execute.
    /// Gate runs BEFORE anything executes (a denial leaves zero side
    /// effects); arg errors name the field. Results are JSON (nonzero bash
    /// exits are data, never errors — exec precedent). Relative paths join
    /// `ctx.workdir`; absolute paths pass through (documented gap: no
    /// sandbox — tools are local by design, the Gate owns dangerous ops).
    /// Dispatch one JSON tool call (Epic F3a, hooks J1): pre-hook consult
    /// → resolve → gate → execute → post-hook notify. A pre-hook denial
    /// maps to typed `CallError::Denied` BEFORE resolve (hooks see probes
    /// too); post-hook observes every outcome including denials and
    /// unknown-tool refusals (telemetry, never control).
    pub async fn call(
        &self,
        name: &str,
        args: serde_json::Value,
        ctx: &ToolCallContext,
    ) -> Result<serde_json::Value, CallError> {
        if let HookDecision::Deny { reason } = self.hooks.consult_pre(name, &args) {
            let denied = CallError::Denied(Denied {
                call: name.to_string(),
                rule: format!("hook: {reason}"),
            });
            self.hooks.notify(HookEvent::PostToolUse {
                name: name.to_string(),
                ok: false,
                detail: denied.to_string(),
            });
            return Err(denied);
        }
        // Script pre-hooks (chunk 87): spawned, in order, first Deny wins.
        // Empty list = zero spawns (the common path costs one branch).
        let args_json = serde_json::to_string(&args).unwrap_or_default();
        for script in self
            .scripts
            .iter()
            .filter(|s| s.on == script_hooks::ScriptOn::Pre)
        {
            if let HookDecision::Deny { reason } =
                script_hooks::run_hook(script, name, &args_json, &ctx.cancel).await
            {
                let denied = CallError::Denied(Denied {
                    call: name.to_string(),
                    rule: reason,
                });
                self.hooks.notify(HookEvent::PostToolUse {
                    name: name.to_string(),
                    ok: false,
                    detail: denied.to_string(),
                });
                return Err(denied);
            }
        }
        // Ask-user hooks (chunk 89, D1): first question wins; sync/script
        // denials above already won. No handlers = one branch (the common
        // path costs nothing). Fires without a prompter = fail-closed Deny
        // (D2), posted like every other denial.
        if let Some(question) = self.ask.question_for(name, &args) {
            if let HookDecision::Deny { reason } = self.ask.resolve(name, question).await {
                let denied = CallError::Denied(Denied {
                    call: name.to_string(),
                    rule: format!("hook: {reason}"),
                });
                self.hooks.notify(HookEvent::PostToolUse {
                    name: name.to_string(),
                    ok: false,
                    detail: denied.to_string(),
                });
                return Err(denied);
            }
        }
        let outcome = self.call_inner(name, args.clone(), ctx).await;
        let (ok, detail) = match &outcome {
            Ok(value) => (true, summarized(value)),
            Err(err) => (false, err.to_string()),
        };
        // Script post-hooks: sequential observation (keep them fast).
        for script in self
            .scripts
            .iter()
            .filter(|s| s.on == script_hooks::ScriptOn::Post)
        {
            script_hooks::run_observe(script, "post", name, &args_json, ok, &detail, &ctx.cancel)
                .await;
        }
        self.hooks.notify(HookEvent::PostToolUse {
            name: name.to_string(),
            ok,
            detail,
        });
        outcome
    }

    /// Hide one file from `grep`/`glob` results (docket #8). Set before
    /// the registry is shared; `read` of the exact path still works.
    pub fn hide_from_scans(&mut self, path: PathBuf) {
        self.scan_hidden.push(path);
    }

    /// Set the `bash` spill policy (docket #9; `[agent]
    /// tool_output_inline_bytes` / `tool_output_keep_files`). Set before
    /// the registry is shared; the default spills past 16 KiB per stream.
    pub fn set_output_spill(&mut self, spill: output::SpillConfig) {
        self.output_spill = spill;
    }

    /// Files hidden from `grep`/`glob` (docket #8); command change
    /// detection never reports them either.
    pub fn scan_hidden(&self) -> &[PathBuf] {
        &self.scan_hidden
    }

    /// The `bash` spill policy (history compaction saves evicted tool
    /// results under the same directory and keep count).
    pub fn output_spill(&self) -> output::SpillConfig {
        self.output_spill
    }

    /// Register one hook handler (consulted in registration order).
    pub fn add_hook<F>(&mut self, handler: F)
    where
        F: Fn(&HookEvent) -> HookDecision + Send + Sync + 'static,
    {
        self.hooks.add_hook(handler);
    }

    /// Register one ask hook: returns the question for a call, or `None`
    /// to pass. First question wins; resolved through the prompter (D1).
    pub fn add_ask_hook<F>(&mut self, handler: F)
    where
        F: Fn(&str, &serde_json::Value) -> Option<String> + Send + Sync + 'static,
    {
        self.ask.add_ask_hook(handler);
    }

    /// Attach the interactive prompter. Absent = Ask fails closed (D2).
    pub fn set_prompter(&mut self, prompter: std::sync::Arc<dyn Prompter>) {
        self.ask.set_prompter(prompter);
    }

    /// Register one language server for write-through enforcement (L2).
    /// `[[lsp_servers]]` maps onto this at L3; duplicate languages refuse.
    /// Async like `inject_mcp_client` (shared `&self` + pool lock); spawn
    /// stays lazy at first write — the pool is empty until then.
    pub async fn add_lsp_server(&self, language: &str, spec: lsp::LspSpec) -> anyhow::Result<()> {
        self.lsp.lock().await.add_server(language, spec)
    }

    /// Register one debug adapter spec for M3 session spawn (M2).
    /// `[[dap_adapters]]` maps onto this; duplicate languages refuse.
    /// Specs only — sessions spawn fresh per use (no shared server).
    pub async fn add_dap_adapter(&self, language: &str, spec: dap::DapSpec) -> anyhow::Result<()> {
        self.dap.lock().await.add_adapter(language, spec)
    }

    /// Adapter spec for one language (M3 reads through here at spawn).
    pub async fn dap_spec_for(&self, language: &str) -> Option<dap::DapSpec> {
        self.dap.lock().await.spec_for(language)
    }

    /// Enroll the RenderDoc binary (N1a reads through here at capture).
    /// `[renderdoc]` maps onto this; absent until wired (fail-closed).
    pub async fn set_renderdoc_spec(&self, spec: renderdoc::RenderdocSpec) {
        *self.renderdoc.lock().await = Some(spec);
    }

    /// Enrolled RenderDoc binary, if any.
    pub async fn renderdoc_spec(&self) -> Option<renderdoc::RenderdocSpec> {
        self.renderdoc.lock().await.clone()
    }

    /// Enroll the serica server address (P1b reads through here).
    /// `[web]` maps onto this; absent until wired (localhost default).
    pub async fn set_web_spec(&self, spec: web::WebSpec) {
        *self.web.lock().await = Some(spec);
    }

    /// Enrolled serica address, or localhost defaults when unwired
    /// (fail-soft: refused connects name `base_url` + `serica server`).
    pub async fn web_spec(&self) -> web::WebSpec {
        self.web.lock().await.clone().unwrap_or_else(|| {
            web::validate(web::DEFAULT_BASE_URL, web::DEFAULT_TIMEOUT_SECS)
                .expect("hardcoded web defaults must validate")
        })
    }

    /// Take the pending LSP notice, if any (enforcement telemetry).
    pub async fn lsp_take_notice(&self) -> Option<String> {
        self.lsp.lock().await.take_notice()
    }

    /// Last recorded diagnostics per URI (L4 gutter feed).
    pub async fn lsp_diagnostics(&self) -> std::collections::BTreeMap<String, Vec<String>> {
        self.lsp.lock().await.diagnostics()
    }

    /// Last recorded diagnostics per URI with line + severity (docket #3:
    /// reviewer evidence + the opt-in error gate; same map, richer rows).
    pub async fn lsp_diagnostic_details(
        &self,
    ) -> std::collections::BTreeMap<String, Vec<lsp::LspDiagnostic>> {
        self.lsp.lock().await.diagnostic_details()
    }

    /// Whether a language is configured or learned as push-only diagnostics.
    pub async fn lsp_is_push_only(&self, language: &str) -> bool {
        self.lsp.lock().await.is_push_only(language)
    }

    /// Test-only seed for the L-actuation (1) brief feed (chunk 98):
    /// writes directly into the pool's last-recorded map — no server,
    /// no IO. Production populates via `lsp_post_write` only.
    pub async fn test_seed_lsp_diagnostics(
        &self,
        map: std::collections::BTreeMap<String, Vec<String>>,
    ) {
        self.lsp.lock().await.test_seed_diagnostics(map);
    }

    /// Test-only structured seed (docket #3): line + severity rows.
    pub async fn test_seed_lsp_diagnostic_details(
        &self,
        map: std::collections::BTreeMap<String, Vec<lsp::LspDiagnostic>>,
    ) {
        self.lsp.lock().await.test_seed_diagnostic_details(map);
    }

    /// Post-write enforcement fan-out (L2, D4): candidate paths from the
    /// tool args, per-path language routing, pool consult. Best-effort
    /// throughout — unknown extensions skip silently, unparseable patches
    /// are impossible post-success (skipped, not errored). No gate check:
    /// the write already passed its own gate; this pull is read-only.
    async fn lsp_post_write(&self, tool: &str, args: &serde_json::Value, ctx: &ToolCallContext) {
        let mut paths: Vec<String> = Vec::new();
        if tool == "write" {
            if let Some(path) = args.get("path").and_then(serde_json::Value::as_str) {
                paths.push(path.to_string());
            }
        } else if tool == "edit" {
            if let Some(patch) = args.get("patch").and_then(serde_json::Value::as_str) {
                if let Ok(parsed) = hashline::parse(patch) {
                    for sec in &parsed.sections {
                        // A `MV` section's file now lives at its destination:
                        // the server sees the moved file there too.
                        let dests = sec.ops.iter().filter_map(|op| match op {
                            hashline::Op::MoveFile { dest } => Some(dest),
                            _ => None,
                        });
                        for p in std::iter::once(&sec.path).chain(dests) {
                            if !paths.contains(p) {
                                paths.push(p.clone());
                            }
                        }
                    }
                }
            }
        }
        self.lsp_consult(&paths, ctx).await;
    }

    /// Consult the LSP pool on `paths` as just written (the post-write
    /// pull, shared by `write`/`edit` and by command change detection for
    /// files a `bash` command changed). Unknown extensions skip silently;
    /// a path with no readable content pulls bare.
    pub async fn lsp_consult(&self, paths: &[String], ctx: &ToolCallContext) {
        if paths.is_empty() {
            return;
        }
        // Workspace root for initialize (chunk-97 live gaps #2 and #5:
        // servers like rust-analyzer discover nothing under a null root,
        // and match file URLs case-sensitively — drive letter goes
        // lowercase via the shared helper).
        // Verbatim root to match verbatim file URIs below when any
        // written language needs it (same server compares both).
        let verbatim = paths
            .iter()
            .filter_map(|p| lsp::language_of(p))
            .any(lsp::prefer_verbatim_uri);
        let root_uri = if verbatim {
            lsp::root_uri_for_verbatim(&ctx.workdir)
        } else {
            lsp::root_uri_for(&ctx.workdir)
        };
        let mut pool = self.lsp.lock().await;
        for path in paths {
            let Some(language) = lsp::language_of(path) else {
                continue;
            };
            let uri = if lsp::prefer_verbatim_uri(language) {
                lsp::uri_for_verbatim(&ctx.workdir, path)
            } else {
                lsp::uri_for(&ctx.workdir, path)
            };
            // Best-effort content read for the didOpen (the file was just
            // written — unreadable means deleted-or-raced; pull bare).
            let rel = std::path::Path::new(path);
            let abs = if rel.is_absolute() {
                rel.to_path_buf()
            } else {
                ctx.workdir.join(rel)
            };
            let text = std::fs::read_to_string(&abs).ok();
            pool.post_write(language, &uri, text.as_deref(), Some(&root_uri))
                .await;
        }
    }

    /// Fire a Stop event (agentic loop termination — observation only).
    /// Async: stop-scripts spawn here (in-process notify first, cheap first).
    pub async fn fire_stop(&self, reason: String) {
        self.hooks.notify(HookEvent::Stop {
            reason: reason.clone(),
        });
        let cancel = tokio_util::sync::CancellationToken::new();
        for script in self
            .scripts
            .iter()
            .filter(|s| s.on == script_hooks::ScriptOn::Stop)
        {
            script_hooks::run_observe(script, "stop", "-", "{}", true, &reason, &cancel).await;
        }
    }

    /// Attach validated script hooks from `[[hooks]] config entries.
    /// Bad entries fail the whole attach (fail-closed at startup).
    pub fn apply_script_hooks(
        &mut self,
        entries: &[polymorph_core::config::HookEntry],
    ) -> anyhow::Result<()> {
        for entry in entries {
            self.scripts.push(script_hooks::validate_entry(entry)?);
        }
        Ok(())
    }

    async fn call_inner(
        &self,
        name: &str,
        args: serde_json::Value,
        ctx: &ToolCallContext,
    ) -> Result<serde_json::Value, CallError> {
        if self.get(name).is_none() {
            return Err(CallError::Failed(anyhow::anyhow!(
                "unknown tool {name:?} (available: {})",
                self.names().join(", ")
            )));
        }
        match name {
            "read" => self.call_read(&args, ctx).await,
            "write" => {
                let out = call_write(&args, ctx).await?;
                // L2 write-through (D4): the server sees every write; the
                // write itself already succeeded — enforcement cannot fail
                // it (post_write returns (), never Err).
                self.lsp_post_write("write", &args, ctx).await;
                Ok(out)
            }
            "edit" => {
                let out = self.call_edit(&args, ctx).await?;
                self.lsp_post_write("edit", &args, ctx).await;
                Ok(out)
            }
            "grep" => call_grep(&args, ctx, &self.scan_hidden).await,
            "glob" => call_glob(&args, ctx, &self.scan_hidden).await,
            "bash" => call_bash(&args, ctx, &self.output_spill).await,
            // LSP read-only queries (chunk 100, L-actuation 3A):
            // fixer-allowlist tools, gate-first like every fs tool.
            "lsp_hover" => self.call_lsp("hover", &args, ctx).await,
            "lsp_definition" => self.call_lsp("definition", &args, ctx).await,
            "lsp_references" => self.call_lsp("references", &args, ctx).await,
            // Debugger sessions (chunk 107, M3): fixer-allowlist tools
            // over the session table, gate-first on the debuggee program.
            "dap_start" => self.call_dap_start(&args, ctx).await,
            "dap_continue" => self.call_dap_session_op("dap_continue", &args, ctx).await,
            "dap_inspect" => self.call_dap_session_op("dap_inspect", &args, ctx).await,
            "dap_evaluate" => self.call_dap_session_op("dap_evaluate", &args, ctx).await,
            "dap_disconnect" => self.call_dap_session_op("dap_disconnect", &args, ctx).await,
            // Graphics capture (chunk 112, N1a): headed capture to .rdc,
            // gate-first on the target program like DAP session tools.
            "rdoc_capture" => self.call_rdoc_capture(&args, ctx).await,
            // Capture introspection (chunk 113, N1b): headed replay
            // queries over .rdc files, gate-first on the capture (data-
            // scoped); user scripts gate on their own path instead.
            "rdoc_draws" => self.call_rdoc_introspect("rdoc_draws", &args, ctx).await,
            "rdoc_state" => self.call_rdoc_introspect("rdoc_state", &args, ctx).await,
            "rdoc_diff" => self.call_rdoc_introspect("rdoc_diff", &args, ctx).await,
            "rdoc_script" => self.call_rdoc_script(&args, ctx).await,
            // Web research (chunk 120, P1b): serica-backed search +
            // extract, gate-first on the query/URL (broad rules cover
            // whole tools via the `rule:` prefix match — DAP precedent).
            "web_search" => self.call_web_search(&args, ctx).await,
            "web_extract" => self.call_web_extract(&args, ctx).await,
            // Structured ambiguity picker (chunk 122, Q): gate-first
            // like every tool; headless dismissal fails closed.
            "ask" => self.call_ask(&args, ctx).await,
            // Atomic-split proposal (chunk 128, T2): propose-only
            // groups, gate-first pre-spawn like every tool.
            "split_plan" => self.call_split_plan(&args, ctx).await,
            // MCP tools (chunk 54): enrolled names dispatch through the
            // pool; gate-first inside (denials precede connects).
            _ if name.starts_with("mcp__") => {
                crate::mcp::call_mcp_tool(&self.mcp, name, &args, &ctx.gate).await
            }
            // Reachable only if a descriptor registers without dispatch —
            // a programming error, loud by construction (registry test pins
            // the nine names both ways).
            _ => Err(CallError::Failed(anyhow::anyhow!(
                "tool {name:?} registered but not dispatchable"
            ))),
        }
    }

    async fn call_edit(
        &self,
        args: &serde_json::Value,
        ctx: &ToolCallContext,
    ) -> Result<serde_json::Value, CallError> {
        let patch = arg_str(args, "edit", "patch")?;
        // Path-level gating: every section target gates before anything
        // validates or writes (apply itself is all-or-nothing, but the gate
        // must speak first). Parse errors bail here with the parser's text.
        let parsed = hashline::parse(&patch)
            // .context (not format!) keeps the typed HashlineError in the
            // chain for the bench classifier (chunk 38) — Display text
            // stays "edit: invalid patch: ..." either way.
            .context("edit: invalid patch")?;
        for sec in &parsed.sections {
            ctx.gate
                .check(&format!("edit:{}", sec.path))
                .map_err(CallError::Denied)?;
        }
        let store = self.snapshots.lock().await;
        let report = hashline::apply_text_to_disk(&patch, &ctx.workdir, &store)
            // Same chain discipline as parse above: apply faults stay
            // downcastable (StaleTag et al.), Display unchanged.
            .context("edit: apply failed")?;
        Ok(serde_json::json!({
            "files": report
                .files
                .iter()
                .map(|(p, a)| serde_json::json!({"path": p, "action": format!("{a:?}")}))
                .collect::<Vec<_>>(),
        }))
    }

    /// Read-only LSP query (chunk 100, L-actuation 3A): gate → route →
    /// read → didOpen + one hover/definition/references request. Never
    /// writes; reviewers never see these (fixer allowlist opt-in only —
    /// reviewer spawns keep `tools = []`). No server / unknown extension
    /// is a model-readable Ok note (L fail-soft), never an error; request
    /// failures evict (the post_write path) and name the op.
    async fn call_lsp(
        &self,
        op: &str,
        args: &serde_json::Value,
        ctx: &ToolCallContext,
    ) -> Result<serde_json::Value, CallError> {
        let tool = match op {
            "hover" => "lsp_hover",
            "definition" => "lsp_definition",
            _ => "lsp_references",
        };
        let path = arg_str(args, tool, "path")?;
        let line = arg_pos(args, tool, "line")?;
        let character = arg_pos(args, tool, "character")?;
        let include_declaration = args
            .get("include_declaration")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);
        // Gate FIRST (before any read or connect — a denial leaves zero
        // side effects, the F3a contract).
        ctx.gate
            .check(&format!("{tool}:{path}"))
            .map_err(CallError::Denied)?;
        let Some(language) = lsp::language_of(&path) else {
            return Ok(serde_json::json!({
                "uri": null, "result": null,
                "note": format!("no language route for {path:?} — plain read stands"),
            }));
        };
        let abs = resolve_path(ctx, &path);
        let bytes = std::fs::read(&abs)
            .map_err(|e| anyhow::anyhow!("{tool}: cannot read {path:?}: {e}"))?;
        if bytes.iter().take(8192).any(|&b| b == 0) {
            return Err(CallError::Failed(anyhow::anyhow!(
                "{tool}: {path:?} looks binary"
            )));
        }
        let text = String::from_utf8_lossy(&bytes).into_owned();
        // Same URI/root discipline as the write-through path (chunk-97
        // gaps #2 + #5: null roots starve rust-analyzer; drive case is
        // per-server via `prefer_verbatim_uri`).
        let verbatim = lsp::prefer_verbatim_uri(language);
        let uri = if verbatim {
            lsp::uri_for_verbatim(&ctx.workdir, &path)
        } else {
            lsp::uri_for(&ctx.workdir, &path)
        };
        let root = if verbatim {
            lsp::root_uri_for_verbatim(&ctx.workdir)
        } else {
            lsp::root_uri_for(&ctx.workdir)
        };
        let mut pool = self.lsp.lock().await;
        match pool
            .query(
                language,
                &uri,
                &text,
                Some(&root),
                op,
                line,
                character,
                include_declaration,
            )
            .await
        {
            None => {
                if pool.warned_missing(language) {
                    tracing::warn!(language, "lsp query fallback: no server");
                }
                Ok(serde_json::json!({
                    "uri": uri, "result": null,
                    "note": format!("no LSP server for '{language}' — plain read stands"),
                }))
            }
            Some(Ok(value)) => {
                Ok(serde_json::json!({"uri": uri, "language": language, "result": value}))
            }
            Some(Err(e)) => Err(CallError::Failed(anyhow::anyhow!(
                "{tool}: {op} failed: {e:#}"
            ))),
        }
    }

    /// Open a debug session (chunk 107, M3): gate → resolve → spawn →
    /// initialize → launch (held) → breakpoints → configured → first
    /// stop. Returns `{session, stopped:{reason,thread} | ended:true}`.
    /// Permissions follow the debuggee: every later session op gates
    /// `dap_<op>:<program>` against the SAME program string.
    /// The table lock is held across the drive (the `lsp_post_write`
    /// precedent — one session drives at a time per registry).
    async fn call_dap_start(
        &self,
        args: &serde_json::Value,
        ctx: &ToolCallContext,
    ) -> Result<serde_json::Value, CallError> {
        const TOOL: &str = "dap_start";
        let program = arg_str(args, TOOL, "program")?;
        // Gate FIRST (before any spawn — a denial leaves zero side
        // effects, the F3a contract).
        ctx.gate
            .check(&format!("{TOOL}:{program}"))
            .map_err(CallError::Denied)?;
        let abs = resolve_path(ctx, &program);
        if !abs.is_file() {
            return Err(CallError::Failed(anyhow::anyhow!(
                "{TOOL}: program {program:?} is not a file"
            )));
        }
        let language = match arg_opt_str(args, "language") {
            Some(l) => l,
            None => lsp::language_of(&program).map(str::to_string).ok_or_else(|| {
                anyhow::anyhow!("{TOOL}: cannot infer language for {program:?} — pass `language` (python/go/rust/csharp)")
            })?,
        };
        let workdir = abs
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        // Absolute paths into the adapter (agents send relative ones,
        // and adapters vary in resolution: dlv matches breakpoint
        // sources against loaded absolute paths — relative never binds
        // and the program silently runs to end. Gate strings above keep
        // the user-supplied form: permissions stay stable and readable).
        let abs_program = abs.to_string_lossy().into_owned();
        let launch_args = dap::launch_args_for(&language, &abs_program, &workdir)?;
        let breakpoints = parse_dap_breakpoints(args, TOOL)?;
        let timeout = std::time::Duration::from_secs(
            arg_opt_u64(args, TOOL, "timeout_secs")?
                .unwrap_or(60)
                .min(600),
        );
        let spec = self.dap.lock().await.spec_for(&language).ok_or_else(|| {
            CallError::Failed(anyhow::anyhow!(
                "{TOOL}: no DAP adapter for '{language}' — register one first"
            ))
        })?;
        // Transport follows the adapter: dlv speaks TCP only (M2),
        // everything else rides stdio. The session box erases the
        // difference (the M2 `DapSession` trait).
        let mut runner: Box<dyn dap::DapSession> = if language == "go" {
            let mut spec = spec;
            spec.args = vec![
                "dap".to_string(),
                "-l".to_string(),
                "127.0.0.1:0".to_string(),
                "--headless".to_string(),
            ];
            spec.workdir = Some(workdir.clone().into());
            Box::new(
                dap::DapTcpClient::connect_discovered(&spec, &[])
                    .await
                    .map_err(|e| anyhow::anyhow!("{TOOL}: dlv spawn/connect failed: {e:#}"))?,
            )
        } else {
            let mut spec = spec;
            spec.workdir = Some(workdir.clone().into());
            Box::new(
                dap::DapClient::spawn(&spec)
                    .await
                    .map_err(|e| anyhow::anyhow!("{TOOL}: adapter spawn failed: {e:#}"))?,
            )
        };
        runner
            .initialize(&language)
            .await
            .map_err(|e| anyhow::anyhow!("{TOOL}: initialize failed: {e:#}"))?;
        let launch_seq = runner
            .send_request("launch", launch_args)
            .await
            .map_err(|e| anyhow::anyhow!("{TOOL}: launch send failed: {e:#}"))?;
        for (path, lines) in &breakpoints {
            let apath = resolve_path(ctx, path).to_string_lossy().into_owned();
            runner
                .set_breakpoints(&apath, lines)
                .await
                .map_err(|e| anyhow::anyhow!("{TOOL}: breakpoints for {path:?} failed: {e:#}"))?;
        }
        runner
            .configuration_done()
            .await
            .map_err(|e| anyhow::anyhow!("{TOOL}: configuration failed: {e:#}"))?;
        runner
            .await_response(launch_seq, "launch")
            .await
            .map_err(|e| anyhow::anyhow!("{TOOL}: launch failed: {e:#}"))?;
        let mut sessions = self.dap_sessions.lock().await;
        match Self::await_stop(runner.as_mut(), timeout).await {
            StopOutcome::Stopped { reason, thread } => {
                let id = sessions.insert(dap::LiveSession {
                    program: program.clone(),
                    language,
                    thread: Some(thread),
                    frame: None,
                    runner,
                });
                // `runner` moved into the table — its teardown rides the
                // table now (`dap_disconnect`, or `kill_on_drop` at worst).
                Ok(serde_json::json!({
                    "session": id,
                    "stopped": {"reason": reason, "thread": thread},
                }))
            }
            StopOutcome::Ended => Ok(serde_json::json!({
                "note": "program ended before any breakpoint — no session kept",
                "ended": true,
            })),
        }
    }

    /// One stopped-or-ended wait shared by start and continue (chunk
    /// 107, M3): whichever of `stopped` vs terminal (`terminated` /
    /// `exited` / `disconnect`) posts FIRST wins — run-to-completion
    /// programs resolve in milliseconds, not after a burnt budget (the
    /// single-name wait would spend the full timeout on every clean
    /// exit). Stopped records thread (frame clears — it belongs to the
    /// next inspect); terminal ends the run (the caller drops the
    /// runner: `kill_on_drop` reaps, no table entry, no leak).
    async fn await_stop(
        runner: &mut dyn dap::DapSession,
        timeout: std::time::Duration,
    ) -> StopOutcome {
        let ev = match runner
            .await_any(&["stopped", "terminated", "exited", "disconnect"], timeout)
            .await
        {
            Ok(ev) => ev,
            Err(_) => return StopOutcome::Ended,
        };
        let kind = ev
            .get("event")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        if kind != "stopped" {
            return StopOutcome::Ended;
        }
        let reason = ev
            .get("body")
            .and_then(|b| b.get("reason"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let thread = ev
            .get("body")
            .and_then(|b| b.get("threadId"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        StopOutcome::Stopped { reason, thread }
    }

    /// Headed graphics capture (chunk 112, N1a): gate → resolve →
    /// fail-closed spec lookup → `renderdoccmd capture` → newest `.rdc`.
    /// Permissions follow the TARGET (`rdoc_capture:<program>` — the DAP
    /// program-scoped precedent; capture ids could never be
    /// pre-authorized). The sample's workdir contains debris by
    /// construction (program-parent rule, the dlv lesson).
    async fn call_rdoc_capture(
        &self,
        args: &serde_json::Value,
        ctx: &ToolCallContext,
    ) -> Result<serde_json::Value, CallError> {
        const TOOL: &str = "rdoc_capture";
        let program = arg_str(args, TOOL, "program")?;
        // Gate FIRST (before any spawn — a denial leaves zero side
        // effects, the F3a contract).
        ctx.gate
            .check(&format!("{TOOL}:{program}"))
            .map_err(CallError::Denied)?;
        let abs = resolve_path(ctx, &program);
        if !abs.is_file() {
            return Err(CallError::Failed(anyhow::anyhow!(
                "{TOOL}: program {program:?} is not a file"
            )));
        }
        let spec = self.renderdoc_spec().await.ok_or_else(|| {
            CallError::Failed(anyhow::anyhow!(
                "{TOOL}: no renderdoc binary — set `[renderdoc] cmd` first"
            ))
        })?;
        let template = match arg_opt_str(args, "capture_file") {
            Some(t) => resolve_path(ctx, &t),
            None => ctx.workdir.join("rdoc_capture"),
        };
        let timeout = std::time::Duration::from_secs(
            arg_opt_u64(args, TOOL, "timeout_secs")?
                .unwrap_or(120)
                .min(600),
        );
        let workdir = abs
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| ctx.workdir.clone());
        let rep = renderdoc::capture(&spec, &abs, &template, &workdir, timeout)
            .await
            .map_err(CallError::Failed)?;
        Ok(serde_json::json!({
            "capture": rep.capture.to_string_lossy(),
            "bytes": rep.bytes,
            "captures": rep.captures,
        }))
    }

    /// Capture introspection (chunk 113, N1b): headed replay queries.
    /// Draws/state/diff share capture-gating + spec lookup; only the
    /// query mode and shaping differ. Permissions follow the CAPTURE
    /// (`rdoc_<op>:<capture>` — data-scoped, the DAP program precedent).
    async fn call_rdoc_introspect(
        &self,
        op: &str,
        args: &serde_json::Value,
        ctx: &ToolCallContext,
    ) -> Result<serde_json::Value, CallError> {
        let (mode, capture_raw, extra) = match op {
            "rdoc_draws" => ("draws", arg_str(args, op, "capture")?, Vec::new()),
            "rdoc_state" => {
                let capture = arg_str(args, op, "capture")?;
                let event = arg_opt_u64(args, op, "event")?.ok_or_else(|| {
                    anyhow::anyhow!("{op}: `event` must be a non-negative integer")
                })?;
                (
                    "state",
                    capture,
                    vec![("RDOC_EVENT".to_string(), event.to_string())],
                )
            }
            "rdoc_diff" => {
                let a = arg_str(args, op, "capture_a")?;
                let b = arg_str(args, op, "capture_b")?;
                for (field, raw) in [("capture_a", &a), ("capture_b", &b)] {
                    ctx.gate
                        .check(&format!("{op}:{raw}"))
                        .map_err(CallError::Denied)?;
                    let abs = resolve_path(ctx, raw);
                    if !abs.is_file() {
                        return Err(CallError::Failed(anyhow::anyhow!(
                            "{op}: `{field}` {raw:?} is not a file"
                        )));
                    }
                }
                let spec = self.renderdoc_spec().await.ok_or_else(|| {
                    CallError::Failed(anyhow::anyhow!(
                        "{op}: no renderdoc binary — set `[renderdoc] cmd` first"
                    ))
                })?;
                let abs_a = resolve_path(ctx, &a);
                let abs_b = resolve_path(ctx, &b);
                let env = vec![(
                    "RDOC_FILE_B".to_string(),
                    abs_b.to_string_lossy().into_owned(),
                )];
                let value = renderdoc::run_query(&spec, "diff", &abs_a, &env)
                    .await
                    .map_err(CallError::Failed)?;
                return Ok(serde_json::json!({
                    "capture_a": abs_a.to_string_lossy(),
                    "capture_b": abs_b.to_string_lossy(),
                    "added": value.get("added").cloned().unwrap_or_default(),
                    "removed": value.get("removed").cloned().unwrap_or_default(),
                    "count_a": value.get("count_a").cloned().unwrap_or_default(),
                    "count_b": value.get("count_b").cloned().unwrap_or_default(),
                }));
            }
            _ => {
                return Err(CallError::Failed(anyhow::anyhow!(
                    "unknown introspection {op:?}"
                )))
            }
        };
        ctx.gate
            .check(&format!("{op}:{capture_raw}"))
            .map_err(CallError::Denied)?;
        let abs = resolve_path(ctx, &capture_raw);
        if !abs.is_file() {
            return Err(CallError::Failed(anyhow::anyhow!(
                "{op}: `capture` {capture_raw:?} is not a file"
            )));
        }
        let spec = self.renderdoc_spec().await.ok_or_else(|| {
            CallError::Failed(anyhow::anyhow!(
                "{op}: no renderdoc binary — set `[renderdoc] cmd` first"
            ))
        })?;
        let value = renderdoc::run_query(&spec, mode, &abs, &extra)
            .await
            .map_err(CallError::Failed)?;
        if op == "rdoc_state" {
            return Ok(serde_json::json!({
                "capture": abs.to_string_lossy(),
                "state": value.get("state").cloned().unwrap_or_default(),
            }));
        }
        let mut shaped = renderdoc::shape_draws(&value);
        shaped["capture"] = serde_json::Value::String(abs.to_string_lossy().into_owned());
        Ok(shaped)
    }

    /// User-script execution (chunk 113, N1b): runs a caller-supplied
    /// Python file under the embedded runtime. Gates on the SCRIPT path
    /// (user code execution — the dangerous arm); embedded first-class
    /// runs bypass this gate as own code (builtin-tool trust).
    async fn call_rdoc_script(
        &self,
        args: &serde_json::Value,
        ctx: &ToolCallContext,
    ) -> Result<serde_json::Value, CallError> {
        const TOOL: &str = "rdoc_script";
        let script = arg_str(args, TOOL, "script")?;
        ctx.gate
            .check(&format!("{TOOL}:{script}"))
            .map_err(CallError::Denied)?;
        let abs = resolve_path(ctx, &script);
        if !abs.is_file() {
            return Err(CallError::Failed(anyhow::anyhow!(
                "{TOOL}: `script` {script:?} is not a file"
            )));
        }
        let spec = self.renderdoc_spec().await.ok_or_else(|| {
            CallError::Failed(anyhow::anyhow!(
                "{TOOL}: no renderdoc binary — set `[renderdoc] cmd` first"
            ))
        })?;
        let content = std::fs::read_to_string(&abs).map_err(|e| {
            CallError::Failed(anyhow::anyhow!("{TOOL}: cannot read {script:?}: {e:#}"))
        })?;
        let value = renderdoc::run_script(&spec, &content, &[])
            .await
            .map_err(CallError::Failed)?;
        let echo = value.to_string();
        let (output, truncated) = if echo.len() > renderdoc::MAX_SCRIPT_ECHO {
            (echo[..renderdoc::MAX_SCRIPT_ECHO].to_string(), true)
        } else {
            (echo, false)
        };
        Ok(serde_json::json!({
            "script": abs.to_string_lossy(),
            "truncated": truncated,
            "output": output,
        }))
    }

    /// Session-op router (chunk 107, M3): continue/inspect/evaluate/
    /// disconnect share lookup + program-scoped gating; only the drive
    /// differs. Unknown sessions fail naming the id (typos stay loud).
    async fn call_dap_session_op(
        &self,
        op: &str,
        args: &serde_json::Value,
        ctx: &ToolCallContext,
    ) -> Result<serde_json::Value, CallError> {
        let session = arg_str(args, op, "session")?;
        let mut sessions = self.dap_sessions.lock().await;
        let program = sessions
            .get(&session)
            .map(|s| s.program.clone())
            .ok_or_else(|| {
                CallError::Failed(anyhow::anyhow!("{op}: no such session {session:?}"))
            })?;
        // Permissions follow the debuggee program (stable across the
        // session — opaque ids could never be pre-authorized).
        ctx.gate
            .check(&format!("{op}:{program}"))
            .map_err(CallError::Denied)?;
        match op {
            "dap_continue" => {
                let timeout = std::time::Duration::from_secs(
                    arg_opt_u64(args, op, "timeout_secs")?
                        .unwrap_or(60)
                        .min(600),
                );
                let thread = sessions
                    .get(&session)
                    .and_then(|s| s.thread)
                    .ok_or_else(|| {
                        CallError::Failed(anyhow::anyhow!(
                            "{op}: session {session:?} has no stopped thread"
                        ))
                    })?;
                let stopped = {
                    let live = sessions.get_mut(&session).ok_or_else(|| {
                        CallError::Failed(anyhow::anyhow!("{op}: no such session {session:?}"))
                    })?;
                    live.runner
                        .request("continue", serde_json::json!({"threadId": thread}))
                        .await
                        .map_err(|e| anyhow::anyhow!("{op}: resume failed: {e:#}"))?;
                    live.thread = None;
                    live.frame = None;
                    Self::await_stop(live.runner.as_mut(), timeout).await
                };
                match stopped {
                    StopOutcome::Stopped { reason, thread } => {
                        if let Some(live) = sessions.get_mut(&session) {
                            live.thread = Some(thread);
                        }
                        Ok(serde_json::json!({
                            "session": session,
                            "stopped": {"reason": reason, "thread": thread},
                        }))
                    }
                    StopOutcome::Ended => {
                        Self::remove_session(&mut sessions, &session).await;
                        Ok(serde_json::json!({"session": session, "ended": true}))
                    }
                }
            }
            "dap_inspect" => {
                let live = sessions.get_mut(&session).ok_or_else(|| {
                    CallError::Failed(anyhow::anyhow!("{op}: no such session {session:?}"))
                })?;
                let threads = live
                    .runner
                    .threads()
                    .await
                    .map_err(|e| anyhow::anyhow!("{op}: threads failed: {e:#}"))?;
                let tid = live
                    .thread
                    .or_else(|| {
                        threads
                            .get("threads")
                            .and_then(|t| t.as_array())
                            .and_then(|t| t.first())
                            .and_then(|t| t.get("id"))
                            .and_then(serde_json::Value::as_u64)
                    })
                    .ok_or_else(|| anyhow::anyhow!("{op}: no threads — is the session stopped?"))?;
                let stack = live
                    .runner
                    .stack_trace(tid, 0, dap::MAX_INSPECT_FRAMES as u64)
                    .await
                    .map_err(|e| anyhow::anyhow!("{op}: stack failed: {e:#}"))?;
                let frames: Vec<serde_json::Value> = stack
                    .get("stackFrames")
                    .and_then(|f| f.as_array())
                    .cloned()
                    .unwrap_or_default();
                let frame_id = live.frame.or_else(|| {
                    frames
                        .first()
                        .and_then(|f| f.get("id"))
                        .and_then(serde_json::Value::as_u64)
                });
                let mut scopes_out = Vec::new();
                if let Some(fid) = frame_id {
                    let scopes = live
                        .runner
                        .scopes(fid)
                        .await
                        .map_err(|e| anyhow::anyhow!("{op}: scopes failed: {e:#}"))?;
                    if let Some(list) = scopes.get("scopes").and_then(|s| s.as_array()) {
                        for scope in list {
                            let vr = scope
                                .get("variablesReference")
                                .and_then(serde_json::Value::as_u64)
                                .unwrap_or(0);
                            let mut vars = Vec::new();
                            let mut omitted = 0usize;
                            if vr > 0 {
                                let got = live.runner.variables(vr).await.map_err(|e| {
                                    anyhow::anyhow!("{op}: variables failed: {e:#}")
                                })?;
                                let all: Vec<serde_json::Value> = got
                                    .get("variables")
                                    .and_then(|v| v.as_array())
                                    .cloned()
                                    .unwrap_or_default();
                                omitted = all.len().saturating_sub(dap::MAX_INSPECT_VARS);
                                vars = all.into_iter().take(dap::MAX_INSPECT_VARS).collect();
                            }
                            scopes_out.push(serde_json::json!({
                                "name": scope.get("name"),
                                "variables": vars,
                                "omitted_variables": omitted,
                            }));
                        }
                    }
                    live.frame = Some(fid);
                }
                Ok(serde_json::json!({
                    "session": session,
                    "threads": threads.get("threads"),
                    "frames": frames,
                    "scopes": scopes_out,
                }))
            }
            "dap_evaluate" => {
                let expression = arg_str(args, op, "expression")?;
                let frame_arg = arg_opt_u64(args, op, "frame")?;
                let live = sessions.get_mut(&session).ok_or_else(|| {
                    CallError::Failed(anyhow::anyhow!("{op}: no such session {session:?}"))
                })?;
                let frame_id = match frame_arg {
                    Some(f) => Some(
                        u32::try_from(f)
                            .map(|f| f as u64)
                            .map_err(|_| anyhow::anyhow!("{op}: `frame` out of range"))?,
                    ),
                    None => live.frame,
                };
                let body = live
                    .runner
                    .evaluate(&expression, frame_id)
                    .await
                    .map_err(|e| anyhow::anyhow!("{op}: evaluate failed: {e:#}"))?;
                Ok(serde_json::json!({"session": session, "result": body}))
            }
            "dap_disconnect" => {
                Self::remove_session(&mut sessions, &session).await;
                Ok(serde_json::json!({"session": session, "disconnected": true}))
            }
            _ => Err(CallError::Failed(anyhow::anyhow!(
                "unknown dap session op {op:?}"
            ))),
        }
    }

    /// Remove a session and best-effort reap its adapter. Never fails
    /// (teardown observes; `kill_on_drop` is the final backstop).
    async fn remove_session(sessions: &mut dap::DapSessions, id: &str) {
        if let Some(mut live) = sessions.remove(id) {
            live.runner.shutdown().await;
        }
    }

    /// Read a file — or a `web://` URL through serica (P1b chunk 120;
    /// other `://` schemes fail closed naming P2). Gate stays first,
    /// unchanged position; `web://` reuses the `read:{path}` grant.
    async fn call_read(
        &self,
        args: &serde_json::Value,
        ctx: &ToolCallContext,
    ) -> Result<serde_json::Value, CallError> {
        let path = arg_str(args, "read", "path")?;
        ctx.gate
            .check(&format!("read:{path}"))
            .map_err(CallError::Denied)?;
        if let Some(rest) = path.strip_prefix("web://") {
            // Bare hosts are shorthand for https (serica needs a full
            // URL — caught live in 120c: bare `example.com` answered
            // INVALID_QUERY). An embedded scheme passes through as-is.
            let url = if rest.contains("://") {
                rest.to_string()
            } else {
                format!("https://{rest}")
            };
            return self.call_scheme_extract(&url, None).await;
        }
        // P2 handlers (chunk 121): scheme URL → fetch URL + fragment
        // echo. Gate stays the single `read:{path}` check above.
        if let Some(rest) = path.strip_prefix("gh://") {
            let (url, fragment) = web::github_url(rest)
                .map_err(|e| CallError::Failed(anyhow::anyhow!("read gh: {e:#}")))?;
            return self.call_scheme_extract(&url, fragment).await;
        }
        if let Some(rest) = path.strip_prefix("npm://") {
            let (url, fragment) = web::npm_url(rest)
                .map_err(|e| CallError::Failed(anyhow::anyhow!("read npm: {e:#}")))?;
            return self.call_scheme_extract(&url, fragment).await;
        }
        if let Some(rest) = path.strip_prefix("arxiv://") {
            let (url, fragment) = web::arxiv_url(rest)
                .map_err(|e| CallError::Failed(anyhow::anyhow!("read arxiv: {e:#}")))?;
            return self.call_scheme_extract(&url, fragment).await;
        }
        if let Some(rest) = path.strip_prefix("renderdoc://") {
            return self.call_renderdoc_url(rest, ctx).await;
        }
        if let Some(rest) = path.strip_prefix("git://") {
            return self.call_git_split_url(rest, ctx).await;
        }
        if let Some(scheme) = scheme_of(&path) {
            return Err(CallError::Failed(anyhow::anyhow!(
                "read: `{scheme}://` is not a served scheme (`web://`, `gh://`, `npm://`, `arxiv://`, `renderdoc://`, `git://`)"
            )));
        }
        let offset = arg_opt_u64(args, "read", "offset")?.unwrap_or(1) as usize;
        let limit = arg_opt_u64(args, "read", "limit")?.unwrap_or(0) as usize;
        let page = fs::read_text(&resolve_path(ctx, &path), offset, limit)
            .map_err(|e| anyhow::anyhow!("read: {e}"))?;
        Ok(serde_json::json!({
            "lines": page.lines,
            "start_line": page.start_line,
            "total_lines": page.total_lines,
            "truncated_before": page.truncated_before,
            "truncated_after": page.truncated_after,
        }))
    }

    /// `web_search` over the enrolled (or default) spec. Gate-first on
    /// `web_search:{q}` — broad `web_search` rules cover every query via
    /// the `rule:` prefix match; narrow rules pin single queries.
    async fn call_web_search(
        &self,
        args: &serde_json::Value,
        ctx: &ToolCallContext,
    ) -> Result<serde_json::Value, CallError> {
        const TOOL: &str = "web_search";
        let q = arg_str(args, TOOL, "q")?;
        ctx.gate
            .check(&format!("{TOOL}:{q}"))
            .map_err(CallError::Denied)?;
        let page = arg_opt_u64(args, TOOL, "page")?.unwrap_or(1);
        let page: u32 = page
            .try_into()
            .map_err(|_| anyhow::anyhow!("{TOOL}: `page` too large"))?;
        let safe = arg_opt_u64(args, TOOL, "safe")?.unwrap_or(1);
        let safe: u8 = safe
            .try_into()
            .map_err(|_| anyhow::anyhow!("{TOOL}: `safe` must be 0, 1, or 2"))?;
        let lang = arg_opt_str(args, "lang");
        let spec = self.web_spec().await;
        let out = web::search(&spec, &q, page, safe, lang.as_deref())
            .await
            .map_err(|e| web_failed(TOOL, e))?;
        serde_json::to_value(out)
            .map_err(|e| CallError::Failed(anyhow::anyhow!("{TOOL}: shape failed: {e}")))
    }

    /// `web_extract` over the enrolled (or default) spec. Gate-first on
    /// `web_extract:{url}` (same broad/narrow rule shape as search).
    async fn call_web_extract(
        &self,
        args: &serde_json::Value,
        ctx: &ToolCallContext,
    ) -> Result<serde_json::Value, CallError> {
        const TOOL: &str = "web_extract";
        let url = arg_str(args, TOOL, "url")?;
        ctx.gate
            .check(&format!("{TOOL}:{url}"))
            .map_err(CallError::Denied)?;
        self.call_web_extract_url(&url).await
    }

    /// `ask` structured picker (Epic Q, chunk 122): validate shape
    /// (non-empty question, 1–8 options, unique non-empty labels, at
    /// most one recommended), gate `ask:{question}`, resolve via the
    /// prompter. `Selected` labels are re-checked against the offered
    /// set (a rogue prompter cannot inject); `Dismissed` fails closed
    /// as `Denied` (loop-visible "user unavailable", never a stall).
    async fn call_ask(
        &self,
        args: &serde_json::Value,
        ctx: &ToolCallContext,
    ) -> Result<serde_json::Value, CallError> {
        const TOOL: &str = "ask";
        let question = arg_str(args, TOOL, "question")?;
        if question.trim().is_empty() {
            return Err(CallError::Failed(anyhow::anyhow!(
                "{TOOL}: `question` must be non-empty"
            )));
        }
        let raw = args
            .get("options")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow::anyhow!("{TOOL}: `options` must be a non-empty array"))?;
        if raw.is_empty() || raw.len() > hooks::MAX_ASK_OPTIONS {
            return Err(CallError::Failed(anyhow::anyhow!(
                "{TOOL}: want 1..={} options (got {})",
                hooks::MAX_ASK_OPTIONS,
                raw.len()
            )));
        }
        let mut options = Vec::with_capacity(raw.len());
        let mut recommended = 0;
        for (i, o) in raw.iter().enumerate() {
            let label = o.get("label").and_then(|v| v.as_str()).unwrap_or("").trim();
            if label.is_empty() {
                return Err(CallError::Failed(anyhow::anyhow!(
                    "{TOOL}: option {i} needs a non-empty `label`"
                )));
            }
            if options.iter().any(|p: &PickOption| p.label == label) {
                return Err(CallError::Failed(anyhow::anyhow!(
                    "{TOOL}: duplicate label {label:?}"
                )));
            }
            let rec = o
                .get("recommended")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            recommended += usize::from(rec);
            options.push(PickOption {
                label: label.to_string(),
                hint: o.get("hint").and_then(|v| v.as_str()).map(str::to_string),
                recommended: rec,
            });
        }
        if recommended > 1 {
            return Err(CallError::Failed(anyhow::anyhow!(
                "{TOOL}: at most one `recommended` (got {recommended})"
            )));
        }
        let multi = args.get("multi").and_then(|v| v.as_bool()).unwrap_or(false);
        let allow_other = args
            .get("allow_other")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        ctx.gate
            .check(&format!("{TOOL}:{question}"))
            .map_err(CallError::Denied)?;
        let offered: Vec<String> = options.iter().map(|o| o.label.clone()).collect();
        let req = PickRequest {
            tool: TOOL.to_string(),
            question: question.clone(),
            options,
            multi,
            allow_other,
        };
        match self.ask.resolve_pick(req).await {
            PickOutcome::Selected(labels) => {
                if labels.is_empty() {
                    return Err(CallError::Failed(anyhow::anyhow!(
                        "{TOOL}: prompter selected nothing"
                    )));
                }
                if !multi && labels.len() > 1 {
                    return Err(CallError::Failed(anyhow::anyhow!(
                        "{TOOL}: single pick admitted several labels"
                    )));
                }
                for label in &labels {
                    if !offered.contains(label) {
                        return Err(CallError::Failed(anyhow::anyhow!(
                            "{TOOL}: prompter picked unoffered {label:?}"
                        )));
                    }
                }
                Ok(serde_json::json!({"selected": labels, "other": serde_json::Value::Null}))
            }
            PickOutcome::Other(text) => {
                if !allow_other {
                    return Err(CallError::Failed(anyhow::anyhow!(
                        "{TOOL}: free text refused (`allow_other` false)"
                    )));
                }
                if text.trim().is_empty() {
                    return Err(CallError::Failed(anyhow::anyhow!(
                        "{TOOL}: prompter returned empty text"
                    )));
                }
                Ok(serde_json::json!({"selected": [], "other": text}))
            }
            PickOutcome::Dismissed { reason } => Err(CallError::Denied(Denied {
                call: format!("{TOOL}:{question}"),
                rule: reason,
            })),
        }
    }

    /// Atomic-split proposal (chunk 128, T2): propose-only groups over a
    /// live repo via `split::plan_live`. Gate FIRST (pre-spawn — a denial
    /// leaves zero side effects, F3a contract). Split faults (non-repo,
    /// cycles) surface as failures, never denials (typed-boundary
    /// precedent: denials are permissions only).
    async fn call_split_plan(
        &self,
        args: &serde_json::Value,
        ctx: &ToolCallContext,
    ) -> Result<serde_json::Value, CallError> {
        const TOOL: &str = "split_plan";
        let repo_raw = arg_str(args, TOOL, "repo")?;
        let mut deps = Vec::new();
        if let Some(arr) = args.get("deps") {
            let list = arr.as_array().ok_or_else(|| {
                anyhow::anyhow!("{TOOL}: `deps` must be an array of {{before, after}}")
            })?;
            for (i, d) in list.iter().enumerate() {
                let before = d
                    .get("before")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim();
                let after = d.get("after").and_then(|v| v.as_str()).unwrap_or("").trim();
                if before.is_empty() || after.is_empty() {
                    return Err(CallError::Failed(anyhow::anyhow!(
                        "{TOOL}: deps[{i}] needs non-empty `before` + `after`"
                    )));
                }
                deps.push((before.to_string(), after.to_string()));
            }
        }
        ctx.gate
            .check(&format!("{TOOL}:{repo_raw}"))
            .map_err(CallError::Denied)?;
        // "." is the workdir spelling (fs-tool precedent: relative
        // paths join the workdir; bare "." must not leak a trailing
        // `/.` into the reported repo).
        let repo = if repo_raw.trim() == "." {
            ctx.workdir.clone()
        } else {
            resolve_path(ctx, &repo_raw)
        };
        let groups = split::plan_live(&repo, &deps)
            .await
            .map_err(CallError::Failed)?;
        Ok(split::groups_json(&repo, &groups))
    }

    /// `git://<repo>/split` preview (chunk 128, T2): split-plan JSON
    /// over a live repo, no-deps floor. Already gated by the caller
    /// (`read:{path}` — the web:// precedent). Only the `/split` suffix
    /// is served; every other `git://` shape fails closed naming the
    /// path (P2 precedent).
    async fn call_git_split_url(
        &self,
        rest: &str,
        ctx: &ToolCallContext,
    ) -> Result<serde_json::Value, CallError> {
        let repo_raw = rest.strip_suffix("/split").ok_or_else(|| {
            anyhow::anyhow!("read: git:// serves `<repo>/split` only (got `{rest}`)")
        })?;
        if repo_raw.trim().is_empty() {
            return Err(CallError::Failed(anyhow::anyhow!(
                "read: git:// serves `<repo>/split` only (got `{rest}`)"
            )));
        }
        let repo = resolve_path(ctx, repo_raw);
        let groups = split::plan_live(&repo, &[])
            .await
            .map_err(CallError::Failed)?;
        Ok(split::groups_json(&repo, &groups))
    }

    /// Shared extract tail (already gated by the caller — `web_extract`
    /// gates `web_extract:{url}`, `read` gates `read:{path}`).
    async fn call_web_extract_url(&self, url: &str) -> Result<serde_json::Value, CallError> {
        let spec = self.web_spec().await;
        let out = web::extract(&spec, url)
            .await
            .map_err(|e| web_failed("web_extract", e))?;
        serde_json::to_value(out)
            .map_err(|e| CallError::Failed(anyhow::anyhow!("web_extract: shape failed: {e}")))
    }

    /// Scheme-read tail (P2 chunk 121): extract + envelope augmentation
    /// (`resolved_url` always — the mapping is the feature; `fragment`
    /// only when given — fragments never ride HTTP, the echo is the
    /// preservation). Already gated by the caller.
    async fn call_scheme_extract(
        &self,
        url: &str,
        fragment: Option<String>,
    ) -> Result<serde_json::Value, CallError> {
        let mut out = self.call_web_extract_url(url).await?;
        if let Some(obj) = out.as_object_mut() {
            obj.insert(
                "resolved_url".into(),
                serde_json::Value::String(url.to_string()),
            );
            if let Some(f) = fragment {
                obj.insert("fragment".into(), serde_json::Value::String(f));
            }
        }
        Ok(out)
    }

    /// `renderdoc://<capture>[#event]` (P2 chunk 121): no event → draws
    /// list, event → pipeline state — straight delegation to the N1b
    /// introspect machinery (live-proven at 113; its own capture-scoped
    /// gates hold under the `read:{path}` grant).
    async fn call_renderdoc_url(
        &self,
        rest: &str,
        ctx: &ToolCallContext,
    ) -> Result<serde_json::Value, CallError> {
        let (capture, event) = match rest.split_once('#') {
            Some((c, e)) if !e.is_empty() => {
                let id: u64 = e.parse().map_err(|_| {
                    CallError::Failed(anyhow::anyhow!(
                        "read: renderdoc:// wants `#event-id` (got `{e}`)"
                    ))
                })?;
                (c, Some(id))
            }
            Some((c, _)) => (c, None),
            None => (rest, None),
        };
        if capture.trim().is_empty() {
            return Err(CallError::Failed(anyhow::anyhow!(
                "read: renderdoc:// wants a capture path (`renderdoc://<capture>[#event]`)"
            )));
        }
        match event {
            Some(id) => {
                self.call_rdoc_introspect(
                    "rdoc_state",
                    &serde_json::json!({"capture": capture, "event": id}),
                    ctx,
                )
                .await
            }
            None => {
                self.call_rdoc_introspect(
                    "rdoc_draws",
                    &serde_json::json!({"capture": capture}),
                    ctx,
                )
                .await
            }
        }
    }
}

/// Outcome of one stopped-or-ended wait (chunk 107, M3).
enum StopOutcome {
    Stopped { reason: String, thread: u64 },
    Ended,
}

/// Parse `dap_start` breakpoints: `[{path, lines: [1-based]}]` — every
/// field named on failure (the F3a arg-error contract).
fn parse_dap_breakpoints(
    args: &serde_json::Value,
    tool: &str,
) -> anyhow::Result<Vec<(String, Vec<u32>)>> {
    let Some(list) = args.get("breakpoints") else {
        return Ok(Vec::new());
    };
    let list = list
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("{tool}: `breakpoints` must be an array"))?;
    let mut out = Vec::new();
    for (i, entry) in list.iter().enumerate() {
        let path = entry
            .get("path")
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!("{tool}: `breakpoints[{i}].path` must be a non-empty string")
            })?
            .to_string();
        let lines_raw = entry
            .get("lines")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| anyhow::anyhow!("{tool}: `breakpoints[{i}].lines` must be an array"))?;
        if lines_raw.is_empty() {
            anyhow::bail!("{tool}: `breakpoints[{i}].lines` must be non-empty");
        }
        let mut lines = Vec::new();
        for line in lines_raw {
            let n = line.as_u64().ok_or_else(|| {
                anyhow::anyhow!("{tool}: `breakpoints[{i}].lines` must be non-negative integers")
            })?;
            lines.push(
                u32::try_from(n).map_err(|_| {
                    anyhow::anyhow!("{tool}: `breakpoints[{i}].lines` out of range")
                })?,
            );
        }
        out.push((path, lines));
    }
    Ok(out)
}

/// Required string arg, with a model-readable error naming tool + field.
fn arg_str(args: &serde_json::Value, tool: &str, field: &str) -> anyhow::Result<String> {
    args.get(field)
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("{tool}: `{field}` must be a non-empty string"))
}

fn arg_opt_str(args: &serde_json::Value, field: &str) -> Option<String> {
    args.get(field)
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

/// Optional JSON integer arg; floats/strings refuse with the field named.
fn arg_opt_u64(args: &serde_json::Value, tool: &str, field: &str) -> anyhow::Result<Option<u64>> {
    match args.get(field) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Number(n)) => n
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("{tool}: `{field}` must be a non-negative integer"))
            .map(Some),
        Some(_) => anyhow::bail!("{tool}: `{field}` must be a non-negative integer"),
    }
}

/// Required 0-based position arg for the LSP query tools (chunk 100):
/// present + non-negative integer + u32-range, else the field is named.
fn arg_pos(args: &serde_json::Value, tool: &str, field: &str) -> anyhow::Result<u32> {
    let n = arg_opt_u64(args, tool, field)?
        .ok_or_else(|| anyhow::anyhow!("{tool}: `{field}` must be a non-negative integer"))?;
    u32::try_from(n).map_err(|_| anyhow::anyhow!("{tool}: `{field}` out of range"))
}

/// Resolve a tool path: relative joins the call workdir, absolute passes
/// through (documented gap — no sandbox; the Gate owns dangerous ops).
fn resolve_path(ctx: &ToolCallContext, raw: &str) -> PathBuf {
    resolve_tool_path(&ctx.workdir, raw)
}

/// [`resolve_path`] over a bare workdir: the file a tool call's path
/// argument names (harness readers re-open exactly what `write` wrote).
pub fn resolve_tool_path(workdir: &Path, raw: &str) -> PathBuf {
    let raw = raw.trim();
    // Normalize Git Bash style drive letters: "/c/project/..." -> "C:/project/..."
    let bytes = raw.as_bytes();
    if bytes.len() >= 3 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() && bytes[2] == b'/' {
        let drive = (bytes[1] as char).to_ascii_uppercase();
        let rest = &raw[3..];
        return PathBuf::from(format!("{drive}:/{rest}"));
    }
    let p = Path::new(raw);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        // Strip leading slashes to prevent Windows Path::join from resetting to drive root
        let trimmed = raw.trim_start_matches(['/', '\\']);
        workdir.join(trimmed)
    }
}

/// A `scheme://` prefix that is NOT a file path (P1b chunk 120).
/// Windows drive letters (`C:/x`) never contain `://`; single-char or
/// non-alphanumeric prefixes fall through to the file path as before.
fn scheme_of(path: &str) -> Option<&str> {
    let (scheme, _) = path.split_once("://")?;
    (!scheme.is_empty() && scheme.len() > 1 && scheme.chars().all(|c| c.is_ascii_alphanumeric()))
        .then_some(scheme)
}

/// Map a classified web error into the registry failure shape (P1b).
fn web_failed(tool: &str, e: web::WebError) -> CallError {
    CallError::Failed(anyhow::anyhow!("{tool}: {e}"))
}

async fn call_write(
    args: &serde_json::Value,
    ctx: &ToolCallContext,
) -> Result<serde_json::Value, CallError> {
    let path = arg_str(args, "write", "path")?;
    let content = args
        .get("content")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("write: `content` must be a string"))?;
    ctx.gate
        .check(&format!("write:{path}"))
        .map_err(CallError::Denied)?;
    fs::write_text(&resolve_path(ctx, &path), content)
        .map_err(|e| anyhow::anyhow!("write: {e}"))?;
    Ok(serde_json::json!({ "bytes_written": content.len() }))
}

async fn call_grep(
    args: &serde_json::Value,
    ctx: &ToolCallContext,
    hidden: &[PathBuf],
) -> Result<serde_json::Value, CallError> {
    let pattern = arg_str(args, "grep", "pattern")?;
    ctx.gate
        .check(&format!("grep:{pattern}"))
        .map_err(CallError::Denied)?;
    let dir = arg_opt_str(args, "dir")
        .map(|d| resolve_path(ctx, &d))
        .unwrap_or_else(|| ctx.workdir.clone());
    let max = arg_opt_u64(args, "grep", "max_hits")?.unwrap_or(50) as usize;
    let (hits, capped) = fs::grep_files_hiding(&pattern, &dir, max.max(1), hidden)
        .map_err(|e| anyhow::anyhow!("grep: {e}"))?;
    Ok(serde_json::json!({
        "hits": hits
            .iter()
            .map(|h| serde_json::json!({
                "path": h.path.display().to_string(),
                "line_number": h.line_number,
                "line": h.line,
            }))
            .collect::<Vec<_>>(),
        "capped": capped,
    }))
}

async fn call_glob(
    args: &serde_json::Value,
    ctx: &ToolCallContext,
    hidden: &[PathBuf],
) -> Result<serde_json::Value, CallError> {
    let pattern = arg_str(args, "glob", "pattern")?;
    ctx.gate
        .check(&format!("glob:{pattern}"))
        .map_err(CallError::Denied)?;
    let dir = arg_opt_str(args, "dir")
        .map(|d| resolve_path(ctx, &d))
        .unwrap_or_else(|| ctx.workdir.clone());
    let paths =
        fs::glob_files_hiding(&pattern, &dir, hidden).map_err(|e| anyhow::anyhow!("glob: {e}"))?;
    Ok(serde_json::json!({
        "paths": paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
    }))
}

async fn call_bash(
    args: &serde_json::Value,
    ctx: &ToolCallContext,
    spill: &output::SpillConfig,
) -> Result<serde_json::Value, CallError> {
    let cmd = arg_str(args, "bash", "cmd")?;
    ctx.gate
        .check(&format!("bash:{cmd}"))
        .map_err(CallError::Denied)?;
    let secs = arg_opt_u64(args, "bash", "timeout_secs")?
        .unwrap_or(120)
        .min(600);
    // The call's workdir, not the process cwd (docket #8): under `--repo`
    // the fixer's commands must see the worktree its file tools edit.
    // Docket #9: large streams spill under that workdir's harness dir.
    let run = exec::run_bash_streamed(
        &cmd,
        std::time::Duration::from_secs(secs),
        &ctx.cancel,
        &ctx.workdir,
        Some(spill),
    )
    .await;
    match run.status {
        Ok(exit_code) => Ok(bash_result(&run.stdout, &run.stderr, Some(exit_code))),
        // Docket #9: a timeout keeps what the command printed before the
        // kill (still an error — the command did not finish). Nothing
        // printed = the pre-#9 message exactly.
        Err(e @ exec::ExecError::TimedOut(_))
            if !(run.stdout.is_empty() && run.stderr.is_empty()) =>
        {
            let partial = bash_result(&run.stdout, &run.stderr, None);
            let partial =
                serde_json::to_string_pretty(&partial).unwrap_or_else(|_| partial.to_string());
            Err(anyhow::anyhow!("bash: {e}; output before the kill:\n{partial}").into())
        }
        Err(e) => Err(anyhow::anyhow!("bash: {e}").into()),
    }
}

/// `bash` result JSON: `{stdout, stderr, exit_code}` exactly as before
/// docket #9 when nothing spilled; a spilled stream's text is its digest,
/// with `<stream>_spill: {path, total_bytes, total_lines[, error]}` beside.
fn bash_result(
    stdout: &output::CapturedStream,
    stderr: &output::CapturedStream,
    exit_code: Option<i32>,
) -> serde_json::Value {
    let mut v = serde_json::json!({
        "stdout": stdout.text(),
        "stderr": stderr.text(),
    });
    if let Some(code) = exit_code {
        v["exit_code"] = serde_json::Value::from(code);
    }
    for (key, stream) in [("stdout_spill", stdout), ("stderr_spill", stderr)] {
        if let Some(spill) = stream.spill_json() {
            v[key] = spill;
        }
    }
    v
}

/// The V1 tool floor (Epic B target): read/write/edit/grep/glob/bash.
/// Chunk 1 registers descriptors only; executors arrive with Epic B.
pub fn v1_tool_floor() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "read".into(),
            description: "Read file or internal :// URL".into(),
        },
        ToolDef {
            name: "write".into(),
            description: "Write file (LSP write-through when available)".into(),
        },
        ToolDef {
            name: "edit".into(),
            description: "Hashline line-anchored patch edit".into(),
        },
        ToolDef {
            name: "grep".into(),
            description: "Ripgrep-class in-process search".into(),
        },
        ToolDef {
            name: "glob".into(),
            description: "Glob with .gitignore-aware walker".into(),
        },
        ToolDef {
            name: "bash".into(),
            description: "Shell exec (non-interactive + PTY)".into(),
        },
        // L-actuation 3A (chunk 100): read-only LSP queries for the
        // fixer allowlist. Reviewers never see them (their spawns keep
        // `tools = []`); fixers opt in per call site (`--tools`).
        ToolDef {
            name: "lsp_hover".into(),
            description: "LSP hover at a position (read-only; needs a registered server)".into(),
        },
        ToolDef {
            name: "lsp_definition".into(),
            description:
                "LSP go-to-definition at a position (read-only; needs a registered server)".into(),
        },
        ToolDef {
            name: "lsp_references".into(),
            description: "LSP find-references at a position (read-only; needs a registered server)"
                .into(),
        },
        // M3 debugger surface (chunk 107): session tools for RUNTIME
        // questions. The bash-vs-DAP instruction lives HERE (tool
        // descriptions reach the model through schemas — the
        // TOOLS_PREAMBLE block is eval-pinned and stays untouched).
        ToolDef {
            name: "dap_start".into(),
            description: "Run a program under a debugger to its first breakpoint and STOP there. PREFER over bash when the question is about RUNTIME state (wrong variable values, crashes, exceptions, hangs): the debugger shows actual values where bash only shows outputs. Use bash for static questions (file contents, running commands, logs). Returns a session id; ALWAYS dap_disconnect it when done.".into(),
        },
        ToolDef {
            name: "dap_continue".into(),
            description: "Resume a stopped debug session to its next stop (breakpoint or program end). Use after dap_inspect/dap_evaluate answered the current stop.".into(),
        },
        ToolDef {
            name: "dap_inspect".into(),
            description: "Read a stopped session's threads, stack frames, scopes and locals in ONE call (capped). Prefer one inspect over guessing values; NEVER invent variable contents.".into(),
        },
        ToolDef {
            name: "dap_evaluate".into(),
            description: "Evaluate an expression in a stopped session's frame (a watch: variables, arithmetic). Side-effecting calls DISCOURAGED — they mutate the debuggee under test.".into(),
        },
        ToolDef {
            name: "dap_disconnect".into(),
            description: "End a debug session and reap its adapter. ALWAYS call when done debugging; sessions never outlive the task.".into(),
        },
        // N1a graphics capture (chunk 112): PREFER over guessing GPU
        // state — a headed capture replays headless for draw/state
        // inspection (N1b). The target must present frames.
        ToolDef {
            name: "rdoc_capture".into(),
            description: "Capture a presenting graphics app under RenderDoc to a .rdc file (headless replay + thumbnail source). Use for GPU-state questions; bash covers static questions. Returns the newest .rdc path.".into(),
        },
        // N1b introspection (chunk 113): headed replay queries over
        // captures. Draws first (event ids feed state), then state,
        // then diff; user scripts last (gated on their own path).
        ToolDef {
            name: "rdoc_draws".into(),
            description: "List GPU actions (draws, clears, markers) in a RenderDoc capture with event ids and counts. Use before rdoc_state; event ids feed it.".into(),
        },
        ToolDef {
            name: "rdoc_state".into(),
            description: "Read pipeline state (topology, viewports, vertex inputs, outputs) at one frame event in a capture. Needs an event id from rdoc_draws.".into(),
        },
        ToolDef {
            name: "rdoc_diff".into(),
            description: "Diff two captures' draw structure (added/removed events by id, flags, counts). Opaque ids are excluded by design — self-diffs of deterministic captures read empty.".into(),
        },
        ToolDef {
            name: "rdoc_script".into(),
            description: "Run a Python analysis script under RenderDoc's embedded runtime for custom GPU questions. The script MUST write JSON to the RDOC_OUT env path and quit the UI or the call times out. Output echoed (32KB cap).".into(),
        },
        // P1b web research (chunk 120): serica-backed, observe-first.
        // External facts and docs live here; NEVER send secrets outward.
        ToolDef {
            name: "web_search".into(),
            description: "Search the web (serica/DuckDuckGo) for external facts, docs, and prior art. PREFER over guessing APIs, versions, or error meanings. total_results is the page-batch count, NOT a corpus total.".into(),
        },
        ToolDef {
            name: "web_extract".into(),
            description: "Extract a web page as markdown (serica, ads stripped, 50KB cap). PREFER over quoting a URL unseen; read() serves web:// URLs through the same path.".into(),
        },
        // Q structured picker (chunk 122): interrupt-worthy ambiguity
        // only — a turn spent asking is a turn not acting. NEVER ask
        // about secrets, and never ask when a documented default exists.
        ToolDef {
            name: "ask".into(),
            description: "Ask the operator to resolve genuine ambiguity: one question plus options (one may be marked recommended). Use ONLY when blocked on a human judgment call; prefer deciding from context, docs, or code. Without an attached human the call is denied — phrase calls so denial reads as proceed-with-defaults.".into(),
        },
        // T atomic-split proposal (chunk 128): groups are data, commits
        // stay operator-owned — the tool never writes history.
        ToolDef {
            name: "split_plan".into(),
            description: "Propose atomic-commit groups for a dirty git working tree (same-file clusters, explicit-dep order, cycle refusal). PROPOSE-ONLY: returns groups as data, never commits. Use when unrelated changes must not share a commit.".into(),
        },
    ]
}

/// Permission decision: deny-rules beat allow on the same prefix (Epic B5).
/// Thin delegate over [`Gate`] — the gate owns the semantics, this keeps the
/// chunk-1 call shape working.
pub fn is_allowed(tool_call: &str, allow: &[String], deny: &[String]) -> bool {
    Gate::new(allow.to_vec(), deny.to_vec())
        .check(tool_call)
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_rejects_duplicates() {
        let mut r = Registry::new();
        r.register(ToolDef {
            name: "read".into(),
            description: "r".into(),
        })
        .unwrap();
        assert!(r
            .register(ToolDef {
                name: "read".into(),
                description: "r2".into()
            })
            .is_err());
    }

    #[test]
    fn v1_floor_has_twentythree_tools() {
        assert_eq!(v1_tool_floor().len(), 23);
    }

    #[test]
    fn deny_beats_allow_on_same_prefix() {
        let allow = vec!["bash".to_string()];
        let deny = vec!["bash:rm".to_string()];
        assert!(!is_allowed("bash:rm -rf /", &allow, &deny));
        assert!(is_allowed("bash:ls", &allow, &deny));
    }

    #[test]
    fn empty_allow_means_allow_all() {
        assert!(is_allowed("read:foo", &[], &[]));
    }

    // ---- Epic F3a: callable registry ----
    use tokio_util::sync::CancellationToken;

    fn ctx_for(dir: &tempfile::TempDir) -> ToolCallContext {
        ToolCallContext::new(
            dir.path().to_path_buf(),
            Gate::new(vec![], vec![]),
            CancellationToken::new(),
        )
    }

    fn denied_ctx_for(dir: &tempfile::TempDir, deny: &[&str]) -> ToolCallContext {
        ToolCallContext::new(
            dir.path().to_path_buf(),
            Gate::new(vec![], deny.iter().map(|s| s.to_string()).collect()),
            CancellationToken::new(),
        )
    }

    #[tokio::test]
    async fn call_error_keeps_denied_typed_and_text() {
        use crate::CallError;
        let r = registry::v1_registry();
        let d = tempfile::tempdir().unwrap();
        let deny_ctx = denied_ctx_for(&d, &["bash"]);
        // Denial stays a matched variant (bench taxonomy reads the type,
        // never message text) with the model-readable text preserved.
        let err = r
            .call("bash", serde_json::json!({"cmd": "echo hi"}), &deny_ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, CallError::Denied(_)), "{err:?}");
        assert!(err.to_string().contains("permission denied"), "{err}");
        // Execution failures stay Failed with their text forwarded.
        let err = r
            .call("frobnicate", serde_json::json!({}), &ctx_for(&d))
            .await
            .unwrap_err();
        assert!(matches!(err, CallError::Failed(_)), "{err:?}");
        assert!(err.to_string().contains("frobnicate"), "{err}");
    }

    // ---- Epic J1: hooks ----
    #[tokio::test]
    async fn hook_pre_deny_blocks_with_zero_side_effects() {
        let mut r = registry::v1_registry();
        r.add_hook(|event| match event {
            crate::HookEvent::PreToolUse { name, .. } if name == "write" => {
                crate::HookDecision::Deny {
                    reason: "test-hook".to_string(),
                }
            }
            _ => crate::HookDecision::Allow,
        });
        let d = tempfile::tempdir().unwrap();
        let err = r
            .call(
                "write",
                serde_json::json!({"path": "x.txt", "content": "hi"}),
                &ctx_for(&d),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, CallError::Denied(_)), "{err:?}");
        assert!(err.to_string().contains("hook: test-hook"), "{err}");
        assert!(
            !d.path().join("x.txt").exists(),
            "denied write must not touch disk"
        );
    }

    #[tokio::test]
    async fn hook_post_observes_ok_and_err() {
        use std::sync::{Arc, Mutex};
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut r = registry::v1_registry();
        let seen_c = Arc::clone(&seen);
        r.add_hook(move |event| {
            if let crate::HookEvent::PostToolUse { name, ok, .. } = event {
                seen_c.lock().unwrap().push((name.clone(), *ok));
            }
            crate::HookDecision::Allow
        });
        let d = tempfile::tempdir().unwrap();
        let ctx = ctx_for(&d);
        r.call("read", serde_json::json!({"path": "missing.txt"}), &ctx)
            .await
            .unwrap_err();
        std::fs::write(d.path().join("ok.txt"), "hi").unwrap();
        r.call("read", serde_json::json!({"path": "ok.txt"}), &ctx)
            .await
            .unwrap();
        assert_eq!(
            *seen.lock().unwrap(),
            vec![("read".to_string(), false), ("read".to_string(), true)]
        );
    }

    /// Scripted picker for Q tests: answers every pick with the canned
    /// outcome (proves the tool→channel→outcome path, not a renderer).
    #[derive(Debug)]
    struct FakePicker {
        outcome: crate::PickOutcome,
        seen: std::sync::Arc<std::sync::Mutex<Vec<crate::PickRequest>>>,
    }

    #[async_trait::async_trait]
    impl crate::Prompter for FakePicker {
        async fn ask(&self, req: crate::PromptRequest) -> crate::HookDecision {
            let _ = req;
            crate::HookDecision::Allow
        }

        async fn pick(&self, req: crate::PickRequest) -> crate::PickOutcome {
            self.seen.lock().unwrap().push(req);
            self.outcome.clone()
        }
    }

    fn mk_picker(
        outcome: crate::PickOutcome,
    ) -> (
        FakePicker,
        std::sync::Arc<std::sync::Mutex<Vec<crate::PickRequest>>>,
    ) {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        (
            FakePicker {
                outcome,
                seen: std::sync::Arc::clone(&seen),
            },
            seen,
        )
    }

    fn ask_args(question: &str) -> serde_json::Value {
        serde_json::json!({
            "question": question,
            "options": [{"label": "a"}, {"label": "b", "recommended": true}],
        })
    }

    #[tokio::test]
    async fn ask_shape_refusals_name_the_field() {
        let r = registry::v1_registry();
        let d = tempfile::tempdir().unwrap();
        let ctx = ctx_for(&d);
        // Empty question.
        let err = r
            .call(
                "ask",
                serde_json::json!({"question": "  ", "options": [{"label": "a"}]}),
                &ctx,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("`question`"), "{err}");
        // Zero options.
        let err = r
            .call(
                "ask",
                serde_json::json!({"question": "q?", "options": []}),
                &ctx,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("1..="), "{err}");
        // Missing options key.
        let err = r
            .call("ask", serde_json::json!({"question": "q?"}), &ctx)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("`options`"), "{err}");
        // Two recommended.
        let err = r
            .call(
                "ask",
                serde_json::json!({"question": "q?", "options": [{"label": "a", "recommended": true}, {"label": "b", "recommended": true}]}),
                &ctx,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("recommended"), "{err}");
        // Duplicate + empty labels.
        for opts in [
            serde_json::json!([{"label": "a"}, {"label": "a"}]),
            serde_json::json!([{"label": "  "}]),
        ] {
            let err = r
                .call(
                    "ask",
                    serde_json::json!({"question": "q?", "options": opts}),
                    &ctx,
                )
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains("label"), "{err}");
        }
        // Nine options over the cap.
        let many: Vec<serde_json::Value> = (0..9)
            .map(|i| serde_json::json!({"label": format!("o{i}")}))
            .collect();
        let err = r
            .call(
                "ask",
                serde_json::json!({"question": "q?", "options": many}),
                &ctx,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("1..=8"), "{err}");
    }

    /// Fresh git repo with one commit + two dirty files (mirrors
    /// `split::tests::init_repo` — that helper is private to its module;
    /// same argv, stated duplication, not drift).
    fn split_repo() -> tempfile::TempDir {
        use std::process::{Command, Stdio};
        let d = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(d.path())
                    .args(args)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .unwrap()
                    .success(),
                "{args:?}"
            );
        };
        run(&["init", "-b", "main"]);
        run(&["config", "core.autocrlf", "false"]);
        std::fs::write(d.path().join("a.txt"), "one\n").unwrap();
        std::fs::write(d.path().join("b.txt"), "uno\n").unwrap();
        run(&["add", "."]);
        run(&[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-m",
            "seed",
        ]);
        std::fs::write(d.path().join("a.txt"), "one\ntwo\n").unwrap();
        std::fs::write(d.path().join("b.txt"), "uno\ndos\n").unwrap();
        d
    }

    #[test]
    fn split_plan_schema_pins_repo_required() {
        let s = Registry::schema("split_plan").expect("split_plan has a schema");
        assert_eq!(s["required"], serde_json::json!(["repo"]));
    }

    #[tokio::test]
    async fn split_plan_gate_denied_before_spawn() {
        // Denial precedes the spawn by construction (gate-first arm);
        // the typed Denied (not Failed) is the observable proof.
        let r = registry::v1_registry();
        let d = tempfile::tempdir().unwrap();
        let err = r
            .call(
                "split_plan",
                serde_json::json!({"repo": "."}),
                &denied_ctx_for(&d, &["split_plan"]),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, CallError::Denied(_)), "{err:?}");
    }

    #[tokio::test]
    async fn split_plan_live_temp_repo_two_groups() {
        let d = split_repo();
        let r = registry::v1_registry();
        let out = r
            .call("split_plan", serde_json::json!({"repo": "."}), &ctx_for(&d))
            .await
            .unwrap();
        assert_eq!(out["groups"].as_array().unwrap().len(), 2);
        assert!(out["groups"].as_array().unwrap().iter().all(|g| g["files"]
            .as_array()
            .unwrap()
            .len()
            == 1));
    }

    #[tokio::test]
    async fn split_plan_bad_deps_refused() {
        let r = registry::v1_registry();
        let d = tempfile::tempdir().unwrap();
        let err = r
            .call(
                "split_plan",
                serde_json::json!({"repo": ".", "deps": [{"before": "a.txt"}]}),
                &ctx_for(&d),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("deps[0]"), "{err}");
    }

    #[tokio::test]
    async fn read_git_split_preview_and_bare_fails_closed() {
        let d = split_repo();
        let r = registry::v1_registry();
        let ctx = ctx_for(&d);
        let repo = d.path().display().to_string();
        let out = r
            .call(
                "read",
                serde_json::json!({"path": format!("git://{repo}/split")}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(out["groups"].as_array().unwrap().len(), 2);
        // Bare repo (no /split suffix) fails closed naming the shape.
        let err = r
            .call(
                "read",
                serde_json::json!({"path": format!("git://{repo}")}),
                &ctx,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("/split"), "{err}");
        // Gate stays first on the read path too.
        let derr = r
            .call(
                "read",
                serde_json::json!({"path": format!("git://{repo}/split")}),
                &denied_ctx_for(&d, &["read"]),
            )
            .await
            .unwrap_err();
        assert!(matches!(derr, CallError::Denied(_)), "{derr:?}");
    }

    #[tokio::test]
    async fn ask_headless_dismisses_fail_closed() {
        // No prompter attached anywhere: denial names ask-no-prompter,
        // loop-visible, never a stall.
        let r = registry::v1_registry();
        let d = tempfile::tempdir().unwrap();
        let err = r
            .call("ask", ask_args("Ship it?"), &ctx_for(&d))
            .await
            .unwrap_err();
        assert!(matches!(err, CallError::Denied(_)), "{err:?}");
        assert!(err.to_string().contains("ask-no-prompter"), "{err}");
    }

    #[tokio::test]
    async fn ask_picker_selected_multi_other_round_trip() {
        // Single pick passes the label through verbatim.
        let (picker, seen) = mk_picker(crate::PickOutcome::Selected(vec!["b".to_string()]));
        let mut r = registry::v1_registry();
        r.ask.set_prompter(std::sync::Arc::new(picker));
        let d = tempfile::tempdir().unwrap();
        let out = r
            .call("ask", ask_args("Pick one"), &ctx_for(&d))
            .await
            .unwrap();
        assert_eq!(out["selected"], serde_json::json!(["b"]));
        assert!(out["other"].is_null());
        let req = seen.lock().unwrap().pop().unwrap();
        assert_eq!(req.question, "Pick one");
        assert!(!req.multi);
        assert!(req.allow_other);
        // Multi admits several; single rejects several.
        let (picker, _) = mk_picker(crate::PickOutcome::Selected(vec![
            "a".to_string(),
            "b".to_string(),
        ]));
        let mut r = registry::v1_registry();
        r.ask.set_prompter(std::sync::Arc::new(picker));
        let mut args = ask_args("Pick any");
        args["multi"] = serde_json::json!(true);
        let out = r.call("ask", args, &ctx_for(&d)).await.unwrap();
        assert_eq!(out["selected"].as_array().map(Vec::len), Some(2));
        let (picker, _) = mk_picker(crate::PickOutcome::Selected(vec![
            "a".to_string(),
            "b".to_string(),
        ]));
        let mut r = registry::v1_registry();
        r.ask.set_prompter(std::sync::Arc::new(picker));
        let err = r
            .call("ask", ask_args("Pick one"), &ctx_for(&d))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("single pick"), "{err}");
        // Free text rides Other when admitted, refused when not.
        let (picker, _) = mk_picker(crate::PickOutcome::Other("neither: c".to_string()));
        let mut r = registry::v1_registry();
        r.ask.set_prompter(std::sync::Arc::new(picker));
        let out = r
            .call("ask", ask_args("Pick one"), &ctx_for(&d))
            .await
            .unwrap();
        assert_eq!(out["other"], serde_json::json!("neither: c"));
        let (picker, _) = mk_picker(crate::PickOutcome::Other("x".to_string()));
        let mut r = registry::v1_registry();
        r.ask.set_prompter(std::sync::Arc::new(picker));
        let mut args = ask_args("Pick one");
        args["allow_other"] = serde_json::json!(false);
        let err = r
            .call("ask", args, &ctx_for(&d))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("allow_other"), "{err}");
    }

    #[tokio::test]
    async fn ask_rejects_rogue_prompter_labels() {
        // Selections outside the offered set fail closed (no injection).
        let (picker, _) = mk_picker(crate::PickOutcome::Selected(vec!["evil".to_string()]));
        let mut r = registry::v1_registry();
        r.ask.set_prompter(std::sync::Arc::new(picker));
        let d = tempfile::tempdir().unwrap();
        let err = r
            .call("ask", ask_args("Pick one"), &ctx_for(&d))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("unoffered"), "{err}");
        // Dismissal with a prompter attached still denies.
        let (picker, _) = mk_picker(crate::PickOutcome::Dismissed {
            reason: "busy".to_string(),
        });
        let mut r = registry::v1_registry();
        r.ask.set_prompter(std::sync::Arc::new(picker));
        let err = r
            .call("ask", ask_args("Pick one"), &ctx_for(&d))
            .await
            .unwrap_err();
        assert!(matches!(err, CallError::Denied(_)), "{err:?}");
        assert!(err.to_string().contains("busy"), "{err}");
    }

    #[tokio::test]
    async fn ask_gate_denies_and_preserves() {
        let (picker, seen) = mk_picker(crate::PickOutcome::Selected(vec!["a".to_string()]));
        let mut r = registry::v1_registry();
        r.ask.set_prompter(std::sync::Arc::new(picker));
        let d = tempfile::tempdir().unwrap();
        let denied = ToolCallContext::new(
            d.path().to_path_buf(),
            Gate::new(vec![], vec!["ask".to_string()]),
            tokio_util::sync::CancellationToken::new(),
        );
        let err = r
            .call("ask", ask_args("Pick one"), &denied)
            .await
            .unwrap_err();
        assert!(matches!(err, CallError::Denied(_)), "{err:?}");
        assert!(
            seen.lock().unwrap().is_empty(),
            "denial must precede the prompter"
        );
    }

    #[tokio::test]
    async fn script_pre_deny_blocks_through_call() {
        // End-to-end boundary proof: script exit-2 denies with typed Deny
        // + hook-script rule, zero side effects (Windows cmd, see module note).
        let mut r = registry::v1_registry();
        r.apply_script_hooks(&[polymorph_core::config::HookEntry {
            event: "pre".to_string(),
            command: "cmd /C echo no-writes 1>&2 & exit 2".to_string(),
            timeout_secs: None,
        }])
        .unwrap();
        let d = tempfile::tempdir().unwrap();
        let err = r
            .call(
                "write",
                serde_json::json!({"path": "x.txt", "content": "hi"}),
                &ctx_for(&d),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, CallError::Denied(_)), "{err:?}");
        assert!(err.to_string().contains("hook-script: no-writes"), "{err}");
        assert!(!d.path().join("x.txt").exists());
    }

    #[tokio::test]
    async fn script_post_observe_does_not_alter_outcome() {
        // Post scripts observe only: exit-2 post-hook leaves an Ok alone.
        let mut r = registry::v1_registry();
        r.apply_script_hooks(&[polymorph_core::config::HookEntry {
            event: "post".to_string(),
            command: "cmd /C exit 2".to_string(),
            timeout_secs: None,
        }])
        .unwrap();
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("ok.txt"), "hi").unwrap();
        let out = r
            .call("read", serde_json::json!({"path": "ok.txt"}), &ctx_for(&d))
            .await
            .unwrap();
        assert!(out.to_string().contains("hi"), "{out}");
    }

    #[tokio::test]
    async fn hook_stop_fires_with_reason() {
        use std::sync::{Arc, Mutex};
        let stops = Arc::new(Mutex::new(Vec::new()));
        let mut r = registry::v1_registry();
        let stops_c = Arc::clone(&stops);
        r.add_hook(move |event| {
            if let crate::HookEvent::Stop { reason } = event {
                stops_c.lock().unwrap().push(reason.clone());
            }
            crate::HookDecision::Allow
        });
        r.fire_stop("0 rounds".to_string()).await;
        assert_eq!(*stops.lock().unwrap(), vec!["0 rounds".to_string()]);
    }

    // ---- Epic J1: ask-user (chunk 89) ----
    #[derive(Debug)]
    struct FakePrompter {
        allow: bool,
        reason: String,
        asked: std::sync::Arc<std::sync::Mutex<Vec<crate::PromptRequest>>>,
    }

    #[async_trait::async_trait]
    impl crate::Prompter for FakePrompter {
        async fn ask(&self, req: crate::PromptRequest) -> crate::HookDecision {
            self.asked.lock().unwrap().push(req);
            if self.allow {
                crate::HookDecision::Allow
            } else {
                crate::HookDecision::Deny {
                    reason: self.reason.clone(),
                }
            }
        }
    }

    fn asker(question: &str) -> impl Fn(&str, &serde_json::Value) -> Option<String> {
        let question = question.to_string();
        move |_, _| Some(question.clone())
    }

    #[tokio::test]
    async fn ask_no_prompter_denies_fail_closed() {
        // No prompter attached: a firing ask hook denies (never hangs),
        // with zero side effects.
        let mut r = registry::v1_registry();
        r.add_ask_hook(asker("write x.txt?"));
        let d = tempfile::tempdir().unwrap();
        let err = r
            .call(
                "write",
                serde_json::json!({"path": "x.txt", "content": "hi"}),
                &ctx_for(&d),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, CallError::Denied(_)), "{err:?}");
        assert!(err.to_string().contains("ask-no-prompter"), "{err}");
        assert!(!d.path().join("x.txt").exists());
    }

    #[tokio::test]
    async fn ask_prompter_allow_proceeds() {
        let asked = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut r = registry::v1_registry();
        r.add_ask_hook(asker("read ok.txt?"));
        r.set_prompter(std::sync::Arc::new(FakePrompter {
            allow: true,
            reason: String::new(),
            asked: std::sync::Arc::clone(&asked),
        }));
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("ok.txt"), "hi").unwrap();
        let out = r
            .call("read", serde_json::json!({"path": "ok.txt"}), &ctx_for(&d))
            .await
            .unwrap();
        assert!(out.to_string().contains("hi"), "{out}");
        assert_eq!(asked.lock().unwrap().len(), 1);
        assert_eq!(asked.lock().unwrap()[0].tool, "read");
    }

    #[tokio::test]
    async fn ask_prompter_deny_blocks() {
        let mut r = registry::v1_registry();
        r.add_ask_hook(asker("no writes today"));
        r.set_prompter(std::sync::Arc::new(FakePrompter {
            allow: false,
            reason: "operator said no".to_string(),
            asked: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        }));
        let d = tempfile::tempdir().unwrap();
        let err = r
            .call(
                "write",
                serde_json::json!({"path": "x.txt", "content": "hi"}),
                &ctx_for(&d),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, CallError::Denied(_)), "{err:?}");
        assert!(err.to_string().contains("hook: operator said no"), "{err}");
        assert!(!d.path().join("x.txt").exists());
    }

    #[tokio::test]
    async fn sync_deny_wins_before_ask() {
        // A sync pre-deny short-circuits before any ask hook runs (order:
        // sync → script → ask → gate).
        use std::sync::{Arc, Mutex};
        let asked = Arc::new(Mutex::new(false));
        let mut r = registry::v1_registry();
        r.add_hook(|event| match event {
            crate::HookEvent::PreToolUse { name, .. } if name == "write" => {
                crate::HookDecision::Deny {
                    reason: "sync-first".to_string(),
                }
            }
            _ => crate::HookDecision::Allow,
        });
        let asked_c = Arc::clone(&asked);
        r.add_ask_hook(move |_, _| {
            *asked_c.lock().unwrap() = true;
            Some("should never fire".to_string())
        });
        let d = tempfile::tempdir().unwrap();
        let err = r
            .call(
                "write",
                serde_json::json!({"path": "x.txt", "content": "hi"}),
                &ctx_for(&d),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("hook: sync-first"), "{err}");
        assert!(!*asked.lock().unwrap(), "ask must not run after sync deny");
        assert!(!d.path().join("x.txt").exists());
    }

    #[tokio::test]
    async fn ask_denial_posts_observed() {
        // Ask-denials travel the existing post path (telemetry, never
        // control): observers see (name, false).
        use std::sync::{Arc, Mutex};
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut r = registry::v1_registry();
        let seen_c = Arc::clone(&seen);
        r.add_hook(move |event| {
            if let crate::HookEvent::PostToolUse { name, ok, .. } = event {
                seen_c.lock().unwrap().push((name.clone(), *ok));
            }
            crate::HookDecision::Allow
        });
        r.add_ask_hook(asker("deny me"));
        let d = tempfile::tempdir().unwrap();
        r.call(
            "read",
            serde_json::json!({"path": "missing.txt"}),
            &ctx_for(&d),
        )
        .await
        .unwrap_err();
        assert_eq!(*seen.lock().unwrap(), vec![("read".to_string(), false)]);
    }

    #[tokio::test]
    async fn every_registered_tool_dispatches() {
        // Both directions pinned: every descriptor dispatches (arg errors
        // count — only unknown/not-dispatchable fail this test), and every
        // schema'd name is registered (see schemas test).
        let r = registry::v1_registry();
        let d = tempfile::tempdir().unwrap();
        let ctx = ctx_for(&d);
        for name in r.names() {
            let err = r
                .call(&name, serde_json::json!({}), &ctx)
                .await
                .unwrap_err()
                .to_string();
            assert!(
                !err.contains("unknown tool") && !err.contains("not dispatchable"),
                "{name}: {err}"
            );
        }
    }

    #[tokio::test]
    async fn call_unknown_tool_names_available() {
        let r = registry::v1_registry();
        let d = tempfile::tempdir().unwrap();
        let err = r
            .call("frobnicate", serde_json::json!({}), &ctx_for(&d))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("frobnicate"), "{err}");
        assert!(err.contains("read"), "{err}");
    }

    // ---- L-actuation 3A (chunk 100): read-only LSP query tools ----
    #[tokio::test]
    async fn lsp_tools_route_unknown_extensions_soft() {
        // No language route → Ok note, no server needed, no error.
        let r = registry::v1_registry();
        let d = tempfile::tempdir().unwrap();
        for tool in ["lsp_hover", "lsp_definition", "lsp_references"] {
            let out = r
                .call(
                    tool,
                    serde_json::json!({"path": "a.xyz", "line": 0, "character": 0}),
                    &ctx_for(&d),
                )
                .await
                .unwrap();
            assert_eq!(out["result"], serde_json::Value::Null, "{tool}");
            assert!(
                out["note"].as_str().unwrap().contains("no language route"),
                "{tool}: {out}"
            );
        }
    }

    #[tokio::test]
    async fn lsp_tools_note_missing_server_soft() {
        // Routed language, no server registered → Ok note naming the
        // language (fail-soft; the model falls back to plain read).
        let r = registry::v1_registry();
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.rs"), "fn main() {}\n").unwrap();
        let out = r
            .call(
                "lsp_hover",
                serde_json::json!({"path": "a.rs", "line": 0, "character": 3}),
                &ctx_for(&d),
            )
            .await
            .unwrap();
        assert_eq!(out["result"], serde_json::Value::Null);
        assert!(out["note"].as_str().unwrap().contains("rust"), "{out}");
    }

    #[tokio::test]
    async fn lsp_tools_gate_first_before_any_io() {
        // Denied path on a MISSING file: gate-first proves itself — a
        // read-first implementation would fail on the missing file
        // instead of denying.
        use crate::CallError;
        let r = registry::v1_registry();
        let d = tempfile::tempdir().unwrap();
        let ctx = denied_ctx_for(&d, &["lsp_hover:nope.rs"]);
        let err = r
            .call(
                "lsp_hover",
                serde_json::json!({"path": "nope.rs", "line": 0, "character": 0}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, CallError::Denied(_)), "{err}");
        assert!(err.to_string().contains("lsp_hover:nope.rs"), "{err}");
    }

    #[tokio::test]
    async fn lsp_tools_name_missing_and_bad_position_fields() {
        let r = registry::v1_registry();
        let d = tempfile::tempdir().unwrap();
        let ctx = ctx_for(&d);
        let err = r
            .call(
                "lsp_hover",
                serde_json::json!({"line": 0, "character": 0}),
                &ctx,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("`path`"), "{err}");
        let err = r
            .call(
                "lsp_definition",
                serde_json::json!({"path": "a.rs", "line": -1, "character": 0}),
                &ctx,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("`line`"), "{err}");
        let err = r
            .call(
                "lsp_references",
                serde_json::json!({"path": "a.rs", "line": 0, "character": "zero"}),
                &ctx,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("`character`"), "{err}");
    }

    // ---- M3 debugger session tools (chunk 107) ----
    #[test]
    fn dap_launch_args_pin_proven_shapes() {
        // One arm per adapter, byte-pinned to the M2 matrix rows that
        // proved them live — a new adapter adds an arm + its row here.
        assert_eq!(
            dap::launch_args_for("python", "a.py", "w").unwrap(),
            serde_json::json!({"program": "a.py"})
        );
        assert_eq!(
            dap::launch_args_for("go", "m.go", "w").unwrap(),
            serde_json::json!({"program": "m.go", "mode": "debug"})
        );
        assert_eq!(
            dap::launch_args_for("rust", "a.exe", "w").unwrap(),
            serde_json::json!({"program": "a.exe", "cwd": "w"})
        );
        assert_eq!(
            dap::launch_args_for("csharp", "a.dll", "w").unwrap(),
            serde_json::json!({"program": "a.dll"})
        );
        let err = dap::launch_args_for("frobnicate", "a", "w")
            .unwrap_err()
            .to_string();
        assert!(err.contains("python/go/rust/csharp"), "{err}");
    }

    #[tokio::test]
    async fn dap_start_gate_first_before_spawn() {
        // Denied program that does not exist: gate-first proves itself —
        // a resolve-first implementation would fail on the missing file
        // instead of denying (the chunk-100 lsp gate test, same shape).
        use crate::CallError;
        let r = registry::v1_registry();
        let d = tempfile::tempdir().unwrap();
        let ctx = denied_ctx_for(&d, &["dap_start:nope.py"]);
        let err = r
            .call("dap_start", serde_json::json!({"program": "nope.py"}), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, CallError::Denied(_)), "{err}");
        assert!(err.to_string().contains("dap_start:nope.py"), "{err}");
    }

    #[tokio::test]
    async fn dap_session_op_unknown_id_names_it() {
        // No adapter, no files needed: lookup precedes everything and
        // typos stay loud.
        let r = registry::v1_registry();
        let d = tempfile::tempdir().unwrap();
        for op in [
            "dap_continue",
            "dap_inspect",
            "dap_evaluate",
            "dap_disconnect",
        ] {
            let args = if op == "dap_evaluate" {
                serde_json::json!({"session": "session-999", "expression": "x"})
            } else {
                serde_json::json!({"session": "session-999"})
            };
            let err = r
                .call(op, args, &ctx_for(&d))
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains("session-999"), "{op}: {err}");
        }
    }

    #[tokio::test]
    async fn dap_start_without_adapter_fails_closed() {
        // Explicit language bypasses inference (built binaries carry no
        // mappable extension) and reaches the fail-closed spec lookup.
        let r = registry::v1_registry();
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("prog.bin"), b"\x7fELF").unwrap();
        let err = r
            .call(
                "dap_start",
                serde_json::json!({"program": "prog.bin", "language": "python"}),
                &ctx_for(&d),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("no DAP adapter for 'python'"), "{err}");
    }

    #[tokio::test]
    async fn dap_breakpoints_parse_names_fields() {
        let r = registry::v1_registry();
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.py"), "x = 1\n").unwrap();
        let ctx = ctx_for(&d);
        // Shape errors must surface before any adapter is needed (no
        // adapter is enrolled here at all).
        for (bps, needle) in [
            (serde_json::json!("nope"), "must be an array"),
            (serde_json::json!([{"lines": [1]}]), "breakpoints[0].path"),
            (
                serde_json::json!([{"path": "a.py", "lines": []}]),
                "non-empty",
            ),
            (
                serde_json::json!([{"path": "a.py", "lines": [-1]}]),
                "non-negative",
            ),
        ] {
            let err = r
                .call(
                    "dap_start",
                    serde_json::json!({"program": "a.py", "breakpoints": bps}),
                    &ctx,
                )
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains(needle), "{bps}: {err}");
        }
    }

    #[test]
    fn dap_tool_descriptions_carry_the_instruction() {
        // M3's bash-vs-DAP instruction lives in tool descriptions (the
        // preamble is eval-pinned — chunk 39/45). Pin the load-bearing
        // phrases so edits cannot silently drop the guidance.
        let r = registry::v1_registry();
        let desc = |name: &str| r.get(name).unwrap().description.clone();
        let start = desc("dap_start");
        assert!(start.contains("PREFER over bash"), "{start}");
        assert!(start.contains("RUNTIME"), "{start}");
        assert!(start.contains("dap_disconnect"), "{start}");
        assert!(desc("dap_inspect").contains("NEVER invent"));
        assert!(desc("dap_evaluate").contains("DISCOURAGED"));
        assert!(desc("dap_disconnect").contains("ALWAYS"));
        assert!(desc("dap_continue").contains("next stop"));
    }

    #[tokio::test]
    async fn call_write_then_read_relative() {
        let r = registry::v1_registry();
        let d = tempfile::tempdir().unwrap();
        let ctx = ctx_for(&d);
        let out = r
            .call(
                "write",
                serde_json::json!({"path": "sub/f.txt", "content": "a\nb\nc\nd"}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(out["bytes_written"], 7);
        // Relative paths resolve against the call workdir.
        assert!(d.path().join("sub").join("f.txt").exists());
        let page = r
            .call(
                "read",
                serde_json::json!({"path": "sub/f.txt", "offset": 2, "limit": 2}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(page["lines"], serde_json::json!(["b", "c"]));
        assert_eq!(page["start_line"], 2);
        assert_eq!(page["total_lines"], 4);
        assert_eq!(page["truncated_before"], true);
        assert_eq!(page["truncated_after"], true);
    }

    #[tokio::test]
    async fn call_read_missing_and_binary_errors_stay_typed() {
        let r = registry::v1_registry();
        let d = tempfile::tempdir().unwrap();
        let ctx = ctx_for(&d);
        let err = r
            .call("read", serde_json::json!({"path": "ghost.txt"}), &ctx)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("ghost.txt"), "{err}");
        std::fs::write(d.path().join("b.bin"), [0x89, 0x50, 0x00]).unwrap();
        let err = r
            .call("read", serde_json::json!({"path": "b.bin"}), &ctx)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("binary"), "{err}");
    }

    #[tokio::test]
    async fn web_search_empty_q_is_badarg_before_spec() {
        // Validation precedes any I/O: the default (localhost) registry
        // answers BadArg with no server involved.
        let r = registry::v1_registry();
        let d = tempfile::tempdir().unwrap();
        let ctx = ctx_for(&d);
        let err = r
            .call("web_search", serde_json::json!({"q": "  "}), &ctx)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("non-empty"), "{err}");
    }

    #[tokio::test]
    async fn web_search_and_extract_deny_preserves() {
        let r = registry::v1_registry();
        let d = tempfile::tempdir().unwrap();
        let denied_search = ToolCallContext::new(
            d.path().to_path_buf(),
            Gate::new(vec![], vec!["web_search".to_string()]),
            tokio_util::sync::CancellationToken::new(),
        );
        let err = r
            .call(
                "web_search",
                serde_json::json!({"q": "rust"}),
                &denied_search,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, CallError::Denied(_)), "{err}");
        let denied_extract = ToolCallContext::new(
            d.path().to_path_buf(),
            Gate::new(vec![], vec!["web_extract".to_string()]),
            tokio_util::sync::CancellationToken::new(),
        );
        let err = r
            .call(
                "web_extract",
                serde_json::json!({"url": "https://example.com"}),
                &denied_extract,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, CallError::Denied(_)), "{err}");
    }

    #[tokio::test]
    async fn read_web_url_without_server_names_serica() {
        // Unroutable spec: proves wiring + fail-soft message with zero
        // dependence on ambient localhost state.
        let r = registry::v1_registry();
        r.set_web_spec(web::validate("http://127.0.0.1:9", 5).unwrap())
            .await;
        let d = tempfile::tempdir().unwrap();
        let ctx = ctx_for(&d);
        let err = r
            .call(
                "read",
                serde_json::json!({"path": "web://example.com"}),
                &ctx,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("127.0.0.1:9"), "{err}");
    }

    #[tokio::test]
    async fn read_web_bare_host_upgrades_to_https() {
        // Live-caught in 120c (bare host answered INVALID_QUERY):
        // `read web://example.com` fetches `https://example.com`.
        // Also pins the shape decision: web reads return the extract
        // envelope (content/title/url), not paginated lines.
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/extract"))
            .and(query_param("url", "https://example.com"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"success":true,"data":{"url":"https://example.com","title":"T","author":null,"date":null,"sitename":null,"page_type":null,"content":"body","excerpt":null,"timing_ms":5,"cached":false}}"#,
            ))
            .mount(&server)
            .await;
        let r = registry::v1_registry();
        r.set_web_spec(web::validate(&server.uri(), 10).unwrap())
            .await;
        let d = tempfile::tempdir().unwrap();
        let ctx = ctx_for(&d);
        let out = r
            .call(
                "read",
                serde_json::json!({"path": "web://example.com"}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(out["content"].as_str(), Some("body"));
    }

    #[tokio::test]
    async fn read_unknown_scheme_fails_closed_naming_served() {
        let r = registry::v1_registry();
        let d = tempfile::tempdir().unwrap();
        let ctx = ctx_for(&d);
        for path in ["example://x", "foo://bar/baz"] {
            let err = r
                .call("read", serde_json::json!({"path": path}), &ctx)
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains("not a served scheme"), "{path}: {err}");
        }
    }

    #[tokio::test]
    async fn read_gh_resolves_blob_and_echoes_fragment() {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/extract"))
            .and(query_param(
                "url",
                "https://github.com/owner/repo/blob/main/f.rs",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"success":true,"data":{"url":"https://github.com/owner/repo/blob/main/f.rs","title":"T","author":null,"date":null,"sitename":null,"page_type":null,"content":"code","excerpt":null,"timing_ms":5,"cached":false}}"#,
            ))
            .mount(&server)
            .await;
        let r = registry::v1_registry();
        r.set_web_spec(web::validate(&server.uri(), 10).unwrap())
            .await;
        let d = tempfile::tempdir().unwrap();
        let ctx = ctx_for(&d);
        let out = r
            .call(
                "read",
                serde_json::json!({"path": "gh://owner/repo/f.rs@main#L10"}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(out["content"].as_str(), Some("code"));
        assert_eq!(
            out["resolved_url"].as_str(),
            Some("https://github.com/owner/repo/blob/main/f.rs")
        );
        assert_eq!(out["fragment"].as_str(), Some("L10"));
    }

    #[tokio::test]
    async fn read_npm_arxiv_resolve_and_npm_files_refused() {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        for url in [
            "https://www.npmjs.com/package/react",
            "https://ar5iv.org/html/1706.03762",
        ] {
            Mock::given(method("GET"))
                .and(path("/api/v1/extract"))
                .and(query_param("url", url))
                .respond_with(ResponseTemplate::new(200).set_body_string(format!(
                    r#"{{"success":true,"data":{{"url":"{url}","title":"T","author":null,"date":null,"sitename":null,"page_type":null,"content":"c","excerpt":null,"timing_ms":5,"cached":false}}}}"#
                )))
                .mount(&server)
                .await;
        }
        let r = registry::v1_registry();
        r.set_web_spec(web::validate(&server.uri(), 10).unwrap())
            .await;
        let d = tempfile::tempdir().unwrap();
        let ctx = ctx_for(&d);
        let out = r
            .call("read", serde_json::json!({"path": "npm://react"}), &ctx)
            .await
            .unwrap();
        assert_eq!(
            out["resolved_url"].as_str(),
            Some("https://www.npmjs.com/package/react")
        );
        assert!(out.get("fragment").is_none());
        let out = r
            .call(
                "read",
                serde_json::json!({"path": "arxiv://1706.03762"}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(
            out["resolved_url"].as_str(),
            Some("https://ar5iv.org/html/1706.03762")
        );
        // File-level npm has no servable page — refused pre-I/O.
        let err = r
            .call(
                "read",
                serde_json::json!({"path": "npm://react/dist/x.js"}),
                &ctx,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("file-level"), "{err}");
    }

    #[tokio::test]
    async fn read_renderdoc_bad_event_and_missing_capture() {
        // Both red BEFORE any headed spawn (offline-safe by shape).
        let r = registry::v1_registry();
        let d = tempfile::tempdir().unwrap();
        let ctx = ctx_for(&d);
        let err = r
            .call(
                "read",
                serde_json::json!({"path": "renderdoc://cap.rdc#abc"}),
                &ctx,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("event-id"), "{err}");
        let err = r
            .call(
                "read",
                serde_json::json!({"path": "renderdoc://ghost.rdc"}),
                &ctx,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("is not a file"), "{err}");
    }

    #[test]
    fn scheme_of_accepts_schemes_rejects_paths() {
        assert_eq!(scheme_of("web://example.com"), Some("web"));
        assert_eq!(scheme_of("gh://owner/repo"), Some("gh"));
        assert_eq!(scheme_of("git://C:/r/split"), Some("git"));
        assert_eq!(scheme_of("src/main.rs"), None);
        assert_eq!(scheme_of("C:/proj/main.rs"), None);
        assert_eq!(scheme_of("/tmp/x.rs"), None);
    }

    #[tokio::test]
    async fn web_spec_defaults_to_localhost_until_wired() {
        let r = registry::v1_registry();
        assert_eq!(r.web_spec().await.base_url, web::DEFAULT_BASE_URL);
        r.set_web_spec(web::validate("http://127.0.0.1:4000", 7).unwrap())
            .await;
        assert_eq!(r.web_spec().await.base_url, "http://127.0.0.1:4000");
        assert!(web::wire_web(&r, &polymorph_core::config::WebConfig::default()).await);
        assert_eq!(r.web_spec().await.base_url, web::DEFAULT_BASE_URL);
        assert!(
            !web::wire_web(
                &r,
                &polymorph_core::config::WebConfig {
                    base_url: String::new(),
                    timeout_secs: 30,
                }
            )
            .await
        );
    }

    #[tokio::test]
    async fn call_grep_and_glob_find_fixtures() {
        let r = registry::v1_registry();
        let d = tempfile::tempdir().unwrap();
        let ctx = ctx_for(&d);
        r.call(
            "write",
            serde_json::json!({"path": "main.rs", "content": "fn main() {\n // needle\n}\n"}),
            &ctx,
        )
        .await
        .unwrap();
        let out = r
            .call("grep", serde_json::json!({"pattern": "needle"}), &ctx)
            .await
            .unwrap();
        assert_eq!(out["hits"].as_array().unwrap().len(), 1);
        assert_eq!(out["capped"], false);
        assert_eq!(out["hits"][0]["line_number"], 2);
        let out = r
            .call("glob", serde_json::json!({"pattern": "**/*.rs"}), &ctx)
            .await
            .unwrap();
        assert_eq!(out["paths"].as_array().unwrap().len(), 1);
        // Bad regex stays a typed model-readable error, not a panic.
        let err = r
            .call("grep", serde_json::json!({"pattern": "(["}), &ctx)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("(["), "{err}");
    }

    #[tokio::test]
    async fn call_bash_echo_and_nonzero_exit_is_data() {
        let r = registry::v1_registry();
        let d = tempfile::tempdir().unwrap();
        let ctx = ctx_for(&d);
        let out = r
            .call("bash", serde_json::json!({"cmd": "echo hello-tools"}), &ctx)
            .await
            .unwrap();
        assert!(out["stdout"].as_str().unwrap().contains("hello-tools"));
        assert_eq!(out["exit_code"], 0);
        // Exec precedent: nonzero exits report, never error.
        let out = r
            .call("bash", serde_json::json!({"cmd": "exit 7"}), &ctx)
            .await
            .unwrap();
        assert_eq!(out["exit_code"], 7);
    }

    /// Docket #8: `bash` runs in the call's workdir (the `--repo`
    /// worktree), not the process cwd — a relative write lands there.
    #[tokio::test]
    async fn call_bash_runs_in_the_call_workdir() {
        let r = registry::v1_registry();
        let d = tempfile::tempdir().unwrap();
        let ctx = ctx_for(&d);
        let out = r
            .call(
                "bash",
                serde_json::json!({"cmd": "echo marker> bash-cwd.txt"}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(out["exit_code"], 0, "{out}");
        assert!(d.path().join("bash-cwd.txt").exists());
        assert!(!std::env::current_dir()
            .unwrap()
            .join("bash-cwd.txt")
            .exists());
    }

    /// Docket #9: a large `bash` stream comes back as a digest naming its
    /// spill file; a diagnostic from the middle survives; the model can
    /// page the file with an exact `read` (offset/limit) and a `grep` whose
    /// `dir` is the file or the spill dir, while workspace scans stay
    /// blind to it.
    #[tokio::test]
    async fn call_bash_spills_large_output_and_the_file_pages() {
        let r = registry::v1_registry();
        let d = tempfile::tempdir().unwrap();
        let ctx = ctx_for(&d);
        let cmd = if cfg!(windows) {
            "powershell -NoProfile -Command \"1..50000 | % { if ($_ -eq 25000) { 'error: boom in the middle' } else { 'line ' + $_ } }\""
        } else {
            "seq 1 50000 | sed 's/^25000$/error: boom in the middle/; s/^\\([0-9]\\)/line \\1/'"
        };
        let out = r
            .call("bash", serde_json::json!({ "cmd": cmd }), &ctx)
            .await
            .unwrap();
        assert_eq!(out["exit_code"], 0, "{out}");
        let stdout = out["stdout"].as_str().unwrap();
        assert!(stdout.contains("error: boom in the middle"), "{stdout}");
        assert!(
            stdout.contains("line 1\n") && stdout.ends_with("line 50000"),
            "{stdout}"
        );
        assert!(stdout.len() < 16 * 1024, "{}", stdout.len());
        assert_eq!(out["stdout_spill"]["total_lines"], 50000);
        assert!(out.get("stderr_spill").is_none(), "{out}");
        let path = out["stdout_spill"]["path"].as_str().unwrap().to_string();
        assert!(stdout.contains(&path));
        let page = r
            .call(
                "read",
                serde_json::json!({ "path": path, "offset": 24999, "limit": 3 }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(
            page["lines"],
            serde_json::json!(["line 24999", "error: boom in the middle", "line 25001"])
        );
        assert_eq!(page["total_lines"], 50000);
        for dir in [path.as_str(), ".polymorph/tool-output"] {
            let hits = r
                .call(
                    "grep",
                    serde_json::json!({ "pattern": "boom", "dir": dir }),
                    &ctx,
                )
                .await
                .unwrap();
            assert_eq!(hits["hits"].as_array().unwrap().len(), 1, "{dir}: {hits}");
            assert_eq!(hits["hits"][0]["line_number"], 25000);
        }
        let hits = r
            .call("grep", serde_json::json!({ "pattern": "boom" }), &ctx)
            .await
            .unwrap();
        assert!(hits["hits"].as_array().unwrap().is_empty(), "{hits}");
    }

    /// Docket #9: a timed-out `bash` still errors, now carrying what the
    /// command printed before the kill.
    #[tokio::test]
    async fn call_bash_timeout_carries_partial_output() {
        let r = registry::v1_registry();
        let d = tempfile::tempdir().unwrap();
        let ctx = ctx_for(&d);
        let cmd = if cfg!(windows) {
            "echo printed-before-kill && powershell -NoProfile -Command Start-Sleep -Seconds 30"
        } else {
            "echo printed-before-kill; sleep 30"
        };
        let err = r
            .call(
                "bash",
                serde_json::json!({ "cmd": cmd, "timeout_secs": 3 }),
                &ctx,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.starts_with(
                "bash: command timed out after 3s (child killed); output before the kill:"
            ),
            "{err}"
        );
        assert!(err.contains("printed-before-kill"), "{err}");
    }

    /// Docket #8: registry-hidden paths and harness state never surface
    /// through `grep`/`glob`; an exact `read` of the hidden file still works.
    #[tokio::test]
    async fn call_grep_and_glob_skip_hidden_and_harness_state() {
        let mut r = registry::v1_registry();
        let d = tempfile::tempdir().unwrap();
        r.hide_from_scans(d.path().join("ledger.json"));
        let ctx = ctx_for(&d);
        std::fs::write(d.path().join("main.rs"), "// needle\n").unwrap();
        std::fs::write(d.path().join("ledger.json"), "needle\n").unwrap();
        std::fs::create_dir_all(d.path().join(".polymorph")).unwrap();
        std::fs::write(d.path().join(".polymorph").join("x.rs"), "needle\n").unwrap();
        let out = r
            .call("grep", serde_json::json!({"pattern": "needle"}), &ctx)
            .await
            .unwrap();
        let hits = out["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 1, "{out}");
        assert!(hits[0]["path"].as_str().unwrap().ends_with("main.rs"));
        let out = r
            .call("glob", serde_json::json!({"pattern": "**/*"}), &ctx)
            .await
            .unwrap();
        let paths = out["paths"].as_array().unwrap();
        assert!(
            paths.iter().all(|p| {
                let p = p.as_str().unwrap();
                !p.ends_with("ledger.json") && !p.contains(".polymorph")
            }),
            "{out}"
        );
        let out = r
            .call("read", serde_json::json!({"path": "ledger.json"}), &ctx)
            .await
            .unwrap();
        assert_eq!(out["lines"][0], "needle");
    }

    #[tokio::test]
    async fn call_bash_denied_executes_nothing() {
        let r = registry::v1_registry();
        let d = tempfile::tempdir().unwrap();
        let deny_all_bash = denied_ctx_for(&d, &["bash"]);
        let canary = d.path().join("canary.txt");
        let err = r
            .call(
                "bash",
                serde_json::json!({"cmd": format!("echo x > {}", canary.display())}),
                &deny_all_bash,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("permission denied"), "{err}");
        assert!(err.contains("bash"), "{err}");
        assert!(!canary.exists(), "denied command must not run");
    }

    #[tokio::test]
    async fn call_edit_roundtrip_through_registry_store() {
        let r = registry::v1_registry();
        let d = tempfile::tempdir().unwrap();
        let ctx = ctx_for(&d);
        r.call(
            "write",
            serde_json::json!({"path": "a", "content": "l1\nl2\nl3\nl4\n"}),
            &ctx,
        )
        .await
        .unwrap();
        // Tags mint through the SAME registry the edit verifies against.
        let tag = {
            let bytes = std::fs::read(d.path().join("a")).unwrap();
            let norm = hashline::apply::normalize(&bytes);
            let mut store = r.snapshots.lock().await;
            store.mint(std::path::Path::new("a"), &norm.text)
        };
        let out = r
            .call(
                "edit",
                serde_json::json!({"patch": format!("[a#{tag}]\nPUT 1.=2:\n+X\n")}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(out["files"].as_array().unwrap().len(), 1);
        let page = r
            .call("read", serde_json::json!({"path": "a"}), &ctx)
            .await
            .unwrap();
        assert_eq!(page["lines"], serde_json::json!(["X", "l3", "l4"]));
    }

    #[tokio::test]
    async fn call_edit_denied_leaves_file_untouched() {
        // Gate speaks before parse/validate/write: a denied edit changes
        // nothing even with a perfectly valid patch.
        let r = registry::v1_registry();
        let d = tempfile::tempdir().unwrap();
        let open = ctx_for(&d);
        r.call(
            "write",
            serde_json::json!({"path": "a", "content": "l1\nl2\n"}),
            &open,
        )
        .await
        .unwrap();
        let tag = {
            let bytes = std::fs::read(d.path().join("a")).unwrap();
            let norm = hashline::apply::normalize(&bytes);
            let mut store = r.snapshots.lock().await;
            store.mint(std::path::Path::new("a"), &norm.text)
        };
        let shut = denied_ctx_for(&d, &["edit"]);
        let err = r
            .call(
                "edit",
                serde_json::json!({"patch": format!("[a#{tag}]\nPUT 1.=1:\n+X\n")}),
                &shut,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("permission denied"), "{err}");
        let after = std::fs::read_to_string(d.path().join("a")).unwrap();
        assert_eq!(after, "l1\nl2\n");
    }

    #[tokio::test]
    async fn call_bad_args_name_the_field() {
        let r = registry::v1_registry();
        let d = tempfile::tempdir().unwrap();
        let ctx = ctx_for(&d);
        let err = r
            .call("read", serde_json::json!({}), &ctx)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("`path`"), "{err}");
        let err = r
            .call("bash", serde_json::json!({"cmd": 42}), &ctx)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("`cmd`"), "{err}");
    }

    #[test]
    fn schemas_pin_every_tool_required_keys() {
        for name in registry::v1_registry().names() {
            let s = Registry::schema(&name).unwrap_or_else(|| panic!("{name} has no schema"));
            assert_eq!(s["type"], "object");
            assert!(
                s["required"].as_array().is_some_and(|r| !r.is_empty()),
                "{name}: required must be non-empty"
            );
        }
        assert!(Registry::schema("frobnicate").is_none());
        // Spot-check the security-relevant shapes.
        assert!(Registry::schema("read").unwrap()["required"]
            .to_string()
            .contains("path"));
        assert!(Registry::schema("bash").unwrap()["required"]
            .to_string()
            .contains("cmd"));
        assert!(Registry::schema("edit").unwrap()["required"]
            .to_string()
            .contains("patch"));
    }
}
