# Security Policy

## Supported Versions

| Version | Supported          |
| ------- | ------------------ |
| 1.0.x   | :white_check_mark: |

## Reporting a Vulnerability

Please **do not** report security vulnerabilities through public GitHub issues.

Use [GitHub Private Vulnerability Reporting](https://github.com/walkthestars/polymorph/security/advisories/new)
to submit a report privately to the maintainer. Include:

- The affected crate and module (for example, `polymorph-tools::gate`, `polymorph-core::config`, or `polymorph-mcp`).
- Configuration or command flags required to trigger the issue.
- A minimal reproducer and impact assessment.

You will receive an acknowledgment within 72 hours and a coordinated remediation timeline once the issue is reproduced.

## Security Model & Operating Boundaries

1. **Permission Gate (`[permissions]`)**: Tool execution in agentic turns (`--tools`) is governed by `polymorph-tools::Gate`. Deny rules always take precedence over allow rules on matching prefixes (for example, `deny = ["bash:rm", "bash:git push"]`).
2. **Read-Only Reviewer Boundary**: When `--reviewer-tools` is enabled, the reviewer turn uses a hardcoded harness allowlist (`read`, `grep`, `glob`, and read-only LSP queries) that ignores operator `allow` grants for mutating or shell tools (`write`, `edit`, `bash`, `mcp`) while still honoring operator `deny` rules and pre-hooks.
3. **Credential Hygiene**: Provider API keys resolved via `api_key_cmd` or `CredentialPool` are redacted in `Debug` implementations and excluded from `tracing` events (enforced by the workspace grep-guard test in `polymorph-swe`). Turn briefs are redacted from the default log; see *Data Handling* below.
4. **Worktree Isolation & Rollback**: Driving untrusted or risky changesets with `--repo <path>` isolates execution inside a temporary detached git worktree; in-place drives (`review drive --tools`) default to `--on-fail rollback`, restoring original bytes from pre-mutation snapshots on failure or cancellation. Neither is a sandbox; see *Execution boundary* below.

## Data Handling

Polymorph runs locally. It ships no telemetry, no analytics and no hosted
service. The only network calls are to the model providers configured in
`[[providers]]` and, when `polymorph update-catalog` is run, to the model
catalog at `models.dev`.

### What is written to disk

Harness state lives under `.polymorph/` in the drive's working directory:

| Path | Contents | Retention |
|---|---|---|
| `logs/` | Daily JSON logs: rounds, verdicts, tool outcomes, and turn-brief metadata | 7 daily files |
| `sessions.db` | Chat and session history | Until deleted |
| `attempts/` | Rollback archives of files a failed in-place drive changed | Until deleted |
| `tool-output/` | Command output that exceeded the inline limit | 200 files per workdir |
| `backup-*/` | Pre-migration copy of the session database | Until deleted manually |

Turn briefs carry the goal, the acceptance criteria and the artifact text;
tool calls carry their arguments; verdict previews and dropped findings
carry their text. The rolling log records only shapes and counts by default
(a brief's kind, byte and line counts; a tool call's argument bytes; a
preview's or a dropped finding's bytes); the raw text is written only when
`POLYMORPH_FORENSIC_LOGS=1` or `true` is set for the process.

`polymorph clean` reclaims the file classes older than a cutoff (7 days by
default):

```bash
polymorph clean --dry-run                 # report only
polymorph clean                           # logs, attempts, tool-output
polymorph clean --all --force             # add session rows
polymorph clean --root /path/to/project   # a different working tree
```

### What leaves the machine

Every reviewer, planner and fixer turn sends its brief to the model provider
configured for that role. The brief includes the goal, the acceptance
criteria, and the artifact text for the files under review (bounded per file
and per turn). The provider's own retention and training terms apply to that
request. Point `[[providers]] base_url` at a self-hosted OpenAI-compatible
endpoint when the code must not leave the network.

API keys are resolved process-local from `api_key_cmd` or `api_key_env`,
redacted in `Debug` output, and never written to logs or session rows.

### Execution boundary

`--tools` runs `bash` and `--verify-cmd` with the invoking user's privileges.
`--repo` isolates a drive in a detached git worktree and `--on-fail rollback`
restores changed bytes, but neither is a sandbox: the process can read and
write anything the user can. Run untrusted repositories in a container or a
virtual machine.
