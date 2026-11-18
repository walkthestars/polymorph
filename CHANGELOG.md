# Changelog

All notable changes to Polymorph are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.1.3] — 2026-11-18

### Changed

- Docs: `docs/LICENSING.md` no longer says a free evaluation licence is available on
  request; it gives `licensing@shienar.com` for evaluation and commercial licences.

## [1.1.2] — 2026-11-16

### Fixed

- The missing-`z3` message no longer contains runs of spaces.
- The console no longer shows the model client's per-chunk `EMPTY CHOICE CONTENT`
  warnings, which some gateways trigger by the hundred. They remain in the log file,
  and `RUST_LOG` still overrides the filter.
- Docs: the MCP server is described with both of its tools, the README command table
  lists `clean`, `proof` and `eval sweep`, and `docs/LICENSING.md` no longer says
  releases include crates.io packages.
- Docs: commercial and evaluation licence enquiries now go to `licensing@shienar.com`
  (previously "open an issue").

## [1.1.1] — 2026-11-14

### Fixed

- An `openai::` model claimed by a `[[providers]]` entry with a `base_url` is sent to
  that endpoint with the entry's `api_key_env`. It went to the default OpenAI endpoint.
- A missing `z3` is reported with install commands for each platform.
- A test that scanned the shared temp folder no longer races its sibling.
- Six tests that assumed Windows paths or `cmd` pass on Linux.

### Changed

- CI runs the whole test suite on Linux as well as Windows.

## [1.1.0] — 2026-11-14

### Added

- `polymorph proof` and `review drive --proof-request` (experimental): an SMT-checked
  proof gate over the `ais-core` fragment, with re-checkable certificates and
  `proof_round` / `proof` ledger events. Needs `z3` on `PATH`. `proof run` calls a
  model only with `--use-model` or `--model`.
- `deny.toml`: a permissive-only `cargo-deny` policy that reproduces the AGPL rejection
  an enterprise allowlist gives, as described in `docs/LICENSING.md`.
- MCP tool `synthesize_invariants` (experimental).
- `docs/PROOF.md`: the proof-gate guide, covering the `ais-core` language, verdicts,
  the trust boundary, certificates and what is not proved.
- `SECURITY.md`: a "Certificates" section on what a proof certificate contains.

### Changed

- The edit corpus moved to `crates/polymorph-tools/tests/edit_corpus/`, which is the
  new default of `eval sweep --corpus`. Its test prints `EDIT-CORPUS SCORE`.
- CI runs the proof-gate crates on Linux as well as the full suite on Windows, with
  `z3` required.

## [1.0.1] — 2026-11-14

### Added

- `polymorph clean`: reclaims `.polymorph/` logs, rollback archives and spilled tool
  output older than a cutoff (7 days by default), and session rows with `--force`.
  `--dry-run` reports without deleting.
- `SECURITY.md`: a data-handling section covering what is stored locally, what is
  sent to the model provider, and the execution boundary of `--tools`.

### Changed

- Drive logs record turn briefs, tool-call arguments, verdict previews and dropped
  findings as shapes and counts only. Raw text is written only when
  `POLYMORPH_FORENSIC_LOGS=1` is set.
- `docs/INTERPOSER.md` states the trust boundary of the MCP server.
- Source comments and prompt-pack headers describe behavior instead of citing
  internal work-item labels. The prompt manifest hashes are updated to match.

## [1.0.0] — 2026-10-30

### Added

- `polymorph-swe` 1.0.0: one `polymorph` binary that executes drives over the CLI
  (`review drive`, `review plan-drive`, `exec`) and over MCP.
- `polymorph mcp`: an MCP stdio server with one bounded tool, `drive`, returning the
  event ledger plus `exit_code`, `passed`, and round counts.
- `polymorph-drive`: the headless drive protocol (intents, events, driver contract)
  separated from any terminal dependency.
- `polymorph-prompts`: the default prompt packs compiled into the binary, so an
  installed binary resolves them without a checkout. A `prompts/` directory on disk
  still takes precedence.
- `polymorph-watch`: a separate, optional read-only terminal viewer for drive
  ledgers, with follow mode.
- AGPL-3.0-only licensing with a documented commercial path (`docs/LICENSING.md`),
  CLA and DCO inbound requirements, and the legal files at the repository root.
- `docs/INTERPOSER.md`: the orchestrator contract — MCP and CLI invocation, the
  event table, exit codes, and per-event actions.

### Changed

- Workspace version 1.0.0; the CLI package is named `polymorph-swe` while the binary
  keeps the name `polymorph`.
- Internal path dependencies carry explicit versions for publication.
- JSON ledgers keep stable key order (`serde_json/preserve_order` is enabled
  explicitly rather than inherited from an unrelated dependency).
- `--events-json` and MCP results share one ledger shape.

### Fixed

- Tool loops preserve provider reasoning content across follow-up requests
  (DeepSeek `reasoning_content`, Anthropic extended thinking), so multi-round tool
  use works with thinking-mode models.

### Removed

- The interactive terminal UI crate (`polymorph-tui`), the ACP server
  (`polymorph-acp`), the HTTP/SSE server (`polymorph-web`), the `polymorph-renderdoc`
  stub crate, their CLI subcommands (`acp`, `serve`), and `--theme`. The core binary
  carries no terminal-UI or editor-protocol dependency. The TUI's ledger rendering
  continues as the separate `polymorph-watch` binary.

## [0.1.0] — 2026-10-18 (initial public tree)

### Added

- Multi-crate Rust workspace (`polymorph-core`, `polymorph-llm`, `polymorph-tools`,
  `polymorph-delegate`, `polymorph-review`, `polymorph-tasks`, `polymorph-skills`,
  `polymorph-tui`, `polymorph-acp`, `polymorph-web`, `polymorph-renderdoc`,
  `polymorph-cli`).
- Clean-context reviewer/executor loop (`polymorph review drive`, `polymorph review
  run`) with consecutive-survival convergence, multi-artifact (`--artifact-files`) and
  worktree-diff (`--diff`) scopes, peripheral mutation tracking, `--verify-cmd` gates,
  `--lsp-error-gate`, read-only `--reviewer-tools`, and byte-exact rollback.
- Task DAG planner and executor (`polymorph review plan-show`, `polymorph review
  plan-drive`) over `PLAN.md` with checkpoint persistence.
- Content-addressed hashline editor with atomic writes and an offline edit corpus.
- Layered configuration with role-routed model chains, path pins, credential pools,
  `api_key_cmd` secret injection, and 429 cooldown/fallback.
- Tool registry with a prefix allow/deny Gate, script hooks, and output spill.
- MCP client pool, LSP client with post-write diagnostics, DAP inspection tools,
  RenderDoc capture, and Serica-backed web search.
