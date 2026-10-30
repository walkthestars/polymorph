//! MCP client substrate.

use crate::{Registry, ToolDef};

/// Server descriptor: which child to spawn.
#[derive(Debug, Clone, PartialEq)]
pub struct McpServerSpec {
    pub id: String,
    pub cmd: String,
    pub args: Vec<String>,
}

impl McpServerSpec {
    fn command(&self) -> tokio::process::Command {
        let mut c = tokio::process::Command::new(&self.cmd);
        c.args(&self.args);
        c
    }
}

impl From<&polymorph_core::config::McpServerEntry> for McpServerSpec {
    fn from(e: &polymorph_core::config::McpServerEntry) -> Self {
        Self {
            id: e.id.clone(),
            cmd: e.cmd.clone(),
            args: e.args.clone(),
        }
    }
}

/// Wire configured servers: add + connect + list + enroll per server, fail-soft (warn + continue —
/// one broken server never kills the run). Returns enrolled tool names for the model allowlist. The
/// bench deliberately does NOT call this (apparatus stability — boards stay comparable; stated at
/// its registry site).
pub async fn wire_mcp_servers(
    registry: &mut Registry,
    servers: &[polymorph_core::config::McpServerEntry],
) -> Vec<String> {
    let mut names = Vec::new();
    for entry in servers {
        let spec = McpServerSpec::from(entry);
        if let Err(e) = registry.add_mcp_server(spec.clone()) {
            tracing::warn!(server = %spec.id, error = %e, "mcp server skipped");
            continue;
        }
        // Connect once here (not again on first call): the listed client
        // is injected straight into the pool.
        let svc = match connect_stdio(&spec).await {
            Err(e) => {
                tracing::warn!(server = %spec.id, error = %e, "mcp server skipped");
                continue;
            }
            Ok(s) => s,
        };
        let tools = match remote_tool_infos(&svc).await {
            Err(e) => {
                tracing::warn!(server = %spec.id, error = %e, "mcp server skipped");
                continue;
            }
            Ok(t) => t,
        };
        registry
            .inject_mcp_client(&spec.id, std::sync::Arc::new(svc))
            .await;
        match enroll_mcp_tools(registry, &spec.id, &tools) {
            Err(e) => tracing::warn!(server = %spec.id, error = %e, "mcp server skipped"),
            Ok(()) => names.extend(tools.iter().map(|t| mcp_tool_name(&spec.id, &t.name))),
        }
    }
    names
}

/// Registry name for one server tool: `mcp__<server>__<tool>`.
/// Server ids never hold `_` (enforced at enroll/connect) so the FIRST
/// `__` split is unambiguous even when tool names contain `__`.
pub fn mcp_tool_name(server: &str, tool: &str) -> String {
    format!("mcp__{server}__{tool}")
}

/// Split back; None unless both segments are non-empty.
pub fn split_mcp_tool_name(name: &str) -> Option<(&str, &str)> {
    let rest = name.strip_prefix("mcp__")?;
    let (server, tool) = rest.split_once("__")?;
    (!server.is_empty() && !tool.is_empty()).then_some((server, tool))
}

/// Server ids are ours (config): `[A-Za-z0-9-]` only, so `__` stays an
/// unambiguous separator and Gate prefix rules stay readable.
fn valid_server_id(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// Minimal tool view for enrollment (D2). Carries the server schema for the `Registry::schema`
/// extension; the server validates args.
#[derive(Debug, Clone, PartialEq)]
pub struct McpToolInfo {
    pub name: String,
    pub description: String,
    pub schema: serde_json::Value,
}

impl McpToolInfo {
    pub fn from_rmcp(tool: &rmcp::model::Tool) -> Self {
        Self {
            name: tool.name.to_string(),
            description: tool.description.as_deref().unwrap_or_default().to_string(),
            schema: serde_json::Value::Object(tool.input_schema.as_ref().clone()),
        }
    }
}

/// Spawn the stdio transport for one server (D1). Thin over
/// `TokioChildProcess::builder().spawn()` — no logic, so the only test is
/// the negative (missing binary fails fast, no hang, no spawn).
pub fn spawn_stdio_transport(
    spec: &McpServerSpec,
) -> std::io::Result<rmcp::transport::TokioChildProcess> {
    if !valid_server_id(&spec.id) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("mcp server id {:?} must match [A-Za-z0-9-]+", spec.id),
        ));
    }
    let (transport, _stderr) =
        rmcp::transport::TokioChildProcess::builder(spec.command()).spawn()?;
    Ok(transport)
}

