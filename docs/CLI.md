# Polymorph CLI Reference

`polymorph` is a substrate binary: it executes drives and exposes machine seams. Running it
with no subcommand prints the command surface and exits `0`.

---

## Global options

| Flag | Default | Description |
|------|---------|-------------|
| `--db <PATH>` | `./.polymorph/sessions.db` | SQLite session database path |
| `--debug` | `false` | Force `options.debug = true` (highest config precedence) |
| `--completions <SHELL>` | — | Emit shell completions (`bash`, `zsh`, `fish`, `powershell`, `elvish`) to stdout and exit |
| `-V`, `--version` | — | Print version and exit |
| `-h`, `--help` | — | Print help and exit |

---

## 1. MCP server (`polymorph mcp`)

Speaks MCP over stdio and exposes one tool, `drive`. Stdout carries the protocol; nothing
else is printed.

```bash
polymorph mcp
```

Client configuration:

```json
{"mcpServers":{"polymorph":{"command":"polymorph","args":["mcp"]}}}
```

Tool input:

| Field | Required | Description |
|-------|----------|-------------|
| `goal` | yes | One-sentence goal, at most 4,000 characters |
| `artifact_files` | yes | 1–32 paths or globs; the first is passed as `--artifact-file`, the rest as `--artifact-files` |
| `criteria` | no | Acceptance criteria, at most 20,000 characters; must not start with `-` |
| `verify_cmds` | no | Up to 8 shell commands, each at most 2,000 characters |
| `repo` | no | Working directory for the drive; must exist (this is not the CLI `--repo` worktree) |
| `mode` | no | `act` only; plan files use `review plan-drive` |
| `max_rounds` | no | 1–5, default 3 |
| `timeout_secs` | no | 30–7,200, default 3,600 |

The result is a JSON object with `exit_code`, `passed`, `rounds_used`, `rounds_max`, `events`,
and `stderr_tail`. `passed` is true only for exit code `0` plus a `finished` event with
verdict `PASSED`; every other outcome is a tool error carrying the findings. The server runs
`review drive --tools --on-fail rollback`, one drive at a time.

---

## 2. Headless prompt execution (`polymorph exec`)

Runs one prompt turn, streams the model output to stdout, and exits.

```bash
polymorph exec <PROMPT> [--model <SPEC>] [--system <TEXT>] [--scope <PATH_PREFIX>]
```

- `--model <SPEC>` — override the `default` role chain (bare model name or `provider::model`).
- `--system <TEXT>` — override the system prompt.
- `--scope <PATH_PREFIX>` — select a path-pinned chain from `[[pins]]`.

---

## 3. Reviewer loops (`polymorph review`)

### `polymorph review drive`

Drives one goal through fixer → harness gates → clean-context reviewer rounds until the
reviewer passes with no blocking findings, or until `--max-rounds` or the 3-strike survivor
guard halts the loop.

```bash
polymorph review drive --goal <TEXT> [scope flags] [execution flags]
```

#### Artifact scope (at least one required)

