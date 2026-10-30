<p align="center">
  <a href="https://www.rust-lang.org"><img src="https://img.shields.io/badge/rust-1.88%2B-orange?logo=rust" alt="Rust 1.88+"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-AGPLv3-blue" alt="License: AGPL-3.0-only"></a>
  <img src="https://img.shields.io/badge/interfaces-CLI%20%C2%B7%20MCP-brightgreen" alt="CLI · MCP">
</p>

<h1 align="center">Polymorph</h1>

<p align="center">A Rust-native execution substrate for AI coding harnesses.<br>
Clean-context reviewer loops, content-addressed hashline edits, role-routed model chains,
and a 32-server LSP matrix.</p>

<p align="center">
  <a href="#install">Install</a> ·
  <a href="#connect-a-harness">MCP</a> ·
  <a href="#cli-reference">CLI</a> ·
  <a href="#configuration">Configuration</a> ·
  <a href="docs/INTERPOSER.md">Interposer contract</a> ·
  <a href="docs/WORKFLOWS.md">Workflows</a>
</p>

---

Polymorph executes one engineering goal at a time under a verified contract. A fixer turn
edits code; the harness runs the commands you declared and collects diagnostics from your
configured LSP servers; a fresh, clean-context reviewer grades the result against your
acceptance criteria and returns a strict JSON verdict; an in-place drive that does not pass
restores the files it touched by default. With `--events-json`, every round is recorded as a
machine-readable ledger.

It is not a chat application. The harness — Claude Code, Codex, Cursor, or any MCP or CLI
client — holds the conversation and decides what to build. Polymorph holds execution and
evidence.

## Install

Version 1.0.0 is built and installable from source. The crates.io publication is pending;
once it ships, the install is:

```bash
cargo install polymorph-swe      # binary: polymorph
```

From source today:

```bash
git clone https://github.com/walkthestars/polymorph.git
cd polymorph
cargo build --release
# binary: target/release/polymorph (target\release\polymorph.exe on Windows)
```

## Connect a harness

MCP over stdio, one tool named `drive`:

```json
{"mcpServers":{"polymorph":{"command":"polymorph","args":["mcp"]}}}
```

The CLI exposes the same contract:

```bash
polymorph review drive \
  --goal "Reject empty frames in FrameBuffer::slice" \
  --artifact-file src/buffer.rs \
  --criteria "1) an empty frame returns Err 2) existing tests pass" \
  --tools \
  --verify-cmd "cargo test" \
  --events-json .polymorph/events.json
```

A successful drive exits `0`; with `--events-json`, the ledger contains a `finished` event
whose verdict is `PASSED` (a `budget` event follows it). Every other outcome returns the
reviewer findings; an in-place drive that does not pass restores the files it changed by
default. The full event and exit-code contract is in
[docs/INTERPOSER.md](docs/INTERPOSER.md).

## What it provides

- **Clean-context review.** The reviewer never sees the fixer's reasoning trace: it grades
  the goal, criteria, numbered artifact text, peripheral diffs, command ledger,
  `--verify-cmd` output, and LSP diagnostics, then returns `Pass` or `Fail` with P0–P3
  findings.
- **Deterministic gates.** A failing `--verify-cmd` or an error-severity LSP diagnostic
  (`--lsp-error-gate`) vetoes a passing verdict in the harness, before the reviewer can
  approve it.
- **Content-addressed hashline edits.** `read` mints a 2-character content hash per line;
  `edit` verifies those hashes before applying replacements, insertions, or deletions, so
  stale line numbers cannot silently corrupt a file.
- **Rollback by default.** An in-place `review drive --tools` snapshots original bytes
  before the first mutation and restores them byte-for-byte when the drive fails,
  escalates, or is interrupted, archiving the attempt under `.polymorph/attempts/`.
- **Machine-readable ledger.** `--events-json` records rounds, verdicts, findings, budget,
  and the terminal verdict as a JSON array that an orchestrator or a human can read.
- **Role-routed models.** `default`, `smol`, `plan`, `act`, and `review` roles each map to a
  fallback chain; `[[pins]]` select chains by path prefix; `api_key_cmd` injects secrets per
  attempt; 429s and transient transport errors cool down and fall back automatically.
- **Plan execution.** A `PLAN.md` task DAG can be validated offline and driven task-by-task
  in topological order with checkpoints.

## CLI reference

The complete flag reference is in [docs/CLI.md](docs/CLI.md).