/// Serve a client over a spawned stdio transport and list its tools.
/// Transport-generic by construction (duplex loopback in tests, child
/// stdio in production — the same `.serve()` call both ways).
pub async fn connect_stdio(
    spec: &McpServerSpec,
) -> anyhow::Result<rmcp::service::RunningService<rmcp::RoleClient, ()>> {
    let transport = spawn_stdio_transport(spec)
        .map_err(|e| anyhow::anyhow!("mcp server {}: spawn failed: {e}", spec.id))?;
    rmcp::ServiceExt::serve((), transport)
        .await
        .map_err(|e| anyhow::anyhow!("mcp server {}: initialize failed: {e}", spec.id))
}

/// Read one server's tool list off a running client.
pub async fn remote_tool_infos(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
) -> anyhow::Result<Vec<McpToolInfo>> {
    let listed = client
        .list_tools(None)
        .await
        .map_err(|e| anyhow::anyhow!("mcp list_tools failed: {e}"))?;
    Ok(listed.tools.iter().map(McpToolInfo::from_rmcp).collect())
}

/// Enroll one server's tools into a registry (D2). Duplicates refuse
/// (`register` bails — programming error, loud, v1 precedent). Schemas
/// ride alongside for `Registry::schema_of` (the server validates args —
/// stated, not re-implemented).
pub fn enroll_mcp_tools(
    registry: &mut Registry,
    server: &str,
    tools: &[McpToolInfo],
) -> anyhow::Result<()> {
    if !valid_server_id(server) {
        anyhow::bail!("mcp server id {server:?} must match [A-Za-z0-9-]+");
    }
    for t in tools {
        if t.name.is_empty() {
            anyhow::bail!("mcp server {server:?} listed an empty tool name");
        }
        let name = mcp_tool_name(server, &t.name);
        registry.mcp_schemas.insert(name.clone(), t.schema.clone());
        registry.register(ToolDef {
            name,
            description: if t.description.is_empty() {
                format!("MCP tool {} (no description)", t.name)
            } else {
                t.description.clone()
            },
        })?;
    }
    Ok(())
}

/// One connected server. `Arc` so calls drop the pool lock before doing
/// network IO (lock sections stay short — never held across awaits).
pub type McpClient = std::sync::Arc<rmcp::service::RunningService<rmcp::RoleClient, ()>>;

/// Persistent clients, one per server. Lazy: the first call connects; eviction-on-break is
/// (a poisoned client fails until process end — no prod callers yet, unit surface only, stated).
#[derive(Debug, Default)]
pub struct McpPool {
    specs: std::collections::BTreeMap<String, McpServerSpec>,
    clients: std::collections::BTreeMap<String, McpClient>,
}

impl McpPool {
    /// Register a server spec (connect stays lazy). Duplicate ids refuse.
    pub fn add_server(&mut self, spec: McpServerSpec) -> anyhow::Result<()> {
        if !valid_server_id(&spec.id) {
            anyhow::bail!("mcp server id {:?} must match [A-Za-z0-9-]+", spec.id);
        }
        if self.specs.contains_key(&spec.id) {
            anyhow::bail!("duplicate mcp server: {}", spec.id);
        }
        self.specs.insert(spec.id.clone(), spec);
        Ok(())
    }

    /// Test + cassette seam: inject an already-connected client.
    pub fn insert_client(&mut self, id: &str, client: McpClient) {
        self.clients.insert(id.to_string(), client);
    }

    /// Connected server ids (observability — the denied-before-connect
    /// proof reads this).
    pub fn connected(&self) -> Vec<String> {
        self.clients.keys().cloned().collect()
    }
}

