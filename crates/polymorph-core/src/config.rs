//! Figment-based configuration.
//!
//! Precedence, lowest to highest: user config < project config < env < CLI.
//! Later figment merges win, so merge in that order.
//!
//! Secrets: `api_key_cmd` output is trimmed process stdout held only in memory
//! by the caller. This module NEVER logs key material — there is no logging
//! call in the resolution path (grep-guard: search for `api_key` in logs,
//! you will only find this doc + field names, never values).

use figment::{
    providers::{Env, Format, Toml},
    Figment,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Top-level config. All sections default so a missing/empty config still loads.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PolymorphConfig {
    #[serde(default)]
    pub providers: Vec<ProviderEntry>,
    #[serde(default)]
    pub permissions: Permissions,
    #[serde(default)]
    pub options: Options,
    /// Role name (e.g. `"default"`, `"review"`) → ordered model chain.
    /// First entry is tried first; on 429 the next entry is tried.
    #[serde(default)]
    pub roles: BTreeMap<String, Vec<String>>,
    /// Role name → reasoning-effort level (`low`/`medium`/`high`).
    /// Absent = unset (ambient). Parsed strictly at use
    /// (unknown names fail the turn naming the valid set); explicit-model
    /// singles (`--model`) stay raw by design.
    #[serde(default)]
    pub effort: BTreeMap<String, String>,
    /// Path-scoped chain pins. Longest `path_prefix` wins;
    /// a pin with `role: None` applies to every role.
    #[serde(default)]
    pub pins: Vec<PathPin>,
    /// MCP servers: spawned over stdio on first tool
    /// call (lazy). Children inherit the process env (no secrets in config
    /// — env names only, `api_key_cmd` pattern; never logged).
    #[serde(default)]
    pub mcp_servers: Vec<McpServerEntry>,
    /// Script hooks: `[[hooks]]` entries validated at
    /// attach (`pre`/`post`/`stop` + command + optional timeout). Empty =
    /// in-process handlers only (all pre-87 behavior unchanged).
    #[serde(default)]
    pub hooks: Vec<HookEntry>,
    /// Language servers: `[[lsp_servers]]` entries
    /// validated at attach (non-empty id/cmd + at least one language).
    /// Spawned over stdio on first write for a registered language (lazy).
    /// Children inherit the process env (no secrets in config — commands
    /// only, never logged).
    #[serde(default)]
    pub lsp_servers: Vec<LspServerEntry>,
    /// Debug adapters: `[[dap_adapters]]` entries
    /// validated at attach (non-empty id/cmd + at least one language).
    /// Sessions spawn fresh per use (debugger sessions are inherently
    /// single-shot — unlike LSP there is no long-lived shared server).
    /// Children inherit the process env (no secrets in config — commands
    /// only, never logged; adapter-scoped env like `PYTHONPATH` stays
    /// API-level until M3 shows a config consumer — stated gap).
    #[serde(default)]
    pub dap_adapters: Vec<DapAdapterEntry>,
    /// RenderDoc binary: single `[renderdoc]` table
    /// (exactly one binary exists — no per-language pool like LSP/DAP).
    /// Absent table = `renderdoccmd` on PATH (fail-soft at wire time).
    /// Children inherit the process env (no secrets in config — commands
    /// only, never logged).
    #[serde(default)]
    pub renderdoc: RenderdocConfig,
    /// Web backend: single `[web]` table (the
    /// `[renderdoc]` shape — exactly one serica server exists).
    /// Absent table = localhost defaults below (fail-soft at call time:
    /// tools error naming `base_url`, startup never fails).
    #[serde(default)]
    pub web: WebConfig,
    /// Watching reviewer: single `[advisor]` table.
    /// Absent = disabled (advisor doubles turn spend — off unless the
    /// project opts in; runner behavior byte-identical when off).
    #[serde(default)]
    pub advisor: AdvisorConfig,
    /// Agent execution parameters: single `[agent]` table.
    #[serde(default)]
    pub agent: AgentConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderEntry {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub base_url: String,
    /// Env var naming the key (also the sink var for `api_key_cmd`/pool
    /// injection: the resolved value is exported under this name
    /// for the genai call only, process-local, never logged).
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Additional env-var key sources for round-robin.
    #[serde(default)]
    pub api_key_envs: Vec<String>,
    #[serde(default)]
    pub api_key_cmd: Option<String>,
    /// Exact model names served by this provider.
    /// Unmapped models keep the pre-D ambient-env behavior (no regression).
    #[serde(default)]
    pub models: Vec<String>,
}

/// One MCP server: a child command speaking MCP over stdio.
///
/// ```toml
/// [[mcp_servers]]
/// id = "ara"
/// cmd = "ara"
/// args = ["mcp"]
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct McpServerEntry {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub cmd: String,
    #[serde(default)]
    pub args: Vec<String>,
}

/// One language server: a child command speaking LSP over stdio
/// for the listed languages (`language_of` names).
///
/// ```toml
/// [[lsp_servers]]
/// id = "rust"
/// cmd = "rust-analyzer"
/// languages = ["rust"]
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LspServerEntry {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub cmd: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub languages: Vec<String>,
}

/// One `[[dap_adapters]]` entry: mirrors
/// [`LspServerEntry`] exactly (same validation, same leniency layers).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DapAdapterEntry {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub cmd: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub languages: Vec<String>,
}

/// RenderDoc binary: a single `[renderdoc]` table.
///
/// ```toml
/// [renderdoc]
/// cmd = "C:/Program Files/RenderDoc/renderdoccmd.exe"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenderdocConfig {
    #[serde(default = "default_renderdoc_cmd")]
    pub cmd: String,
}

