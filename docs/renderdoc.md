# RenderDoc & Shader Debugging Reference

Polymorph provides graphics frame capture and draw/pipeline introspection tools (`polymorph-tools::renderdoc`) alongside `renderdoc://` URI readers.

---

## Prerequisites

- **RenderDoc** (`1.30+`, tested with `1.46.0`): `renderdoccmd` and `qrenderdoc` installed on the host (`C:/Program Files/RenderDoc/` on Windows or on `PATH`).
- **Slang Compiler** (`slangc`, optional): Required only for compiling `.slang` shaders to debug-info SPIR-V (`NonSemantic.Shader.DebugInfo.100`).

---

## Configuration

Configure the RenderDoc binary paths in `config.toml` if installed outside standard locations:

```toml
[renderdoc]
cmd = "C:/Program Files/RenderDoc/renderdoccmd.exe"
py_cmd = "C:/Program Files/RenderDoc/qrenderdoc.exe"
```

---

## Exposed Tools & URI Schemes

When enabled in a tool-bearing turn:
- `rdoc_capture` — Launches an executable under `renderdoccmd capture`, writes an `.rdc` frame capture, and optionally exports a thumbnail preview.
- `rdoc_draws` / `renderdoc://<path.rdc>/draws` — Enumerates draw calls and dispatch events inside an `.rdc` capture.
- `rdoc_state` / `renderdoc://<path.rdc>/state` — Inspects pipeline state at a specific event ID (`eid`).
- `rdoc_diff` — Compares draw call counts and state between two `.rdc` captures.

---

## Running Graphics Integration Tests

Tests that open a headed OpenGL window or launch `renderdoccmd`/`qrenderdoc` are opt-in via `POLYMORPH_HEADED=1`:

```bash
POLYMORPH_HEADED=1 cargo test -p polymorph-tools --test rdoc_capture --test rdoc_introspect
cargo test -p polymorph-tools --test slang_smoke
```
