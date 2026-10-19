# Polymorph Configuration Guide

Polymorph uses layered TOML configuration powered by `figment`.

## Precedence Order (Lowest to Highest)

1. **User configuration**: `~/.config/polymorph/config.toml` (or `%USERPROFILE%\.config\polymorph\config.toml` on Windows)
2. **Project configuration**: `./.polymorph/config.toml`
3. **Environment variables**: `POLYMORPH_*` and provider API key variables
4. **CLI flags**: `--db`, `--debug`, `--theme`, and subcommand flags

Inspect the active merged configuration at any time with:

```bash
polymorph config show
```

---

## 1. Providers & Secret Injection (`[[providers]]`)

Each `[[providers]]` entry registers an OpenAI-compatible or native LLM endpoint and maps how credentials are supplied.

```toml
[[providers]]
id = "openrouter"
base_url = "https://openrouter.ai/api/v1"
api_key_env = "OPENROUTER_API_KEY"
# Optional: execute a command per attempt to fetch the key (stdout trimmed, never logged)
api_key_cmd = "op read op://Personal/OpenRouter/credential"
# Optional: round-robin pool of keys or commands for 429 rotation
keys = ["env:OPENROUTER_KEY_1", "env:OPENROUTER_KEY_2"]
```

- **Credential security**: Key values resolved from `api_key_cmd` or `keys` are injected via `ScopedEnv` for the duration of a single model attempt and are redacted from all `Debug` formatting and `tracing` logs.

---

## 2. Role Chains, Reasoning Effort & Path Pins (`[roles]`, `[effort]`, `[[pins]]`)

Polymorph routes turns by role (`default`, `smol`, `plan`, `act`, `review`). Each role maps to an ordered fallback chain of `provider::model` specs (or bare model names resolved against claiming providers).

On HTTP `429 Too Many Requests` or transient upstream transport errors, the router cools down the affected model/credential and falls back to the next entry in the chain, restoring the primary automatically once its cooldown expires.

```toml
[roles]
default = ["openrouter::deepseek/deepseek-chat", "openrouter::google/gemini-2.5-flash"]
act     = ["openrouter::deepseek/deepseek-chat"]
review  = ["openrouter::anthropic/claude-sonnet-4"]
plan    = ["openrouter::anthropic/claude-sonnet-4"]
smol    = ["openrouter::google/gemini-2.5-flash"]

[effort]
# Valid values: "low", "medium", "high"
review = "high"
plan   = "high"
act    = "medium"

[[pins]]
# Longest matching path_prefix wins when --scope <path> is provided
path_prefix = "crates/polymorph-review"
role        = "act"
chain       = ["openrouter::anthropic/claude-sonnet-4"]
```

---

## 3. Permissions & Tool Gate (`[permissions]`)

Controls which tools agentic turns (`--tools`) are allowed to invoke.

```toml
[permissions]
allow = ["read", "write", "edit", "grep", "glob", "bash"]
deny  = [
  "bash:rm",
  "bash:git push",
  "bash:git reset",
  "bash:git clean",
]
```

- **Deny beats allow**: Any tool invocation matching a prefix in `deny` is rejected before execution, even if a broader prefix is listed in `allow`.
- **Command-specific rules**: For `bash`, rules match `"bash:<command_prefix>"` against the trimmed command string.

---

## 4. Agent & Reviewer-Loop Hardening (`[agent]`, `[advisor]`)

Configures the behavior of `polymorph review drive` and `polymorph review run`:

```toml
[agent]
# Maximum fixer tool-call rounds per reviewer round (default: 30)
max_tool_rounds = 30

# Additional bonus tool rounds granted while the fixer is actively mutating or verifying (default: 3)
max_bonus_tool_rounds = 3

# Synthesize round-1 creation finding automatically when target artifact does not exist yet (default: true)
create_seed = true

# Veto passing verdicts when mutated files still carry error-severity LSP diagnostics (default: false)
lsp_error_gate = true

# Grant the reviewer read-only workspace & LSP inspection tools before emitting a verdict (default: false)
reviewer_tools = true
reviewer_tool_rounds = 4

# Shell commands executed after every fixer turn before the reviewer grades; non-zero exit vetoes Pass
verify_cmds = ["cargo test --workspace", "cargo clippy --workspace --all-targets -- -D warnings"]
verify_timeout_secs = 600

# In-place failure handling: "rollback" (restores pre-drive file bytes) or "keep" (default: "rollback")
drive_on_fail = "rollback"

# Tool output spill and context compaction budgets (bytes / rounds)
tool_output_inline_bytes = 16384
tool_output_keep_files   = 200
tool_history_budget_bytes = 262144
tool_history_keep_rounds  = 2

[advisor]
# Optional clean-context watching advisor mounted alongside the fixer (ASIDE / CONCERN / BLOCKER)
enabled = false
```

---

## 5. Script Hooks (`[[hooks]]`)

Run shell commands on `pre` (before a tool call), `post` (after a tool call), or `stop` (when a turn finishes).

```toml
[[hooks]]
event = "pre"
command = "python scripts/check_tool_policy.py"
timeout_secs = 10
```

- Exit code `0`: allow the tool call.
- Exit code `2` (or any non-zero exit): deny the tool call and feed `stderr` back as the rejection reason.

---

## 6. MCP Servers (`[[mcp_servers]]`)

Enrolls external Model Context Protocol (MCP) stdio servers into the tool registry. Servers spawn lazily on first call and tools are exposed under `<id>_<tool_name>` (governed by `[permissions]`).

```toml
[[mcp_servers]]
id = "toothpick"
cmd = "uv"
args = ["--directory", "C:/path/to/toothpick", "run", "toothpick", "mcp", "--db", "C:/path/to/toothpick.db"]
```

---

## 7. Language Servers (`[[lsp_servers]]`) & Debug Adapters (`[[dap_adapters]]`)

Enrolls LSP servers for post-write diagnostic checks and reviewer/fixer symbol queries (`lsp_hover`, `lsp_definition`, `lsp_references`, `lsp_diagnostics`), and DAP debug adapters for runtime breakpoint inspection (`dap_start`, `dap_inspect`, `dap_evaluate`, `dap_disconnect`).

```toml
[[lsp_servers]]
id = "rust"
cmd = "rust-analyzer"
languages = ["rust"]

[[lsp_servers]]
id = "python"
cmd = "basedpyright-langserver"
args = ["--stdio"]
languages = ["python"]

[[dap_adapters]]
id = "python"
cmd = "python"
args = ["-m", "debugpy.adapter"]
languages = ["python"]
```

See [docs/lsp-servers.md](lsp-servers.md) and [docs/dap-adapters.md](dap-adapters.md) for the full 35-language roster and adapter setup details.

---

## 8. Web Search & URI Schemes (`[web]`)

Connects `web_search`, `web_extract`, and `read` URI schemes (`web://`, `https://`, `gh://`, `npm://`, `arxiv://`) to a local [`serica`](https://github.com/walkthestars/serica) HTTP instance:

```toml
[web]
base_url = "http://127.0.0.1:3000"
timeout_secs = 30
```