impl Default for RenderdocConfig {
    fn default() -> Self {
        Self {
            cmd: default_renderdoc_cmd(),
        }
    }
}

fn default_renderdoc_cmd() -> String {
    "renderdoccmd".to_string()
}

/// Web backend: a single `[web]` table pointing at
/// a local serica server (HTTP process boundary — serica is AGPL, never
/// a crate dep; binaries live host-side, the lsp-servers.md precedent).
///
/// ```toml
/// [web]
/// base_url = "http://127.0.0.1:3000"
/// timeout_secs = 30
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebConfig {
    #[serde(default = "default_web_base_url")]
    pub base_url: String,
    #[serde(default = "default_web_timeout_secs")]
    pub timeout_secs: u64,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            base_url: default_web_base_url(),
            timeout_secs: default_web_timeout_secs(),
        }
    }
}

fn default_web_base_url() -> String {
    "http://127.0.0.1:3000".to_string()
}

fn default_web_timeout_secs() -> u64 {
    30
}

/// Watching reviewer: a single `[advisor]` table.
///
/// ```toml
/// [advisor]
/// enabled = true
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AdvisorConfig {
    #[serde(default)]
    pub enabled: bool,
}

/// Agent execution settings: a single `[agent]` table.
///
/// ```toml
/// [agent]
/// max_tool_rounds = 30
/// lsp_error_gate = false
/// verify_cmds = ["cargo build", "cargo test"]
/// verify_timeout_secs = 600
/// max_bonus_tool_rounds = 3
/// create_seed = true
/// drive_on_fail = "rollback"
/// tool_output_inline_bytes = 16384
/// tool_output_keep_files = 200
/// tool_history_budget_bytes = 262144
/// tool_history_keep_rounds = 2
/// reviewer_tools = false
/// reviewer_tool_rounds = 4
/// command_change_max_files = 20000
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentConfig {
    #[serde(default = "default_max_tool_rounds")]
    pub max_tool_rounds: u32,
    /// Opt-in LSP error gate: when true, `review run`/`drive`
    /// with `--tools` turn a passing verdict into a fail while the last fix
    /// turn left error-severity LSP diagnostics on files it mutated
    /// (`--lsp-error-gate` enables it per run). Off = warn-only.
    #[serde(default)]
    pub lsp_error_gate: bool,
    /// Verify-command gate: operator commands `review run`/
    /// `drive` run in order in the drive's workdir after every fix turn
    /// (stopping at the first failure), outside the tool Gate. Any failure
    /// reaches the reviewer as evidence and turns a passing verdict into a
    /// fail. Empty = off. A non-empty `--verify-cmd` list replaces this one.
    #[serde(default)]
    pub verify_cmds: Vec<String>,
    /// Per-command timeout of the verify gate, seconds (`--verify-timeout-secs`
    /// overrides). Expiry kills the process tree and counts as a failure.
    #[serde(default = "default_verify_timeout_secs")]
    pub verify_timeout_secs: u64,
    /// Verification bonus tool rounds past `max_tool_rounds`:
    /// granted one at a time, only while the previous round made progress
    /// (a successful write/edit, or a command that ran after an
    /// unverified mutation). 0 = none. The one-shot pending-mutation bonus
    /// is separate and unaffected.
    #[serde(default = "default_max_bonus_tool_rounds")]
    pub max_bonus_tool_rounds: u32,
    /// CREATE-job seed: when the artifact file is missing and
    /// no content was given, `review run`/`drive` answer round 1 with the
    /// harness-authored creation finding instead of a reviewer call
    /// (`--no-create-seed` turns it off per run). False = the
    /// review-first round 1 on CREATE jobs too.
    #[serde(default = "default_create_seed")]
    pub create_seed: bool,
    /// What a non-passing in-place `review drive` (and TUI drive) does with
    /// the files its fixer's `write`/`edit` calls changed:
    /// `"rollback"` restores them (conflict-safe, archived under
    /// `.polymorph/attempts/`), `"keep"` leaves the tree as the drive left
    /// it. `--on-fail` overrides per run; `review run` keeps unless asked.
    #[serde(default = "default_drive_on_fail")]
    pub drive_on_fail: String,
    /// Per-stream inline limit of the fixer's `bash` output, bytes:
    /// a larger stream is written in full to
    /// `.polymorph/tool-output/` as it is read and reaches the model as a
    /// digest (head, tail, diagnostic lines, every gap named). 0 = off
    /// (every stream inline, unbounded).
    #[serde(default = "default_tool_output_inline_bytes")]
    pub tool_output_inline_bytes: usize,
    /// Spill files kept per workdir: older ones are deleted
    /// when a new one is written (only harness-named files in
    /// `.polymorph/tool-output/`).
    #[serde(default = "default_tool_output_keep_files")]
    pub tool_output_keep_files: usize,
    /// Budget for the tool results a fix turn re-sends every round, bytes
    /// Past it, the oldest results outside the latest
    /// `tool_history_keep_rounds` rounds become stubs pointing at a saved
    /// copy (or, for `read`, at the file). 0 = off.
    #[serde(default = "default_tool_history_budget_bytes")]
    pub tool_history_budget_bytes: usize,
    /// Latest tool rounds whose results are never compacted (at least 1).
    #[serde(default = "default_tool_history_keep_rounds")]
    pub tool_history_keep_rounds: u32,
    /// Read-only reviewer tools: when true, `review run`/`drive`
    /// reviewers may `read`/`grep`/`glob` the drive's workdir (plus the
    /// `lsp_*` lookups when LSP servers are wired) before their verdict,
    /// and findings on files they neither saw in the brief nor inspected
    /// are dropped (`--reviewer-tools` enables it per run). False = the
    /// single text-turn reviewer, byte-identical.
    #[serde(default)]
    pub reviewer_tools: bool,
    /// Tool rounds a tool reviewer gets before its forced tool-free
    /// verdict turn (at least 1).
    #[serde(default = "default_reviewer_tool_rounds")]
    pub reviewer_tool_rounds: u32,
    /// Command change detection: the fixer's workdir is stamped (size +
    /// mtime, gitignore-aware) right before and after every `bash`/`exec`
    /// call, so files a command created, modified or deleted count as
    /// mutations (ledger, peripheral attachment, LSP evidence, rollback).
    /// A tree with more files than this skips detection for the rest of
    /// the fix turn, with an explicit note. 0 = off.
    #[serde(default = "default_command_change_max_files")]
    pub command_change_max_files: usize,
}

