# Polymorph Operational Workflows

Step-by-step guides for driving Polymorph across its four primary interfaces: headless Reviewer Loops (`review drive`), Task DAG Plans (`review plan-drive`), the interactive Terminal UI (`polymorph`), and Editor ACP (`polymorph acp`).

---

## Workflow 1: Driving a Verified Code Change (`polymorph review drive`)

Use `review drive` when you want an automated Fixer + Clean-Context Reviewer loop backed by compiler, test, and LSP gates.

### Single-file creation or modification

```bash
polymorph review drive \
  --goal "Implement a thread-safe LRU cache with O(1) get and put" \
  --criteria "All unit tests in src/lru.rs pass; zero clippy warnings; no unwrap() on user inputs" \
  --artifact-file src/lru.rs \
  --tools \
  --lsp-error-gate \
  --reviewer-tools \
  --verify-cmd "cargo test --workspace" \
  --verify-cmd "cargo clippy --workspace --all-targets -- -D warnings" \
  --events-json lru-events.json
```

### Multi-file subsystem refactor (`--artifact-files` & `--diff`)

When a task spans multiple files or a full git changeset, pass repeatable `--artifact-files` globs or `--diff`:

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

What happens on each round:
1. **Snapshot**: Before the fixer touches any file via `write` or `edit`, `RestoreLedger` snapshots the exact original bytes on disk.
2. **Fixer turn**: The fixer reads files (minting hashline tags), applies atomic hashline edits, and runs tests.
3. **Harness verification gate**: Polymorph runs every `--verify-cmd` in order and queries enrolled LSP servers for diagnostics on all mutated files.
4. **Reviewer turn**: A fresh, clean-context reviewer inspects the primary artifacts, any peripheral files mutated during the drive, the command execution ledger, `--verify-cmd` digests, and LSP diagnostics. If `--verify-cmd` failed or `--lsp-error-gate` found error diagnostics, the harness vetoes any passing verdict automatically.
5. **Convergence or rollback**: If the reviewer emits `Pass` (with no harness vetoes), the drive succeeds. If the drive fails, escalates, or is cancelled with `Ctrl-C`, `--on-fail rollback` (the default for `drive`) restores the working tree byte-for-byte and saves the attempt under `.polymorph/attempts/<timestamp>/`.

---

## Workflow 2: Executing a Multi-Task DAG (`PLAN.md`)

For multi-step features, write a `PLAN.md` file with task dependencies and acceptance criteria:

```markdown
# Plan: Authentication Token Refresh

## [task-1] Add RefreshToken model and SQLite migration
Status: pending
Deps: none
Criteria:
- `RefreshToken` struct added in `src/auth/model.rs`
- Migration `004_refresh_tokens.sql` passes round-trip test

## [task-2] Wire /auth/refresh endpoint
Status: pending
Deps: task-1
Criteria:
- `POST /auth/refresh` rotates expired tokens and rejects revoked tokens
- Integration tests in `tests/auth_refresh.rs` pass
```

1. **Inspect and validate the task DAG offline**:
   ```bash
   polymorph review plan-show --plan PLAN.md
   ```

2. **Drive the plan in topological order with checkpointing**:
   ```bash
   polymorph review plan-drive \
     --plan PLAN.md \
     --out PLAN.completed.md \
     --checkpoint .polymorph/plan-checkpoint.json \
     --max-rounds 3
   ```
   If interrupted, re-running with the same `--checkpoint` file resumes from the last completed task without re-running finished nodes.

---

## Workflow 3: Interactive Terminal UI (`polymorph`)

Run `polymorph` with no arguments (or `polymorph --theme tokyo-night`) to open the multi-pane TUI:

- **Chat & streaming**: Converse with the active role chain with live tool-call transparency and 4-stage circuit status.
- **File picker & diff pane**: Browse workspace files, inspect diffs, and review LSP diagnostic gutters.
- **Slash commands**:
  - `/attach <path>` — Attach an image to the next turn for multimodal vision analysis.
  - `/session` — Create, switch, or fork persistent SQLite sessions.
  - `/theme <name>` — Switch TUI color themes live.
- **Ask-user confirmation overlay**: When a tool or hook requests confirmation, an interactive picker modal prompts for approval without losing your draft input.

---

## Workflow 4: Editor Integration via ACP (`polymorph acp`)

Polymorph implements the Agent Client Protocol (`agent-client-protocol`) over stdio so you can drive it directly from **Zed** or other ACP-compatible editors.

Add the following to your Zed `settings.json`:

```json
{
  "agent_servers": {
    "polymorph": {
      "command": "C:/path/to/polymorph.exe",
      "args": [
        "acp",
        "--tools",
        "--max-tool-rounds",
        "30",
        "--act-pack",
        "C:/path/to/polymorph/prompts/glm/glm-5.3/act.md"
      ]
    }
  }
}
```
