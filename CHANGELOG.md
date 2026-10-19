# Changelog

All notable changes to Polymorph will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] — 2026-10-18

### Added

- Multi-crate Rust workspace (`polymorph-core`, `polymorph-llm`, `polymorph-tools`, `polymorph-delegate`, `polymorph-review`, `polymorph-tasks`, `polymorph-skills`, `polymorph-tui`, `polymorph-acp`, `polymorph-web`, `polymorph-renderdoc`, `polymorph-cli`).
- Clean-context reviewer/executor loop (`polymorph review drive`, `polymorph review run`) with consecutive-survival convergence, multi-artifact (`--artifact-files`) and worktree-diff (`--diff`) review scopes, peripheral mutation tracking, `--verify-cmd` post-fix verification gates, `--lsp-error-gate` diagnostics gating, read-only `--reviewer-tools`, and byte-exact rollback (`--on-fail rollback`).
- Task DAG planner and executor (`polymorph review plan-show`, `polymorph review plan-drive`) over `PLAN.md` specifications with SQLite checkpoint persistence and worktree merge-back.
- Content-addressed hashline file editor (`polymorph-tools::hashline`) with atomic temp-file writes, read-minted tags, and a 20-task offline edit corpus.
- Layered Figment configuration (`~/.config/polymorph/config.toml` and `./.polymorph/config.toml`) with role-routed model chains, path-scoped pins (`[[pins]]`), round-robin credential pools, 1Password-style `api_key_cmd` secret injection, and two-phase 429 cooldown/fallback.
- Built-in tool registry (`read`, `write`, `edit`, `grep`, `glob`, `bash`) with prefix-based allow/deny `Gate`, pre/post/stop script hooks (`[[hooks]]`), ask-user confirmation prompts, and automatic spill-to-disk output digests.
- Model Context Protocol (MCP) stdio client pool (`[[mcp_servers]]`), Language Server Protocol (LSP) client and post-write diagnostic enforcement across 35 language mappings (`[[lsp_servers]]`), Debug Adapter Protocol (DAP) inspection tools (`[[dap_adapters]]`), RenderDoc frame capture/introspection (`polymorph-renderdoc`), and Serica-backed web search/extract (`[web]`).
- Interactive Ratatui terminal interface (`polymorph`), Agent Client Protocol stdio server (`polymorph acp`) for Zed and Neovim, and HTTP/SSE session server (`polymorph serve`).
