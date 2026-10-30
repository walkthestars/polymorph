# Security Policy

## Supported Versions

| Version | Supported          |
| ------- | ------------------ |
| 0.1.x   | :white_check_mark: |

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
3. **Credential Hygiene**: Provider API keys resolved via `api_key_cmd` or `CredentialPool` are redacted in `Debug` implementations and excluded from `tracing` events (enforced by the workspace grep-guard test in `polymorph-swe`).
4. **Worktree Isolation & Rollback**: Driving untrusted or risky changesets with `--repo <path>` isolates execution inside a temporary detached git worktree; in-place drives (`review drive --tools`) default to `--on-fail rollback`, restoring original bytes from pre-mutation snapshots on failure or cancellation.