fn default_max_tool_rounds() -> u32 {
    30
}

fn default_verify_timeout_secs() -> u64 {
    600
}

fn default_max_bonus_tool_rounds() -> u32 {
    3
}

fn default_create_seed() -> bool {
    true
}

fn default_drive_on_fail() -> String {
    "rollback".to_string()
}

fn default_tool_output_inline_bytes() -> usize {
    16 * 1024
}

fn default_tool_output_keep_files() -> usize {
    200
}

fn default_tool_history_budget_bytes() -> usize {
    256 * 1024
}

fn default_tool_history_keep_rounds() -> u32 {
    2
}

fn default_reviewer_tool_rounds() -> u32 {
    4
}

fn default_command_change_max_files() -> usize {
    20_000
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_tool_rounds: default_max_tool_rounds(),
            lsp_error_gate: false,
            verify_cmds: Vec::new(),
            verify_timeout_secs: default_verify_timeout_secs(),
            max_bonus_tool_rounds: default_max_bonus_tool_rounds(),
            create_seed: default_create_seed(),
            drive_on_fail: default_drive_on_fail(),
            tool_output_inline_bytes: default_tool_output_inline_bytes(),
            tool_output_keep_files: default_tool_output_keep_files(),
            tool_history_budget_bytes: default_tool_history_budget_bytes(),
            tool_history_keep_rounds: default_tool_history_keep_rounds(),
            reviewer_tools: false,
            reviewer_tool_rounds: default_reviewer_tool_rounds(),
            command_change_max_files: default_command_change_max_files(),
        }
    }
}

/// One `[[hooks]]` entry: event point + shell command + optional timeout
/// . Validated by tools (`validate_entry`) at attach,
/// never trusted raw.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct HookEntry {
    #[serde(default)]
    pub event: String,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// One path-scoped chain override.
