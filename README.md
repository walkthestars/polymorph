<p align="center">
  <a href="https://www.rust-lang.org"><img src="https://img.shields.io/badge/rust-1.88%2B-orange?logo=rust" alt="Rust 1.88+"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-AGPLv3-blue" alt="License: AGPL-3.0-only"></a>
  <img src="https://img.shields.io/badge/interfaces-CLI%20%C2%B7%20TUI%20%C2%B7%20ACP%20%C2%B7%20REST-brightgreen" alt="CLI · TUI · ACP · REST">
</p>

<h1 align="center">Polymorph</h1>

<p align="center">A Rust-native AI coding agent and headless SWE compiler.<br>
Clean-context reviewer loops, content-addressed hashline edits, role-routed model chains, and 35-language LSP enforcement.</p>

<p align="center">
  <a href="#quick-start">Quick start</a> ·
  <a href="#architecture">Architecture</a> ·
  <a href="#cli-reference">CLI</a> ·
  <a href="#configuration">Configuration</a> ·
  <a href="docs/CLI.md">Full CLI Reference</a> ·
  <a href="docs/CONFIGURATION.md">Configuration Guide</a> ·
  <a href="docs/WORKFLOWS.md">Workflows</a>
</p>

---

Polymorph is a single-binary AI software engineering engine written in Rust. Instead of relying on a single long-context chat session to both write and self-grade code, Polymorph separates **Act** (an agentic fixer wielding `read`, `write`, `edit`, `grep`, `glob`, `bash`, MCP, LSP, and DAP tools) from **Review** (an independent, clean-context reviewer that inspects the target artifact set, peripheral file diffs, command execution ledgers, verification command outputs, and LSP diagnostics before emitting a strict JSON verdict).

You can drive Polymorph headlessly from an orchestrator or CI script (`polymorph review drive`, `polymorph review plan-drive`, `polymorph exec`), interactively in its Ratatui terminal UI (`polymorph`), inside editors over the Agent Client Protocol (`polymorph acp` for Zed and Neovim), or over HTTP/SSE (`polymorph serve`).

## Quick start

### Prerequisites

- **Rust**: **1.88+** (stable toolchain)
- **OS**: Windows 10/11 (x64), Linux, or macOS

### Build from source

```bash
git clone https://github.com/walkthestars/polymorph.git
cd polymorph
cargo build --release
```

The compiled binary is written to `target/release/polymorph` (`target\release\polymorph.exe` on Windows).

### Verify offline

```bash
# Run the offline reviewer-loop demonstration (no API key required)
polymorph review demo --mode plan --script fail-once

# Run the subagent fan-out bus demonstration
polymorph delegate run --children 2 --task "probe"

# Run the workspace test suite
cargo test --workspace
```

### Run a live turn or verified drive

Configure a provider in `~/.config/polymorph/config.toml` (or `.polymorph/config.toml` in your project root) and set your API key in the environment:

```bash
# Single headless turn
polymorph exec "Summarize the public API in src/lib.rs"

# Drive a code change through the clean-context reviewer loop with compiler/test verification
polymorph review drive \
  --goal "Add bounds checking to FrameBuffer::slice and unit tests" \
  --artifact-file src/buffer.rs \
  --tools \
  --lsp-error-gate \
  --verify-cmd "cargo test --workspace" \
  --events-json drive-events.json
```

## Architecture

```text
  Interactive TUI (polymorph)  │  Headless CLI (review drive / plan-drive / exec)  │  ACP / HTTP
  ─────────────────────────────┴─────────────────────────┬─────────────────────────┴────────────
                                                         │
                                                         ▼
                ┌─────────────────────────────────────────────────────────────────┐
                │                 Clean-Context Reviewer Loop                     │
                │                                                                 │
                │   1. Fixer Turn (Act Role)                                      │
                │      • Hashline edits (content-addressed line tags, atomic IO)  │
                │      • Tool Gate (allow/deny prefixes, pre/post/stop hooks)     │
                │      • MCP stdio pool, LSP post-write consult, DAP debugger     │
                │      • Pre-mutation RestoreLedger snapshots                     │
                │                          │                                      │
                │                          ▼                                      │
                │   2. Harness Verification Gates                                 │
                │      • Runs repeatable --verify-cmd in workdir                  │
                │      • Collects LSP diagnostics & command-mutation ledger       │
                │      • Attaches peripheral mutated file diffs                   │
                │                          │                                      │
                │                          ▼                                      │
                │   3. Clean-Context Reviewer Turn (Review Role)                  │
                │      • Grades artifact set + harness evidence against criteria  │
                │      • Optional read-only --reviewer-tools inspection           │
                │      • Strict JSON Verdict (Pass / Fail with P0–P3 findings)    │
                │      • Consecutive-survival tracker & automatic rollback        │
                └─────────────────────────────────────────────────────────────────┘
```

