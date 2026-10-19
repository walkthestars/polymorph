//! `polymorph-acp::agent` — I build 2 wiring (Full Deck, chunk 60).
//!
//! Minimal v1 ACP agent: initialize → new session → prompt echo + EndTurn,
//! mirrored into the session store (best-effort) and served over stdio by
//! `polymorph acp`. Tested over `Channel::duplex` (no subprocess, no editor)
//! plus a stdio smoke at verify time. Tool-call gating rides the tool loop
//! (chunk 58 D5 conditions on Registry calls — the echo agent makes none).

use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use agent_client_protocol::{
    schema::{
        v1::{
            AgentCapabilities, ContentBlock, ContentChunk, InitializeRequest, InitializeResponse,
            LoadSessionRequest, LoadSessionResponse, NewSessionRequest, NewSessionResponse,
            PromptRequest, PromptResponse, ResumeSessionRequest, ResumeSessionResponse,
            SessionCapabilities, SessionId, SessionNotification, SessionResumeCapabilities,
            SessionUpdate, StopReason, TextContent,
        },
        ProtocolVersion,
    },
    Agent, Channel, Stdio,
};
use polymorph_core::sessions::{MessageRole, Store};

/// Shared agent state: session-store mirror for the echo loop.
///
/// The store is observability, not authority — the ACP response is always
/// served even when the mirror fails (warn + serve-as-is, merge-back
/// precedent). Known residual: `SESSION_SEQ` restarts per process, so a
/// second `acp` run against the same DB re-mints ids; the duplicate create
/// refuses and the turn still serves (warned, tested below).
#[derive(Clone)]
pub struct Shared {
    store: Arc<Store>,
    live: Option<Arc<crate::session_loop::LiveTextLoop>>,
    tools: Option<Arc<crate::session_loop::LiveToolLoop>>,
}

impl Shared {
    /// Wrap an open store for handler captures (cheap `Arc` clone).
    /// Live loop absent = echo turns (tests/offline).
    #[must_use]
    pub fn new(store: Store) -> Self {
        Self {
            store: Arc::new(store),
            live: None,
            tools: None,
        }
    }

    /// Attach the live text loop (chunk 67): prompts drive model turns.
    #[must_use]
    pub fn with_live(mut self, live: crate::session_loop::LiveTextLoop) -> Self {
        self.live = Some(Arc::new(live));
        self
    }

    /// Attach the live tool loop (chunk 63c): prompts drive model→tools
    /// turns. Takes precedence over the text loop when both attach.
    #[must_use]
    pub fn with_tools(mut self, tools: crate::session_loop::LiveToolLoop) -> Self {
        self.tools = Some(Arc::new(tools));
        self
    }

    /// Mirror `session/new`: best-effort `create_session_with_id`.
    /// Duplicate ids (cross-run seq restart) warn and continue — the ACP
    /// response carries the id regardless.
    pub async fn on_new_session(&self, id: &SessionId, cwd: &std::path::Path) {
        let title = format!("acp {}", cwd.display());
        if let Err(e) = self
            .store
            .create_session_with_id(&id.to_string(), &title)
            .await
        {
            tracing::warn!(session = %id.to_string(), error = %e, "acp store mirror: session create failed, serving anyway");
        }
    }

    /// Borrow the mirrored store (tests + future read paths).
    #[must_use]
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Mirror a prompt turn: append User + Assistant rows, best-effort.
    /// Ghost sessions (store wiped mid-run) warn and continue — the echo
    /// notification + EndTurn still serve.
    pub async fn on_prompt(&self, id: &SessionId, user: &str, assistant: &str) {
        let sid = id.to_string();
        if let Err(e) = self
            .store
            .append_message(&sid, MessageRole::User, user)
            .await
        {
            tracing::warn!(session = %sid, error = %e, "acp store mirror: user append failed, serving anyway");
            return;
        }
        if let Err(e) = self
            .store
            .append_message(&sid, MessageRole::Assistant, assistant)
            .await
        {
            tracing::warn!(session = %sid, error = %e, "acp store mirror: assistant append failed, serving anyway");
        }
    }

