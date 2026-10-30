# Driving Polymorph from an orchestrator

Polymorph is the execution substrate; the harness (Claude Code, Codex, Cursor,
any MCP/CLI client) holds the conversation and the intent. The contract is one
driven goal per invocation, machine-readable events, and an honest exit code.

## 0. MCP server (preferred)

`polymorph mcp` speaks MCP over stdio and exposes one tool, `drive`. Register it
in any MCP client:

```json
{"mcpServers":{"polymorph":{"command":"polymorph","args":["mcp"]}}}
```

Tool input: `goal` (required), `artifact_files` (required, 1–32 paths or globs),
`criteria`, `verify_cmds` (≤ 8), `repo` (working directory), `mode` (`act`),
`max_rounds` (1–5, default 3), `timeout_secs` (30–7200, default 3600). The result
is the same ledger as `--events-json` plus `exit_code`, `passed`, and
`rounds_used`/`rounds_max`. Success is the same rule: exit 0 and a `finished`
`PASSED` event; every other outcome returns the findings with `isError`.

The server runs `review drive --tools` in a subprocess with `--on-fail rollback`,
one drive at a time. `repo` is the child's working directory, not the CLI
`--repo` worktree, so edits land in the tree.

## 1. Install and configure

```bash
cargo install polymorph-swe      # binary: polymorph (once published; build from source until then)
polymorph config show            # providers, roles, permissions
```

A drive needs a provider key (env, `api_key_cmd`, or `api_key_env` in the config)
and at least one model in the `act` and `review` roles. Check with
`polymorph providers list` and `polymorph models list`.

## 2. Paste this into the orchestrator

```text
You have the `polymorph` CLI. You do not edit project files yourself.

For each atomic change:
1. Write one sentence of goal, name every file to grade, and list falsifiable
   acceptance criteria plus one shell verify command. Goal and criteria are
   single quoted arguments; never start either with "-" (clap reads it as a
   flag) — number them 1) 2) 3).
2. Run:
   polymorph review drive --goal "<goal>" --artifact-file <path> \
     --criteria "<criteria>" --tools --verify-cmd "<cmd>" \
     --events-json .polymorph/events.json --max-rounds 3
3. Success is BOTH: exit code 0 AND the events array contains
   {"type":"finished","verdict":"PASSED"} (a budget event follows it).
   Nothing else counts.
4. On exit 1, read every `finding` in the events file. Fix the premise, the
   criteria, or the artifact list, then run ONCE more. After a second halt,
   stop and report the findings to me; do not retry the same command.
5. Never claim a change landed without the exit 0 + PASSED pair. Never run two
   drives at once. Never use --on-fail keep unless I ask. Split multi-file work
   into ordered drives, one goal each.
6. Keep --max-rounds at 3 unless I raise it. Report rounds used from the
   `budget` event.
```

## 3. Invocations

```bash
# one artifact, executor may edit and run
polymorph review drive --goal "…" --artifact-file src/lib.rs --criteria "…" \
  --tools --verify-cmd "cargo test" --events-json events.json --max-rounds 3

# several files graded together (repeatable path or glob)
polymorph review drive --goal "…" --artifact-file src/a.rs --artifact-files 'src/*.rs' …

# grade the whole worktree diff against HEAD (re-read every round)
polymorph review drive --goal "…" --diff --tools --verify-cmd "cargo test" …

# a plan file, task by task (no events file; exit 0 just means it finished)
polymorph review plan-drive --plan PLAN.md --out PLAN.updated.md \
  --checkpoint .polymorph/plan-checkpoint.json

# a single headless turn, no reviewer loop
polymorph exec "Summarize the public API in src/lib.rs"
```

Run from the project root: the drive writes `./.polymorph/sessions.db` and the
`--events-json` path relative to the current directory (missing parent
directories are created), and it overwrites the events file on every run (it is
not appended across runs). Git is not required for an `--artifact-file` drive;
it is required for `--diff` and `--repo`. `--verify-cmd` inherits the process
PATH and runs through the platform shell.

Quote the arguments the shell sees: `--goal "..."`, `--criteria "1) ... 2) ..."`,
and wrap `--verify-cmd` so the shell receives one string (single-quote it when
the command itself contains double quotes).

`--repo <path>` runs the whole drive in a detached worktree and removes it at
the end: nothing is merged back. Use it to verify a change without touching the
operator's tree, not to land one.

## 4. Events (`--events-json`)

The file is a JSON array, written in order and overwritten per run. `budget` is
the last event; `finished` precedes it. `review drive` writes these types (stdout
also carries human board lines and tracing warnings — the events file, not
stdout, is the machine truth):

| `type` | Keys | Orchestrator action |
|---|---|---|
| `round_started` | `round`, `max_rounds` | Progress only. |
| `verdict` | `round`, `passed`, `summary`; optional `invalid`, `parse_error`, `synthesized`, `reviewer_tools` | `passed:false` = findings follow. `invalid` = the reviewer reply was unparseable; treat as fail. `synthesized` = harness seed, not a reviewer call. |
| `finding` | `location`, `message` | The work to do. `location` may be `""`; then the file/line is inside `message`. Feed these into the next goal/criteria, do not ignore. |
| `halted` | `reason`; optional `rollback` | Terminal failure. Tree was rolled back by default. Re-plan, then at most one retry. |
| `finished` | `verdict` | Terminal. `PASSED` is the only success value. |
| `budget` | `rounds_used`, `rounds_max`, `tools_used`, `tools_max` | Spend record. Log it. |

The full drive protocol also has `diff` and `plan` events; `review drive` does not
emit them — do not depend on keys that are not in this table.

## 5. Exit codes

| Code | Meaning | Trust |
|---|---|---|
| `0` | `review drive`: `finished PASSED`. `plan-drive`: completed (judge the out file; exit 0 is not a verdict). | success only for drive |
| `1` | Halted/escalated, bad arguments, missing key, or missing pack. stderr names the cause. | failure |
| `2` | CLI parse error (clap). | failure |
| `130` | `exec` interrupted. | failure |

## 6. Spend guard

- One goal per drive; `--max-rounds 3` caps reviewer→fixer rounds.
- `--max-tool-rounds` caps tool iterations per fix turn (default 30).
- `--verify-cmd` is the cheapest quality signal: a deterministic command beats a
  reviewer round. Pass the narrowest command that proves the goal.
- `--diff` re-reads the worktree every round; on a large tree prefer an explicit
  `--artifact-file`/`--artifact-files` set.
- Keep ledgers: `--events-json` per drive, and archive `budget` events for cost
  accounting (S8's receipt surface will formalise this).
