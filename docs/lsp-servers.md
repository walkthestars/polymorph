# Language Server Protocol (LSP) Configuration Reference

Polymorph's LSP subsystem (`polymorph-tools::lsp`) maps 35 language keys by file extension and filename (`CMakeLists.txt`, `Dockerfile`) and spawns configured language servers lazily on first file interaction.

When enrolled under `[[lsp_servers]]` in `config.toml`:
- **Post-write diagnostic consults**: Every `write` and `edit` tool call notifies the language server and collects updated diagnostics.
- **Reviewer evidence & gating**: Diagnostics on mutated files are ranked by severity (errors first) and attached to `HARNESS EVIDENCE` for the clean-context reviewer. Passing `--lsp-error-gate` (or setting `[agent] lsp_error_gate = true`) deterministically vetoes any `Pass` verdict while error-severity (`severity == 1`) diagnostics remain.
- **Navigation tools**: Fixes and `--reviewer-tools` turns can query `lsp_hover`, `lsp_definition`, `lsp_references`, and `lsp_diagnostics`.

---

## Supported Language Keys & Server Commands

Add any of the entries below to `~/.config/polymorph/config.toml` or `./.polymorph/config.toml` for the languages used in your workspace. Missing binaries fail soft with a warning and never block startup.

| Language Key | File Extensions / Names | Standard Server Command (`cmd` & `args`) |
|--------------|-------------------------|------------------------------------------|
| `rust` | `.rs` | `cmd = "rust-analyzer"` |
| `go` | `.go` | `cmd = "gopls"` |
| `python` | `.py`, `.pyi` | `cmd = "basedpyright-langserver"`, `args = ["--stdio"]` (or `cmd = "ruff"`, `args = ["server"]`) |
| `toml` | `.toml` | `cmd = "taplo"`, `args = ["lsp", "stdio"]` |
| `c` / `cpp` | `.c`, `.h`, `.cpp`, `.hpp`, `.cc`, `.cxx` | `cmd = "clangd"` |
| `zig` | `.zig` | `cmd = "zls"` |
| `lua` | `.lua` | `cmd = "lua-language-server"` |
| `markdown` | `.md`, `.markdown` | `cmd = "marksman"` |
| `cmake` | `CMakeLists.txt`, `.cmake` | `cmd = "neocmakelsp"`, `args = ["stdio"]` |
| `terraform` | `.tf`, `.tfvars` | `cmd = "terraform-ls"`, `args = ["serve"]` |
| `wgsl` | `.wgsl` | `cmd = "wgsl-analyzer"` |
| `glsl` | `.glsl`, `.vert`, `.frag`, `.comp` | `cmd = "glsl_analyzer"` |
| `slang` | `.slang` | `cmd = "slangd"` |
| `csharp` | `.cs` | `cmd = "csharp-ls"` |
| `fsharp` | `.fs`, `.fsi`, `.fsx` | `cmd = "fsautocomplete"` |
| `java` | `.java` | `cmd = "jdtls"` |
| `kotlin` | `.kt`, `.kts` | `cmd = "kotlin-lsp"` |
| `typescript` / `javascript` | `.ts`, `.tsx`, `.js`, `.jsx`, `.mjs`, `.cjs` | `cmd = "vtsls"`, `args = ["--stdio"]` (or `cmd = "deno"`, `args = ["lsp"]`) |
| `svelte` | `.svelte` | `cmd = "svelteserver"`, `args = ["--stdio"]` |
| `astro` | `.astro` | `cmd = "astro-ls"`, `args = ["--stdio"]` |
| `html` | `.html`, `.htm` | `cmd = "vscode-html-language-server"`, `args = ["--stdio"]` |
| `css` | `.css`, `.scss`, `.less` | `cmd = "vscode-css-language-server"`, `args = ["--stdio"]` |
| `json` | `.json`, `.jsonc` | `cmd = "vscode-json-language-server"`, `args = ["--stdio"]` |
| `yaml` | `.yaml`, `.yml` | `cmd = "yaml-language-server"`, `args = ["--stdio"]` |
| `xml` | `.xml`, `.xsd`, `.xsl` | `cmd = "lemminx"` |
| `bash` | `.sh`, `.bash` | `cmd = "bash-language-server"`, `args = ["--stdio"]` |
| `dockerfile` | `Dockerfile`, `.dockerfile` | `cmd = "docker-langserver"`, `args = ["--stdio"]` |
| `php` | `.php` | `cmd = "intelephense"`, `args = ["--stdio"]` |
| `clojure` | `.clj`, `.cljs`, `.cljc`, `.edn` | `cmd = "clojure-lsp"` |
| `dart` | `.dart` | `cmd = "dart"`, `args = ["language-server", "--protocol=lsp"]` |
| `racket` | `.rkt` | `cmd = "racket"`, `args = ["-l", "racket-langserver"]` |
| `d` | `.d` | `cmd = "serve-d"` |

---

## Example Configuration

```toml
[[lsp_servers]]
id = "rust"
cmd = "rust-analyzer"
languages = ["rust"]

[[lsp_servers]]
id = "python"
cmd = "basedpyright-langserver"
args = ["--stdio"]
languages = ["python"]

[[lsp_servers]]
id = "c-cpp"
cmd = "clangd"
languages = ["c", "cpp"]

[[lsp_servers]]
id = "toml"
cmd = "taplo"
args = ["lsp", "stdio"]
languages = ["toml"]
```