/// Call one enrolled MCP tool. Transport and `is_error` faults become `Failed` (recoverable at the
/// feed loop, F3b precedent); only the gate denies (taxonomy stays meaningful). No new `CallError`
/// variant.
pub async fn call_mcp_tool(
    pool: &tokio::sync::Mutex<McpPool>,
    name: &str,
    args: &serde_json::Value,
    gate: &crate::Gate,
) -> Result<serde_json::Value, crate::CallError> {
    use crate::CallError;
    // Gate FIRST: before connect and before call. A denial leaves
    // the pool untouched — connecting is itself a side effect.
    gate.check(name).map_err(CallError::Denied)?;
    let (server, tool) = split_mcp_tool_name(name)
        .ok_or_else(|| CallError::Failed(anyhow::anyhow!("bad mcp tool name {name:?}")))?;
    // Server-level rules use the SAME `__` joining, which the `:`/` `-segment
    // Gate never matches (caught by test — `mcp__probe` does not prefix-match
    // `mcp__probe__echo` under segment rules). Check the server prefix
    // explicitly so one rule still governs a whole server.
    gate.check(&format!("mcp__{server}"))
        .map_err(CallError::Denied)?;
    // Resolve or lazily connect in two short lock sections — the lock is
    // never held across network IO (single-user CLI; concurrent first
    // calls may double-spawn, last insert wins, the loser drops clean).
    let resolved = {
        let p = pool.lock().await;
        (p.clients.get(server).cloned(), p.specs.get(server).cloned())
    };
    let client = match resolved {
        (Some(c), _) => c,
        (None, Some(spec)) => {
            let svc = connect_stdio(&spec).await.map_err(CallError::Failed)?;
            let c: McpClient = std::sync::Arc::new(svc);
            pool.lock()
                .await
                .clients
                .insert(server.to_string(), c.clone());
            c
        }
        (None, None) => {
            return Err(CallError::Failed(anyhow::anyhow!(
                "mcp server {server:?} unknown (add it before calling)"
            )));
        }
    };
    // The model replies with args matching the tool schema directly (V1
    // shape) — the whole object IS the arguments. Non-objects fail naming
    // tool + field (arg_str precedent).
    let obj = args.as_object().cloned().ok_or_else(|| {
        CallError::Failed(anyhow::anyhow!("mcp {name}: arguments must be an object"))
    })?;
    let mut params = rmcp::model::CallToolRequestParams::new(tool.to_string());
    params.arguments = Some(obj);
    let result = client.call_tool(params).await;
    let result = match result {
        Err(e) => {
            // Evict the poisoned client (D5): the transport is broken, so
            // the next call reconnects from spec instead of failing on a
            // dead handle forever.
            pool.lock().await.clients.remove(server);
            return Err(CallError::Failed(anyhow::anyhow!(
                "mcp {name}: call failed: {e}"
            )));
        }
        Ok(r) => r,
    };
    if result.is_error == Some(true) {
        return Err(CallError::Failed(anyhow::anyhow!(
            "mcp {name} reported error: {}",
            result_text(&result)
        )));
    }
    let mut out = serde_json::Map::new();
    out.insert(
        "content".to_string(),
        serde_json::to_value(&result.content).map_err(|e| CallError::Failed(e.into()))?,
    );
    out.insert("is_error".to_string(), serde_json::Value::Bool(false));
    if let Some(sc) = result.structured_content {
        out.insert("structured_content".to_string(), sc);
    }
    Ok(serde_json::Value::Object(out))
}