    /// Does the store hold this session id? (Reattach gate for load/resume.)
    pub async fn session_exists(&self, id: &SessionId) -> bool {
        let sid = id.to_string();
        match self.store.list_sessions().await {
            Ok(sessions) => sessions.iter().any(|s| s.id == sid),
            Err(e) => {
                tracing::warn!(session = %sid, error = %e, "acp store mirror: list failed");
                false
            }
        }
    }

    /// Replay one history row as its stream chunk. Returns `None` for roles
    /// with no honest mapping (System/Tool are loop-internal — never
    /// fabricate agent thought from them).
    fn replay_chunk(
        session_id: &SessionId,
        role: &MessageRole,
        text: String,
    ) -> Option<SessionNotification> {
        let chunk = match role {
            MessageRole::User => SessionUpdate::UserMessageChunk(ContentChunk::new(
                ContentBlock::Text(TextContent::new(text)),
            )),
            MessageRole::Assistant => SessionUpdate::AgentMessageChunk(ContentChunk::new(
                ContentBlock::Text(TextContent::new(text)),
            )),
            MessageRole::System | MessageRole::Tool => return None,
        };
        Some(SessionNotification::new(session_id.clone(), chunk))
    }
}

/// Counter for echo-first session ids (uniqueness within a process).
static SESSION_SEQ: AtomicU64 = AtomicU64::new(1);

/// Build the initialize response: echo the client version, advertise
/// load + resume (chunk 65 — both bound to the store mirror below).
/// Modes/auth still undeclared — invented caps would lie to Zed.
#[must_use]
pub fn initialize_response(version: ProtocolVersion) -> InitializeResponse {
    InitializeResponse::new(version).agent_capabilities(
        AgentCapabilities::new()
            .load_session(true)
            .session_capabilities(
                SessionCapabilities::new().resume(SessionResumeCapabilities::new()),
            ),
    )
}

/// Mint a fresh echo-first session id (`polymorph-sess-{seq}`).
#[must_use]
pub fn session_id_for(seq: u64) -> SessionId {
    SessionId::new(format!("polymorph-sess-{seq}"))
}

/// Build a new-session response for the given id.
#[must_use]
pub fn new_session_response(id: SessionId) -> NewSessionResponse {
    NewSessionResponse::new(id)
}

/// Extract prompt text: concatenate `Text` blocks, ignore others.
/// (Image/audio/resource need prompt capabilities — deferred to tool binding.)
#[must_use]
pub fn echo_text(prompt: &[ContentBlock]) -> String {
    let mut out = String::new();
    for block in prompt {
        if let ContentBlock::Text(t) = block {
            out.push_str(&t.text);
        }
    }
    out
}

/// Build the prompt response: always `EndTurn` at c59 (no tools, no refusal).
#[must_use]
pub fn prompt_end_turn() -> PromptResponse {
    PromptResponse::new(StopReason::EndTurn)
}

/// Build the echo notification for a prompt turn.
#[must_use]
pub fn echo_notification(session_id: SessionId, text: String) -> SessionNotification {
    let chunk = ContentChunk::new(ContentBlock::Text(TextContent::new(text)));
    SessionNotification::new(session_id, SessionUpdate::AgentMessageChunk(chunk))
}

