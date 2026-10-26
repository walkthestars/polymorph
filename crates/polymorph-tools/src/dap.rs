//! DAP client substrate (framing + spawn + initialize).
//!
//! D1/D2 (design): `tools::dap` module (NOT a new crate —
//! debugging is a tool-execution concern, no new surface ships), stdio +
//! `Content-Length` framing REUSED from [`crate::lsp`] (`encode_message` /
//! `read_message` are transport-generic: `Value` in, framed bytes out —
//! zero LSP-specific content), client-only v1. Zero new deps (offline
//! survey: no DAP-protocol crates in the local cargo cache, network
//! survey policy-denied — the L hand-rolled precedent stands).
//!
//! DAP wire differences from LSP (general knowledge re-verifies
//! against a scripted peer below — assumed nowhere): every message
//! carries `seq` + `type` (`request` | `response` | `event`); responses
//! pair via `request_seq` (not `id`); server→client traffic is `event`
//! frames (`initialized`, `stopped`, `output` …) rather than
//! notifications. Lifecycles overlap by protocol design (the adapter
//! holds `launch` until `configurationDone`), so `send_request` +
//! `await_response` split the round-trip and strangers park in a pending
//! map — never refused, never mispairing.

use anyhow::Context as _;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

/// Per-call ceiling (spawn + handshake + requests): fail naming the
/// timeout, never hang a loop on a sick adapter (L `CALL_TIMEOUT`
/// direction, same 30 s).
const DAP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Server descriptor: which child to spawn (argv passes as items, never
/// a shell string class; the `[[dap_adapters]]` table maps onto
/// this at M2). `env` carries adapter-scoped variables (e.g. `PYTHONPATH`
/// at extension-bundled libs) — per-child, never process-global (the
/// `set_var` race class).
#[derive(Debug, Clone, PartialEq)]
pub struct DapSpec {
    pub cmd: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// Spawn cwd (e.g. dlv builds debug binaries beside the package —
    /// without this they land in the harness cwd dlv finding).
    /// `None` inherits (debugpy/codelldb need none — absolute paths).
    pub workdir: Option<std::path::PathBuf>,
}

impl DapSpec {
    fn command(&self) -> tokio::process::Command {
        let mut c = tokio::process::Command::new(&self.cmd);
        c.args(&self.args);
        c.envs(self.env.iter().map(|(k, v)| (k, v)));
        if let Some(dir) = &self.workdir {
            c.current_dir(dir);
        }
        // A dropped client must not strand an adapter: kill on drop covers
        // hung processes (the `exec.rs` / `LspSpec::command` precedent).
        c.kill_on_drop(true);
        c
    }
}

/// Protocol half: seq pairing + event queue over any byte transport.
/// Generic so the handshake state machine proves out over loopback
/// duplex AND over real adapter pipes with zero duplication:
/// [`DapClient`] is one child plus this conn.
#[derive(Debug)]
pub struct DapConn<R, W> {
    reader: R,
    writer: W,
    next_seq: u64,
    /// Server-pushed `event` frames seen while awaiting responses,
    /// oldest-first. Observation only (client-only v1 posture).
    events: Vec<serde_json::Value>,
    /// Responses for OTHER outstanding requests, keyed by `request_seq`.
    /// The launch→configurationDone lifecycle overlaps two requests by
    /// protocol design (debugpy holds the `launch` response until
    /// configured), so awaiting one call must park — never refuse —
    /// strangers (debugpy finding, same chunk as the fix).
    pending: std::collections::BTreeMap<u64, serde_json::Value>,
}

impl<R, W> DapConn<R, W>
where
    R: AsyncBufReadExt + Unpin,
    W: AsyncWriteExt + Unpin,
{
    /// One request/response round-trip: writes carry the next `seq`;
    /// reads skip `event` frames (queued for [`Self::next_event`]) and
    /// park foreign responses (queued for their awaiter — the
    /// launch/configurationDone overlap is protocol-normal) until the
    /// response with OUR `request_seq` arrives. Transport AND
    /// `success:false` failures name the command.
    pub async fn request(
        &mut self,
        command: &str,
        arguments: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        let seq = self.send_request(command, arguments).await?;
        self.await_response(seq, command).await
    }

    /// Send half of [`Self::request`]: writes the next-`seq` frame and
    /// returns its `seq` WITHOUT awaiting. The caller pairs it later
    /// with [`Self::await_response`] — the launch-then-configure
    /// lifecycle needs the send outstanding while further requests go
    /// out (single `request()` calls would deadlock it: debugpy holds
    /// `launch` until `configurationDone`).
    pub async fn send_request(
        &mut self,
        command: &str,
        arguments: serde_json::Value,
    ) -> anyhow::Result<u64> {
        self.next_seq += 1;
        let seq = self.next_seq;
        let frame = crate::lsp::encode_message(&serde_json::json!({
            "seq": seq,
            "type": "request",
            "command": command,
            "arguments": arguments,
        }));
        let write = self.writer.write_all(&frame);
        tokio::time::timeout(DAP_TIMEOUT, write)
            .await
            .context("dap write timeout")?
            .context("dap write request")?;
        Ok(seq)
    }

    /// Await half of [`Self::request`]: resolves the response for `seq`,
    /// parking anything else (events → event queue, foreign responses →
    /// pending map) so overlapping lifecycles read in ANY arrival order.
    pub async fn await_response(
        &mut self,
        seq: u64,
        command: &str,
    ) -> anyhow::Result<serde_json::Value> {
        if let Some(msg) = self.pending.remove(&seq) {
            return Self::unwrap_response(msg, command);
        }
        loop {
            let read = crate::lsp::read_message(&mut self.reader);
            let msg = tokio::time::timeout(DAP_TIMEOUT, read)
                .await
                .context("dap read timeout")?
                .context("dap read frame")?;
            let kind = msg.get("type").and_then(serde_json::Value::as_str);
            if kind != Some("response") {
                // Server event (or reverse request — observed, never
                // answered, the lsp `incoming` precedent): queue it for
                // the event half — the pipe stays pumped, nothing lost.
                self.events.push(msg);
                continue;
            }
            let req_seq = msg.get("request_seq").and_then(serde_json::Value::as_u64);
            let Some(req_seq) = req_seq else {
                // Malformed per DAP (every response pairs): loud, never
                // parked — an unpairable frame in the pending map would
                // rot silently and poison later counts.
                anyhow::bail!("dap response without request_seq: {msg}");
            };
            if req_seq != seq {
                self.pending.insert(req_seq, msg);
                continue;
            }
            return Self::unwrap_response(msg, command);
        }
    }

    /// `success:false` fails naming the command; else the `body`
    /// (missing body = Null — some acks carry none).
    fn unwrap_response(msg: serde_json::Value, command: &str) -> anyhow::Result<serde_json::Value> {
        if msg.get("success") != Some(&serde_json::Value::Bool(true)) {
            let detail = msg
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown failure");
            anyhow::bail!("dap {command} error: {detail}");
        }
        Ok(msg.get("body").cloned().unwrap_or(serde_json::Value::Null))
    }

    /// Next server event: queued-first, else one frame off the pipe.
    /// A non-event frame here is a named error (responses belong to
    /// [`Self::request`], which owns the only outstanding `seq`).
    /// Per-read ceiling is [`DAP_TIMEOUT`]; [`Self::await_event`] owns
    /// longer bounded waits.
    pub async fn next_event(&mut self) -> anyhow::Result<serde_json::Value> {
        if !self.events.is_empty() {
            return Ok(self.events.remove(0));
        }
        let read = self.read_event_raw();
        tokio::time::timeout(DAP_TIMEOUT, read)
            .await
            .context("dap event wait timeout")?
    }

    /// One event frame off the pipe, no queue touch, no timeout — the
    /// callers ([`Self::next_event`], [`Self::await_event`]) own queue
    /// discipline and ceilings. (A queued-first read here would defeat
    /// `await_event`'s skip logic: the same non-matching event would
    /// return forever — caught live by the skip test, fixed same chunk.)
    async fn read_event_raw(&mut self) -> anyhow::Result<serde_json::Value> {
        let msg = crate::lsp::read_message(&mut self.reader)
            .await
            .context("dap read frame")?;
        if msg.get("type").and_then(serde_json::Value::as_str) != Some("event") {
            anyhow::bail!("dap expected event, got: {msg}");
        }
        Ok(msg)
    }

    /// Drain queued events observed so far, oldest-first.
    pub fn drain_events(&mut self) -> Vec<serde_json::Value> {
        std::mem::take(&mut self.events)
    }

    /// Wait for a named server event up to `budget`, skipping other
    /// events into the queue (the `await_publish` precedent: bounded
    /// listen, grace-free — the caller owns retry policy). Expiry names
    /// the awaited event, never a bare timeout.
    pub async fn await_event(
        &mut self,
        name: &str,
        budget: std::time::Duration,
    ) -> anyhow::Result<serde_json::Value> {
        self.await_any(std::slice::from_ref(&name), budget).await
    }

    /// Wait for the FIRST of several server events up to `budget`
    /// Continue/start must notice `terminated` the
    /// moment it posts — awaiting `stopped` alone would burn the whole
    /// budget on every run-to-completion program. One remaining-clock
    /// for all names; expiry names the set. Non-matching frames queue.
    pub async fn await_any(
        &mut self,
        names: &[&str],
        budget: std::time::Duration,
    ) -> anyhow::Result<serde_json::Value> {
        let start = std::time::Instant::now();
        // Queued-first, in arrival order: a previously observed event
        // resolves instantly.
        if let Some(i) = self.events.iter().position(|e| {
            e.get("event")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|k| names.contains(&k))
        }) {
            return Ok(self.events.remove(i));
        }
        loop {
            let remaining = budget
                .checked_sub(start.elapsed())
                .ok_or_else(|| anyhow::anyhow!("dap timed out waiting for event {names:?}"))?;
            // `read_event_raw` carries no ceiling of its own — `remaining`
            // is the only clock here, so minute-scale M2 budgets and
            // 50 ms unit budgets share one mechanism.
            match tokio::time::timeout(remaining, self.read_event_raw()).await {
                Err(_) => anyhow::bail!("dap timed out waiting for event {names:?}"),
                Ok(Err(e)) => return Err(e),
                Ok(Ok(ev)) => {
                    let kind = ev.get("event").and_then(serde_json::Value::as_str);
                    if kind.is_some_and(|k| names.contains(&k)) {
                        return Ok(ev);
                    }
                    self.events.push(ev);
                }
            }
        }
    }

    /// DAP handshake open: `initialize` with a fixed minimal capability
    /// set, returning the adapter's capabilities body. The server's
    /// `initialized` event arrives separately — read it with
    /// [`Self::next_event`] (the duplex test below proves the pair).
    pub async fn initialize(&mut self, adapter_id: &str) -> anyhow::Result<serde_json::Value> {
        self.request(
            "initialize",
            serde_json::json!({
                "clientID": "polymorph",
                "adapterID": adapter_id,
                "pathFormat": "path",
                "linesStartAt1": true,
                "columnsStartAt1": true,
            }),
        )
        .await
    }

    /// Core session ops: thin typed wrappers over [`Self::request`]
    /// so the M3 tool surface builds on proven argument shapes. Results
    /// pass through as `Value` (the L1b precedent — canonical mapping is
    /// M3's business, when agent-facing reads need field-level types).
    /// Lines are 1-based throughout (our `initialize` declares it).
    pub async fn launch(&mut self, args: serde_json::Value) -> anyhow::Result<serde_json::Value> {
        self.request("launch", args).await
    }

    pub async fn attach(&mut self, args: serde_json::Value) -> anyhow::Result<serde_json::Value> {
        self.request("attach", args).await
    }

    pub async fn set_breakpoints(
        &mut self,
        path: &str,
        lines: &[u32],
    ) -> anyhow::Result<serde_json::Value> {
        self.request(
            "setBreakpoints",
            serde_json::json!({
                "source": {"path": path},
                "breakpoints": lines.iter().map(|l| serde_json::json!({"line": l})).collect::<Vec<_>>(),
            }),
        )
        .await
    }

    pub async fn configuration_done(&mut self) -> anyhow::Result<serde_json::Value> {
        self.request("configurationDone", serde_json::Value::Null)
            .await
    }

    pub async fn threads(&mut self) -> anyhow::Result<serde_json::Value> {
        self.request("threads", serde_json::Value::Null).await
    }

    pub async fn stack_trace(
        &mut self,
        thread_id: u64,
        start_frame: u64,
        levels: u64,
    ) -> anyhow::Result<serde_json::Value> {
        self.request(
            "stackTrace",
            serde_json::json!({"threadId": thread_id, "startFrame": start_frame, "levels": levels}),
        )
        .await
    }

    pub async fn scopes(&mut self, frame_id: u64) -> anyhow::Result<serde_json::Value> {
        self.request("scopes", serde_json::json!({"frameId": frame_id}))
            .await
    }

    pub async fn variables(
        &mut self,
        variables_reference: u64,
    ) -> anyhow::Result<serde_json::Value> {
        self.request(
            "variables",
            serde_json::json!({"variablesReference": variables_reference}),
        )
        .await
    }

    pub async fn evaluate(
        &mut self,
        expression: &str,
        frame_id: Option<u64>,
    ) -> anyhow::Result<serde_json::Value> {
        let mut args = serde_json::json!({"expression": expression});
        if let Some(f) = frame_id {
            args["frameId"] = f.into();
        }
        self.request("evaluate", args).await
    }
}