///
/// ```toml
/// [[pins]]
/// path_prefix = "crates/polymorph-llm"
/// role = "review"          # optional; absent = all roles
/// chain = ["model-x"]
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PathPin {
    #[serde(default)]
    pub path_prefix: String,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub chain: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Permissions {
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
}

impl Default for Permissions {
    fn default() -> Self {
        Self {
            allow: vec!["read".into(), "grep".into(), "glob".into()],
            deny: vec!["bash:rm".into(), "bash:git push".into()],
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Options {
    #[serde(default)]
    pub debug: bool,
}

impl PolymorphConfig {
    /// Built-in fallback when no config file exists: a single-model default
    /// chain. The model name is passed straight to genai's model mapper.
    pub fn default_chain_for(role: crate::Role) -> Vec<String> {
        match role {
            crate::Role::Review | crate::Role::Advisor => vec!["muse-spark-1.3".into()],
            crate::Role::Smol | crate::Role::Tiny => vec!["glm-5.3-flash".into()],
            _ => vec!["glm-5.3".into()],
        }
    }

    /// Model chain for a role: config `roles["{role}"]` if present and
    /// non-empty, else the built-in fallback.
    pub fn chain_for(&self, role: crate::Role) -> Vec<String> {
        chain_for_role(&self.roles, role)
    }
}

/// Single source of truth for role→chain lookup (used here and by
/// `polymorph-llm::RoleRouter` so the two can never diverge).
pub fn chain_for_role(chains: &BTreeMap<String, Vec<String>>, role: crate::Role) -> Vec<String> {
    let key = format!("{role:?}").to_lowercase();
    match chains.get(&key) {
        Some(chain) if !chain.is_empty() => chain.clone(),
        _ => PolymorphConfig::default_chain_for(role),
    }
}

/// Default user config path: `~/.config/polymorph/config.toml`.
/// Default project config path: `./.polymorph/config.toml`.
pub fn default_paths() -> (PathBuf, PathBuf) {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    (
        home.join(".config").join("polymorph").join("config.toml"),
        PathBuf::from(".polymorph").join("config.toml"),
    )
}

/// CLI-layer overrides (highest precedence). Only `Some` fields
/// apply; `None` leaves the merged file+env value untouched.
#[derive(Debug, Clone, Default)]
pub struct CliOverrides {
    pub debug: Option<bool>,
}

/// Load config. Missing files are skipped (not errors); a present-but-malformed
/// file IS an error (refuse, don't guess).
pub fn load(user_path: &Path, project_path: &Path) -> anyhow::Result<PolymorphConfig> {
    load_with_cli(user_path, project_path, &CliOverrides::default())
}

/// [`load`] plus the CLI layer: figment merges user < project < env, then
/// explicit `Some` CLI overrides win (figment's `Env` cannot see clap flags,
/// so the CLI layer is applied structurally, not via figment).
pub fn load_with_cli(
    user_path: &Path,
    project_path: &Path,
    cli: &CliOverrides,
) -> anyhow::Result<PolymorphConfig> {
    let mut fig = Figment::new();
    for p in [user_path, project_path] {
        if p.exists() {
            fig = fig.merge(Toml::file(p));
        }
    }
    fig = fig.merge(Env::prefixed("POLYMORPH_").split("__"));
    let mut cfg: PolymorphConfig = fig
        .extract()
        .map_err(|e| anyhow::anyhow!("config load failed: {e}"))?;
    if let Some(debug) = cli.debug {
        cfg.options.debug = debug;
    }
    Ok(cfg)
}

/// `cmd /S /C "<cmd>"`, passed raw: `Command::args` would escape embedded
/// quotes as `\"`, which `cmd.exe` passes through literally (same fix as
/// `polymorph_tools::exec`).
#[cfg(windows)]
fn key_cmd_command(cmd: &str) -> std::process::Command {
    use std::os::windows::process::CommandExt;
    let mut c = std::process::Command::new("cmd");
    c.raw_arg(format!("/S /C \"{cmd}\""));
    c
}

#[cfg(not(windows))]
fn key_cmd_command(cmd: &str) -> std::process::Command {
    let mut c = std::process::Command::new("sh");
    c.args(["-c", cmd]);
    c
}

/// Run an `api_key_cmd` via the platform shell; trim trailing whitespace.
/// Shared by [`resolve_api_key`] and [`resolve_all_api_keys`] so the two
/// can never diverge on trimming, failure, or empty-output semantics.
fn run_key_cmd(entry: &ProviderEntry, cmd: &str) -> anyhow::Result<String> {
    let output = key_cmd_command(cmd).output()?;
    if !output.status.success() {
        anyhow::bail!(
            "api_key_cmd for provider '{}' failed with status {}",
            entry.id,
            output.status
        );
    }
    let key = String::from_utf8(output.stdout)
        .map_err(|e| anyhow::anyhow!("api_key_cmd output was not UTF-8: {e}"))?;
    let key = key.trim_end().to_string();
    if key.is_empty() {
        anyhow::bail!(
            "api_key_cmd for provider '{}' produced empty output",
            entry.id
        );
    }
    Ok(key)
}

/// Non-empty, trimmed `api_key_cmd`, if configured.
fn key_cmd_of(entry: &ProviderEntry) -> Option<&str> {
    entry
        .api_key_cmd
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Resolve a provider's API key: `api_key_cmd` first, else `api_key_env`.
/// Runs the command via the platform shell, trims trailing whitespace.
/// Output is returned to the caller only — never logged, never stored.
pub fn resolve_api_key(entry: &ProviderEntry) -> anyhow::Result<Option<String>> {
    if let Some(cmd) = key_cmd_of(entry) {
        return Ok(Some(run_key_cmd(entry, cmd)?));
    }
    if let Some(env) = entry.api_key_env.as_deref() {
        return Ok(std::env::var(env).ok().filter(|v| !v.is_empty()));
    }
    Ok(None)
}

/// Pin lookup: longest matching `path_prefix` for (path, role).
/// A pin with `role: None` matches every role; role strings are the
/// lowercase `Role` variant name (same convention as `chain_for_role`).
/// Pins with an empty prefix or empty chain never match (an empty prefix
/// would otherwise shadow the whole tree — refused by construction).
/// Returns the pinned chain, or `None` (caller falls back to role chains).
pub fn pin_for<'a>(pins: &'a [PathPin], path: &str, role: crate::Role) -> Option<&'a Vec<String>> {
    let key = format!("{role:?}").to_lowercase();
    pins.iter()
        .filter(|p| {
            !p.chain.is_empty()
                && !p.path_prefix.is_empty()
                && path.starts_with(p.path_prefix.as_str())
                && p.role.as_deref().map(|r| r == key).unwrap_or(true)
        })
        .max_by_key(|p| p.path_prefix.len())
        .map(|p| &p.chain)
}

/// Model→provider mapping: exact model-name match against
/// each entry's `models` list. Unmapped models keep ambient behavior.
pub fn find_provider_for<'a>(
    providers: &'a [ProviderEntry],
    model: &str,
) -> Option<&'a ProviderEntry> {
    providers
        .iter()
        .find(|p| p.models.iter().any(|m| m == model))
}

/// Resolve a bare model name through configured providers (deployment-time
/// mapping, config.md D3): `glm-5.3` with an entry listing
/// `opencode_go::glm-5.3` becomes that qualified spec, so the attempt rides
/// the claiming provider's adapter + key instead of failing closed on
/// whatever the bare name infers to (e.g. Z.ai's `ZAI_API_KEY`). Qualified
/// names (`a::b`) and names no entry claims pass through unchanged. First
/// config entry wins on competing claims (deterministic; overlapping
/// catalogs are operator error — say so, don't guess). Pure: the managed
/// and single attempt paths qualify before inject + call, so cooldown keys,
/// fallback labels, and provider matching all see the same string.
pub fn qualify_model(providers: &[ProviderEntry], model: &str) -> String {
    if model.contains("::") {
        return model.to_string();
    }
    for p in providers {
        if let Some(claimed) = p
            .models
            .iter()
            .find(|m| m.split("::").last() == Some(model))
        {
            return claimed.clone();
        }
    }
    model.to_string()
}

/// All usable key values for a provider: `api_key_cmd` output if
/// configured (single value), else every set non-empty var in `api_key_envs`
/// plus the legacy single `api_key_env`, deduplicated, order-preserved.
/// Empty vec = no key material configured (caller keeps ambient behavior).
/// Values are returned to the caller only — never logged, never stored.
pub fn resolve_all_api_keys(entry: &ProviderEntry) -> anyhow::Result<Vec<String>> {
    if let Some(cmd) = key_cmd_of(entry) {
        return Ok(vec![run_key_cmd(entry, cmd)?]);
    }
    let mut keys = Vec::new();
    for name in entry.api_key_envs.iter().chain(entry.api_key_env.iter()) {
        if let Ok(v) = std::env::var(name) {
            let v = v.trim().to_string();
            if !v.is_empty() && !keys.contains(&v) {
                keys.push(v);
            }
        }
    }
    Ok(keys)
}

/// Round-robin credential pool over one provider's resolved keys.
/// The caller takes [`CredentialPool::next`] per attempt, so consecutive
/// attempts (429 rotation, or successive turns sharing the pool) spread load.
/// Holds values in memory only; Debug is redacted (never print keys).
#[derive(Clone, Default)]
pub struct CredentialPool {
    keys: Vec<String>,
    cursor: usize,
}

impl CredentialPool {
    pub fn new(keys: Vec<String>) -> Self {
        Self { keys, cursor: 0 }
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Next key, advancing the cursor (wraps). `None` when empty.
    /// Named `next_key` (not `next`) so it cannot be confused with
    /// `Iterator::next` (clippy::should_implement_trait).
    pub fn next_key(&mut self) -> Option<&str> {
        if self.keys.is_empty() {
            return None;
        }
        let i = self.cursor % self.keys.len();
        self.cursor = self.cursor.wrapping_add(1);
        Some(self.keys[i].as_str())
    }
}

impl std::fmt::Debug for CredentialPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialPool")
            .field("len", &self.keys.len())
            .field("cursor", &self.cursor)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_toml(dir: &tempfile::TempDir, name: &str, body: &str) -> PathBuf {
        let p = dir.path().join(name);
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        p
    }

    #[test]
    fn missing_files_load_defaults() {
        let cfg = load(
            Path::new("/nonexistent-a/config.toml"),
            Path::new("/nonexistent-b/config.toml"),
        )
        .unwrap();
        assert!(!cfg.options.debug);
        assert!(cfg.providers.is_empty());
    }

    #[test]
    fn malformed_toml_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let bad = write_toml(&dir, "bad.toml", "[[providers]\nthis is = = garbage {{{");
        let err = load(&bad, Path::new("/nonexistent/config.toml")).unwrap_err();
        assert!(err.to_string().contains("config load failed"));
    }

    /// RAII guard for a process env var: restores on drop so a
    /// panic mid-matrix cannot leak state into other tests. All precedence
    /// cases live in ONE test fn because tests share one process env.
    struct EnvGuard {
        name: &'static str,
        prev: Option<String>,
    }

    impl EnvGuard {
        fn set(name: &'static str, value: &str) -> Self {
            let prev = std::env::var(name).ok();
            std::env::set_var(name, value);
            Self { name, prev }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var(self.name, v),
                None => std::env::remove_var(self.name),
            }
        }
    }

    #[test]
    fn precedence_matrix_user_lt_project_lt_env_lt_cli() {
        let user_dir = tempfile::tempdir().unwrap();
        let proj_dir = tempfile::tempdir().unwrap();
        let user_f = write_toml(&user_dir, "u.toml", "[options]\ndebug = false\n");
        let user_t = write_toml(&user_dir, "ut.toml", "[options]\ndebug = true\n");
        let proj_t = write_toml(&proj_dir, "p.toml", "[options]\ndebug = true\n");
        let missing = Path::new("/nonexistent/config.toml");
        let no_cli = CliOverrides::default();
        let cli_t = CliOverrides { debug: Some(true) };
        let cli_f = CliOverrides { debug: Some(false) };

        // user < project (preserved)
        assert!(load(&user_f, &proj_t).unwrap().options.debug);
        // project < env
        {
            let _g = EnvGuard::set("POLYMORPH_OPTIONS__DEBUG", "false");
            assert!(!load(&user_f, &proj_t).unwrap().options.debug);
        }
        // env < CLI (both directions)
        {
            let _g = EnvGuard::set("POLYMORPH_OPTIONS__DEBUG", "false");
            assert!(
                load_with_cli(&user_f, &proj_t, &cli_t)
                    .unwrap()
                    .options
                    .debug
            );
        }
        {
            let _g = EnvGuard::set("POLYMORPH_OPTIONS__DEBUG", "true");
            assert!(
                !load_with_cli(&user_f, &proj_t, &cli_f)
                    .unwrap()
                    .options
                    .debug
            );
        }
        // user < CLI directly (no project, no env)
        assert!(
            !load_with_cli(&user_t, missing, &cli_f)
                .unwrap()
                .options
                .debug
        );
        // full stack: user(false) < project(true) < env(false) < CLI(true)
        {
            let _g = EnvGuard::set("POLYMORPH_OPTIONS__DEBUG", "false");
            assert!(
                load_with_cli(&user_f, &proj_t, &cli_t)
                    .unwrap()
                    .options
                    .debug
            );
        }
        // CLI None leaves the merged file+env value untouched
        {
            let _g = EnvGuard::set("POLYMORPH_OPTIONS__DEBUG", "true");
            assert!(
                load_with_cli(&user_f, missing, &no_cli)
                    .unwrap()
                    .options
                    .debug
            );
        }
    }

    #[test]
    fn roles_chain_config_or_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let f = write_toml(
            &dir,
            "r.toml",
            "[roles]\ndefault = [\"model-a\", \"model-b\"]\n",
        );
        let cfg = load(&f, Path::new("/nonexistent/config.toml")).unwrap();
        assert_eq!(
            cfg.chain_for(crate::Role::Default),
            vec!["model-a", "model-b"]
        );
        // unconfigured role falls back to built-ins
        assert_eq!(cfg.chain_for(crate::Role::Review), vec!["muse-spark-1.3"]);
        assert_eq!(cfg.chain_for(crate::Role::Smol), vec!["glm-5.3-flash"]);
    }

    #[test]
    fn api_key_cmd_trims_trailing_newline() {
        // `echo` appends a newline; resolution must strip it.
        let entry = ProviderEntry {
            id: "t".into(),
            api_key_cmd: Some("echo secret123".into()),
            ..Default::default()
        };
        // custom Debug must never print key material — but the entry itself
        // holds no key, only the command. The resolved value is separate.
        let key = resolve_api_key(&entry).unwrap().unwrap();
        assert_eq!(key, "secret123");
    }

    #[test]
    fn api_key_cmd_failure_is_error() {
        let entry = ProviderEntry {
            id: "t".into(),
            api_key_cmd: Some("exit 3".into()),
            ..Default::default()
        };
        assert!(resolve_api_key(&entry).is_err());
    }

    #[test]
    fn api_key_env_missing_is_none_not_error() {
        let entry = ProviderEntry {
            id: "t".into(),
            api_key_env: Some("POLYMORPH_TEST_MISSING_KEY_XYZ".into()),
            ..Default::default()
        };
        assert_eq!(resolve_api_key(&entry).unwrap(), None);
    }

    #[test]
    fn all_keys_cmd_wins_over_envs() {
        let entry = ProviderEntry {
            id: "t".into(),
            api_key_cmd: Some("echo fromcmd".into()),
            api_key_envs: vec!["POLYMORPH_TEST_KEYS_CMDONLY".into()],
            ..Default::default()
        };
        let _g = EnvGuard::set("POLYMORPH_TEST_KEYS_CMDONLY", "fromenv");
        assert_eq!(resolve_all_api_keys(&entry).unwrap(), vec!["fromcmd"]);
    }

    #[test]
    fn all_keys_collects_envs_deduped_in_order() {
        let _a = EnvGuard::set("POLYMORPH_TEST_KEYS_A", "k1");
        let _b = EnvGuard::set("POLYMORPH_TEST_KEYS_B", "k2");
        // POLYMORPH_TEST_KEYS_MISSING is deliberately unset: skipped, not fatal.
        // Legacy single `api_key_env` participates as a source (k1 duplicate).
        let entry = ProviderEntry {
            id: "t".into(),
            api_key_env: Some("POLYMORPH_TEST_KEYS_A".into()),
            api_key_envs: vec![
                "POLYMORPH_TEST_KEYS_MISSING".into(),
                "POLYMORPH_TEST_KEYS_A".into(),
                "POLYMORPH_TEST_KEYS_B".into(),
            ],
            ..Default::default()
        };
        assert_eq!(
            resolve_all_api_keys(&entry).unwrap(),
            vec!["k1".to_string(), "k2".to_string()]
        );
    }

    #[test]
    fn all_keys_empty_when_nothing_configured() {
        let entry = ProviderEntry {
            id: "t".into(),
            api_key_env: Some("POLYMORPH_TEST_MISSING_KEY_XYZ".into()),
            ..Default::default()
        };
        assert!(resolve_all_api_keys(&entry).unwrap().is_empty());
    }

    #[test]
    fn credential_pool_round_robins_and_wraps() {
        let mut pool =
            CredentialPool::new(vec!["k1".to_string(), "k2".to_string(), "k3".to_string()]);
        assert_eq!(pool.len(), 3);
        assert_eq!(pool.next_key(), Some("k1"));
        assert_eq!(pool.next_key(), Some("k2"));
        assert_eq!(pool.next_key(), Some("k3"));
        assert_eq!(pool.next_key(), Some("k1"));
        // Debug redacts values (grep-guard companion: lengths only).
        let dbg = format!("{pool:?}");
        assert!(!dbg.contains("k1"));
        assert!(dbg.contains("len"));
    }

    #[test]
    fn credential_pool_empty_is_none() {
        let mut pool = CredentialPool::default();
        assert!(pool.is_empty());
        assert_eq!(pool.next_key(), None);
    }

    #[test]
    fn provider_mapping_exact_match_only() {
        let providers = vec![
            ProviderEntry {
                id: "oa".into(),
                models: vec!["glm-5.3".into()],
                ..Default::default()
            },
            ProviderEntry {
                id: "zb".into(),
                models: vec!["glm-5.3-flash".into()],
                ..Default::default()
            },
        ];
        assert_eq!(find_provider_for(&providers, "glm-5.3").unwrap().id, "oa");
        // Prefix is NOT a match (exact only — no surprise routing).
        assert!(find_provider_for(&providers, "glm").is_none());
        assert!(find_provider_for(&providers, "unknown-model").is_none());
        assert!(find_provider_for(&[], "glm-5.3").is_none());
    }

    fn test_entry(id: &str, models: &[&str]) -> ProviderEntry {
        ProviderEntry {
            id: id.into(),
            models: models.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn qualify_model_resolves_bare_names_through_providers() {
        let providers = vec![
            test_entry(
                "go",
                &["opencode_go::glm-5.3", "opencode_go::glm-5.3-flash"],
            ),
            test_entry("zai", &["zai::glm-5.3"]),
        ];
        // Bare name claimed by an entry becomes that entry's spec.
        assert_eq!(
            qualify_model(&providers, "glm-5.3-flash"),
            "opencode_go::glm-5.3-flash"
        );
        // Competing claims: first config entry wins, deterministically.
        assert_eq!(qualify_model(&providers, "glm-5.3"), "opencode_go::glm-5.3");
        // Qualified names pass through untouched (never re-resolved).
        assert_eq!(qualify_model(&providers, "zai::glm-5.3"), "zai::glm-5.3");
        assert_eq!(qualify_model(&providers, "other::x"), "other::x");
        // Unclaimed bare names pass through (ambient behavior preserved).
        assert_eq!(
            qualify_model(&providers, "muse-spark-1.3"),
            "muse-spark-1.3"
        );
        assert_eq!(qualify_model(&[], "glm-5.3"), "glm-5.3");
        // A bare entry model matches itself exactly (no-op, not a loop).
        let bare = vec![test_entry("oa", &["glm-5.3"])];
        assert_eq!(qualify_model(&bare, "glm-5.3"), "glm-5.3");
    }

    fn test_pin(prefix: &str, role: Option<&str>, chain: &[&str]) -> PathPin {
        PathPin {
            path_prefix: prefix.into(),
            role: role.map(str::to_string),
            chain: chain.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn pins_longest_prefix_wins_and_role_scoped() {
        let pins = vec![
            test_pin("crates", None, &["global-crates"]),
            test_pin("crates/polymorph-llm", None, &["llm-any"]),
            test_pin("crates/polymorph-llm", Some("review"), &["llm-review"]),
        ];
        // Role-scoped pin wins for its role at the same depth.
        assert_eq!(
            pin_for(&pins, "crates/polymorph-llm/src/x.rs", crate::Role::Review).unwrap(),
            &vec!["llm-review".to_string()]
        );
        // Other roles fall to the unscoped pin at the same depth.
        assert_eq!(
            pin_for(&pins, "crates/polymorph-llm/src/x.rs", crate::Role::Default).unwrap(),
            &vec!["llm-any".to_string()]
        );
        // Shallower prefix applies outside the deeper tree.
        assert_eq!(
            pin_for(&pins, "crates/other/y.rs", crate::Role::Default).unwrap(),
            &vec!["global-crates".to_string()]
        );
        // No match at all.
        assert!(pin_for(&pins, "docs/z.md", crate::Role::Default).is_none());
    }

    #[test]
    fn pins_with_empty_prefix_or_chain_never_match() {
        let pins = vec![
            test_pin("", None, &["shadow-all"]),
            test_pin("crates", None, &[]),
        ];
        assert!(pin_for(&pins, "crates/x.rs", crate::Role::Default).is_none());
        assert!(pin_for(&pins, "anything", crate::Role::Review).is_none());
    }

    #[test]
    fn web_config_defaults_and_override() {
        // Single [web] table (renderdoc shape); absent section
        // = localhost defaults (progressive — tools fail-soft at call).
        let dir = tempfile::tempdir().unwrap();
        let cfg_file = write_toml(
            &dir,
            "c.toml",
            "[web]\nbase_url = \"http://127.0.0.1:4000\"\ntimeout_secs = 10\n",
        );
        let missing = Path::new("/nonexistent/config.toml");
        let cfg = load_with_cli(missing, &cfg_file, &CliOverrides::default()).unwrap();
        assert_eq!(cfg.web.base_url, "http://127.0.0.1:4000");
        assert_eq!(cfg.web.timeout_secs, 10);
        let bare = load_with_cli(missing, missing, &CliOverrides::default()).unwrap();
        assert_eq!(bare.web.base_url, "http://127.0.0.1:3000");
        assert_eq!(bare.web.timeout_secs, 30);
    }

    #[test]
    fn advisor_config_defaults_off_and_overrides() {
        // Absent section = disabled (spend-safe default).
        let dir = tempfile::tempdir().unwrap();
        let cfg_file = write_toml(&dir, "c.toml", "[advisor]\nenabled = true\n");
        let missing = Path::new("/nonexistent/config.toml");
        let cfg = load_with_cli(missing, &cfg_file, &CliOverrides::default()).unwrap();
        assert!(cfg.advisor.enabled);
        let bare = load_with_cli(missing, missing, &CliOverrides::default()).unwrap();
        assert!(!bare.advisor.enabled);
    }

    #[test]
    fn agent_config_defaults_and_override() {
        // [agent] max_tool_rounds defaults to 8 and can be overridden.
        let dir = tempfile::tempdir().unwrap();
        let cfg_file = write_toml(&dir, "c.toml", "[agent]\nmax_tool_rounds = 14\n");
        let missing = Path::new("/nonexistent/config.toml");
        let cfg = load_with_cli(missing, &cfg_file, &CliOverrides::default()).unwrap();
        assert_eq!(cfg.agent.max_tool_rounds, 14);
        let bare = load_with_cli(missing, missing, &CliOverrides::default()).unwrap();
        assert_eq!(bare.agent.max_tool_rounds, 30);
        // The LSP error gate is off unless the table says so.
        assert!(!cfg.agent.lsp_error_gate);
        assert!(!bare.agent.lsp_error_gate);
        let gated = write_toml(&dir, "g.toml", "[agent]\nlsp_error_gate = true\n");
        let cfg = load_with_cli(missing, &gated, &CliOverrides::default()).unwrap();
        assert!(cfg.agent.lsp_error_gate);
        assert_eq!(cfg.agent.max_tool_rounds, 30);
        // The verify gate is off (no commands) by default.
        assert!(bare.agent.verify_cmds.is_empty());
        assert_eq!(bare.agent.verify_timeout_secs, 600);
        let verified = write_toml(
            &dir,
            "v.toml",
            "[agent]
verify_cmds = [\"cargo build\", \"cargo test\"]
verify_timeout_secs = 90
",
        );
        let cfg = load_with_cli(missing, &verified, &CliOverrides::default()).unwrap();
        assert_eq!(cfg.agent.verify_cmds, ["cargo build", "cargo test"]);
        assert_eq!(cfg.agent.verify_timeout_secs, 90);
        assert!(!cfg.agent.lsp_error_gate);
        // Bounded verification bonus + CREATE seed on by default.
        assert_eq!(bare.agent.max_bonus_tool_rounds, 3);
        assert!(bare.agent.create_seed);
        let tuned = write_toml(
            &dir,
            "b.toml",
            "[agent]
max_bonus_tool_rounds = 0
create_seed = false
",
        );
        let cfg = load_with_cli(missing, &tuned, &CliOverrides::default()).unwrap();
        assert_eq!(cfg.agent.max_bonus_tool_rounds, 0);
        assert!(!cfg.agent.create_seed);
        assert_eq!(cfg.agent.max_tool_rounds, 30);
        // Failed drives roll back unless the table says keep.
        assert_eq!(bare.agent.drive_on_fail, "rollback");
        let kept = write_toml(&dir, "k.toml", "[agent]\ndrive_on_fail = \"keep\"\n");
        let cfg = load_with_cli(missing, &kept, &CliOverrides::default()).unwrap();
        assert_eq!(cfg.agent.drive_on_fail, "keep");
        // Output spilling and history compaction on by default.
        assert_eq!(bare.agent.tool_output_inline_bytes, 16 * 1024);
        assert_eq!(bare.agent.tool_output_keep_files, 200);
        assert_eq!(bare.agent.tool_history_budget_bytes, 256 * 1024);
        assert_eq!(bare.agent.tool_history_keep_rounds, 2);
        let bounded = write_toml(
            &dir,
            "o.toml",
            "[agent]
tool_output_inline_bytes = 0
tool_output_keep_files = 50
tool_history_budget_bytes = 65536
tool_history_keep_rounds = 3
",
        );
        let cfg = load_with_cli(missing, &bounded, &CliOverrides::default()).unwrap();
        assert_eq!(cfg.agent.tool_output_inline_bytes, 0);
        assert_eq!(cfg.agent.tool_output_keep_files, 50);
        assert_eq!(cfg.agent.tool_history_budget_bytes, 65536);
        assert_eq!(cfg.agent.tool_history_keep_rounds, 3);
        // Reviewer tools off by default, a small round budget.
        assert!(!bare.agent.reviewer_tools);
        assert_eq!(bare.agent.reviewer_tool_rounds, 4);
        let reviewing = write_toml(
            &dir,
            "r.toml",
            "[agent]
reviewer_tools = true
reviewer_tool_rounds = 2
",
        );
        let cfg = load_with_cli(missing, &reviewing, &CliOverrides::default()).unwrap();
        assert!(cfg.agent.reviewer_tools);
        assert_eq!(cfg.agent.reviewer_tool_rounds, 2);
        // Command change detection on by default, 0 turns it off.
        assert_eq!(bare.agent.command_change_max_files, 20_000);
        let off = write_toml(&dir, "d.toml", "[agent]\ncommand_change_max_files = 0\n");
        let cfg = load_with_cli(missing, &off, &CliOverrides::default()).unwrap();
        assert_eq!(cfg.agent.command_change_max_files, 0);
    }

    #[test]
    fn mcp_servers_parse_with_defaults() {
        // Servers are plain data (id/cmd/args); absent section
        // defaults to empty (MCP stays off unless configured — progressive,
        // Zechner shape).
        let dir = tempfile::tempdir().unwrap();
        let cfg_file = write_toml(
            &dir,
            "c.toml",
            "[[mcp_servers]]\nid = \"ara\"\ncmd = \"ara\"\nargs = [\"mcp\"]\n",
        );
        let missing = Path::new("/nonexistent/config.toml");
        let cfg = load_with_cli(missing, &cfg_file, &CliOverrides::default()).unwrap();
        assert_eq!(cfg.mcp_servers.len(), 1);
        assert_eq!(cfg.mcp_servers[0].id, "ara");
        assert_eq!(cfg.mcp_servers[0].cmd, "ara");
        assert_eq!(cfg.mcp_servers[0].args, vec!["mcp".to_string()]);
        let bare = load_with_cli(missing, missing, &CliOverrides::default()).unwrap();
        assert!(bare.mcp_servers.is_empty());
    }

    #[test]
    fn lsp_servers_parse_with_defaults() {
        // Same progressive shape as MCP (absent section =
        // empty = LSP off), plus the language list L2's pool keys on.
        let dir = tempfile::tempdir().unwrap();
        let cfg_file = write_toml(
            &dir,
            "c.toml",
            "[[lsp_servers]]\nid = \"rust\"\ncmd = \"rust-analyzer\"\nlanguages = [\"rust\"]\n",
        );
        let missing = Path::new("/nonexistent/config.toml");
        let cfg = load_with_cli(missing, &cfg_file, &CliOverrides::default()).unwrap();
        assert_eq!(cfg.lsp_servers.len(), 1);
        assert_eq!(cfg.lsp_servers[0].id, "rust");
        assert_eq!(cfg.lsp_servers[0].cmd, "rust-analyzer");
        assert_eq!(cfg.lsp_servers[0].languages, vec!["rust".to_string()]);
        let bare = load_with_cli(missing, missing, &CliOverrides::default()).unwrap();
        assert!(bare.lsp_servers.is_empty());
    }
}
