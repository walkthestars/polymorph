# Contributing to Polymorph

Thanks for your interest in contributing to Polymorph!

## Getting Started

Prerequisites:
- Rust **1.88+** (stable toolchain with `rustfmt` and `clippy`, configured via `rust-toolchain.toml`)

```bash
git clone https://github.com/walkthestars/polymorph.git
cd polymorph
cargo build --workspace --all-targets
cargo test --workspace
```

The default test suite runs completely offline (LLM, MCP, and web endpoints are tested against local `wiremock` / duplex fixtures; headed RenderDoc and TCP tests are gated behind `POLYMORPH_HEADED=1`).

## Development Workflow

1. **Fork** the repository and create a branch (`feat/lsp-workspace-symbols`, `fix/hashline-crlf`).
2. **Make your changes** following workspace conventions:
   - Tools self-register in `crates/polymorph-tools/src/registry.rs` only; nothing registers from `polymorph-core`.
   - Pure-core text inputs are terminator-stripped normalized text.
   - API keys and secret material must never appear in `tracing` macro calls (enforced by `tracing_setup` grep-guard test).
   - If you add or edit a prompt file under `crates/polymorph-prompts/prompts/`, update its SHA-256 entry in `prompts.toml` (enforced by CI).
3. **Run the verification suite**:
   ```bash
   cargo test --workspace
   cargo clippy --workspace --all-targets -- -D warnings
   cargo fmt --all -- --check
   ```
4. **Sign off your commits (DCO)**: `git commit -s -m "feat(review): ..."` (see below).
5. **Accept the CLA** (first-time contributors only, once ever) — see [Contributor License Agreement](#contributor-license-agreement-cla) below.

## Sign Your Work (DCO)

Polymorph uses the [Developer Certificate of Origin](DCO) **and** a Contributor License Agreement ([CLA.md](CLA.md)). They do different jobs and neither replaces the other: the DCO certifies **where the code came from** (every commit, `Signed-off-by`), while the CLA grants the project's maintainer the relicensing rights described in [CLA.md](CLA.md) (once per contributor). You retain copyright in your contribution under both.

Add a `Signed-off-by` line to each commit certifying that you wrote the code (or otherwise have the right to submit it) and are contributing it under AGPL-3.0-only and the terms of `CLA.md`. Git adds this with `-s`:

```bash
git commit -s -m "feat(tools): add timeout guard to LSP shutdown"
```

## Contributor License Agreement (CLA)

In addition to the DCO, Polymorph asks first-time contributors to accept the [Contributor License Agreement](CLA.md) — **once, ever**, not per pull request. It grants the Project Owner the right to license your contribution as part of the project under other terms (such as a commercial or enterprise license), while your contribution itself remains public under AGPL-3.0-only as part of the open-source repository.

**How to sign:** on your first pull request, the CLA Assistant bot will comment with instructions. Read [CLA.md](CLA.md), then comment this exact phrase on the pull request:

```text
I have read the CLA Document and I hereby sign the CLA
```

The bot records your signature on the `cla-signatures` branch and marks the check green for all present and future pull requests.

## Commit Messages

Follow [Conventional Commits](https://www.conventionalcommits.org/):

```text
feat(review): add multi-artifact glob expansion
fix(llm): retry transient stream decode errors on pair path
docs(config): document [agent] verify_cmds table
```

## Pull Request Checklist

- [ ] `cargo test --workspace` passes
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes with zero warnings
- [ ] `cargo fmt --all -- --check` passes
- [ ] `prompts.toml` SHA-256 manifest matches all files under `crates/polymorph-prompts/prompts/`
- [ ] Documentation (`README.md`, `docs/`) is updated if CLI flags or config keys changed
- [ ] `CHANGELOG.md` is updated
- [ ] Every commit carries a valid `Signed-off-by` line (`git commit -s`) and the [CLA](CLA.md) is signed (first-time contributors)