/// One connected debug adapter: piped stdio child + protocol conn.
/// Client-only v1: requests + events — no serving.
#[derive(Debug)]
pub struct DapClient {
    child: tokio::process::Child,
    conn: DapConn<tokio::io::BufReader<tokio::process::ChildStdout>, tokio::process::ChildStdin>,
}

impl DapClient {
    /// Spawn the adapter child (piped stdio; stderr nulled so a chatty
    /// adapter can never pollute the terminal or the ledger).
    pub async fn spawn(spec: &DapSpec) -> anyhow::Result<Self> {
        let mut child = spec
            .command()
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .with_context(|| format!("dap spawn {} {:?}", spec.cmd, spec.args))?;
        let stdin = child.stdin.take().context("dap stdin not piped")?;
        let stdout = child.stdout.take().context("dap stdout not piped")?;
        Ok(Self {
            child,
            conn: DapConn {
                reader: tokio::io::BufReader::new(stdout),
                writer: stdin,
                next_seq: 0,
                events: Vec::new(),
                pending: std::collections::BTreeMap::new(),
            },
        })
    }

    /// Handshake open over the adapter pipe (see [`DapConn::initialize`]).
    pub async fn initialize(&mut self, adapter_id: &str) -> anyhow::Result<serde_json::Value> {
        self.conn.initialize(adapter_id).await
    }

    /// One request/response round-trip (see [`DapConn::request`]).
    pub async fn request(
        &mut self,
        command: &str,
        arguments: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        self.conn.request(command, arguments).await
    }

    /// Send half (see [`DapConn::send_request`]): returns the `seq` for
    /// a later [`Self::await_response`].
    pub async fn send_request(
        &mut self,
        command: &str,
        arguments: serde_json::Value,
    ) -> anyhow::Result<u64> {
        self.conn.send_request(command, arguments).await
    }

    /// Await half (see [`DapConn::await_response`]).
    pub async fn await_response(
        &mut self,
        seq: u64,
        command: &str,
    ) -> anyhow::Result<serde_json::Value> {
        self.conn.await_response(seq, command).await
    }

    /// Next server event (see [`DapConn::next_event`]).
    pub async fn next_event(&mut self) -> anyhow::Result<serde_json::Value> {
        self.conn.next_event().await
    }