| Command | Description |
|---------|-------------|
| `polymorph mcp` | MCP stdio server exposing the bounded `drive` tool. |
| `polymorph review drive` | Drive one goal to a verdict over a file, artifact set, or worktree diff (`--tools`, `--verify-cmd`, `--lsp-error-gate`, `--reviewer-tools`, `--events-json`). |
| `polymorph review plan-drive` | Drive a `PLAN.md` DAG task-by-task with write-back and optional checkpoint resume. |
| `polymorph review plan-show` | Validate and display a `PLAN.md` task board offline. |
| `polymorph review run` | Run the live reviewer/fixer loop and print the human summary board. |
| `polymorph review check` | Validate a reviewer verdict JSON file offline. |
| `polymorph review demo` | Run the reviewer loop offline against scripted doubles. |
| `polymorph review eval-ab` | Compare two reviewer models on one seeded artifact. |
| `polymorph review bench`, `bench-check` | Run the agent-bench corpus live, or re-score recordings offline. |
| `polymorph exec` | Run one headless prompt turn and stream the response. |
| `polymorph sessions` | List, create, switch, or delete SQLite sessions. |
| `polymorph config`, `providers`, `models` | Inspect and update configuration, providers, and the model catalog. |
| `polymorph delegate run` | Exercise the subagent bus with echo workers (offline demo). |
| `polymorph eval run`, `eval check` | Run one model-output evaluation, or verify a recording offline. |
| `polymorph update-catalog` | Refresh the cached `models.dev` capability catalog. |

`polymorph` with no subcommand prints this command surface and exits.

### Watch a drive

A separate, optional binary renders a drive ledger in the terminal:

```bash
cargo build --release -p polymorph-watch
polymorph-watch --events .polymorph/events.json --follow
```

It shows rounds, verdicts, findings, budget, and the final status. It does not call a model
and is not part of the `polymorph` binary.

## Architecture

```text
  MCP client (drive tool)  │  CLI (review drive / plan-drive / exec)
  ─────────────────────────┴──────────────────────────────────────────
                                   │
                                   ▼
                ┌─────────────────────────────────────────────────┐
                │             Clean-Context Reviewer Loop         │
                │                                                 │
                │  1. Fixer turn (act role)                       │
                │     • hashline edits, atomic writes             │
                │     • tool Gate, MCP pool, LSP, DAP             │
                │     • pre-mutation snapshots                    │
                │                     │                           │
                │                     ▼                           │
                │  2. Harness gates                               │
                │     • --verify-cmd in order                     │
                │     • LSP diagnostics on mutated files          │
                │     • peripheral diffs, command ledger          │
                │                     │                           │
                │                     ▼                           │
                │  3. Clean-context reviewer (review role)        │
                │     • strict JSON verdict, P0–P3 findings       │
                │     • survival tracker, rollback on non-pass    │
                └─────────────────────────────────────────────────┘
```

## Configuration

Configuration merges four layers, lowest to highest precedence:

1. **User:** `~/.config/polymorph/config.toml`
2. **Project:** `./.polymorph/config.toml`
3. **Environment:** `POLYMORPH_*` and provider key variables
4. **CLI flags:** `--db`, `--debug`, and subcommand flags

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

Providers, role chains, pins, permissions, hooks, MCP enrollment, LSP and DAP adapters, and
web search are documented in [docs/CONFIGURATION.md](docs/CONFIGURATION.md).

## Workspace layout

| Crate | Responsibility |
|-------|----------------|
| `polymorph-core` | Configuration, sessions, shared types, credential pools, path pins |
| `polymorph-llm` | Streaming model client, role routing, 429 cooldown and fallback, key injection, catalog |
| `polymorph-tools` | Tool registry, permission Gate, hashline editor, output spill, MCP client pool, LSP and DAP clients, hooks |
| `polymorph-delegate` | Subagent bus, turn budgets, worktree isolation |
| `polymorph-review` | Reviewer loop, convergence tracker, artifact sets, diff scopes, verify and LSP gates, plan driver |
| `polymorph-tasks` | Task DAG model and `PLAN.md` parser/renderer |
| `polymorph-skills` | `SKILL.md` parsing and progressive disclosure |
| `polymorph-drive` | Headless drive protocol: intents, events, driver contract |
| `polymorph-prompts` | Default prompt packs, embedded in the binary |
| `polymorph-mcp` | MCP stdio server and the bounded `drive` tool |
| `polymorph-swe` | `polymorph` binary: CLI, MCP wiring, review dispatch |
| `polymorph-watch` | Separate `polymorph-watch` binary: read-only ledger viewer |

## Development

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

## Contributing

Contributions are welcome; see [CONTRIBUTING.md](CONTRIBUTING.md). Each commit needs a
`Signed-off-by` trailer (the [Developer Certificate of Origin](DCO), `git commit -s`), and
each contributor accepts the [Contributor License Agreement](CLA.md) once on their first
pull request.

## License

AGPL-3.0-only, with commercial and enterprise licensing available from the Project Owner.
See [LICENSE](LICENSE), [COPYRIGHT](COPYRIGHT), [CLA.md](CLA.md), and
[docs/LICENSING.md](docs/LICENSING.md) for what the AGPL permits, when the commercial
license is required, and why contributions carry a CLA.