/// Serve the minimal agent over any transport with a store mirror. Runs
/// until peer closes. Handlers: initialize (version echo) + session/new
/// (unique id + mirror) + prompt (mirror + echo notification + EndTurn).
/// Never sends permission requests (chunk 58 D5 deny-first is vacuous here
/// — nothing to approve).
pub async fn serve_on(
    transport: impl agent_client_protocol::ConnectTo<Agent> + 'static,
    shared: Shared,
) -> Result<(), agent_client_protocol::Error> {
    let shared_new = shared.clone();
    let shared_prompt = shared.clone();
    let shared_load = shared.clone();
    let shared_resume = shared.clone();
    Agent
        .builder()
        .name("polymorph-acp")
        .on_receive_request(
            async move |req: InitializeRequest, responder, _connection| {
                responder.respond(initialize_response(req.protocol_version))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: NewSessionRequest, responder, _connection| {
                let seq = SESSION_SEQ.fetch_add(1, Ordering::Relaxed);
                let id = session_id_for(seq);
                shared_new.on_new_session(&id, &req.cwd).await;
                responder.respond(new_session_response(id))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: PromptRequest, responder, connection| {
                let text = echo_text(&req.prompt);
                // Driver precedence (chunks 67 → 63c): tools > text > echo.
                // Echo (no loop attached) keeps tests/offline key-free.
                let answer: anyhow::Result<String> = if let Some(tools) =
                    shared_prompt.tools.clone()
                {
                    let hist = shared_prompt
                        .store()
                        .history(&req.session_id.to_string(), 0)
                        .await
                        .unwrap_or_default();
                    let brief = crate::session_loop::render_brief(&hist, &text);
                    let workdir = std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir());
                    let cancel = tokio_util::sync::CancellationToken::new();
                    tools.drive_live_tools(&brief, workdir, cancel).await
                } else if let Some(live) = shared_prompt.live.clone() {
                    let hist = shared_prompt
                        .store()
                        .history(&req.session_id.to_string(), 0)
                        .await
                        .unwrap_or_default();
                    let brief = crate::session_loop::render_brief(&hist, &text);
                    let workdir = std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir());
                    // Fresh cancel token per turn; $/cancel_request wiring
                    // lands when a client actually sends it (no sender yet).
                    let cancel = tokio_util::sync::CancellationToken::new();
                    live.drive_live(&brief, workdir, cancel).await
                } else {
                    shared_prompt.on_prompt(&req.session_id, &text, &text).await;
                    Ok(text.clone())
                };
                match answer {
                    Ok(answer) => {
                        // Live branches mirror here; echo already mirrored above.
                        if shared_prompt.tools.is_some() || shared_prompt.live.is_some() {
                            shared_prompt
                                .on_prompt(&req.session_id, &text, &answer)
                                .await;
                        }
                        let notif = echo_notification(req.session_id.clone(), answer);
                        connection.send_notification(notif)?;
                        responder.respond(prompt_end_turn())
                    }
                    Err(e) => responder.respond_with_error(
                        agent_client_protocol::Error::internal_error().data(serde_json::json!({
                            "phase": "live-turn",
                            "detail": e.to_string(),
                        })),
                    ),
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: LoadSessionRequest, responder, connection| {
                // Ghost reattach fails CLOSED, naming the id (sessions
                // NoSession precedent) — never an empty session.
                if !shared_load.session_exists(&req.session_id).await {
                    return responder.respond_with_error(
                        agent_client_protocol::Error::invalid_request().data(
                            serde_json::json!({"unknown_session": req.session_id.to_string()}),
                        ),
                    );
                }
                // Replay BEFORE responding (SDK restore contract: routing
                // is installed before load publishes, so pre-response
                // notifications reach the restored session).
                if let Ok(hist) = shared_load
                    .store()
                    .history(&req.session_id.to_string(), 0)
                    .await
                {
                    for m in &hist {
                        if let Some(notif) =
                            Shared::replay_chunk(&req.session_id, &m.role, m.content.clone())
                        {
                            // Best-effort like the mirror: a broken sink ends
                            // the turn through the transport, not a panic.
                            if connection.send_notification(notif).is_err() {
                                break;
                            }
                        }
                    }
                }
                responder.respond(LoadSessionResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: ResumeSessionRequest, responder, _connection| {
                // Resume reattaches WITHOUT replay (spec distinction: the
                // client already holds the history). Same ghost rule.
                if !shared_resume.session_exists(&req.session_id).await {
                    return responder.respond_with_error(
                        agent_client_protocol::Error::invalid_request().data(
                            serde_json::json!({"unknown_session": req.session_id.to_string()}),
                        ),
                    );
                }
                responder.respond(ResumeSessionResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_to(transport)
        .await
}

/// Serve over an in-process `Channel` (tests + future embedders).
/// Thin over [`serve_on`] — one handler stack, two transports.
pub async fn serve_agent(
    transport: Channel,
    shared: Shared,
) -> Result<(), agent_client_protocol::Error> {
    serve_on(transport, shared).await
}

/// Serve `polymorph acp` over stdio (the M9 editor path). Exits cleanly on
/// editor disconnect (stdin EOF → `incoming_closed` → drain → `Ok`).
/// DB-open failures are the caller's to report (CLI maps them with the
/// sessions error pattern); transport errors propagate for the same mapping.
/// The live loop is caller-built (config + pack are CLI-owned); `None`
/// keeps the echo serve for tests.
pub async fn serve_stdio(
    store: Store,
    live: Option<crate::session_loop::LiveTextLoop>,
    tools: Option<crate::session_loop::LiveToolLoop>,
) -> Result<(), agent_client_protocol::Error> {
    let mut shared = Shared::new(store);
    if let Some(loop_) = live {
        shared = shared.with_live(loop_);
    }
    if let Some(loop_) = tools {
        shared = shared.with_tools(loop_);
    }
    serve_on(Stdio::new(), shared).await
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use agent_client_protocol::{
        schema::v1::{
            ContentBlock, LoadSessionRequest, NewSessionRequest, PromptRequest,
            ResumeSessionRequest, SessionNotification,
        },
        Channel, Client,
    };

    use polymorph_core::sessions::Store;

    use super::*;

    async fn mem_shared() -> Shared {
        Shared::new(Store::open_in_memory().await.unwrap())
    }

    #[test]
    fn echo_text_concats_text_ignores_other() {
        let prompt = vec![
            ContentBlock::Text(TextContent::new("hello ")),
            ContentBlock::Text(TextContent::new("world")),
        ];
        assert_eq!(super::echo_text(&prompt), "hello world");
        assert_eq!(super::echo_text(&[]), "");
    }

    #[test]
    fn constructors_echo_version_and_end_turn() {
        let init = super::initialize_response(ProtocolVersion::V1);
        assert_eq!(init.protocol_version, ProtocolVersion::V1);
        // Caps pin: load + resume advertised (chunk 65 handlers below).
        assert!(init.agent_capabilities.load_session);
        assert!(init
            .agent_capabilities
            .session_capabilities
            .resume
            .is_some());
        let a = super::session_id_for(1);
        let b = super::session_id_for(2);
        assert_ne!(a, b);
        let end = super::prompt_end_turn();
        assert_eq!(end.stop_reason, StopReason::EndTurn);
    }

    #[tokio::test]
    async fn initialize_handshake_echoes_version() {
        let shared = mem_shared().await;
        let (agent_ch, client_ch) = Channel::duplex();
        let agent_fut = super::serve_agent(agent_ch, shared.clone());
        let client_fut = Client.builder().connect_with(client_ch, async |conn| {
            let resp = conn
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            assert_eq!(resp.protocol_version, ProtocolVersion::V1);
            Ok::<_, agent_client_protocol::Error>(())
        });
        let (a, c) = tokio::join!(agent_fut, client_fut);
        a.unwrap();
        c.unwrap();
        // Initialize is wire-only: no store rows.
        assert!(shared.store().list_sessions().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn new_session_ids_are_unique_and_mirrored() {
        let shared = mem_shared().await;
        let (agent_ch, client_ch) = Channel::duplex();
        let agent_fut = super::serve_agent(agent_ch, shared.clone());
        let client_fut = Client.builder().connect_with(client_ch, async |conn| {
            conn.send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            let cwd = std::env::temp_dir();
            let r1 = conn
                .send_request(NewSessionRequest::new(cwd.clone()))
                .block_task()
                .await?;
            let r2 = conn
                .send_request(NewSessionRequest::new(cwd))
                .block_task()
                .await?;
            assert_ne!(r1.session_id, r2.session_id);
            Ok::<_, agent_client_protocol::Error>(())
        });
        let (a, c) = tokio::join!(agent_fut, client_fut);
        a.unwrap();
        c.unwrap();
        // Both sessions mirrored into the store.
        assert_eq!(shared.store().list_sessions().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn prompt_echoes_text_then_ends_turn() {
        let shared = mem_shared().await;
        let (agent_ch, client_ch) = Channel::duplex();
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_h = seen.clone();
        let agent_fut = super::serve_agent(agent_ch, shared.clone());
        let sid: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let sid_h = sid.clone();
        let client_fut = Client
            .builder()
            .on_receive_notification(
                async move |notif: SessionNotification, _cx| {
                    if let SessionUpdate::AgentMessageChunk(chunk) = &notif.update {
                        if let ContentBlock::Text(t) = &chunk.content {
                            seen_h.lock().unwrap().push(t.text.clone());
                        }
                    }
                    Ok::<_, agent_client_protocol::Error>(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(client_ch, async |conn| {
                conn.send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let cwd = std::env::temp_dir();
                let sess = conn
                    .send_request(NewSessionRequest::new(cwd))
                    .block_task()
                    .await?;
                *sid_h.lock().unwrap() = Some(sess.session_id.to_string());
                let resp = conn
                    .send_request(PromptRequest::new(
                        sess.session_id.clone(),
                        vec![ContentBlock::Text(TextContent::new("echo-me"))],
                    ))
                    .block_task()
                    .await?;
                assert_eq!(resp.stop_reason, StopReason::EndTurn);
                Ok::<_, agent_client_protocol::Error>(())
            });
        let (a, c) = tokio::join!(agent_fut, client_fut);
        a.unwrap();
        c.unwrap();
        assert_eq!(*seen.lock().unwrap(), vec!["echo-me".to_string()]);
        // Turn mirrored as User + Assistant rows.
        let sid = sid.lock().unwrap().clone().unwrap();
        let hist = shared.store().history(&sid, 0).await.unwrap();
        assert_eq!(hist.len(), 2);
        assert_eq!(hist[0].content, "echo-me");
        assert_eq!(hist[1].content, "echo-me");
    }

    #[tokio::test]
    async fn peer_close_ends_serve_cleanly() {
        // Client initializes then drops: the agent must finish (no hang).
        // This is the offline half of the stdio EOF-shutdown proof.
        let shared = mem_shared().await;
        let (agent_ch, client_ch) = Channel::duplex();
        let agent_fut = super::serve_agent(agent_ch, shared);
        let client_fut = Client.builder().connect_with(client_ch, async |conn| {
            conn.send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            Ok::<_, agent_client_protocol::Error>(())
        });
        let (a, c) = tokio::join!(agent_fut, client_fut);
        a.unwrap();
        c.unwrap();
    }

    #[tokio::test]
    async fn store_trouble_warns_and_serves_anyway() {
        // Duplicate create + ghost appends refuse inside the mirror but must
        // never panic: the ACP response stays authoritative (fail-soft).
        let shared = mem_shared().await;
        let id = SessionId::new("dup-id");
        let cwd = std::env::temp_dir();
        shared.on_new_session(&id, &cwd).await;
        shared.on_new_session(&id, &cwd).await; // duplicate → warn
        shared.on_prompt(&id, "u", "a").await;
        shared
            .on_prompt(&SessionId::new("ghost-id"), "u", "a")
            .await; // NoSession → warn
        let hist = shared.store().history(&id.to_string(), 0).await.unwrap();
        assert_eq!(hist.len(), 2);
    }

    /// Raw-JSON peer over `Lines`: the test speaks newline-delimited
    /// JSON-RPC itself with no SDK client types — the closest thing to a
    /// real editor (Zed) available in-process. Unknown methods must error
    /// loudly (`-32601`) WITHOUT killing the connection.
    #[tokio::test]
    async fn unknown_method_errors_and_serve_continues() {
        use futures::{SinkExt as _, StreamExt as _};
        let shared = mem_shared().await;
        let (to_agent_tx, to_agent_rx) = futures::channel::mpsc::unbounded::<String>();
        let (from_agent_tx, mut from_agent_rx) = futures::channel::mpsc::unbounded::<String>();
        let agent_lines = agent_client_protocol::Lines::new(
            from_agent_tx.sink_map_err(|_| std::io::Error::other("peer gone")),
            to_agent_rx.map(Ok),
        );
        let agent_fut = super::serve_on(agent_lines, shared);
        let peer = async move {
            to_agent_tx
                .unbounded_send(
                    r#"{"jsonrpc":"2.0","id":9,"method":"nope/missing","params":{}}"#.into(),
                )
                .unwrap();
            let line = from_agent_rx.next().await.expect("error response");
            let v: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(v["id"], 9);
            assert_eq!(v["error"]["code"], -32601);
            // Same connection still serves real requests afterwards.
            to_agent_tx
                .unbounded_send(
                    r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#
                        .into(),
                )
                .unwrap();
            let line = from_agent_rx.next().await.expect("init response");
            let v: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(v["id"], 1);
            assert_eq!(v["result"]["protocolVersion"], 1);
        };
        let (a, ()) = tokio::join!(agent_fut, peer);
        // Peer senders dropped with `peer`: EOF → clean shutdown.
        a.unwrap();
    }

    /// A garbage line is TOLERATED (skipped by the transport) and EOF still
    /// shuts down cleanly — observed SDK semantics, better than teardown:
    /// one corrupt line never kills the session. Timeout-guarded so hangs
    /// become reds (chunk-57 divergence precedent).
    #[tokio::test]
    async fn malformed_line_is_tolerated_eof_still_clean() {
        use futures::{SinkExt as _, StreamExt as _};
        let shared = mem_shared().await;
        let (to_agent_tx, to_agent_rx) = futures::channel::mpsc::unbounded::<String>();
        let (from_agent_tx, _from_agent_rx) = futures::channel::mpsc::unbounded::<String>();
        let agent_lines = agent_client_protocol::Lines::new(
            from_agent_tx.sink_map_err(|_| std::io::Error::other("peer gone")),
            to_agent_rx.map(Ok),
        );
        to_agent_tx
            .unbounded_send("this is not json".into())
            .unwrap();
        drop(to_agent_tx);
        let done = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            super::serve_on(agent_lines, shared),
        )
        .await
        .expect("serve hung on malformed input");
        assert!(
            done.is_ok(),
            "tolerated garbage + clean EOF must end Ok, got {done:?}"
        );
    }

    /// `session/load` reattaches AND replays the stored turn before responding.
    /// Composition: 1 live echo + 2 replayed rows = 3 notifications.
    #[tokio::test]
    async fn load_replays_history_then_responds() {
        let shared = mem_shared().await;
        let (agent_ch, client_ch) = Channel::duplex();
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_h = seen.clone();
        let agent_fut = super::serve_agent(agent_ch, shared);
        let client_fut = Client
            .builder()
            .on_receive_notification(
                async move |notif: SessionNotification, _cx| {
                    let text = match &notif.update {
                        SessionUpdate::UserMessageChunk(c)
                        | SessionUpdate::AgentMessageChunk(c) => match &c.content {
                            ContentBlock::Text(t) => t.text.clone(),
                            _ => String::new(),
                        },
                        _ => String::new(),
                    };
                    if !text.is_empty() {
                        seen_h.lock().unwrap().push(text);
                    }
                    Ok::<_, agent_client_protocol::Error>(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(client_ch, async |conn| {
                conn.send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let cwd = std::env::temp_dir();
                let sess = conn
                    .send_request(NewSessionRequest::new(cwd.clone()))
                    .block_task()
                    .await?;
                conn.send_request(PromptRequest::new(
                    sess.session_id.clone(),
                    vec![ContentBlock::Text(TextContent::new("first"))],
                ))
                .block_task()
                .await?;
                conn.send_request(LoadSessionRequest::new(sess.session_id.clone(), cwd))
                    .block_task()
                    .await?;
                Ok::<_, agent_client_protocol::Error>(())
            });
        let (a, c) = tokio::join!(agent_fut, client_fut);
        a.unwrap();
        c.unwrap();
        assert_eq!(
            *seen.lock().unwrap(),
            vec![
                "first".to_string(),
                "first".to_string(),
                "first".to_string()
            ]
        );
    }

    /// Ghost load fails CLOSED with the id named — never an empty session.
    #[tokio::test]
    async fn load_ghost_fails_closed_naming_id() {
        let shared = mem_shared().await;
        let (agent_ch, client_ch) = Channel::duplex();
        let agent_fut = super::serve_agent(agent_ch, shared);
        let client_fut = Client.builder().connect_with(client_ch, async |conn| {
            conn.send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            let err = conn
                .send_request(LoadSessionRequest::new(
                    SessionId::new("ghost-1"),
                    std::env::temp_dir(),
                ))
                .block_task()
                .await
                .expect_err("ghost load must refuse");
            assert!(format!("{err:?}").contains("ghost-1"), "names id: {err:?}");
            Ok::<_, agent_client_protocol::Error>(())
        });
        let (a, c) = tokio::join!(agent_fut, client_fut);
        a.unwrap();
        c.unwrap();
    }

    /// `session/resume` reattaches WITHOUT replay (client holds history).
    #[tokio::test]
    async fn resume_ok_without_replay() {
        let shared = mem_shared().await;
        let (agent_ch, client_ch) = Channel::duplex();
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_h = seen.clone();
        let agent_fut = super::serve_agent(agent_ch, shared);
        let client_fut = Client
            .builder()
            .on_receive_notification(
                async move |notif: SessionNotification, _cx| {
                    seen_h.lock().unwrap().push(format!("{:?}", notif.update));
                    Ok::<_, agent_client_protocol::Error>(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(client_ch, async |conn| {
                conn.send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let cwd = std::env::temp_dir();
                let sess = conn
                    .send_request(NewSessionRequest::new(cwd.clone()))
                    .block_task()
                    .await?;
                conn.send_request(ResumeSessionRequest::new(sess.session_id, cwd))
                    .block_task()
                    .await?;
                Ok::<_, agent_client_protocol::Error>(())
            });
        let (a, c) = tokio::join!(agent_fut, client_fut);
        a.unwrap();
        c.unwrap();
        assert!(seen.lock().unwrap().is_empty());
    }

    /// Ghost resume refuses like ghost load.
    #[tokio::test]
    async fn resume_ghost_fails_closed() {
        let shared = mem_shared().await;
        let (agent_ch, client_ch) = Channel::duplex();
        let agent_fut = super::serve_agent(agent_ch, shared);
        let client_fut = Client.builder().connect_with(client_ch, async |conn| {
            conn.send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            let err = conn
                .send_request(ResumeSessionRequest::new(
                    SessionId::new("ghost-2"),
                    std::env::temp_dir(),
                ))
                .block_task()
                .await
                .expect_err("ghost resume must refuse");
            assert!(format!("{err:?}").contains("ghost-2"), "names id: {err:?}");
            Ok::<_, agent_client_protocol::Error>(())
        });
        let (a, c) = tokio::join!(agent_fut, client_fut);
        a.unwrap();
        c.unwrap();
    }

    /// Continuity: prompt → load → prompt appends to the SAME rows (2 → 4).
    /// This is the reconnect story (agent restart, client keeps the id).
    #[tokio::test]
    async fn prompt_after_load_appends_continuity() {
        let shared = mem_shared().await;
        let (agent_ch, client_ch) = Channel::duplex();
        let agent_fut = super::serve_agent(agent_ch, shared.clone());
        let client_fut = Client.builder().connect_with(client_ch, async |conn| {
            conn.send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            let cwd = std::env::temp_dir();
            let sess = conn
                .send_request(NewSessionRequest::new(cwd.clone()))
                .block_task()
                .await?;
            for word in ["one", "two"] {
                if word == "two" {
                    conn.send_request(LoadSessionRequest::new(
                        sess.session_id.clone(),
                        cwd.clone(),
                    ))
                    .block_task()
                    .await?;
                }
                conn.send_request(PromptRequest::new(
                    sess.session_id.clone(),
                    vec![ContentBlock::Text(TextContent::new(word))],
                ))
                .block_task()
                .await?;
            }
            Ok::<_, agent_client_protocol::Error>(())
        });
        let (a, c) = tokio::join!(agent_fut, client_fut);
        a.unwrap();
        c.unwrap();
        // The reattached session holds all four rows (2 live + 2 continued).
        let sessions = shared.store().list_sessions().await.unwrap();
        let mut lens: Vec<usize> = Vec::new();
        for s in &sessions {
            lens.push(shared.store().history(&s.id, 0).await.unwrap().len());
        }
        lens.sort_unstable();
        assert!(lens.contains(&4), "continuity rows 2→4: {lens:?}");
    }
}