    /// Drain queued events (see [`DapConn::drain_events`]).
    pub fn drain_events(&mut self) -> Vec<serde_json::Value> {
        self.conn.drain_events()
    }

    /// Wait for a named server event (see [`DapConn::await_event`]).
    pub async fn await_event(
        &mut self,
        name: &str,
        budget: std::time::Duration,
    ) -> anyhow::Result<serde_json::Value> {
        self.conn.await_event(name, budget).await
    }

    /// Core session ops (see `DapConn` M1b wrappers).
    pub async fn launch(&mut self, args: serde_json::Value) -> anyhow::Result<serde_json::Value> {
        self.conn.launch(args).await
    }
    pub async fn attach(&mut self, args: serde_json::Value) -> anyhow::Result<serde_json::Value> {
        self.conn.attach(args).await
    }
    pub async fn set_breakpoints(
        &mut self,
        path: &str,
        lines: &[u32],
    ) -> anyhow::Result<serde_json::Value> {
        self.conn.set_breakpoints(path, lines).await
    }
    pub async fn configuration_done(&mut self) -> anyhow::Result<serde_json::Value> {
        self.conn.configuration_done().await
    }
    pub async fn threads(&mut self) -> anyhow::Result<serde_json::Value> {
        self.conn.threads().await
    }
    pub async fn stack_trace(
        &mut self,
        thread_id: u64,
        start_frame: u64,
        levels: u64,
    ) -> anyhow::Result<serde_json::Value> {
        self.conn.stack_trace(thread_id, start_frame, levels).await
    }
    pub async fn scopes(&mut self, frame_id: u64) -> anyhow::Result<serde_json::Value> {
        self.conn.scopes(frame_id).await
    }
    pub async fn variables(
        &mut self,
        variables_reference: u64,
    ) -> anyhow::Result<serde_json::Value> {
        self.conn.variables(variables_reference).await
    }
    pub async fn evaluate(
        &mut self,
        expression: &str,
        frame_id: Option<u64>,
    ) -> anyhow::Result<serde_json::Value> {
        self.conn.evaluate(expression, frame_id).await
    }

    /// Tear down: best-effort `disconnect` (never reads — a wedged
    /// adapter must not stall teardown), then reap the child (kill on
    /// best-effort — a hung adapter must not wedge test teardown OR
    /// the loop).
    pub async fn disconnect(mut self) -> anyhow::Result<()> {
        self.teardown().await;
        Ok(())
    }

    /// Best-effort teardown shared by [`Self::disconnect`] and the
    /// `DapSession::shutdown` trait path (session-table removal must
    /// never fail the caller — `kill_on_drop` is the final backstop).
    async fn teardown(&mut self) {
        self.next_seq_disconnect().await;
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
    }

    /// Best-effort disconnect request without reading the response.
    /// Timeout-guarded (never reads, never blocks teardown past the
    /// ceiling — a wedged adapter with a full stdin pipe must not stall
    /// the loop's teardown path).
    async fn next_seq_disconnect(&mut self) {
        self.conn.next_seq += 1;
        let seq = self.conn.next_seq;
        let frame = crate::lsp::encode_message(&serde_json::json!({
            "seq": seq,
            "type": "request",
            "command": "disconnect",
            "arguments": {"terminateDebuggee": true},
        }));
        let write = self.conn.writer.write_all(&frame);
        let _ = tokio::time::timeout(DAP_TIMEOUT, write).await;
    }
}

/// TCP-connected debug adapter: some adapters speak
/// DAP over TCP only (`dlv dap -l`, `codelldb --port`) — stdio is not
/// universal. `DapConn`'s R/W genericity carries the whole protocol
/// with zero duplication: split halves plug straight in.
/// Owns the child for teardown parity with [`DapClient`] (kill + reap).
pub struct DapTcpClient {
    child: tokio::process::Child,
    conn: DapConn<
        tokio::io::BufReader<tokio::net::tcp::OwnedReadHalf>,
        tokio::net::tcp::OwnedWriteHalf,
    >,
}

impl DapTcpClient {
    /// Spawn `spec` (expected to listen) and connect to `addr` with
    /// retries (the adapter bind races process start — poll, never
    /// assume). Stderr/stdout disposition is the caller's (pass piped
    /// handles when port discovery needs them; else null).
    pub async fn connect(
        spec: &DapSpec,
        addr: std::net::SocketAddr,
        env_extra: &[(String, String)],
    ) -> anyhow::Result<Self> {
        let mut cmd = spec.command();
        cmd.envs(env_extra.iter().map(|(k, v)| (k, v)));
        let mut child = cmd
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .with_context(|| format!("dap tcp spawn {} {:?}", spec.cmd, spec.args))?;
        let stdout = child.stdout.take().context("dap tcp stdout not piped")?;
        let stderr = child.stderr.take().context("dap tcp stderr not piped")?;
        // Drain both pipes in the background: a chatty adapter must never
        // block on full buffers while we poll-connect (detached on drop —
        // the child keeps running piped; kill + EOF end the task).
        let drain = |mut r: tokio::process::ChildStdout, mut e: tokio::process::ChildStderr| async move {
            use tokio::io::AsyncReadExt;
            let mut rb = vec![0u8; 8192];
            let mut eb = vec![0u8; 8192];
            loop {
                tokio::select! {
                    a = r.read(&mut rb) => { if a.unwrap_or(0) == 0 { break; } }
                    b = e.read(&mut eb) => { if b.unwrap_or(0) == 0 { break; } }
                }
            }
        };
        let _drainer = tokio::spawn(drain(stdout, stderr));
        Self::wrap_connected(child, addr).await
    }

    /// Spawn `spec` (expected to print its listening address, e.g.
    /// `dlv dap -l 127.0.0.1:0`) and connect to the DISCOVERED port:
    /// stderr lines are scanned with [`parse_listening_port`] (first
    /// match wins, 30 s ceiling), then [`Self::connect`]'s retry loop
    /// takes over. stdout drains in the background throughout.
    pub async fn connect_discovered(
        spec: &DapSpec,
        env_extra: &[(String, String)],
    ) -> anyhow::Result<Self> {
        let mut cmd = spec.command();
        cmd.envs(env_extra.iter().map(|(k, v)| (k, v)));
        let mut child = cmd
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .with_context(|| format!("dap tcp spawn {} {:?}", spec.cmd, spec.args))?;
        let stdout = child.stdout.take().context("dap tcp stdout not piped")?;
        let stderr = child.stderr.take().context("dap tcp stderr not piped")?;
        let port = {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let mut out_lines = BufReader::new(stdout).lines();
            let mut err_lines = BufReader::new(stderr).lines();
            let mut found = None;
            let start = std::time::Instant::now();
            // Adapters disagree which pipe carries the address (dlv:
            // stdout `DAP server listening at:`; others: stderr) —
            // scan BOTH, first match wins (dlv finding, same chunk).
            // EOF on either pipe breaks the scan: a healthy adapter
            // holds both pipes open through startup, so EOF here means
            // a dying adapter — the no-port bail below names it.
            while start.elapsed() < DAP_TIMEOUT {
                let remaining = DAP_TIMEOUT
                    .checked_sub(start.elapsed())
                    .unwrap_or(std::time::Duration::ZERO);
                let tick = tokio::time::timeout(remaining, async {
                    tokio::select! {
                        l = out_lines.next_line() => l,
                        l = err_lines.next_line() => l,
                    }
                })
                .await;
                let line = match tick {
                    Err(_) => break,
                    Ok(r) => r.context("dap port-scan read")?,
                };
                let Some(line) = line else { break };
                if let Some(p) = parse_listening_port(&line) {
                    found = Some(p);
                    break;
                }
            }
            // Keep draining both pipes after the scan: dropping either
            // would close it and SIGPIPE-kill a logging adapter mid-run
            // (unwrap both line-readers back to raw handles first).
            let mut stdout = out_lines.into_inner().into_inner();
            let mut stderr = err_lines.into_inner().into_inner();
            tokio::spawn(async move {
                use tokio::io::AsyncReadExt;
                let mut ob = vec![0u8; 8192];
                let mut eb = vec![0u8; 8192];
                loop {
                    tokio::select! {
                        a = stdout.read(&mut ob) => { if a.unwrap_or(0) == 0 { break; } }
                        b = stderr.read(&mut eb) => { if b.unwrap_or(0) == 0 { break; } }
                    }
                }
            });
            found
        };
        let Some(port) = port else {
            let _ = child.kill().await;
            anyhow::bail!("dap adapter printed no listening port in 30 s");
        };
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        Self::wrap_connected(child, addr).await
    }