/// Readable text of a result: Text blocks joined, else a wire-JSON fallback
/// (never empty — error paths must name something).
fn result_text(result: &rmcp::model::CallToolResult) -> String {
    let mut texts = Vec::new();
    for block in &result.content {
        if let rmcp::model::ContentBlock::Text(t) = block {
            texts.push(t.text.clone());
        }
    }
    if texts.is_empty() {
        serde_json::to_string(&result.content).unwrap_or_else(|_| "(unreadable)".into())
    } else {
        texts.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shared loopback server (echo + add + failing tool). One definition
    /// serves every protocol test — tool-name drift would desync enroll
    /// asserts otherwise.
    #[derive(Clone, Default)]
    struct ProbeServer;

    #[rmcp::tool_router(server_handler)]
    impl ProbeServer {
        #[rmcp::tool(description = "probe echo tool")]
        async fn probe_echo(&self) -> String {
            "echo".to_string()
        }

        #[rmcp::tool(description = "probe adder")]
        async fn probe_add(&self) -> String {
            "added".to_string()
        }

        #[rmcp::tool(description = "probe failing tool")]
        async fn probe_fail(&self) -> rmcp::model::CallToolResult {
            rmcp::model::CallToolResult::error(vec![rmcp::model::ContentBlock::text("boom")])
        }
    }

    #[test]
    fn mcp_names_split_inverses_and_reject_shapes() {
        // Round-trips, including `__` inside tool names (server ids never
        // hold `_`, so the first split is unambiguous).
        for (server, tool) in [("ara", "deep"), ("srv-1", "a__b"), ("s", "mcp__x")] {
            let joined = mcp_tool_name(server, tool);
            assert_eq!(split_mcp_tool_name(&joined), Some((server, tool)));
        }
        // Refusals: missing prefix, empty segments, bare prefix.
        for bad in [
            "read", "mcp__", "mcp__srv", "mcp____", "mcp____t", "mcp__s__",
        ] {
            assert_eq!(split_mcp_tool_name(bad), None, "{bad}");
        }
        // Server-id charset enforced at enroll (keeps `__` unambiguous).
        let mut r = Registry::new();
        let info = McpToolInfo {
            name: "t".into(),
            description: "d".into(),
            schema: serde_json::json!({"type": "object"}),
        };
        for bad_id in ["", "has_space", "under_score", "srv!"] {
            let err = enroll_mcp_tools(&mut r, bad_id, std::slice::from_ref(&info))
                .unwrap_err()
                .to_string();
            assert!(err.contains(bad_id), "{err}");
        }
        assert!(r.is_empty(), "refused enrollments register nothing");
    }

    #[test]
    fn spawn_missing_binary_fails_fast() {
        // Negative spawn path only (positive needs a protocol-speaking
        // child — loopback covers the protocol, covers transcripts).
        let spec = McpServerSpec {
            id: "probe".into(),
            cmd: "definitely-not-a-real-mcp-binary-xyz".into(),
            args: vec![],
        };
        assert!(spawn_stdio_transport(&spec).is_err());
    }

    /// Loopback: a real rmcp server (two tools) over `tokio::io::duplex`,
    /// listed through a real client, enrolled into a real registry.
    /// Proves connect + list + enroll for real; production swaps only the
    /// transport (child stdio) through the same `.serve()` call.
    #[tokio::test]
    async fn loopback_lists_and_enrolls() {
        let (a, b) = tokio::io::duplex(65536);
        let (ar, aw) = tokio::io::split(a);
        let (br, bw) = tokio::io::split(b);
        let server_t =
            rmcp::transport::async_rw::AsyncRwTransport::<rmcp::RoleServer, _, _>::new(ar, aw);
        let client_t =
            rmcp::transport::async_rw::AsyncRwTransport::<rmcp::RoleClient, _, _>::new(br, bw);
        // Both ends must progress together: each `serve` awaits the peer's
        // handshake, so sequential awaits deadlock (observed as a 60s hang).
        let (server, client) = tokio::join!(
            rmcp::serve_server(ProbeServer, server_t),
            rmcp::ServiceExt::serve((), client_t)
        );
        let _server = server.unwrap();
        let client = client.unwrap();

        let infos = remote_tool_infos(&client).await.unwrap();
        assert_eq!(infos.len(), 3);
        let mut names: Vec<&str> = infos.iter().map(|t| t.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["probe_add", "probe_echo", "probe_fail"]);
        assert!(infos.iter().all(|t| t.schema["type"] == "object"));

        let mut r = Registry::new();
        enroll_mcp_tools(&mut r, "probe", &infos).unwrap();
        assert!(r.get("mcp__probe__probe_echo").is_some());
        assert!(r.get("mcp__probe__nope").is_none());
        // Dispatch lands here: inject the loopback client and call through the registry exactly
        // like the feed loop does.
        r.inject_mcp_client("probe", std::sync::Arc::new(client))
            .await;
        let d = tempfile::tempdir().unwrap();
        let ctx = crate::ToolCallContext::new(
            d.path().to_path_buf(),
            crate::Gate::new(vec![], vec![]),
            tokio_util::sync::CancellationToken::new(),
        );
        let out = r
            .call("mcp__probe__probe_echo", serde_json::json!({}), &ctx)
            .await
            .unwrap();
        assert!(out.to_string().contains("echo"), "{out}");
        // `is_error` maps to Failed (recoverable at the feed loop),
        // naming the reported text — never mistaken for a denial.
        let err = r
            .call("mcp__probe__probe_fail", serde_json::json!({}), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, crate::CallError::Failed(_)), "{err:?}");
        assert!(err.to_string().contains("boom"), "{err}");
    }

    fn open_ctx(d: &tempfile::TempDir, deny: &[&str]) -> crate::ToolCallContext {
        crate::ToolCallContext::new(
            d.path().to_path_buf(),
            crate::Gate::new(vec![], deny.iter().map(|s| s.to_string()).collect()),
            tokio_util::sync::CancellationToken::new(),
        )
    }

    fn hand_info(name: &str) -> McpToolInfo {
        McpToolInfo {
            name: name.into(),
            description: "hand-built".into(),
            schema: serde_json::json!({"type": "object"}),
        }
    }

    #[tokio::test]
    async fn gate_denies_before_connect() {
        // D4: the gate speaks before any connect — a denial must leave the
        // pool untouched (no silent spawns). The spec points at a missing
        // binary, so any connect attempt would fail loudly instead.
        let mut r = Registry::new();
        enroll_mcp_tools(&mut r, "probe", &[hand_info("probe_echo")]).unwrap();
        r.add_mcp_server(McpServerSpec {
            id: "probe".into(),
            cmd: "definitely-not-a-real-mcp-binary-xyz".into(),
            args: vec![],
        })
        .unwrap();
        let d = tempfile::tempdir().unwrap();
        let err = r
            .call(
                "mcp__probe__probe_echo",
                serde_json::json!({}),
                &open_ctx(&d, &["mcp__probe"]),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, crate::CallError::Denied(_)), "{err:?}");
        assert!(r.mcp_connected().await.is_empty());
    }

    #[tokio::test]
    async fn call_without_spec_fails_naming_server() {
        // Enrolled names with no registered server fail naming the server
        // (specs arrive via `add_mcp_server`, names via enroll — either
        // side missing is a loud, attributable error).
        let mut r = Registry::new();
        enroll_mcp_tools(&mut r, "ghost", &[hand_info("t")]).unwrap();
        let d = tempfile::tempdir().unwrap();
        let err = r
            .call("mcp__ghost__t", serde_json::json!({}), &open_ctx(&d, &[]))
            .await
            .unwrap_err();
        assert!(matches!(err, crate::CallError::Failed(_)), "{err:?}");
        assert!(err.to_string().contains("ghost"), "{err}");
    }

    #[test]
    fn schema_of_prefers_server_schemas() {
        let mut r = Registry::new();
        enroll_mcp_tools(&mut r, "probe", &[hand_info("probe_echo")]).unwrap();
        assert_eq!(
            r.schema_of("mcp__probe__probe_echo"),
            Some(serde_json::json!({"type": "object"}))
        );
        // V1 static table still serves through the same call.
        assert!(r.schema_of("read").is_some());
        assert_eq!(r.schema_of("frobnicate"), None);
    }

    fn bogus_entry(id: &str) -> polymorph_core::config::McpServerEntry {
        polymorph_core::config::McpServerEntry {
            id: id.into(),
            cmd: "definitely-not-a-real-mcp-binary-xyz".into(),
            args: vec![],
        }
    }

    #[tokio::test]
    async fn wire_fail_soft_on_missing_binary() {
        // One broken server never kills the run: warn-and-continue leaves
        // names, pool, and specs-visible state exactly as found (spec
        // registration itself succeeds — retries stay possible).
        let mut r = Registry::new();
        let before = r.names();
        let names = wire_mcp_servers(&mut r, &[bogus_entry("ghost")]).await;
        assert!(names.is_empty());
        assert_eq!(r.names(), before);
        assert!(r.mcp_connected().await.is_empty());
    }

    #[tokio::test]
    async fn evict_on_transport_break() {
        // D5: a call failing on a dead transport evicts the poisoned
        // client, so the next call reconnects from spec instead of failing
        // on the dead handle forever. Deterministic: the server side is
        // dropped BEFORE the call, so EOF is immediate (no hang).
        let (a, b) = tokio::io::duplex(65536);
        let (ar, aw) = tokio::io::split(a);
        let (br, bw) = tokio::io::split(b);
        let server_t =
            rmcp::transport::async_rw::AsyncRwTransport::<rmcp::RoleServer, _, _>::new(ar, aw);
        let client_t =
            rmcp::transport::async_rw::AsyncRwTransport::<rmcp::RoleClient, _, _>::new(br, bw);
        let (server, client) = tokio::join!(
            rmcp::serve_server(ProbeServer, server_t),
            rmcp::ServiceExt::serve((), client_t)
        );
        let _server = server.unwrap();
        let client = client.unwrap();
        let pool = tokio::sync::Mutex::new(McpPool::default());
        pool.lock()
            .await
            .insert_client("probe", std::sync::Arc::new(client));
        drop(_server); // transport breaks here; the call below must fail fast
        let err = call_mcp_tool(
            &pool,
            "mcp__probe__probe_echo",
            &serde_json::json!({}),
            &crate::Gate::new(vec![], vec![]),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, crate::CallError::Failed(_)),
            "break is Failed, never Denied: {err:?}"
        );
        assert!(pool.lock().await.connected().is_empty());
    }
}