### Core capabilities

- **Clean-context review & deterministic gates (`polymorph-review`)**: The reviewer never sees the fixer's noisy reasoning trace — only the goal, acceptance criteria, numbered artifact text, peripheral diffs, command ledger (highlighting non-zero exits, zero-test runs, or unverified post-command mutations), `--verify-cmd` output digests, and LSP diagnostics. Any failing `--verify-cmd` or error-severity LSP diagnostic (`--lsp-error-gate`) deterministically vetoes a false `Pass` verdict.
- **Content-addressed hashline editing (`polymorph-tools`)**: `read` mints 2-character content hashes per line (`14:a3| fn main() {`). `edit` verifies those hashes before applying replacements, insertions, or deletions via atomic temporary-file renames, eliminating stale line-number drift.
- **Automatic rollback on non-passing drives**: In-place `review drive --tools` runs snapshot original file bytes before first mutation and automatically restore the working tree byte-for-byte if the drive escalates, fails, or is interrupted with `Ctrl-C` (archiving the failed attempt under `.polymorph/attempts/`).
- **Role-routed model chains & 429 resilience (`polymorph-llm`)**: Route `default`, `smol`, `plan`, `act`, and `review` roles to distinct model fallback chains, pin specific directory prefixes (`[[pins]]`) to specialized models, rotate round-robin credential pools, and inject secrets per attempt via `api_key_cmd`.
- **Task DAG execution (`polymorph-tasks`)**: Parse structured `PLAN.md` dependency graphs (`polymorph review plan-show`) and drive every task in topological ready-order (`polymorph review plan-drive`) with SQLite state persistence.

## CLI reference

See [docs/CLI.md](docs/CLI.md) for the complete subcommand and flag reference.

| Command | Description |
|---------|-------------|
| `polymorph` | Launch the interactive Ratatui terminal UI (chat, file picker, diff pane, tasks board, LSP gutter, `/attach` vision sidecar). |
| `polymorph exec <prompt>` | Run a single headless prompt turn (`--model`, `--system`, `--scope`) and stream the response to stdout. |
| `polymorph review drive` | Drive a single file (`--artifact-file`), glob set (`--artifact-files`), or git diff (`--diff`) through the reviewer/fixer loop (`--tools`, `--verify-cmd`, `--lsp-error-gate`, `--reviewer-tools`, `--repo`, `--on-fail rollback\|keep`, `--events-json`). |
| `polymorph review run` | Run the reviewer/fixer loop and print the human summary board. |
| `polymorph review plan-show --plan <PLAN.md>` | Parse and validate a `PLAN.md` task DAG and display its topological execution board. |
| `polymorph review plan-drive --plan <PLAN.md> --out <OUT.md>` | Drive each ready task in a `PLAN.md` DAG through per-task reviewer loops with write-back and optional `--checkpoint` resume. |
| `polymorph review check <verdict.json>` | Validate a reviewer verdict JSON file offline. |
| `polymorph review demo` | Run the reviewer loop offline against scripted doubles (`--mode plan\|act`, `--script pass\|fail-once\|never\|survivor`). |
| `polymorph review eval-ab` | Compare two reviewer models (`--review-model-a`, `--review-model-b`) on the same seeded artifact. |
| `polymorph acp` | Start the Agent Client Protocol stdio server (`--tools`, `--max-tool-rounds`, `--act-pack`) for Zed and Neovim. |
| `polymorph serve` | Start the HTTP REST and Server-Sent Events (SSE) session server (`--port 8420`). |
| `polymorph sessions list\|create\|switch\|delete` | Manage persistent SQLite sessions (`.polymorph/sessions.db`). |
| `polymorph config show\|set` | Inspect merged configuration or update project settings (`options.debug`, `roles.<role>`, `permissions.*`). |
| `polymorph providers list` / `polymorph models list` | Inspect configured LLM providers and the capability catalog (`polymorph update-catalog`). |
| `polymorph delegate run` | Fan out isolated subagent workers (`--children`, `--task`, `--repo`). |
| `polymorph eval run\|check` | Run or offline-verify model edit evaluations. |

## Configuration

Polymorph merges configuration across four layers (lowest to highest precedence):