    /// Retry-connect to `addr` for a live `child`, then wrap the stream.
    /// Shared tail of [`Self::connect`] and [`Self::connect_discovered`]
    /// (the adapter bind races process start — poll, never assume).
    async fn wrap_connected(
        mut child: tokio::process::Child,
        addr: std::net::SocketAddr,
    ) -> anyhow::Result<Self> {
        let deadline = std::time::Instant::now() + DAP_TIMEOUT;
        let stream = loop {
            match tokio::net::TcpStream::connect(addr).await {
                Ok(s) => break s,
                Err(e) => {
                    if std::time::Instant::now() >= deadline {
                        let _ = child.kill().await;
                        anyhow::bail!("dap tcp connect {addr} failed: {e:#}");
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }
        };
        let (rh, wh) = stream.into_split();
        Ok(Self {
            child,
            conn: DapConn {
                reader: tokio::io::BufReader::new(rh),
                writer: wh,
                next_seq: 0,
                events: Vec::new(),
                pending: std::collections::BTreeMap::new(),
            },
        })
    }

    /// Tear down: best-effort `disconnect` (no read — see
    /// [`DapClient::disconnect`]), then reap the child.
    pub async fn disconnect(mut self) -> anyhow::Result<()> {
        self.teardown().await;
        Ok(())
    }

    /// Best-effort teardown shared by [`Self::disconnect`] and the
    /// `DapSession::shutdown` trait path (see `DapClient::teardown`).
    async fn teardown(&mut self) {
        let seq = {
            self.conn.next_seq += 1;
            self.conn.next_seq
        };
        let frame = crate::lsp::encode_message(&serde_json::json!({
            "seq": seq,
            "type": "request",
            "command": "disconnect",
            "arguments": {"terminateDebuggee": true},
        }));
        let write = self.conn.writer.write_all(&frame);
        let _ = tokio::time::timeout(DAP_TIMEOUT, write).await;
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
    }
}

// Delegate the protocol surface so callers drive stdio and TCP
// sessions identically (matrix rows differ only in construction).
macro_rules! delegate_conn {
    ($( $name:ident ( $( $arg:ident : $ty:ty ),* ) ),* $(,)?) => {
        impl DapTcpClient {
            $(
                pub async fn $name(&mut self, $( $arg : $ty ),*) -> anyhow::Result<serde_json::Value> {
                    self.conn.$name($( $arg ),*).await
                }
            )*
        }
    };
}

delegate_conn! {
    initialize(adapter_id: &str),
    request(command: &str, arguments: serde_json::Value),
    next_event(),
    await_event(name: &str, budget: std::time::Duration),
    await_any(names: &[&str], budget: std::time::Duration),
    launch(args: serde_json::Value),
    attach(args: serde_json::Value),
    set_breakpoints(path: &str, lines: &[u32]),
    configuration_done(),
    threads(),
    stack_trace(thread_id: u64, start_frame: u64, levels: u64),
    scopes(frame_id: u64),
    variables(variables_reference: u64),
    evaluate(expression: &str, frame_id: Option<u64>),
}

// `send_request` returns `u64` (not `Value`) and `await_response` takes
// a leading `seq` — neither fits the delegate macro above, so both are
// spelled out (two methods, no cleverness).
impl DapTcpClient {
    /// Send half (see [`DapConn::send_request`]).
    pub async fn send_request(
        &mut self,
        command: &str,
        arguments: serde_json::Value,
    ) -> anyhow::Result<u64> {
        self.conn.send_request(command, arguments).await
    }

    /// Await half (see [`DapConn::await_response`]).
    pub async fn await_response(
        &mut self,
        seq: u64,
        command: &str,
    ) -> anyhow::Result<serde_json::Value> {
        self.conn.await_response(seq, command).await
    }
}

/// Uniform session surface over stdio ([`DapClient`]) and TCP
/// ([`DapTcpClient`]) sessions: the matrix driver —
/// and later M3's tool surface — writes ONE flow against this trait
/// instead of per-transport copies. Lifecycles, not transports.
#[async_trait::async_trait]
pub trait DapSession: Send {
    async fn initialize(&mut self, adapter_id: &str) -> anyhow::Result<serde_json::Value>;
    async fn send_request(
        &mut self,
        command: &str,
        arguments: serde_json::Value,
    ) -> anyhow::Result<u64>;
    async fn await_response(
        &mut self,
        seq: u64,
        command: &str,
    ) -> anyhow::Result<serde_json::Value>;
    async fn request(
        &mut self,
        command: &str,
        arguments: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value>;
    async fn await_event(
        &mut self,
        name: &str,
        budget: std::time::Duration,
    ) -> anyhow::Result<serde_json::Value>;
    async fn await_any(
        &mut self,
        names: &[&str],
        budget: std::time::Duration,
    ) -> anyhow::Result<serde_json::Value>;
    async fn launch(&mut self, args: serde_json::Value) -> anyhow::Result<serde_json::Value>;
    async fn set_breakpoints(
        &mut self,
        path: &str,
        lines: &[u32],
    ) -> anyhow::Result<serde_json::Value>;
    async fn configuration_done(&mut self) -> anyhow::Result<serde_json::Value>;
    async fn threads(&mut self) -> anyhow::Result<serde_json::Value>;
    async fn stack_trace(
        &mut self,
        thread_id: u64,
        start_frame: u64,
        levels: u64,
    ) -> anyhow::Result<serde_json::Value>;
    async fn scopes(&mut self, frame_id: u64) -> anyhow::Result<serde_json::Value>;
    async fn variables(&mut self, variables_reference: u64) -> anyhow::Result<serde_json::Value>;
    async fn evaluate(
        &mut self,
        expression: &str,
        frame_id: Option<u64>,
    ) -> anyhow::Result<serde_json::Value>;
    /// Best-effort teardown (see `DapClient::disconnect`): fire the
    /// disconnect request without reading, then reap. Never fails the
    /// caller — teardown observes, and `kill_on_drop` is the backstop.
    async fn shutdown(&mut self);
}

#[async_trait::async_trait]
impl DapSession for DapClient {
    async fn initialize(&mut self, adapter_id: &str) -> anyhow::Result<serde_json::Value> {
        DapClient::initialize(self, adapter_id).await
    }
    async fn send_request(
        &mut self,
        command: &str,
        arguments: serde_json::Value,
    ) -> anyhow::Result<u64> {
        self.conn.send_request(command, arguments).await
    }
    async fn await_response(
        &mut self,
        seq: u64,
        command: &str,
    ) -> anyhow::Result<serde_json::Value> {
        self.conn.await_response(seq, command).await
    }
    async fn request(
        &mut self,
        command: &str,
        arguments: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        DapClient::request(self, command, arguments).await
    }
    async fn await_event(
        &mut self,
        name: &str,
        budget: std::time::Duration,
    ) -> anyhow::Result<serde_json::Value> {
        self.conn.await_event(name, budget).await
    }
    async fn launch(&mut self, args: serde_json::Value) -> anyhow::Result<serde_json::Value> {
        DapClient::launch(self, args).await
    }
    async fn set_breakpoints(
        &mut self,
        path: &str,
        lines: &[u32],
    ) -> anyhow::Result<serde_json::Value> {
        DapClient::set_breakpoints(self, path, lines).await
    }
    async fn configuration_done(&mut self) -> anyhow::Result<serde_json::Value> {
        DapClient::configuration_done(self).await
    }
    async fn threads(&mut self) -> anyhow::Result<serde_json::Value> {
        DapClient::threads(self).await
    }
    async fn stack_trace(
        &mut self,
        thread_id: u64,
        start_frame: u64,
        levels: u64,
    ) -> anyhow::Result<serde_json::Value> {
        DapClient::stack_trace(self, thread_id, start_frame, levels).await
    }
    async fn scopes(&mut self, frame_id: u64) -> anyhow::Result<serde_json::Value> {
        DapClient::scopes(self, frame_id).await
    }
    async fn variables(&mut self, variables_reference: u64) -> anyhow::Result<serde_json::Value> {
        DapClient::variables(self, variables_reference).await
    }
    async fn evaluate(
        &mut self,
        expression: &str,
        frame_id: Option<u64>,
    ) -> anyhow::Result<serde_json::Value> {
        DapClient::evaluate(self, expression, frame_id).await
    }
    async fn await_any(
        &mut self,
        names: &[&str],
        budget: std::time::Duration,
    ) -> anyhow::Result<serde_json::Value> {
        self.conn.await_any(names, budget).await
    }
    async fn shutdown(&mut self) {
        self.teardown().await;
    }
}

#[async_trait::async_trait]
impl DapSession for DapTcpClient {
    async fn initialize(&mut self, adapter_id: &str) -> anyhow::Result<serde_json::Value> {
        DapTcpClient::initialize(self, adapter_id).await
    }
    async fn send_request(
        &mut self,
        command: &str,
        arguments: serde_json::Value,
    ) -> anyhow::Result<u64> {
        self.conn.send_request(command, arguments).await
    }
    async fn await_response(
        &mut self,
        seq: u64,
        command: &str,
    ) -> anyhow::Result<serde_json::Value> {
        self.conn.await_response(seq, command).await
    }
    async fn request(
        &mut self,
        command: &str,
        arguments: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        DapTcpClient::request(self, command, arguments).await
    }
    async fn await_event(
        &mut self,
        name: &str,
        budget: std::time::Duration,
    ) -> anyhow::Result<serde_json::Value> {
        self.conn.await_event(name, budget).await
    }
    async fn launch(&mut self, args: serde_json::Value) -> anyhow::Result<serde_json::Value> {
        DapTcpClient::launch(self, args).await
    }
    async fn set_breakpoints(
        &mut self,
        path: &str,
        lines: &[u32],
    ) -> anyhow::Result<serde_json::Value> {
        DapTcpClient::set_breakpoints(self, path, lines).await
    }
    async fn configuration_done(&mut self) -> anyhow::Result<serde_json::Value> {
        DapTcpClient::configuration_done(self).await
    }
    async fn threads(&mut self) -> anyhow::Result<serde_json::Value> {
        DapTcpClient::threads(self).await
    }
    async fn stack_trace(
        &mut self,
        thread_id: u64,
        start_frame: u64,
        levels: u64,
    ) -> anyhow::Result<serde_json::Value> {
        DapTcpClient::stack_trace(self, thread_id, start_frame, levels).await
    }
    async fn scopes(&mut self, frame_id: u64) -> anyhow::Result<serde_json::Value> {
        DapTcpClient::scopes(self, frame_id).await
    }
    async fn variables(&mut self, variables_reference: u64) -> anyhow::Result<serde_json::Value> {
        DapTcpClient::variables(self, variables_reference).await
    }
    async fn evaluate(
        &mut self,
        expression: &str,
        frame_id: Option<u64>,
    ) -> anyhow::Result<serde_json::Value> {
        DapTcpClient::evaluate(self, expression, frame_id).await
    }
    async fn await_any(
        &mut self,
        names: &[&str],
        budget: std::time::Duration,
    ) -> anyhow::Result<serde_json::Value> {
        self.conn.await_any(names, budget).await
    }
    async fn shutdown(&mut self) {
        self.teardown().await;
    }
}

/// Parse a `127.0.0.1:PORT` (or `host:PORT`) listening address out of one
/// adapter log line (`dlv dap -l` prints `API server listening at: ...`).
/// Pure for unit tests; `None` = not a listening line (caller keeps
/// scanning — first match wins).
pub fn parse_listening_port(line: &str) -> Option<u16> {
    let at = line.rfind("listening at")?;
    let tail = &line[at..];
    let colon = tail.rfind(':')?;
    tail[colon + 1..].trim().parse().ok()
}

/// A currently-free loopback port (bind `:0`, read back, release).
/// TOCTOU-accepted: tests connect within milliseconds; production
/// adapters that pick their own port (`dlv -l 127.0.0.1:0`) never call this.
pub fn free_port() -> anyhow::Result<u16> {
    let sock = std::net::TcpListener::bind("127.0.0.1:0").context("dap free_port bind")?;
    sock.local_addr()
        .map(|a| a.port())
        .context("dap free_port addr")
}

/// Adapter spec table: language → [`DapSpec`], mirroring
/// `LspPool`'s spec side. Sessions spawn FRESH per use (debugger
/// sessions are inherently single-shot — unlike LSP there is deliberately
/// no connected-client pool to go stale). M3's tool surface reads specs
/// from here via the registry.
#[derive(Debug, Default)]
pub struct DapPool {
    specs: std::collections::BTreeMap<String, DapSpec>,
}

impl DapPool {
    /// Register one adapter (duplicate languages refused, not
    /// overwritten — the `register` precedent).
    pub fn add_adapter(&mut self, language: &str, spec: DapSpec) -> anyhow::Result<()> {
        if self.specs.contains_key(language) {
            anyhow::bail!("duplicate dap adapter for language: {language}");
        }
        self.specs.insert(language.to_string(), spec);
        Ok(())
    }

    /// Spec for one language (session spawn reads through here).
    pub fn spec_for(&self, language: &str) -> Option<DapSpec> {
        self.specs.get(language).cloned()
    }

    /// Registered languages (observability).
    pub fn languages(&self) -> Vec<String> {
        self.specs.keys().cloned().collect()
    }
}

/// Validate one `[[dap_adapters]]` entry (fail-closed at attach, the
/// `[[hooks]]`/`[[lsp_servers]]` precedent): non-empty id/cmd plus at
/// least one language. Returns the languages with their shared spec.
pub fn validate_entry(
    entry: &polymorph_core::config::DapAdapterEntry,
) -> anyhow::Result<(Vec<String>, DapSpec)> {
    if entry.id.trim().is_empty() {
        anyhow::bail!("dap adapter entry is missing `id`");
    }
    if entry.cmd.trim().is_empty() {
        anyhow::bail!("dap adapter `{}` is missing `cmd`", entry.id);
    }
    if entry.languages.is_empty() {
        anyhow::bail!("dap adapter `{}` names no languages", entry.id);
    }
    Ok((
        entry.languages.clone(),
        DapSpec {
            cmd: entry.cmd.clone(),
            args: entry.args.clone(),
            env: Vec::new(),
            workdir: None,
        },
    ))
}

/// Wire configured adapters (the `wire_lsp_servers` shape): validate +
/// register per language, fail-soft per entry (warn + continue — one bad
/// entry never kills the run). Returns enrolled languages. The bench
/// deliberately does NOT call this (apparatus stability).
pub async fn wire_dap_adapters(
    registry: &crate::Registry,
    adapters: &[polymorph_core::config::DapAdapterEntry],
) -> Vec<String> {
    let mut enrolled = Vec::new();
    for entry in adapters {
        let (languages, spec) = match validate_entry(entry) {
            Ok(v) => v,
            Err(err) => {
                tracing::warn!(entry = %entry.id, error = %format!("{err:#}"), "dap adapter skipped");
                continue;
            }
        };
        for language in &languages {
            if let Err(err) = registry.add_dap_adapter(language, spec.clone()).await {
                tracing::warn!(language = %language, error = %format!("{err:#}"), "dap language skipped");
                continue;
            }
            enrolled.push(language.clone());
        }
    }
    enrolled
}

/// Per-language `launch` arguments: shapes proven by
/// the M2 matrix rows — a new adapter adds one arm here (plus its
/// matrix row), never a new call path. `workdir` feeds adapters that
/// need an explicit cwd (codelldb); others ignore it.
pub fn launch_args_for(
    language: &str,
    program: &str,
    workdir: &str,
) -> anyhow::Result<serde_json::Value> {
    match language {
        "python" => Ok(serde_json::json!({"program": program})),
        "go" => Ok(serde_json::json!({"program": program, "mode": "debug"})),
        "rust" => Ok(serde_json::json!({"program": program, "cwd": workdir})),
        "csharp" => Ok(serde_json::json!({"program": program})),
        _ => anyhow::bail!("no DAP launch shape for language {language:?} (python/go/rust/csharp)"),
    }
}

/// Caps for the combined inspect payload: the agent's
/// tool budget (8 calls/round, 4 rounds) favors ONE chunky read over
/// threads→stack→scopes→variables round-trips — but unbounded dumps
/// would flood context. Over-cap entries count, never silently drop
/// (the same caps discipline as diagnostics).
pub const MAX_INSPECT_FRAMES: usize = 20;
pub const MAX_INSPECT_VARS: usize = 50;

/// One live debug session: the boxed transport (stdio or TCP — the
/// M2 `DapSession` trait), its debuggee identity (permissions follow
/// the PROGRAM, not the handle — every session tool gates
/// `dap_<op>:<program>`), and the last stopped thread/frame (drives
/// `continue` and frameless `evaluate`; cleared on resume).
pub struct LiveSession {
    pub program: String,
    pub language: String,
    pub thread: Option<u64>,
    pub frame: Option<u64>,
    pub runner: Box<dyn DapSession>,
}

impl std::fmt::Debug for LiveSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveSession")
            .field("program", &self.program)
            .field("language", &self.language)
            .field("thread", &self.thread)
            .field("frame", &self.frame)
            .finish()
    }
}

/// Session table: id (`session-N`, deterministic for tests) → live
/// session. Owned by the registry (the snapshot-store precedent).
#[derive(Debug, Default)]
pub struct DapSessions {
    next: u64,
    map: std::collections::BTreeMap<String, LiveSession>,
}

impl DapSessions {
    /// Insert a session, minting its id.
    pub fn insert(&mut self, session: LiveSession) -> String {
        self.next += 1;
        let id = format!("session-{}", self.next);
        self.map.insert(id.clone(), session);
        id
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn get(&self, id: &str) -> Option<&LiveSession> {
        self.map.get(id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut LiveSession> {
        self.map.get_mut(id)
    }

    pub fn remove(&mut self, id: &str) -> Option<LiveSession> {
        self.map.remove(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    /// Loopback pair over TWO duplex channels (one per direction): the
    /// client conn reads `c1` / writes `c2`; the scripted peer reads `p2` /
    /// writes `p1`. Peer frames go through the same `encode_message` the
    /// client uses (reuse proven correct by lsp's duplex tests).
    fn loopback() -> (
        DapConn<tokio::io::BufReader<tokio::io::DuplexStream>, tokio::io::DuplexStream>,
        tokio::io::BufReader<tokio::io::DuplexStream>,
        tokio::io::DuplexStream,
    ) {
        let (c1, p1) = tokio::io::duplex(64 * 1024);
        let (c2, p2) = tokio::io::duplex(64 * 1024);
        let conn = DapConn {
            reader: tokio::io::BufReader::new(c1),
            writer: c2,
            next_seq: 0,
            events: Vec::new(),
            pending: std::collections::BTreeMap::new(),
        };
        (conn, tokio::io::BufReader::new(p2), p1)
    }

    async fn peer_read(
        reader: &mut tokio::io::BufReader<tokio::io::DuplexStream>,
    ) -> serde_json::Value {
        crate::lsp::read_message(reader).await.unwrap()
    }

    async fn peer_write(writer: &mut tokio::io::DuplexStream, body: &serde_json::Value) {
        writer
            .write_all(&crate::lsp::encode_message(body))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn initialize_handshake_pairs_and_surfaces_initialized() {
        // The M1a hello: client `initialize` → peer asserts the shape →
        // capabilities body back → `initialized` event observed.
        let (mut conn, mut peer_rx, mut peer_tx) = loopback();
        let drive = async move {
            let got = peer_read(&mut peer_rx).await;
            assert_eq!(got["type"], "request");
            assert_eq!(got["command"], "initialize");
            assert_eq!(got["seq"], 1);
            assert_eq!(got["arguments"]["clientID"], "polymorph");
            assert_eq!(got["arguments"]["adapterID"], "probe");
            peer_write(
                &mut peer_tx,
                &serde_json::json!({
                    "seq": 7, "type": "response", "request_seq": 1,
                    "success": true, "command": "initialize",
                    "body": {"supportsConfigurationDoneRequest": true},
                }),
            )
            .await;
            peer_write(
                &mut peer_tx,
                &serde_json::json!({"seq": 8, "type": "event", "event": "initialized"}),
            )
            .await;
        };
        let (caps, _) = tokio::join!(
            async {
                let caps = conn.initialize("probe").await.unwrap();
                let ev = conn.next_event().await.unwrap();
                (caps, ev)
            },
            drive
        );
        assert_eq!(caps.0["supportsConfigurationDoneRequest"], true);
        assert_eq!(caps.1["event"], "initialized");
    }

    #[tokio::test]
    async fn responses_pair_past_interleaved_events() {
        // An `output` event arriving BEFORE the response must not steal
        // the pairing: the response still resolves, the event queues.
        let (mut conn, mut peer_rx, mut peer_tx) = loopback();
        let drive = async move {
            let got = peer_read(&mut peer_rx).await;
            assert_eq!(got["seq"], 1);
            peer_write(
                &mut peer_tx,
                &serde_json::json!({"seq": 2, "type": "event", "event": "output",
                    "body": {"output": "hi\n"}}),
            )
            .await;
            peer_write(
                &mut peer_tx,
                &serde_json::json!({
                    "seq": 3, "type": "response", "request_seq": 1,
                    "success": true, "command": "evaluate",
                    "body": {"result": "42"},
                }),
            )
            .await;
        };
        let (body, _) = tokio::join!(
            conn.request("evaluate", serde_json::json!({"expression": "6*7"})),
            drive
        );
        assert_eq!(body.unwrap()["result"], "42");
        // Queued-first half of `next_event` (pipe-read half proven by
        // the handshake test above).
        let first = conn.next_event().await.unwrap();
        assert_eq!(first["event"], "output");
        assert!(conn.drain_events().is_empty());
    }

    #[tokio::test]
    async fn next_event_rejects_non_event_frames() {
        // Responses belong to `request` (the only outstanding `seq`): a
        // response arriving at the event half fails naming the frame.
        // (No request is sent here — the peer writes unprompted.)
        let (mut conn, _peer_rx, mut peer_tx) = loopback();
        peer_write(
            &mut peer_tx,
            &serde_json::json!({
                "seq": 2, "type": "response", "request_seq": 1,
                "success": true, "command": "threads", "body": {},
            }),
        )
        .await;
        let err = conn.next_event().await.unwrap_err().to_string();
        assert!(err.contains("expected event"), "{err}");
    }

    #[tokio::test]
    async fn unmatched_response_parks_for_its_awaiter() {
        // Overlapped lifecycles (launch held past configurationDone) make
        // out-of-order responses PROTOCOL-NORMAL: the stranger parks in
        // the pending map and our response still resolves (debugpy
        // finding — the old fail-loud arm could not drive a session).
        let (mut conn, mut peer_rx, mut peer_tx) = loopback();
        let drive = async move {
            let _ = peer_read(&mut peer_rx).await;
            peer_write(
                &mut peer_tx,
                &serde_json::json!({
                    "seq": 2, "type": "response", "request_seq": 999,
                    "success": true, "command": "launch", "body": {"held": true},
                }),
            )
            .await;
            peer_write(
                &mut peer_tx,
                &serde_json::json!({
                    "seq": 3, "type": "response", "request_seq": 1,
                    "success": true, "command": "threads", "body": {"threads": []},
                }),
            )
            .await;
        };
        let (out, _) = tokio::join!(conn.request("threads", serde_json::Value::Null), drive);
        assert_eq!(out.unwrap()["threads"], serde_json::json!([]));
        assert_eq!(conn.pending.len(), 1);
        assert_eq!(
            conn.pending[&999]["body"],
            serde_json::json!({"held": true})
        );
    }

    #[tokio::test]
    async fn failed_response_names_the_command() {
        // `success:false` surfaces the adapter message with the command
        // named (mirrors lsp's `{method} error` arm).
        let (mut conn, mut peer_rx, mut peer_tx) = loopback();
        let drive = async move {
            let _ = peer_read(&mut peer_rx).await;
            peer_write(
                &mut peer_tx,
                &serde_json::json!({
                    "seq": 2, "type": "response", "request_seq": 1,
                    "success": false, "command": "setBreakpoints",
                    "message": "no such source",
                }),
            )
            .await;
        };
        let (out, _) = tokio::join!(conn.request("setBreakpoints", serde_json::json!({})), drive);
        let err = out.unwrap_err().to_string();
        assert!(err.contains("setBreakpoints"), "{err}");
        assert!(err.contains("no such source"), "{err}");
    }

    #[tokio::test]
    async fn response_without_request_seq_fails_loud() {
        // Audit-found hole (same chunk): a response that pairs with
        // NOTHING must fail naming the frame — parking it would rot in
        // the pending map and poison later lifecycles silently.
        let (mut conn, mut peer_rx, mut peer_tx) = loopback();
        let drive = async move {
            let _ = peer_read(&mut peer_rx).await;
            peer_write(
                &mut peer_tx,
                &serde_json::json!({
                    "seq": 2, "type": "response",
                    "success": true, "command": "threads", "body": {},
                }),
            )
            .await;
        };
        let (out, _) = tokio::join!(conn.request("threads", serde_json::Value::Null), drive);
        let err = out.unwrap_err().to_string();
        assert!(err.contains("without request_seq"), "{err}");
    }

    #[tokio::test]
    async fn eof_is_a_named_error_not_a_stall() {
        // Dropped peer writer, live peer reader: our write still lands
        // (buffered), then the read hits EOF with context — never a hang
        // (the timeout is the backstop, not the mechanism).
        let (mut conn, _peer_rx, peer_tx) = loopback();
        drop(peer_tx);
        let err = conn
            .request("threads", serde_json::Value::Null)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("dap read"), "{err}");
    }

    #[tokio::test]
    async fn bogus_command_fails_at_spawn() {
        // No interpreter needed: an unspawnable adapter names its command.
        let err = DapClient::spawn(&DapSpec {
            cmd: "polymorph-definitely-not-an-adapter".to_string(),
            args: vec![],
            env: vec![],
            workdir: None,
        })
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("polymorph-definitely-not-an-adapter"), "{err}");
    }

    #[tokio::test]
    async fn disconnect_reaps_a_live_child() {
        // `node` sleeps without speaking DAP: disconnect must NOT read
        // (it would stall to timeout) — write best-effort, then reap.
        // Loud skip where node is absent (always-green floor preserved).
        let probe = std::process::Command::new("node").arg("--version").output();
        let have_node = probe.map(|o| o.status.success()).unwrap_or(false);
        if !have_node {
            eprintln!("SKIP disconnect_reaps_a_live_child: no node (expected on CI)");
            return;
        }
        let client = DapClient::spawn(&DapSpec {
            cmd: "node".to_string(),
            args: vec!["-e".to_string(), "setTimeout(()=>{},30000)".to_string()],
            env: vec![],
            workdir: None,
        })
        .await
        .unwrap();
        let pid = client.child.id();
        client.disconnect().await.unwrap();
        assert!(pid.is_some(), "test child must have a pid");
    }

    /// All nine M1b op helpers in session order over one connection:
    /// the peer asserts each command + its key argument fields and
    /// answers canned bodies. Seq pairing across turns is proven by
    /// construction (any mismatch fails the run loud).
    #[tokio::test]
    async fn core_ops_send_shaped_arguments() {
        let (mut conn, mut peer_rx, mut peer_tx) = loopback();
        let drive = async move {
            let step = async |peer_rx: &mut tokio::io::BufReader<tokio::io::DuplexStream>,
                              peer_tx: &mut tokio::io::DuplexStream,
                              command: &str,
                              body: serde_json::Value|
                   -> serde_json::Value {
                let got = peer_read(peer_rx).await;
                assert_eq!(got["type"], "request", "{got}");
                assert_eq!(got["command"], command, "{got}");
                let seq = got["seq"].clone();
                peer_write(
                    peer_tx,
                    &serde_json::json!({
                        "seq": 100, "type": "response", "request_seq": seq,
                        "success": true, "command": command, "body": body,
                    }),
                )
                .await;
                got["arguments"].clone()
            };
            let args = step(
                &mut peer_rx,
                &mut peer_tx,
                "launch",
                serde_json::json!({"ok": true}),
            )
            .await;
            assert_eq!(args["program"], "a.py");
            let args = step(&mut peer_rx, &mut peer_tx, "attach", serde_json::json!({})).await;
            assert_eq!(args["connect"]["port"], 5678);
            let args = step(
                &mut peer_rx,
                &mut peer_tx,
                "setBreakpoints",
                serde_json::json!({"breakpoints": [{"verified": true}]}),
            )
            .await;
            assert_eq!(args["source"]["path"], "a.py");
            assert_eq!(args["breakpoints"], serde_json::json!([{"line": 2}]));
            step(
                &mut peer_rx,
                &mut peer_tx,
                "configurationDone",
                serde_json::Value::Null,
            )
            .await;
            let args = step(&mut peer_rx, &mut peer_tx, "threads", serde_json::json!({})).await;
            assert!(args.is_null(), "{args}");
            let args = step(
                &mut peer_rx,
                &mut peer_tx,
                "stackTrace",
                serde_json::json!({}),
            )
            .await;
            assert_eq!(args["threadId"], 1);
            assert_eq!(args["startFrame"], 0);
            assert_eq!(args["levels"], 20);
            let args = step(&mut peer_rx, &mut peer_tx, "scopes", serde_json::json!({})).await;
            assert_eq!(args["frameId"], 11);
            let args = step(
                &mut peer_rx,
                &mut peer_tx,
                "variables",
                serde_json::json!({}),
            )
            .await;
            assert_eq!(args["variablesReference"], 22);
            let args = step(
                &mut peer_rx,
                &mut peer_tx,
                "evaluate",
                serde_json::json!({"result": "42"}),
            )
            .await;
            assert_eq!(args["expression"], "x");
            assert_eq!(args["frameId"], 11);
            // Frameless evaluate omits `frameId` (the Option arm).
            let args = step(
                &mut peer_rx,
                &mut peer_tx,
                "evaluate",
                serde_json::json!({"result": "3"}),
            )
            .await;
            assert!(args.get("frameId").is_none(), "{args}");
        };
        let run = async move {
            assert_eq!(
                conn.launch(serde_json::json!({"program": "a.py"}))
                    .await
                    .unwrap()["ok"],
                true
            );
            conn.attach(serde_json::json!({"connect": {"host": "h", "port": 5678}}))
                .await
                .unwrap();
            assert_eq!(
                conn.set_breakpoints("a.py", &[2]).await.unwrap()["breakpoints"],
                serde_json::json!([{"verified": true}])
            );
            conn.configuration_done().await.unwrap();
            conn.threads().await.unwrap();
            conn.stack_trace(1, 0, 20).await.unwrap();
            conn.scopes(11).await.unwrap();
            conn.variables(22).await.unwrap();
            assert_eq!(conn.evaluate("x", Some(11)).await.unwrap()["result"], "42");
            assert_eq!(conn.evaluate("1+2", None).await.unwrap()["result"], "3");
        };
        tokio::join!(run, drive);
    }

    #[tokio::test]
    async fn await_event_skips_others_and_queues_them() {
        // `output` arrives first: skipped into the queue, `stopped`
        // resolves, and the skipped event is still observable after.
        let (mut conn, _peer_rx, mut peer_tx) = loopback();
        peer_write(
            &mut peer_tx,
            &serde_json::json!({"seq": 1, "type": "event", "event": "output",
                "body": {"output": "hi\n"}}),
        )
        .await;
        peer_write(
            &mut peer_tx,
            &serde_json::json!({"seq": 2, "type": "event", "event": "stopped",
                "body": {"reason": "breakpoint", "threadId": 1}}),
        )
        .await;
        let ev = conn
            .await_event("stopped", std::time::Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(ev["body"]["threadId"], 1);
        let queued = conn.drain_events();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0]["event"], "output");
    }

    #[tokio::test]
    async fn await_event_expiry_names_the_event() {
        // Silent peer + 50 ms budget: expiry is instant-ish and names
        // `stopped` (one mechanism for ms and minute-scale budgets).
        let (mut conn, _peer_rx, _peer_tx) = loopback();
        let before = std::time::Instant::now();
        let err = conn
            .await_event("stopped", std::time::Duration::from_millis(50))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("stopped"), "{err}");
        assert!(
            before.elapsed() < std::time::Duration::from_secs(5),
            "expiry must not ride the 30 s ceiling"
        );
    }

    #[tokio::test]
    async fn await_any_returns_first_listed_arrival() {
        // `output` skips to the queue; the FIRST listed arrival wins
        // even when it is not the first name in the set.
        let (mut conn, _peer_rx, mut peer_tx) = loopback();
        peer_write(
            &mut peer_tx,
            &serde_json::json!({"seq": 1, "type": "event", "event": "output",
                "body": {"output": "hi\n"}}),
        )
        .await;
        peer_write(
            &mut peer_tx,
            &serde_json::json!({"seq": 2, "type": "event", "event": "terminated"}),
        )
        .await;
        let ev = conn
            .await_any(
                &["stopped", "terminated"],
                std::time::Duration::from_secs(5),
            )
            .await
            .unwrap();
        assert_eq!(ev["event"], "terminated");
        let queued = conn.drain_events();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0]["event"], "output");
    }