| Flag | Description |
|------|-------------|
| `--artifact-file <PATH>` | Primary file under review. When the path does not exist, round 1 seeds the creation finding and starts the fixer immediately (unless `--no-create-seed`) |
| `--artifact-files <PATH\|GLOB>` | Repeatable path or workdir-relative glob; re-expanded after every fixer turn and graded together |
| `--diff [<BASE>]` | Grade every file changed, added, deleted, or untracked relative to `<BASE>` (default `HEAD`; with `--repo`, the drive's start commit) |

#### Loop and verification flags

| Flag | Default | Description |
|------|---------|-------------|
| `--criteria <TEXT>` | `""` | Acceptance criteria threaded to the fixer and reviewer. A value starting with `-` is parsed as a flag; number criteria `1) 2)` |
| `--mode act\|plan` | `act` | `act` grades code and files; `plan` grades a `PLAN.md` specification |
| `--tools` | `false` | Grant the fixer the tool registry (`read`, `write`, `edit`, `grep`, `glob`, `bash`, MCP, LSP, DAP) under `[permissions]` |
| `--max-rounds <N>` | `3` | Maximum reviewer → fixer rounds |
| `--max-tool-rounds <N>` | `30` | Maximum tool rounds per fixer turn, plus up to `max_bonus_tool_rounds` while the fixer makes progress |
| `--verify-cmd <CMD>` | `[agent] verify_cmds` | Repeatable command run in the workdir after every fixer turn; a non-zero exit or timeout vetoes a `Pass` |
| `--verify-timeout-secs <SECS>` | `600` | Per-command timeout; expiry reaps the process tree |
| `--lsp-error-gate` | `[agent] lsp_error_gate` | Veto a `Pass` while mutated files carry error-severity LSP diagnostics (requires `--tools`) |
| `--reviewer-tools` | `[agent] reviewer_tools` | Let the reviewer take up to `reviewer_tool_rounds` (default 4) read-only inspection turns before its verdict |
| `--on-fail rollback\|keep` | `rollback` (`drive`), `keep` (`run`) | On a non-passing outcome or `Ctrl-C`, `rollback` restores mutated bytes and archives the attempt under `.polymorph/attempts/` |
| `--repo <DIR>` | — | Run the drive in a temporary detached worktree of `<DIR>`. The worktree is removed when the drive ends and nothing is merged back; use it to verify without touching the working tree, not to land a change |
| `--scope <PREFIX>` | — | Path prefix for `[[pins]]` model routing |
| `--review-pack <PATH>` | `prompts/muse/muse-spark-1.3/review.md` | Reviewer prompt pack (embedded defaults ship in the binary) |
| `--exec-pack <PATH>` | `prompts/glm/glm-5.3/act.md` | Act-mode fixer prompt pack |
| `--plan-pack <PATH>` | `prompts/glm/glm-5.3/plan.md` | Plan-mode fixer prompt pack |
| `--no-create-seed` | `false` | Ask the reviewer in round 1 even when the artifact file does not exist yet |
| `--events-json <PATH>` | — | Write the drive event ledger as JSON to `<PATH>` |

### `polymorph review run`

Accepts the `review drive` flags except `--events-json`; `--on-fail` defaults to `keep`. Prints
the human summary board.

### `polymorph review plan-show`

Parses and validates a `PLAN.md` task DAG offline and prints the topological board.

```bash
polymorph review plan-show --plan PLAN.md [--session <SESSION_ID>]
```

### `polymorph review plan-drive`

Drives every task in a `PLAN.md` DAG through per-task reviewer loops and writes the updated
plan to `--out`. The input file is never overwritten.

```bash
polymorph review plan-drive \
  --plan PLAN.md \
  --out PLAN.out.md \
  [--checkpoint checkpoint.json] \
  [--session <SESSION_ID>] \
  [--max-rounds 3]
```

### `polymorph review check <PATH>`

Validates a reviewer verdict JSON file offline; exits non-zero when malformed.

### `polymorph review demo`

Runs the reviewer loop offline over scripted doubles:

```bash
polymorph review demo [--mode plan|act] [--script pass|fail-once|never|survivor]
```

### `polymorph review eval-ab`

Runs one seeded artifact through two live reviewer arms (`--review-model-a`,
`--review-model-b`) and compares convergence and findings.

### `polymorph review bench`, `polymorph review bench-check`

`bench` runs the agent-bench corpus live and scores hidden tests. `bench-check` re-scores a
recording (file or directory of `*.bench.json`) offline and fails below `--min-rate`.

---

## 4. Evaluation (`polymorph eval`)

| Command | Description |
|---------|-------------|
| `polymorph eval run` | Run one live model turn against an expected end state and score it |
| `polymorph eval sweep` | Run the B6 edit corpus live, one turn per scorable task; skips rejection tasks by name |
| `polymorph eval check` | Re-score a recording (file or directory of `*.eval.json`) offline and gate it |

---

## 5. Sessions, configuration, providers, models, subagents

### `polymorph sessions`

```bash
polymorph sessions list
polymorph sessions create <TITLE>
polymorph sessions switch <ID>
polymorph sessions delete <ID>
```

### `polymorph config`

- `polymorph config show` — print the merged configuration (user + project + env + CLI).
- `polymorph config set <KEY> <VALUE>` — update `.polymorph/config.toml`
  (`options.debug`, `roles.<role>`, `permissions.allow`, `permissions.deny`).

### `polymorph providers`, `polymorph models`, `polymorph update-catalog`

- `polymorph providers list` — configured providers and their key sources.
- `polymorph models list` — models in the local capability catalog.
- `polymorph update-catalog [--url <URL>]` — refresh the cached `models.dev` catalog.

### `polymorph delegate run`

```bash
polymorph delegate run --children <1..8> --task <TEXT> [--role <ROLE>] [--repo <GIT_DIR>]
```

Exercises the subagent bus with echo workers (offline), with per-child git worktrees when
`--repo` is given.

---

## 6. Watch a drive (`polymorph-watch`)

A separate binary, built with `cargo build --release -p polymorph-watch`, renders a drive
ledger without calling a model:

```bash
polymorph-watch --events <PATH> [--follow] [--interval-ms 250]
```

`--follow` re-reads the file when it changes, so a live `review drive --events-json` shows
rounds, verdicts, findings, budget, and the final status as they are written.

---

## Exit codes

| Code | Meaning |
|------|---------|
| `0` | `review drive`: `finished` / `PASSED`. `plan-drive`: completed (judge the output file; exit 0 is not a verdict). Bare invocation: help printed |
| `1` | Halted or escalated, invalid arguments, missing key, or missing prompt pack |
| `2` | CLI parse error |
| `130` | `exec` interrupted |

The event and exit-code contract, including what to do per event, is in
[docs/INTERPOSER.md](INTERPOSER.md).