1. **User config**: `~/.config/polymorph/config.toml`
2. **Project config**: `./.polymorph/config.toml`
3. **Environment variables**: `POLYMORPH_*` and provider API key variables
4. **CLI flags**: `--db`, `--debug`, `--theme`, and subcommand flags

```toml
# ~/.config/polymorph/config.toml
[[providers]]
id = "openrouter"
base_url = "https://openrouter.ai/api/v1"
api_key_env = "OPENROUTER_API_KEY"
# Optional command-based secret injection (stdout trimmed, never logged):
# api_key_cmd = "op read op://vault/openrouter/token"

[roles]
default = ["openrouter::deepseek/deepseek-chat"]
act     = ["openrouter::deepseek/deepseek-chat"]
review  = ["openrouter::anthropic/claude-sonnet-4"]
smol    = ["openrouter::google/gemini-2.5-flash"]

[effort]
review = "high"

[permissions]
allow = ["read", "grep", "glob", "write", "edit", "bash"]
deny  = ["bash:rm", "bash:git push", "bash:git reset"]

[agent]
max_tool_rounds = 30
lsp_error_gate  = true
reviewer_tools  = true
drive_on_fail   = "rollback"

[[lsp_servers]]
id = "rust"
cmd = "rust-analyzer"
languages = ["rust"]
```

Full configuration tables for MCP servers (`[[mcp_servers]]`), 35-language LSP routing (`[[lsp_servers]]`), DAP debug adapters (`[[dap_adapters]]`), script hooks (`[[hooks]]`), path pins (`[[pins]]`), and Serica web search (`[web]`) are documented in [docs/CONFIGURATION.md](docs/CONFIGURATION.md).

## Workspace layout

| Crate | Responsibility |
|-------|----------------|
| `polymorph-core` | `Role`, `Verdict`, `LoopConfig`, layered Figment config, path pins, credential pools, SQLite session store & read ledger |
| `polymorph-llm` | Streaming LLM client, `RoleRouter`, two-phase 429 cooldown/fallback, `ScopedEnv` key injection, output rule matchers, `models.dev` catalog |
| `polymorph-tools` | Tool `Registry` & permission `Gate`, content-addressed `hashline` editor, spill-to-disk output digests, MCP stdio pool, LSP client, DAP client, script hooks, git split planner, evolution harness |
| `polymorph-delegate` | Subagent `Bus` (`spawn`/`steer`/`stop`), turn budgets, `JoinSet` fan-out, git worktree isolation |
| `polymorph-review` | Clean-context reviewer loop, survival convergence tracker, multi-artifact & git-diff scopes, `VerifyGateRunner`, `LspGateRunner`, `ToolReviewTurnRunner`, `RestoreLedger`, `PLAN.md` DAG driver |
| `polymorph-tasks` | Task DAG model (`ready`/`topo`/`merge` order, checkpoints) and `PLAN.md` parser/renderer |
| `polymorph-skills` | `SKILL.md` frontmatter parser and progressive disclosure loader |
| `polymorph-tui` | Ratatui terminal interface (chat, picker, diff, tasks, LSP gutter, ask-user modal, themes) |
| `polymorph-acp` | Stdio Agent Client Protocol (`agent-client-protocol`) server |
| `polymorph-web` | Serica HTTP search/extract client, `web://`/`gh://`/`npm://`/`arxiv://` scheme handlers, and `polymorph serve` HTTP/SSE server |
| `polymorph-renderdoc` | RenderDoc frame capture and draw/state introspection bindings |
| `polymorph-cli` | `clap` CLI front door and split ANSI/JSON rolling tracing setup |

## Development

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

## Contributing

Contributions are welcome — see [CONTRIBUTING.md](CONTRIBUTING.md). Two things are asked of contributors: a `Signed-off-by` line on every commit (the [Developer Certificate of Origin](DCO), `git commit -s`; CI checks it), and a one-time acceptance of the [Contributor License Agreement](CLA.md) on a contributor's first pull request.

## License

AGPL-3.0-only, with commercial/enterprise dual licensing available. See [LICENSE](LICENSE), [COPYRIGHT](COPYRIGHT), and [CLA.md](CLA.md).

Running unmodified Polymorph locally places no obligations on your own code or repositories. Modifying Polymorph, distributing modified versions, or hosting a modified Polymorph over a network (including `polymorph serve`, `polymorph acp`, or networked CI/CD orchestration) triggers the AGPL-3.0 copyleft/network provisions — offer those modifications under the same license, or contact the Project Owner (`walkthestars`) for a commercial enterprise license.