    #[test]
    fn parse_listening_port_reads_dlv_lines() {
        // dlv prints `API server listening at: 127.0.0.1:54321`.
        assert_eq!(
            parse_listening_port("API server listening at: 127.0.0.1:54321"),
            Some(54321)
        );
        // Bracketed IPv6 parses too (last colon wins — harmless: dlv
        // prints IPv4 here, and a wrong-family port fails fast at connect).
        assert_eq!(parse_listening_port("listening at [::]:40434"), Some(40434));
        assert_eq!(parse_listening_port("some other log line"), None);
        assert_eq!(parse_listening_port(""), None);
    }

    #[test]
    fn free_port_returns_a_bindable_port() {
        // Smoke only: the returned port binds (TOCTOU-accepted, tests
        // connect within milliseconds).
        let port = free_port().unwrap();
        let sock = std::net::TcpListener::bind(("127.0.0.1", port));
        assert!(sock.is_ok(), "port {port} must bind");
    }

    #[test]
    fn dap_pool_refuses_duplicate_languages() {
        let mut pool = DapPool::default();
        let spec = DapSpec {
            cmd: "x".into(),
            args: vec![],
            env: vec![],
            workdir: None,
        };
        pool.add_adapter("python", spec.clone()).unwrap();
        assert!(pool.add_adapter("python", spec).is_err());
        assert_eq!(pool.languages(), vec!["python".to_string()]);
        assert!(pool.spec_for("python").is_some());
        assert!(pool.spec_for("go").is_none());
    }

