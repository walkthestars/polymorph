# Polymorph CLI Reference

Running `polymorph` with no subcommand starts the interactive terminal UI (TUI). Pass a subcommand for headless execution, review loops, editor integration, or configuration management.

---

## Global Options

| Flag | Default | Description |
|------|---------|-------------|
| `--db <PATH>` | `./.polymorph/sessions.db` | SQLite session database path |
| `--debug` | `false` | Force `options.debug = true` (highest config precedence) |
| `--theme <NAME>` | `default` | TUI color theme (`default`, `nord`, `gruvbox`, `catppuccin`, `solarized`, `dracula`, `monokai`, `tokyo-night`) |
| `--completions <SHELL>` | — | Emit shell completions (`bash`, `zsh`, `fish`, `powershell`, `elvish`) to stdout and exit |
| `-V`, `--version` | — | Print version and exit |

---

## 1. Headless Prompt Execution (`polymorph exec`)

Runs a single prompt turn, streams the model output to `stdout`, and exits.

```bash
polymorph exec <PROMPT> [--model <SPEC>] [--system <TEXT>] [--scope <PATH_PREFIX>]
```

- `--model <SPEC>` — Override the configured `default` role chain (accepts bare model names resolved through configured entries or qualified `provider::model` specs).
- `--system <TEXT>` — Override the default system prompt.
- `--scope <PATH_PREFIX>` — Select a path-pinned model chain from `[[pins]]`.

---

## 2. Clean-Context Reviewer Loops (`polymorph review`)

### `polymorph review drive`

Drives an engineering goal through iterative Fixer → Verification Gate → Clean-Context Reviewer rounds until the reviewer emits `Pass` with zero blocking findings, or until `--max-rounds` / 3-strike survivor escalation halts the loop.

```bash
polymorph review drive --goal <TEXT> [scope flags] [execution flags]
```

#### Artifact scope flags (at least one required)

| Flag | Description |
|------|-------------|
| `--artifact-file <PATH>` | Primary file under review. In `CREATE` jobs where `<PATH>` does not yet exist on disk, round 1 automatically seeds the creation finding and starts the fixer immediately (unless `--no-create-seed` is set). |
| `--artifact-files <PATH\|GLOB>` | Repeatable file path or workdir-relative glob (`src/**/*.rs`). Re-expanded after every fixer turn and graded together under `ARTIFACT FILES UNDER REVIEW`. |
| `--diff [<BASE>]` | Grade all files modified, added, deleted, or untracked relative to `<BASE>` (defaults to `HEAD`, or the worktree start commit when `--repo` is used). |

#### Loop and verification flags

| Flag | Default | Description |
|------|---------|-------------|
| `--criteria <TEXT>` | `""` | Explicit acceptance criteria threaded to both the fixer and reviewer |
| `--mode act\|plan` | `act` | `act` critiques code/files; `plan` critiques a `PLAN.md` specification |
| `--tools` | `false` | Grant the fixer access to the tool registry (`read`, `write`, `edit`, `grep`, `glob`, `bash`, MCP, LSP, DAP) governed by `[permissions]` |
| `--max-rounds <N>` | `3` | Maximum Reviewer ↔ Fixer rounds |
| `--max-tool-rounds <N>` | `30` | Maximum tool-call rounds per fixer turn (plus up to `max_bonus_tool_rounds` verification rounds when making progress) |
| `--verify-cmd <CMD>` | `[agent] verify_cmds` | Repeatable shell command run in the workdir after every fixer turn before the reviewer grades. Non-zero exit or timeout deterministically vetoes a `Pass` verdict |
| `--verify-timeout-secs <SECS>` | `600` | Per-command timeout for `--verify-cmd` (reaps the full process tree on expiry) |
| `--lsp-error-gate` | `[agent] lsp_error_gate` | Veto any `Pass` verdict while mutated files still carry error-severity (`severity == 1`) LSP diagnostics |
| `--reviewer-tools` | `[agent] reviewer_tools` | Allow the reviewer up to `reviewer_tool_rounds` (default `4`) read-only inspection turns (`read`, `grep`, `glob`, `lsp_hover`, `lsp_definition`, `lsp_references`) before emitting its verdict |
| `--on-fail rollback\|keep` | `rollback` (`drive`), `keep` (`run`) | On non-passing outcome or `Ctrl-C`, `rollback` restores mutated files byte-for-byte from pre-drive snapshots and archives the failed attempt under `.polymorph/attempts/` |
| `--repo <DIR>` | — | Run the entire drive inside a temporary detached git worktree of `<DIR>`, leaving the host working tree untouched |
| `--scope <PREFIX>` | — | Path prefix for `[[pins]]` model routing |
| `--review-pack <PATH>` | `prompts/muse/muse-spark-1.3/review.md` | Reviewer prompt pack path |
| `--exec-pack <PATH>` | `prompts/glm/glm-5.3/act.md` | Act-mode fixer prompt pack path |
| `--plan-pack <PATH>` | `prompts/glm/glm-5.3/plan.md` | Plan-mode fixer prompt pack path |
| `--no-create-seed` | `false` | Call the LLM reviewer in round 1 even when the target artifact file does not exist yet |
| `--events-json <PATH>` | — | Write machine-readable `DriveEvent` records as JSON to `<PATH>` |

