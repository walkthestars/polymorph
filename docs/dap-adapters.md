# Debug Adapter Protocol (DAP) Configuration Reference

Polymorph includes a built-in Debug Adapter Protocol client (`polymorph-tools::dap`) supporting both stdio and discovered TCP adapter transports. Each debugging session spawns fresh per invocation (`dap_start`, `dap_inspect`, `dap_evaluate`, `dap_disconnect`).

---

## Configured Adapters (`[[dap_adapters]]`)

Register adapters in `~/.config/polymorph/config.toml` or `./.polymorph/config.toml`:

```toml
[[dap_adapters]]
id = "python"
cmd = "python"
args = ["-m", "debugpy.adapter"]
languages = ["python"]

[[dap_adapters]]
id = "go"
cmd = "dlv"
args = ["dap", "-l", "127.0.0.1:0", "--headless"]
languages = ["go"]

[[dap_adapters]]
id = "rust"
cmd = "codelldb"
languages = ["rust", "c", "cpp"]

[[dap_adapters]]
id = "csharp"
cmd = "netcoredbg"
args = ["--interpreter=vscode"]
languages = ["csharp"]
```

---

## Adapter Notes & Environment Overrides

| Language | Adapter | Transport | Environment Override (Tests) |
|----------|---------|-----------|------------------------------|
| **Python** | `debugpy` (`python -m debugpy.adapter`) | stdio | `PYTHON_BIN` |
| **Go** | Delve (`dlv dap -l 127.0.0.1:0 --headless`) | TCP (port auto-discovered from `DAP server listening at:`) | `PATH` (`dlv`) |
| **Rust / C / C++** | `codelldb` (`vadimcn/vscode-lldb`) | stdio | `DAP_CODELLDB` |
| **C# / .NET** | Samsung `netcoredbg` (`--interpreter=vscode`) | stdio | `DAP_NETCOREDBG` |

> **Headed / TCP test gate**: Integration tests that bind local TCP listeners (`dlv dap`) are gated behind `POLYMORPH_HEADED=1` so default `cargo test --workspace` runs never trigger OS firewall prompts.