    #[test]
    fn validate_entry_fails_closed_naming_the_gap() {
        use polymorph_core::config::DapAdapterEntry;
        let ok = DapAdapterEntry {
            id: "dbg".into(),
            cmd: "x".into(),
            args: vec![],
            languages: vec!["python".into()],
        };
        assert!(validate_entry(&ok).is_ok());
        let mut no_id = ok.clone();
        no_id.id.clear();
        assert!(validate_entry(&no_id)
            .unwrap_err()
            .to_string()
            .contains("id"));
        let mut no_cmd = ok.clone();
        no_cmd.cmd.clear();
        assert!(validate_entry(&no_cmd)
            .unwrap_err()
            .to_string()
            .contains("cmd"));
        let mut no_langs = ok.clone();
        no_langs.languages.clear();
        assert!(validate_entry(&no_langs)
            .unwrap_err()
            .to_string()
            .contains("languages"));
    }

    #[tokio::test]
    async fn wire_registers_per_language_and_skips_bad_entries() {
        // Mirror of the L3 wire matrix, small: one good entry enrolls
        // two languages; a bad entry and a dup-language claim change
        // nothing (first claim wins, run stays up).
        use polymorph_core::config::DapAdapterEntry;
        let r = crate::Registry::new();
        let good = DapAdapterEntry {
            id: "dbg".into(),
            cmd: "x".into(),
            args: vec![],
            languages: vec!["python".into(), "go".into()],
        };
        let bad = DapAdapterEntry {
            id: "".into(),
            cmd: "".into(),
            args: vec![],
            languages: vec![],
        };
        let dup = DapAdapterEntry {
            id: "dbg2".into(),
            cmd: "y".into(),
            args: vec![],
            languages: vec!["python".into()],
        };
        let enrolled = wire_dap_adapters(&r, &[good, bad, dup]).await;
        assert_eq!(enrolled, vec!["python".to_string(), "go".to_string()]);
        assert_eq!(r.dap_spec_for("python").await.unwrap().cmd, "x");
    }

