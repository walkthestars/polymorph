# Polymorph Operational Workflows

Step-by-step guides for the three ways to drive Polymorph: a single verified change
(`review drive`), a multi-task plan (`review plan-drive`), and an MCP client (`polymorph
mcp`). A read-only viewer (`polymorph-watch`) is covered last.

---

## Workflow 1: Drive a verified code change

Use `review drive` for an automated fixer + clean-context reviewer loop backed by compiler,
test, and LSP gates.

### One file

```bash
polymorph review drive \
  --goal "Implement a thread-safe LRU cache with O(1) get and put" \
  --criteria "1) all unit tests in src/lru.rs pass 2) zero clippy warnings 3) no unwrap() on user input" \
  --artifact-file src/lru.rs \
  --tools \
  --lsp-error-gate \
  --reviewer-tools \
  --verify-cmd "cargo test --workspace" \
  --verify-cmd "cargo clippy --workspace --all-targets -- -D warnings" \
  --events-json .polymorph/events.json
```

### Several files or a whole diff

```bash
polymorph review drive \
  --goal "Migrate error types in the parser subsystem to thiserror" \
  --artifact-files "src/parser/**/*.rs" \
  --diff HEAD \
  --tools \
  --lsp-error-gate \
  --reviewer-tools \
  --verify-cmd "cargo test --workspace"
```

Each round:

1. **Snapshot.** Before the fixer touches a file with `write` or `edit`, `RestoreLedger`
   records the original bytes.
2. **Fixer turn.** The fixer reads files (minting hashline tags), applies atomic hashline
   edits, and runs tests.
3. **Harness gates.** Polymorph runs every `--verify-cmd` in order and collects LSP
   diagnostics for every mutated file.
4. **Reviewer turn.** A fresh reviewer inspects the artifacts, peripheral diffs, command
   ledger, verify output, and diagnostics. A failed verify command or an error-severity
   diagnostic (with `--lsp-error-gate`) vetoes a passing verdict.
5. **Outcome.** A pass keeps the edits. A failure, escalation, or `Ctrl-C` restores the
   working tree byte-for-byte (`--on-fail rollback`, the default for `drive`) and archives
   the attempt under `.polymorph/attempts/`.

`--repo <DIR>` runs the whole drive in a detached worktree that is removed at the end;
nothing is merged back. Use it to verify against a clean tree, not to land a change.

---

## Workflow 2: Execute a multi-task DAG (`PLAN.md`)

For multi-step features, write a `PLAN.md` with dependencies and acceptance criteria:

```markdown
# Plan: Authentication Token Refresh

## [task-1] Add RefreshToken model and SQLite migration
Status: pending
Deps: none
Criteria:
- `RefreshToken` struct added in `src/auth/model.rs`
- Migration `004_refresh_tokens.sql` passes a round-trip test

## [task-2] Wire /auth/refresh endpoint
Status: pending
Deps: task-1
Criteria:
- `POST /auth/refresh` rotates expired tokens and rejects revoked tokens
- Integration tests in `tests/auth_refresh.rs` pass
```

1. **Validate the DAG offline**:
   ```bash
   polymorph review plan-show --plan PLAN.md
   ```

2. **Drive it in topological order with checkpointing**:
   ```bash
   polymorph review plan-drive \
     --plan PLAN.md \
     --out PLAN.completed.md \
     --checkpoint .polymorph/plan-checkpoint.json \
     --max-rounds 3
   ```
   Re-running with the same checkpoint resumes from the last completed task. The input
   `PLAN.md` is never overwritten; the updated plan is written to `--out`. Exit `0` means the
   drive completed, not that every task passed: read the output plan.

---

## Workflow 3: Drive from an MCP client

`polymorph mcp` speaks MCP over stdio and exposes one tool, `drive`. Register it in Claude
Code, Codex, Cursor, or any MCP client:

```json
{
  "mcpServers": {
    "polymorph": {
      "command": "polymorph",
      "args": ["mcp"]
    }
  }
}
```

A tool call carries the same contract as the CLI:

```json
{
  "goal": "Reject empty frames in FrameBuffer::slice",
  "artifact_files": ["src/buffer.rs"],
  "criteria": "1) an empty frame returns Err 2) existing tests pass",
  "verify_cmds": ["cargo test"],
  "repo": "/path/to/project",
  "max_rounds": 3
}
```

The result is `{exit_code, passed, rounds_used, rounds_max, events, stderr_tail}`. Treat the
change as landed only when `passed` is `true` — exit code `0` plus a `finished` event with
verdict `PASSED`. Any other result carries the findings; re-plan the goal or criteria and
retry once.

The server runs one drive at a time and always uses `--on-fail rollback`. `repo` is the
child's working directory, not the CLI `--repo` worktree. Limits: 1–5 rounds, 1–32 artifacts,
up to 8 verify commands, 30–7,200 seconds.

The full event and exit-code contract is in [INTERPOSER.md](INTERPOSER.md).

---

## Workflow 4: Watch a drive

`polymorph-watch` is a separate binary that reads an event ledger and renders it; it never
calls a model.

```bash
cargo build --release -p polymorph-watch
polymorph-watch --events .polymorph/events.json --follow
```

The viewer shows the header path and event count, the status (`running`, `PASSED`, or
`HALTED: <reason>`), one row per round with its verdict, the budget line, and recent
findings. `--follow` re-reads the file every 250 ms (adjust with `--interval-ms`). Press
`q`, `Esc`, or `Ctrl-C` to exit.