### `polymorph review run`

Accepts the same `ReviewRunArgs` flags as `review drive` (except `--events-json`, and `--on-fail` defaults to `keep`) and prints the human summary board.

### `polymorph review plan-show`

Parses and validates a `PLAN.md` task DAG offline and prints the topological task board (`Ready`, `Blocked`, `Done`, `MergeOrder`).

```bash
polymorph review plan-show --plan PLAN.md [--session <SESSION_ID>]
```

### `polymorph review plan-drive`

Drives every task in a `PLAN.md` DAG through per-task reviewer loops in topological ready-order and writes the updated plan to `--out`.

```bash
polymorph review plan-drive \
  --plan PLAN.md \
  --out PLAN.out.md \
  [--checkpoint checkpoint.json] \
  [--session <SESSION_ID>] \
  [--max-rounds 3]
```

### `polymorph review check <PATH>`

Validates a reviewer verdict JSON file offline and exits non-zero if malformed.

### `polymorph review demo`

Runs the reviewer loop offline over scripted test doubles (`--mode plan|act`, `--script pass|fail-once|never|survivor`).

### `polymorph review eval-ab`

Runs the same seeded artifact through two live reviewer arms (`--review-model-a` vs `--review-model-b`) and compares convergence and findings.

### `polymorph review bench` & `polymorph review bench-check`

Runs or offline-scores the multi-task agent benchmark suite (`--path`, `--min-rate`).

---

## 3. Editor & Server Gateways

### `polymorph acp`

Starts the Agent Client Protocol (ACP) stdio server for editor integrations (Zed, Neovim, JetBrains).

```bash
polymorph acp [--tools] [--max-tool-rounds 30] [--act-pack /abs/path/to/prompts/glm/glm-5.3/act.md]
```

> **Editor cwd note**: Editors spawn ACP servers with the workspace root as `cwd`. Always pass an **absolute path** to `--act-pack` in your editor configuration.

### `polymorph serve`

Starts the HTTP REST and Server-Sent Events (SSE) server for session CRUD and event replay.

```bash
polymorph serve [--port 8420]
```

Endpoints:
- `GET /sessions` — List sessions
- `POST /sessions` — Create a session (`{"title": "..."}`)
- `GET /sessions/<id>` — Fetch session metadata and messages
- `DELETE /sessions/<id>` — Delete a session
- `GET /sessions/<id>/events` — Stream session events via `text/event-stream` (SSE)

---

## 4. Sessions, Configuration, Providers & Subagents

### `polymorph sessions`
- `polymorph sessions list`
- `polymorph sessions create <TITLE>`
- `polymorph sessions switch <ID>`
- `polymorph sessions delete <ID>`

### `polymorph config`
- `polymorph config show` — Display the merged configuration (user + project + env + CLI).
- `polymorph config set <KEY> <VALUE>` — Update `.polymorph/config.toml` (`options.debug`, `roles.<role>`, `permissions.allow`, `permissions.deny`).

### `polymorph providers` & `polymorph models`
- `polymorph providers list` — List configured LLM providers and credential pool sizes.
- `polymorph models list` — List models in the local capability catalog.
- `polymorph update-catalog [--url <URL>]` — Refresh the cached `models.dev` capability catalog.

### `polymorph delegate run`
- `polymorph delegate run --children <1..8> --task <TEXT> [--role <ROLE>] [--repo <GIT_DIR>]` — Fan out isolated subagent workers over the internal bus (with per-child git worktrees when `--repo` is given).