    #[tokio::test]
    async fn tcp_connect_drives_handshake_and_reaps() {
        // `DapTcpClient::connect` end to end: spawn a `node` sleeper
        // (child to reap), connect to a scripted TCP listener serving
        // initialize, then disconnect reaps. Loud skip without node.
        let probe = std::process::Command::new("node").arg("--version").output();
        if !probe.map(|o| o.status.success()).unwrap_or(false) {
            eprintln!("SKIP tcp_connect_drives_handshake_and_reaps: no node");
            return;
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let serve = async move {
            let (sock, _) = listener.accept().await.unwrap();
            let (rh, wh) = sock.into_split();
            let mut peer = DapConn {
                reader: tokio::io::BufReader::new(rh),
                writer: wh,
                next_seq: 100,
                events: Vec::new(),
                pending: std::collections::BTreeMap::new(),
            };
            let got: serde_json::Value = {
                let read = crate::lsp::read_message(&mut peer.reader);
                read.await.unwrap()
            };
            assert_eq!(got["command"], "initialize");
            let seq = got["seq"].as_u64().unwrap();
            peer.next_seq += 1;
            let frame = crate::lsp::encode_message(&serde_json::json!({
                "seq": peer.next_seq, "type": "response", "request_seq": seq,
                "success": true, "command": "initialize", "body": {"tcp": true},
            }));
            use tokio::io::AsyncWriteExt;
            peer.writer.write_all(&frame).await.unwrap();
        };
        let server = tokio::spawn(serve);
        let spec = DapSpec {
            cmd: "node".to_string(),
            args: vec!["-e".to_string(), "setTimeout(()=>{},30000)".to_string()],
            env: vec![],
            workdir: None,
        };
        let mut client = DapTcpClient::connect(&spec, addr, &[]).await.unwrap();
        let caps = client.initialize("probe-tcp").await.unwrap();
        assert_eq!(caps["tcp"], true);
        client.disconnect().await.unwrap();
        server.await.unwrap();
    }
}
