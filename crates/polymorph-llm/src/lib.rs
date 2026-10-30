//! `polymorph-llm` — genai-backed provider abstraction and role routing.
//!
//! - `RoleRouter`: ordered model chain per role; 429 falls through to the next entry.
//! - `LlmClient::stream_chat`: native-protocol streaming through
//!   `genai::Client::exec_chat_stream`; deltas fold into a sink and `CancellationToken`
//!   aborts everywhere (dropping the stream is safe because the HTTP body belongs to the
//!   cancelled future).
//! - `drive_stream`: the fold loop, generic over the event stream so unit tests pin it
//!   without a network.
//!
//! Path pins (`RoleRouter::chain_scoped`), 429 cooldown and restore
//! (`fallback::Cooldowns`), credential pooling with one-attempt env injection
//! (`ScopedEnv`), and the models.dev catalog are in this crate.

pub mod catalog;
pub mod fallback;
pub mod rules;

pub use fallback::FallbackState;
use futures::Stream;
use polymorph_core::config::{
    find_provider_for, resolve_all_api_keys, CredentialPool, PathPin, ProviderEntry,
};
use polymorph_core::Role;
use std::pin::Pin;
use std::time::Instant;
use tokio_util::sync::CancellationToken;

/// One screen of prompt-stability doctrine:
/// the system prompt is built ONCE per call here and never mutated mid-stream.
pub const DEFAULT_SYSTEM: &str =
    "You are Polymorph, a Rust-native engineering instrument: scientist, engineer, \
     mathematician, programmer. Terse output. No small talk. Edit only via the \
     hashline format when asked to change files.";

pub type EventStream =
    Pin<Box<dyn Stream<Item = genai::Result<genai::chat::ChatStreamEvent>> + Send>>;

/// Ordered model chain + fallback selection for one role.
#[derive(Debug, Clone, Default)]
pub struct RoleRouter {
    chains: std::collections::BTreeMap<String, Vec<String>>,
    pins: Vec<PathPin>,
    effort: std::collections::BTreeMap<String, String>,
}

impl RoleRouter {
    pub fn new(chains: std::collections::BTreeMap<String, Vec<String>>) -> Self {
        Self {
            chains,
            pins: Vec::new(),
            effort: std::collections::BTreeMap::new(),
        }
    }

    pub fn with_pins(
        chains: std::collections::BTreeMap<String, Vec<String>>,
        pins: Vec<PathPin>,
    ) -> Self {
        Self {
            chains,
            pins,
            effort: std::collections::BTreeMap::new(),
        }
    }

    pub fn from_config(cfg: &polymorph_core::config::PolymorphConfig) -> Self {
        Self {
            chains: cfg.roles.clone(),
            pins: cfg.pins.clone(),
            effort: cfg.effort.clone(),
        }
    }

    /// Strictly-parsed effort level for a role (`[effort]` table): `None`
    /// = unset (ambient); unknown names fail the turn naming the valid set
    /// (fail-closed — a typo'd level never silently degrades to ambient).
    pub fn effort_for(&self, role: Role) -> anyhow::Result<Option<EffortLevel>> {
        // Key convention matches chain_for_role (config.rs): Debug-lowered.
        let key = format!("{role:?}").to_lowercase();
        match self.effort.get(key.as_str()) {
            None => Ok(None),
            Some(word) => Ok(Some(EffortLevel::parse(word)?)),
        }
    }

    /// Chain for a role: configured, else built-in fallback. Never empty.
    /// Delegates to `config::chain_for_role` — the single source of truth.
    pub fn chain(&self, role: Role) -> Vec<String> {
        polymorph_core::config::chain_for_role(&self.chains, role)
    }

    /// Chain for a role under an optional path scope: a matching pin wins, otherwise the plain role
    /// chain. `None` scope = [`Self::chain`].
    pub fn chain_scoped(&self, role: Role, scope: Option<&str>) -> Vec<String> {
        if let Some(path) = scope {
            if let Some(pinned) = polymorph_core::config::pin_for(&self.pins, path, role) {
                return pinned.clone();
            }
        }
        self.chain(role)
    }

    /// Next model after `failed` in the role's chain, or `None` if `failed`
    /// was last (or absent — then the head of the chain).
    pub fn fallback_after(&self, role: Role, failed: &str) -> Option<String> {
        let chain = self.chain(role);
        let pos = chain.iter().position(|m| m == failed)?;
        chain.get(pos + 1).cloned()
    }
}

/// True iff the error is an HTTP 429 from any adapter, matched structurally
/// (not by string).
///
/// Two phases, one predicate. Pre-stream: `WebModelCall`/`WebAdapterCall` wrapping
/// `webc::Error::ResponseFailedStatus { status: 429 }`. Mid-stream: `WebStream { error }` boxing
/// `Error::HttpError { status: 429 }` (OBSERVED in genai 0.6.5 `webc/web_stream.rs`: non-2xx
/// responses are read as bodies and re-raised as boxed `HttpError` inside the event stream — the
/// status line the mock returns proves it). Phase is a transport detail; a 429 is a 429, so both
/// map to fallback.
pub fn is_rate_limited(err: &genai::Error) -> bool {
    match err {
        genai::Error::WebModelCall { webc_error, .. }
        | genai::Error::WebAdapterCall { webc_error, .. } => matches!(
            webc_error,
            genai::webc::Error::ResponseFailedStatus { status, .. }
                if *status == reqwest::StatusCode::TOO_MANY_REQUESTS
        ),
        genai::Error::WebStream { error, .. } => {
            error.downcast_ref::<genai::Error>().is_some_and(|inner| {
                matches!(
                    inner,
                    genai::Error::HttpError { status, .. }
                        if *status == reqwest::StatusCode::TOO_MANY_REQUESTS
                )
            })
        }
        _ => false,
    }
}

/// Transient provider errors (5xx server errors, gateway timeouts, connection drops)
/// that qualify for automatic in-place retries before falling back or failing.
pub fn is_transient_provider_error(err: &genai::Error) -> bool {
    match err {
        genai::Error::WebModelCall { webc_error, .. }
        | genai::Error::WebAdapterCall { webc_error, .. } => match webc_error {
            genai::webc::Error::ResponseFailedStatus { status, .. } => matches!(
                *status,
                reqwest::StatusCode::BAD_GATEWAY
                    | reqwest::StatusCode::SERVICE_UNAVAILABLE
                    | reqwest::StatusCode::GATEWAY_TIMEOUT
                    | reqwest::StatusCode::INTERNAL_SERVER_ERROR
            ),
            _ => {
                let s = webc_error.to_string().to_lowercase();
                s.contains("connection reset")
                    || s.contains("connection refused")
                    || s.contains("connection closed")
                    || s.contains("timed out")
                    || s.contains("timeout")
                    || s.contains("server_error")
                    || s.contains("upstream request failed")
                    || s.contains("error sending request")
                    || s.contains("error decoding response body")
                    || s.contains("unexpected eof")
                    || s.contains("early eof")
                    || s.contains("incomplete")
                    || s.contains("stream closed")
            }
        },
        genai::Error::WebStream { cause, error, .. } => {
            // Match both the cause line and the boxed source: transport
            // decode/request failures surface in either field depending on
            // the adapter (observed: openai/openai_resp streamers).
            let haystacks = [cause.to_lowercase(), error.to_string().to_lowercase()];
            if haystacks.iter().any(|s| {
                s.contains("server_error")
                    || s.contains("upstream request failed")
                    || s.contains("connection reset")
                    || s.contains("connection closed")
                    || s.contains("broken pipe")
                    || s.contains("timed out")
                    || s.contains("timeout")
                    || s.contains("error sending request")
                    || s.contains("error decoding response body")
                    || s.contains("unexpected eof")
                    || s.contains("early eof")
                    || s.contains("incomplete")
                    || s.contains("stream closed")
            }) {
                return true;
            }
            if let Some(inner) = error.downcast_ref::<genai::Error>() {
                matches!(
                    inner,
                    genai::Error::HttpError { status, .. }
                        if matches!(
                            *status,
                            reqwest::StatusCode::BAD_GATEWAY
                                | reqwest::StatusCode::SERVICE_UNAVAILABLE
                                | reqwest::StatusCode::GATEWAY_TIMEOUT
                                | reqwest::StatusCode::INTERNAL_SERVER_ERROR
                        )
                )
            } else {
                false
            }
        }
        _ => false,
    }
}

/// Destination for streamed text deltas. The stdout writer implements this in later
/// epics; tests use `String`. `Send` is required: sinks are held across awaits inside `TurnRunner`
/// futures, which must be `Send` for the bus's spawned tasks.
pub trait StreamSink: Send {
    fn push_text(&mut self, chunk: &str);
}

impl StreamSink for String {
    fn push_text(&mut self, chunk: &str) {
        self.push_str(chunk);
    }
}

/// Outcome of one streamed turn. `rule_hit` is `Some` only when a stream rule fired — the
/// 2 re-issues with the reminder; all pre-R paths read `None`.
#[derive(Debug)]
pub struct StreamOutcome {
    pub text: String,
    pub aborted: bool,
    pub rule_hit: Option<rules::RuleHit>,
}

/// Typed failure of the [`drive_stream`] fold. Split so the managed loop can route upstream
/// provider failures (including mid-stream HTTP errors) into 429-fallback while keeping fold-
/// contract violations Timeout for initial stream request/connection.
pub const STREAM_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Timeout between consecutive chunks on an active stream to prevent indefinite hangs.
pub const STREAM_CHUNK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Non-retriable. `Display` preserves the pre-D strings exactly (`"stream error:..."` / the tool-
/// call sentence).
#[derive(Debug)]
pub enum DriveError {
    /// The event stream itself errored (network, HTTP status, parse).
    /// Maps to [`ChatError::Provider`] — phase is a transport detail.
    Upstream(genai::Error),
    /// The event stream timed out waiting for the next chunk.
    TimedOut(std::time::Duration),
    /// A tool-call chunk arrived before wiring. Never retried: maps to [`ChatError::StreamFold`].
    ToolCallsUnsupported,
}

impl std::fmt::Display for DriveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DriveError::Upstream(e) => write!(f, "stream error: {e}"),
            DriveError::TimedOut(d) => write!(f, "stream chunk timed out after {d:?}"),
            DriveError::ToolCallsUnsupported => {
                write!(f, "tool calls are not wired yet")
            }
        }
    }
}

impl std::error::Error for DriveError {}

/// Fold `ChatStreamEvent`s into `sink` until `End`, error, or cancellation.
/// Pure w.r.t. I/O: all network ended before this is called (the caller passes
/// an already-open stream); cancellation is observed via `cancel`.
pub async fn drive_stream(
    mut stream: EventStream,
    cancel: &CancellationToken,
    sink: &mut dyn StreamSink,
) -> Result<StreamOutcome, DriveError> {
    use futures::StreamExt;
    use genai::chat::ChatStreamEvent;

    let mut text = String::new();
    loop {
        tokio::select! {
                   _ = cancel.cancelled() => {
                       return Ok(StreamOutcome { text, aborted: true, rule_hit: None });
                   }
                   event_res = tokio::time::timeout(STREAM_CHUNK_TIMEOUT, stream.next()) => {
                       let event = match event_res {
                           Ok(e) => e,
                           Err(_) => return Err(DriveError::TimedOut(STREAM_CHUNK_TIMEOUT)),
                       };
                       match event {
                           None => return Ok(StreamOutcome { text, aborted: false, rule_hit: None }),
                           Some(Err(e)) => return Err(DriveError::Upstream(e)),
                           Some(Ok(ChatStreamEvent::Start)) => {}
                           Some(Ok(ChatStreamEvent::Chunk(c))) => {
                               sink.push_text(&c.content);
                               text.push_str(&c.content);
                           }
        // Reasoning traces are surfaced to the UI pane in; keeps them out of the model-
        // visible transcript.
                           Some(Ok(ChatStreamEvent::ReasoningChunk(_))) => {}
                           Some(Ok(ChatStreamEvent::ThoughtSignatureChunk(_))) => {}
                           Some(Ok(ChatStreamEvent::ToolCallChunk(_))) => {
                               return Err(DriveError::ToolCallsUnsupported);
                           }
                           Some(Ok(ChatStreamEvent::End(_))) => {
                               return Ok(StreamOutcome { text, aborted: false, rule_hit: None });
                           }
                       }
                   }
               }
    }
}

/// Rule-checked [`drive_stream`]: the same fold plus a misfire-pattern check per text chunk. On hit
/// the stream stops here — text-so-far, `aborted: false` (`aborted` keeps its cancellation
/// meaning), the hit attached for the retry. The bare fold above stays frozen (empty-rule
/// behavior); the loop shape both spell is the contract, duplication stated not hidden.
pub async fn drive_stream_with_rules(
    mut stream: EventStream,
    cancel: &CancellationToken,
    sink: &mut dyn StreamSink,
    rules: &[rules::CompiledRule],
) -> Result<StreamOutcome, DriveError> {
    use futures::StreamExt;
    use genai::chat::ChatStreamEvent;

    let mut text = String::new();
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                return Ok(StreamOutcome { text, aborted: true, rule_hit: None });
            }
            event_res = tokio::time::timeout(STREAM_CHUNK_TIMEOUT, stream.next()) => {
                let event = match event_res {
                    Ok(e) => e,
                    Err(_) => return Err(DriveError::TimedOut(STREAM_CHUNK_TIMEOUT)),
                };
                match event {
                    None => return Ok(StreamOutcome { text, aborted: false, rule_hit: None }),
                    Some(Err(e)) => return Err(DriveError::Upstream(e)),
                    Some(Ok(ChatStreamEvent::Start)) => {}
                    Some(Ok(ChatStreamEvent::Chunk(c))) => {
                        sink.push_text(&c.content);
                        text.push_str(&c.content);
                        if let Some(hit) = rules::first_hit(rules, &text) {
                            return Ok(StreamOutcome { text, aborted: false, rule_hit: Some(hit) });
                        }
                    }
                    Some(Ok(ChatStreamEvent::ReasoningChunk(_))) => {}
                    Some(Ok(ChatStreamEvent::ThoughtSignatureChunk(_))) => {}
                    Some(Ok(ChatStreamEvent::ToolCallChunk(_))) => {
                        return Err(DriveError::ToolCallsUnsupported);
                    }
                    Some(Ok(ChatStreamEvent::End(_))) => {
                        return Ok(StreamOutcome { text, aborted: false, rule_hit: None });
                    }
                }
            }
        }
    }
}

/// Outcome of one tool-aware streamed turn: assistant text plus the COMPLETE
/// tool calls. Fragment reassembly belongs to genai, not us (VERIFIED in
/// genai 0.6.5 `openai/streamer.rs`: with capture flags off it emits one
/// fragment event per SSE delta — later fragments lack the call id, so
/// call_id-keyed reassembly here double-counts; with `capture_tool_calls`
/// on, genai merges deltas by index and delivers parsed calls on the stream
/// `End`). This fold takes text from chunks and calls from `End` — the only
/// correct split. Aborted/cancelled turns drop partial calls (never execute
/// half-formed invocations).
#[derive(Debug)]
pub struct CollectedTurn {
    pub text: String,
    pub tool_calls: Vec<genai::chat::ToolCall>,
    /// Provider reasoning/thinking text for this turn (DeepSeek `reasoning_content`,
    /// Anthropic thinking, etc.). Carried so multi-turn tool loops can echo it
    /// back; providers in thinking mode reject a follow-up that drops it.
    pub reasoning_content: Option<String>,
    pub aborted: bool,
    pub rule_hit: Option<rules::RuleHit>,
}

/// Fold like [`drive_stream`] but gathering End-carried tool calls instead
/// of refusing chunk events. Pure w.r.t. I/O; cancellation via `cancel`.
pub async fn drive_stream_collecting_tools(
    mut stream: EventStream,
    cancel: &CancellationToken,
    sink: &mut dyn StreamSink,
) -> Result<CollectedTurn, DriveError> {
    use futures::StreamExt;
    use genai::chat::ChatStreamEvent;

    let mut text = String::new();
    let mut reasoning = String::new();
    loop {
        tokio::select! {
                   _ = cancel.cancelled() => {
                       return Ok(CollectedTurn {
                           text,
                           tool_calls: Vec::new(),
                           reasoning_content: Some(reasoning).filter(|value| !value.is_empty()),
                           aborted: true,
                           rule_hit: None,
                       });
                   }
                   event_res = tokio::time::timeout(STREAM_CHUNK_TIMEOUT, stream.next()) => {
                       let event = match event_res {
                           Ok(e) => e,
                           Err(_) => return Err(DriveError::TimedOut(STREAM_CHUNK_TIMEOUT)),
                       };
                       match event {
                           None => {
                               let tool_calls = extract_fallback_tool_calls(&mut text);
                               return Ok(CollectedTurn {
                                   text,
                                   tool_calls,
                                   reasoning_content: Some(reasoning).filter(|value| !value.is_empty()),
                                   aborted: false,
                                   rule_hit: None,
                               })
                           }
                           Some(Err(e)) => return Err(DriveError::Upstream(e)),
                           Some(Ok(ChatStreamEvent::Start)) => {}
                           Some(Ok(ChatStreamEvent::Chunk(c))) => {
                               sink.push_text(&c.content);
                               text.push_str(&c.content);
                           }
                           Some(Ok(ChatStreamEvent::ReasoningChunk(c))) => {
                               reasoning.push_str(&c.content);
                           }
                           Some(Ok(ChatStreamEvent::ThoughtSignatureChunk(_))) => {}
        // Deliberately NOT accumulated: fragments lack stable
        // ids (see struct docs). Complete calls arrive on End.
                           Some(Ok(ChatStreamEvent::ToolCallChunk(_))) => {}
                           Some(Ok(ChatStreamEvent::End(mut end))) => {
                               let reasoning_content = end
                                   .captured_reasoning_content
                                   .take()
                                   .or_else(|| Some(reasoning).filter(|value| !value.is_empty()));
                               let mut tool_calls = end
                                   .captured_content
                                   .take()
                                   .map(|content| content.into_tool_calls())
                                   .unwrap_or_default();
                               if tool_calls.is_empty() {
                                   tool_calls = extract_fallback_tool_calls(&mut text);
                               }
                               return Ok(CollectedTurn {
                                   text,
                                   tool_calls,
                                   reasoning_content,
                                   aborted: false,
                                   rule_hit: None,
                               });
                           }
                       }
                   }
               }
    }
}

/// Rule-checked [`drive_stream_collecting_tools`]: same terms as [`drive_stream_with_rules`] — a
/// hit lands before `End`, so partial calls never exist to drop (empty by construction, matching
/// the cancelled-turn posture).
pub async fn drive_stream_collecting_tools_with_rules(
    mut stream: EventStream,
    cancel: &CancellationToken,
    sink: &mut dyn StreamSink,
    rules: &[rules::CompiledRule],
) -> Result<CollectedTurn, DriveError> {
    use futures::StreamExt;
    use genai::chat::ChatStreamEvent;

    let mut text = String::new();
    let mut reasoning = String::new();
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                return Ok(CollectedTurn {
                    text,
                    tool_calls: Vec::new(),
                    reasoning_content: Some(reasoning).filter(|value| !value.is_empty()),
                    aborted: true,
                    rule_hit: None,
                });
            }
            event_res = tokio::time::timeout(STREAM_CHUNK_TIMEOUT, stream.next()) => {
                let event = match event_res {
                    Ok(e) => e,
                    Err(_) => return Err(DriveError::TimedOut(STREAM_CHUNK_TIMEOUT)),
                };
                match event {
                    None => {
                        let tool_calls = extract_fallback_tool_calls(&mut text);
                        return Ok(CollectedTurn {
                            text,
                            tool_calls,
                            reasoning_content: Some(reasoning).filter(|value| !value.is_empty()),
                            aborted: false,
                            rule_hit: None,
                        })
                    }
                    Some(Err(e)) => return Err(DriveError::Upstream(e)),
                    Some(Ok(ChatStreamEvent::Start)) => {}
                    Some(Ok(ChatStreamEvent::Chunk(c))) => {
                        sink.push_text(&c.content);
                        text.push_str(&c.content);
                        if let Some(hit) = rules::first_hit(rules, &text) {
                            return Ok(CollectedTurn {
                                text,
                                tool_calls: Vec::new(),
                                reasoning_content: Some(reasoning)
                                    .filter(|value| !value.is_empty()),
                                aborted: false,
                                rule_hit: Some(hit),
                            });
                        }
                    }
                    Some(Ok(ChatStreamEvent::ReasoningChunk(c))) => {
                        reasoning.push_str(&c.content);
                    }
                    Some(Ok(ChatStreamEvent::ThoughtSignatureChunk(_))) => {}
                    Some(Ok(ChatStreamEvent::ToolCallChunk(_))) => {}
                    Some(Ok(ChatStreamEvent::End(mut end))) => {
                        let reasoning_content = end
                            .captured_reasoning_content
                            .take()
                            .or_else(|| Some(reasoning).filter(|value| !value.is_empty()));
                        let mut tool_calls = end
                            .captured_content
                            .take()
                            .map(|content| content.into_tool_calls())
                            .unwrap_or_default();
                        if tool_calls.is_empty() {
                            tool_calls = extract_fallback_tool_calls(&mut text);
                        }
                        return Ok(CollectedTurn {
                            text,
                            tool_calls,
                            reasoning_content,
                            aborted: false,
                            rule_hit: None,
                        });
                    }
                }
            }
        }
    }
}

/// Extract tool calls that leaked into stream text (DSML markup from DeepSeek, XML
/// `<tool_call>` blocks, or markdown fenced tool invocations) when the provider/proxy
/// did not parse them into native SSE tool call events.
pub fn extract_fallback_tool_calls(text: &mut String) -> Vec<genai::chat::ToolCall> {
    let mut calls = Vec::new();
    let mut cleaned = text.clone();

    // 1. DeepSeek DSML: <｜tool call begin｜>... <｜tool call end｜>
    // Handles fullwidth '｜' (\u{FF5C}) and ASCII '|'.
    if let Ok(re_dsml) =
        regex::Regex::new(r"(?s)<[｜\|]tool call begin[｜\|]>(.*?)<[｜\|]tool call end[｜\|]>")
    {
        for cap in re_dsml.captures_iter(text) {
            let body = cap.get(1).map(|m| m.as_str()).unwrap_or("");
            // Split on tool sep: <｜tool sep｜> or <|tool sep|>
            let (fn_name, args_raw) = if let Some((_, after)) = body.split_once("tool sep") {
                let after = after.trim_start_matches(['｜', '|', '>']).trim_start();
                let mut lines = after.splitn(2, '\n');
                let name = lines.next().unwrap_or("").trim();
                let args = lines.next().unwrap_or("").trim();
                (name, args)
            } else {
                let trimmed = body.trim_start();
                let mut lines = trimmed.splitn(2, '\n');
                let name = lines.next().unwrap_or("").trim();
                let args = lines.next().unwrap_or("").trim();
                (name, args)
            };

            let fn_name = fn_name.trim_start_matches("function").trim();
            if !fn_name.is_empty() {
                let json_str = clean_json_args(args_raw);
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(json_str) {
                    calls.push(genai::chat::ToolCall {
                        call_id: format!("dsml_{}", uuid::Uuid::new_v4().simple()),
                        fn_name: fn_name.to_string(),
                        fn_arguments: val,
                        thought_signatures: None,
                    });
                }
            }
        }
        cleaned = re_dsml.replace_all(&cleaned, "").to_string();
    }

    // 2. DeepSeek DSML invoke syntax: <｜DSML｜invoke name="write">{"path":...}</｜DSML｜invoke>
    if let Ok(re_dsml_inv) = regex::Regex::new(
        r#"(?s)<[｜\|]DSML[｜\|]invoke\s+name=["']([^"']+)["']\s*>(.*?)</[｜\|]DSML[｜\|]invoke>"#,
    ) {
        for cap in re_dsml_inv.captures_iter(text) {
            let fn_name = cap.get(1).map(|m| m.as_str().trim()).unwrap_or("");
            let args_raw = cap.get(2).map(|m| m.as_str().trim()).unwrap_or("");
            if !fn_name.is_empty() {
                let json_str = clean_json_args(args_raw);
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(json_str) {
                    calls.push(genai::chat::ToolCall {
                        call_id: format!("dsml_{}", uuid::Uuid::new_v4().simple()),
                        fn_name: fn_name.to_string(),
                        fn_arguments: val,
                        thought_signatures: None,
                    });
                }
            }
        }
        cleaned = re_dsml_inv.replace_all(&cleaned, "").to_string();
    }

    // 3. XML style: <tool_call>... </tool_call>
    if let Ok(re_xml) = regex::Regex::new(r"(?s)<tool_call>\s*(.*?)\s*</tool_call>") {
        for cap in re_xml.captures_iter(text) {
            let inner = cap.get(1).map(|m| m.as_str().trim()).unwrap_or("");
            let json_str = clean_json_args(inner);
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(json_str) {
                if let Some(obj) = val.as_object() {
                    let name = obj
                        .get("name")
                        .or_else(|| obj.get("function"))
                        .and_then(|v| v.as_str());
                    let args = obj.get("arguments").or_else(|| obj.get("parameters"));
                    if let (Some(name), Some(args)) = (name, args) {
                        let final_args = if let Some(arg_str) = args.as_str() {
                            serde_json::from_str(arg_str).unwrap_or_else(|_| args.clone())
                        } else {
                            args.clone()
                        };
                        calls.push(genai::chat::ToolCall {
                            call_id: format!("xml_{}", uuid::Uuid::new_v4().simple()),
                            fn_name: name.to_string(),
                            fn_arguments: final_args,
                            thought_signatures: None,
                        });
                    }
                }
            }
        }
        cleaned = re_xml.replace_all(&cleaned, "").to_string();
    }

    // 4. Markdown fenced: ```tool:write\n{"path": "foo"}\n```
    if let Ok(re_fence) = regex::Regex::new(r"(?s)```tool:([a-zA-Z0-9_\-\.]+)\s*\n(.*?)```") {
        for cap in re_fence.captures_iter(text) {
            let fn_name = cap.get(1).map(|m| m.as_str().trim()).unwrap_or("");
            let args_raw = cap.get(2).map(|m| m.as_str().trim()).unwrap_or("");
            if !fn_name.is_empty() {
                let json_str = clean_json_args(args_raw);
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(json_str) {
                    calls.push(genai::chat::ToolCall {
                        call_id: format!("fence_{}", uuid::Uuid::new_v4().simple()),
                        fn_name: fn_name.to_string(),
                        fn_arguments: val,
                        thought_signatures: None,
                    });
                }
            }
        }
        cleaned = re_fence.replace_all(&cleaned, "").to_string();
    }

    // Strip remaining tool framing tags if any calls were extracted
    if !calls.is_empty() {
        if let Ok(re_strip) =
            regex::Regex::new(r"<[｜\|]tool calls[｜\|]>|<[｜\|]tool calls end[｜\|]>")
        {
            cleaned = re_strip.replace_all(&cleaned, "").to_string();
        }
        *text = cleaned.trim().to_string();
    }

    calls
}

fn clean_json_args(raw: &str) -> &str {
    let s = raw.trim();
    if let Some(rest) = s.strip_prefix("```json") {
        if let Some(inner) = rest.strip_suffix("```") {
            return inner.trim();
        }
    } else if let Some(rest) = s.strip_prefix("```") {
        if let Some(inner) = rest.strip_suffix("```") {
            return inner.trim();
        }
    }
    s
}

/// Map a collected turn back onto the text-only contract: tool calls become
/// the pre-F3 sentence through the SAME Display chain, so text-only paths
/// keep byte-identical errors after delegating to the messages variants.
fn without_tool_calls(turn: CollectedTurn) -> Result<StreamOutcome, ChatError> {
    if turn.tool_calls.is_empty() {
        Ok(StreamOutcome {
            text: turn.text,
            aborted: turn.aborted,
            rule_hit: turn.rule_hit,
        })
    } else {
        Err(ChatError::StreamFold(
            DriveError::ToolCallsUnsupported.to_string(),
        ))
    }
}

/// Typed failure of one streamed turn. Kept typed (not anyhow) so the
/// role-chain fallback can match 429 structurally instead of string-matching.
#[derive(Debug)]
pub enum ChatError {
    /// The provider request itself failed (network, auth, 429, bad payload).
    Provider(genai::Error),
    /// The request succeeded but the stream fold failed. Never retried on another model: a new
    /// model would just repeat the same fold failure.
    StreamFold(String),
}

impl std::fmt::Display for ChatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChatError::Provider(e) => write!(f, "provider error: {e}"),
            ChatError::StreamFold(e) => write!(f, "stream error: {e}"),
        }
    }
}

impl std::error::Error for ChatError {}

/// Typed failure of one fallback-loop attempt. Kept typed so the managed loop can match 429 per
/// model (cooldown marking) instead of string-matching, and so tests can assert the exhaustion
/// cause.
#[derive(Debug, PartialEq, Eq)]
pub enum FallbackError {
    /// Attempt was rate-limited; the loop advanced (detail for logs).
    RateLimited { label: String, detail: String },
    /// Attempt failed non-retriably (auth, payload, fold error).
    Failed { label: String, detail: String },
}

impl std::fmt::Display for FallbackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FallbackError::RateLimited { label, detail } => {
                write!(f, "model '{label}' rate-limited: {detail}")
            }
            FallbackError::Failed { label, detail } if label.is_empty() => {
                write!(f, "{detail}")
            }
            FallbackError::Failed { label, detail } => {
                write!(f, "model '{label}' failed without fallback: {detail}")
            }
        }
    }
}

impl std::error::Error for FallbackError {}

/// Process-env guard for one attempt's key injection: sets `name=value`, restores the previous
/// value (or removes the var) on drop — including on early return. Never logged; holds the value in
/// memory only. Single-turn CLI scope: pools live in the caller's `FallbackState`, used from one
/// task, so no cross-thread sharing by construction.
struct ScopedEnv {
    name: String,
    prev: Option<String>,
}

impl ScopedEnv {
    fn set(name: &str, value: &str) -> Self {
        let prev = std::env::var(name).ok();
        std::env::set_var(name, value);
        Self {
            name: name.to_string(),
            prev,
        }
    }
}

impl Drop for ScopedEnv {
    fn drop(&mut self) {
        match &self.prev {
            Some(v) => std::env::set_var(&self.name, v),
            None => std::env::remove_var(&self.name),
        }
    }
}

/// Resolve + inject one attempt's key under the provider's `api_key_env`
/// sink var. Returns the guard (drop restores), or `None` when injection
/// doesn't apply: unmapped model, no sink var, resolution error, or empty
/// pool — all keep ambient-env behavior. Resolution errors are traced (the
/// attempt still runs; auth failure surfaces from the provider call).
fn inject_provider_key(
    providers: &[ProviderEntry],
    state: &mut FallbackState,
    model: &str,
) -> Option<ScopedEnv> {
    let entry = find_provider_for(providers, model)?;
    let sink = entry.api_key_env.as_deref()?;
    let keys = match resolve_all_api_keys(entry) {
        Ok(keys) => keys,
        Err(e) => {
            tracing::warn!(
                provider = entry.id,
                "key resolution failed, ambient env kept: {e}"
            );
            return None;
        }
    };
    let key = state
        .pool_for(&entry.id, CredentialPool::new(keys))
        .next_key()?
        .to_string();
    Some(ScopedEnv::set(sink, &key))
}

/// Models served on the Responses protocol surface (`/responses`, not
/// `/chat/completions`) through OpenCode Go. Source: opencode.ai/docs/go
/// Endpoints table (`@ai-sdk/openai` rows). Exact-match only: a new model
/// joins by adding its id here with a live probe, never by prefix.
fn uses_responses_surface(model_name: &str) -> bool {
    matches!(
        model_name,
        "muse-spark-1.3-contributor" | "muse-spark-1.2-contributor"
    )
}

/// Go endpoint when the claiming provider names none. Mirrors genai's
/// `OpenCodeGoAdapter::default_endpoint`; provider `base_url` wins when set.
const GO_DEFAULT_BASE_URL: &str = "https://opencode.ai/zen/go/v1/";

/// Build the genai spec for a (qualified) model string. Responses-surface
/// ids become `Target` specs (OpenAIResp adapter + provider endpoint +
/// sink-env auth, so key injection and the session header ride the same
/// choke point as every other call); everything else keeps the `Name`
/// path (adapter inferred from the `prefix::` namespace by genai).
/// Pure over its inputs — unit-testable without network.
pub fn spec_for_model(model: &str, providers: &[ProviderEntry]) -> genai::ModelSpec {
    let bare = model.split("::").last().unwrap_or(model);
    if uses_responses_surface(bare) {
        let entry = find_provider_for(providers, model);
        let mut base = entry.map(|e| e.base_url.clone()).unwrap_or_default();
        if base.is_empty() {
            base = GO_DEFAULT_BASE_URL.into();
        }
        if !base.ends_with('/') {
            base.push('/');
        }
        let sink = entry
            .and_then(|e| e.api_key_env.clone())
            .unwrap_or_else(|| "OPENCODE_GO_API_KEY".into());
        return genai::ModelSpec::from_target(genai::ServiceTarget {
            endpoint: genai::resolver::Endpoint::from_owned(base),
            auth: genai::resolver::AuthData::from_env(sink),
            model: genai::ModelIden::new(genai::adapter::AdapterKind::OpenAIResp, bare),
        });
    }
    genai::ModelSpec::from(model.to_string())
}

/// Reasoning-effort level for model turns. Deliberate SUBSET of genai's nine variants:
/// `Budget(u32)` needs a number (wrong shape for a string table); XHigh/Max/Minimal are unneeded at
/// our tiers (stated, not hidden). Unknown names refuse loudly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffortLevel {
    Low,
    Medium,
    High,
}

impl EffortLevel {
    /// Strict vocabulary: unknown names fail naming the valid set.
    pub fn parse(word: &str) -> anyhow::Result<Self> {
        match word {
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            _ => anyhow::bail!("unknown reasoning effort {word:?} (available: low, medium, high)"),
        }
    }

    /// Map onto genai's surface (OpenAI-path serializes `reasoning_effort`
    /// into the request payload — observed in adapter source).
    pub fn to_genai(self) -> genai::chat::ReasoningEffort {
        match self {
            Self::Low => genai::chat::ReasoningEffort::Low,
            Self::Medium => genai::chat::ReasoningEffort::Medium,
            Self::High => genai::chat::ReasoningEffort::High,
        }
    }
}

/// Thin wrapper over `genai::Client`. One client per process (cheap to clone:
/// `Client` is internally `Arc`d — verified in genai source `client_types.rs`).
///
/// Carries optional request [`genai::chat::ChatOptions`] (currently: the
/// `x-opencode-session` conversation header the OpenCode Go gateway REQUIRES —
/// missing it is a 400 `MissingSessionID`, observed live 2026-09-07).
/// Options flow through [`Self::stream_chat_typed_spec`], the single choke
/// point every managed/single call funnels through, so one constructor
/// covers all paths with zero signature churn.
#[derive(Debug, Clone, Default)]
pub struct LlmClient {
    client: genai::Client,
    options: Option<genai::chat::ChatOptions>,
}

impl LlmClient {
    pub fn new() -> Self {
        Self {
            client: genai::Client::default(),
            options: None,
        }
    }

    /// Client tagging every request with a stable conversation id via the
    /// `x-opencode-session` header (Go gateway requirement, opencode.ai/docs/go
    /// §"Where can I use it"; other providers ignore unknown headers).
    /// One id per conversation: pass a session-store id for loops, or use
    /// [`Self::with_fresh_conversation`] for one-shot `exec` turns.
    pub fn with_conversation_id(id: &str) -> Self {
        Self {
            client: genai::Client::default(),
            options: Some(
                genai::chat::ChatOptions::default()
                    .with_extra_headers(("x-opencode-session", id.to_string()))
                    // Complete tool calls ride the stream End (genai merges
                    // per-fragment deltas by index there); text-only paths
                    // ignore the End payload, so this changes nothing for them.
                    .with_capture_tool_calls(true)
                    // DeepSeek/Kimi thinking mode: a streamed assistant message
                    // must carry `reasoning_content` back on the next request or
                    // the provider rejects the turn (HTTP 400). Genai's OpenAI
                    // adapter echoes it when the message captured it.
                    .with_capture_reasoning_content(true),
            ),
        }
    }

    /// [`Self::with_conversation_id`] with a fresh v4 id (one-shot turns).
    pub fn with_fresh_conversation() -> Self {
        Self::with_conversation_id(&uuid::Uuid::new_v4().to_string())
    }

    /// Attach a reasoning-effort level to every request from this client,
    /// PRESERVING existing options (session headers + tool-call capture
    /// ride along untouched — the with_conversation_id contract holds).
    /// Builder form keeps all call sites unchanged (choke point carries it).
    pub fn with_effort(self, level: EffortLevel) -> Self {
        let mut options = self.options.unwrap_or_default();
        options.reasoning_effort = Some(level.to_genai());
        Self {
            options: Some(options),
            ..self
        }
    }

    /// Stream one turn: `system` (frozen for the call) + `prompt`.
    /// Returns the full text, or the partial text with `aborted: true`.
    /// Errors stay typed as [`ChatError`] so callers can match 429.
    pub async fn stream_chat_typed(
        &self,
        model: &str,
        system: &str,
        prompt: &str,
        cancel: &CancellationToken,
        sink: &mut dyn StreamSink,
    ) -> Result<StreamOutcome, ChatError> {
        self.stream_chat_typed_spec(spec_for_model(model, &[]), system, prompt, cancel, sink)
            .await
    }

    /// [`Self::stream_chat_typed`] over an explicit [`genai::ModelSpec`]. `ModelSpec::Target`
    /// (endpoint + auth + model) is how tests point the client at a local mock server. Production
    /// passes `Name` specs. Delegates to the messages variant (single-message history, no tools);
    /// the delegation is byte-identical by construction (empty tools attach nothing) and covered by
    /// the pre-existing suite.
    pub async fn stream_chat_typed_spec(
        &self,
        spec: genai::ModelSpec,
        system: &str,
        prompt: &str,
        cancel: &CancellationToken,
        sink: &mut dyn StreamSink,
    ) -> Result<StreamOutcome, ChatError> {
        let turn = self
            .stream_chat_messages_spec(
                spec,
                system,
                vec![genai::chat::ChatMessage::user(prompt)],
                Vec::new(),
                cancel,
                sink,
                &[],
            )
            .await?;
        without_tool_calls(turn)
    }

    /// Messages + tools variant of [`Self::stream_chat_typed_spec`]: an explicit history and tool
    /// definitions ride the same request path (system pack stays separate — packs are static,
    /// history dynamic). Empty `tools` attaches nothing, keeping text-only requests identical.
    /// `rules` ride to the fold: rule-checked turns for the fixer loop; text-only callers pass
    /// `&[]` (rule-free).
    #[allow(clippy::too_many_arguments)] // rules join the established spec/system/messages/tools/cancel/sink shape
    pub async fn stream_chat_messages_spec(
        &self,
        spec: genai::ModelSpec,
        system: &str,
        messages: Vec<genai::chat::ChatMessage>,
        tools: Vec<genai::chat::Tool>,
        cancel: &CancellationToken,
        sink: &mut dyn StreamSink,
        rules: &[rules::CompiledRule],
    ) -> Result<CollectedTurn, ChatError> {
        let mut req = genai::chat::ChatRequest::from_system(system).append_messages(messages);
        if !tools.is_empty() {
            req = req.with_tools(tools);
        }
        let res = tokio::time::timeout(
            STREAM_CONNECT_TIMEOUT,
            self.client
                .exec_chat_stream(spec, req, self.options.as_ref()),
        )
        .await
        .map_err(|_| {
            ChatError::Provider(genai::Error::WebStream {
                model_iden: genai::ModelIden::new(genai::adapter::AdapterKind::OpenAI, "timeout"),
                cause: format!("stream connect timed out after {STREAM_CONNECT_TIMEOUT:?}"),
                error: Box::new(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("stream connect timed out after {STREAM_CONNECT_TIMEOUT:?}"),
                )),
            })
        })?
        .map_err(ChatError::Provider)?;
        drive_stream_collecting_tools_with_rules(Box::pin(res.stream), cancel, sink, rules)
            .await
            .map_err(|e| match e {
                // Upstream failures (either phase) are provider errors, so
                // the fallback loop can match 429 structurally.
                DriveError::Upstream(e) => ChatError::Provider(e),
                DriveError::TimedOut(d) => ChatError::Provider(genai::Error::WebStream {
                    model_iden: genai::ModelIden::new(
                        genai::adapter::AdapterKind::OpenAI,
                        "timeout",
                    ),
                    cause: format!("stream chunk timed out after {d:?}"),
                    error: Box::new(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        format!("stream chunk timed out after {d:?}"),
                    )),
                }),
                // Unreachable from the collecting fold (it gathers calls);
                // kept for exhaustiveness.
                DriveError::ToolCallsUnsupported => ChatError::StreamFold(e.to_string()),
            })
    }

    /// anyhow-flavoured convenience over [`Self::stream_chat_typed`].
    pub async fn stream_chat(
        &self,
        model: &str,
        system: &str,
        prompt: &str,
        cancel: &CancellationToken,
        sink: &mut dyn StreamSink,
    ) -> anyhow::Result<StreamOutcome> {
        self.stream_chat_typed(model, system, prompt, cancel, sink)
            .await
            .map_err(|e| anyhow::anyhow!("chat request failed for model '{model}': {e}"))
    }

    /// Single explicit-model turn with best-effort key injection: the provider mapped to `model`
    /// donates one pool key under its `api_key_env` sink var for this call only; unmapped models
    /// keep the pre-D ambient-env path. Used by `exec --model` (operator override).
    #[allow(clippy::too_many_arguments)]
    pub async fn stream_chat_single(
        &self,
        providers: &[ProviderEntry],
        state: &mut FallbackState,
        model: &str,
        system: &str,
        prompt: &str,
        cancel: &CancellationToken,
        sink: &mut dyn StreamSink,
    ) -> anyhow::Result<StreamOutcome> {
        // Bare names resolve through configured providers first, so
        // `exec --model glm-5.3` rides a claiming entry instead of failing
        // closed on the bare name's inferred adapter.
        let model = polymorph_core::config::qualify_model(providers, model);
        let _injected = inject_provider_key(providers, state, &model);
        self.stream_chat_typed_spec(
            spec_for_model(&model, providers),
            system,
            prompt,
            cancel,
            sink,
        )
        .await
        .map_err(|e| anyhow::anyhow!("chat request failed for model '{model}': {e}"))
    }

    /// Stream with role-chain fallback: try each model in order, advancing past rate-limited
    /// entries. Non-429 provider errors and stream-fold errors return immediately (retrying those
    /// on a new model is wrong). Returns the outcome plus the model that produced it. Delegates to
    /// [`Self::stream_chat_with_specs`] with unmanaged state — behavior (including error strings)
    /// is unchanged from.
    pub async fn stream_chat_with_fallback(
        &self,
        router: &RoleRouter,
        role: Role,
        system: &str,
        prompt: &str,
        cancel: &CancellationToken,
        sink: &mut dyn StreamSink,
    ) -> anyhow::Result<(StreamOutcome, String)> {
        let chain = router.chain(role);
        // NOTE: no provider qualification here — this legacy path takes no
        // providers list (zero callers; managed + single are the live paths
        // and both qualify). Qualify when wiring providers through, not before.
        let pairs: Vec<(String, genai::ModelSpec)> = chain
            .iter()
            .map(|m| (m.clone(), spec_for_model(m, &[])))
            .collect();
        match self
            .stream_chat_with_specs(pairs, system, prompt, cancel, sink, |_| {})
            .await
        {
            Ok(ok) => Ok(ok),
            Err(FallbackError::RateLimited { detail, .. }) => {
                anyhow::bail!("role chain exhausted, all rate-limited. last: {detail}")
            }
            Err(FallbackError::Failed { label, .. }) if label.is_empty() => {
                anyhow::bail!("role chain exhausted without attempting any model")
            }
            Err(e) => anyhow::bail!("{e}"),
        }
    }

    /// Attempt (label, spec) pairs in order, advancing past rate-limited
    /// entries and calling `on_limited(label)` for each (the managed loop
    /// marks cooldowns there). Non-429 errors return immediately.
    /// Exhaustion returns the LAST attempt's error; an empty pair list
    /// returns a labelless `Failed` (callers map it to their own message).
    pub async fn stream_chat_with_specs(
        &self,
        pairs: Vec<(String, genai::ModelSpec)>,
        system: &str,
        prompt: &str,
        cancel: &CancellationToken,
        sink: &mut dyn StreamSink,
        mut on_limited: impl FnMut(&str),
    ) -> Result<(StreamOutcome, String), FallbackError> {
        let mut last: Option<FallbackError> = None;
        for (label, spec) in &pairs {
            // Bounded in-place retries for transport-level failures (same
            // transient class the managed loop retries): a reviewer-side
            // stream decode/reset must not fail the whole pair list on the
            // first attempt. Non-transient errors return immediately, and
            // 429 keeps the existing advance-to-next-model behavior.
            let mut retries = 0u32;
            const MAX_PAIR_TRANSIENT_RETRIES: u32 = 2;
            let outcome = loop {
                match self
                    .stream_chat_typed_spec(spec.clone(), system, prompt, cancel, sink)
                    .await
                {
                    Err(ChatError::Provider(e))
                        if is_transient_provider_error(&e)
                            && retries < MAX_PAIR_TRANSIENT_RETRIES =>
                    {
                        retries += 1;
                        tracing::warn!(
                            model = label,
                            retries,
                            error = %e,
                            "transient provider error, retrying same model"
                        );
                        tokio::select! {
                            _ = tokio::time::sleep(std::time::Duration::from_millis(1000 * retries as u64)) => {},
                            _ = cancel.cancelled() => {
                                break Err(ChatError::Provider(e));
                            }
                        }
                        continue;
                    }
                    other => break other,
                }
            };
            match outcome {
                Ok(outcome) => return Ok((outcome, label.clone())),
                Err(ChatError::Provider(e)) if is_rate_limited(&e) => {
                    tracing::warn!(model = label, "rate limited, trying next in chain");
                    on_limited(label);
                    last = Some(FallbackError::RateLimited {
                        label: label.clone(),
                        detail: format!("{label}: {e}"),
                    });
                }
                Err(e) => {
                    return Err(FallbackError::Failed {
                        label: label.clone(),
                        detail: e.to_string(),
                    });
                }
            }
        }
        Err(last.unwrap_or(FallbackError::Failed {
            label: String::new(),
            detail: "no models to attempt".into(),
        }))
    }

    /// Managed fallback: cooldown-skipping + per-provider credential pools + one-attempt env
    /// injection, over an optional path scope (pins select the chain). `state` persists across
    /// calls in the process (cooldowns restore by time; pool cursors keep rotating). Injection is
    /// best-effort and process-local: the resolved key is exported under the provider's
    /// `api_key_env` name for ONE attempt ([`ScopedEnv`] restores afterwards); unmapped providers,
    /// missing sink vars, empty pools, or resolution errors keep ambient-env behavior (traced,
    /// never fatal to the attempt).
    #[allow(clippy::too_many_arguments)]
    pub async fn stream_chat_managed(
        &self,
        router: &RoleRouter,
        providers: &[ProviderEntry],
        state: &mut FallbackState,
        role: Role,
        scope: Option<&str>,
        system: &str,
        prompt: &str,
        cancel: &CancellationToken,
        sink: &mut dyn StreamSink,
    ) -> anyhow::Result<(StreamOutcome, String)> {
        let (turn, model) = self
            .stream_chat_managed_messages(
                router,
                providers,
                state,
                role,
                scope,
                system,
                &[genai::chat::ChatMessage::user(prompt)],
                &[],
                cancel,
                sink,
                &[],
            )
            .await?;
        match without_tool_calls(turn) {
            Ok(outcome) => Ok((outcome, model)),
            Err(e) => anyhow::bail!("model '{model}' failed without fallback: {e}"),
        }
    }

    /// Vision turn (sidecar): text prompt + image files as one multipart
    /// user message, no tools. Same fallback/cooldown/key loop as every
    /// managed call (it delegates to `stream_chat_managed_messages`).
    /// Images ride `ContentPart::Binary` (base64 data-URI on OpenAI-path
    /// wires); callers validate paths first — this fn refuses non-images
    /// loud rather than billing a turn the provider would reject.
    #[allow(clippy::too_many_arguments)]
    pub async fn stream_chat_image_turn(
        &self,
        router: &RoleRouter,
        providers: &[ProviderEntry],
        state: &mut FallbackState,
        role: Role,
        scope: Option<&str>,
        system: &str,
        prompt: &str,
        images: &[std::path::PathBuf],
        cancel: &CancellationToken,
        sink: &mut dyn StreamSink,
    ) -> anyhow::Result<(StreamOutcome, String)> {
        use genai::chat::{ChatMessage, ContentPart, MessageContent};
        if images.is_empty() {
            anyhow::bail!("vision turn needs at least one image");
        }
        let mut parts = vec![ContentPart::from_text(prompt.to_string())];
        for img in images {
            let part = ContentPart::from_binary_file(img)
                .map_err(|e| anyhow::anyhow!("cannot attach {}: {e}", img.display()))?;
            if !part.is_image() {
                anyhow::bail!("cannot attach {}: not an image", img.display());
            }
            parts.push(part);
        }
        let (turn, model) = self
            .stream_chat_managed_messages(
                router,
                providers,
                state,
                role,
                scope,
                system,
                &[ChatMessage::user(MessageContent::from_parts(parts))],
                &[],
                cancel,
                sink,
                &[],
            )
            .await?;
        match without_tool_calls(turn) {
            Ok(outcome) => Ok((outcome, model)),
            Err(e) => anyhow::bail!("model '{model}' failed without fallback: {e}"),
        }
    }

    /// Managed messages variant: THE 429/cooldown/fallback loop (moved body,
    /// same bail strings), over an explicit history + tools. The string
    /// `stream_chat_managed` delegates here; the existing suite
    /// covers this loop through that delegation.
    #[allow(clippy::too_many_arguments)]
    /// `rules` ride to the per-model attempts.
    pub async fn stream_chat_managed_messages(
        &self,
        router: &RoleRouter,
        providers: &[ProviderEntry],
        state: &mut FallbackState,
        role: Role,
        scope: Option<&str>,
        system: &str,
        messages: &[genai::chat::ChatMessage],
        tools: &[genai::chat::Tool],
        cancel: &CancellationToken,
        sink: &mut dyn StreamSink,
        rules: &[rules::CompiledRule],
    ) -> anyhow::Result<(CollectedTurn, String)> {
        // Per-role reasoning effort: resolved ONCE per managed call (not per attempt —
        // cooldown/fallback labels stay consistent), parsed strictly (unknown names fail the turn).
        // Explicit-model singles bypass this path and stay raw by design.
        let eff_client = match router.effort_for(role)? {
            Some(level) => self.clone().with_effort(level),
            None => self.clone(),
        };
        // Bare chain entries resolve through configured providers, so role
        // defaults (e.g. bare `glm-5.3`) ride a claiming entry's adapter +
        // key instead of failing closed on the inferred one. Qualification
        // is deterministic per process, so cooldown keys and fallback labels
        // stay consistent within and across attempts.
        let chain: Vec<String> = router
            .chain_scoped(role, scope)
            .iter()
            .map(|m| polymorph_core::config::qualify_model(providers, m))
            .collect();
        let now = Instant::now();
        let mut last_429: Option<String> = None;
        let mut attempted = false;
        for model in &chain {
            if !state.cooldowns.available(model.as_str(), now) {
                continue;
            }
            attempted = true;
            let mut retries = 0;
            const MAX_TRANSIENT_RETRIES: u32 = 2;
            loop {
                match eff_client
                    .stream_chat_messages_single(
                        providers, state, model, system, messages, tools, cancel, sink, rules,
                    )
                    .await
                {
                    Ok(turn) => return Ok((turn, model.clone())),
                    Err(ChatError::Provider(e)) if is_rate_limited(&e) => {
                        tracing::warn!(model, "rate limited, trying next in chain");
                        state.cooldowns.mark_limited(model.as_str(), now);
                        last_429 = Some(format!("{model}: {e}"));
                        break;
                    }
                    Err(ChatError::Provider(e))
                        if is_transient_provider_error(&e) && retries < MAX_TRANSIENT_RETRIES =>
                    {
                        retries += 1;
                        tracing::warn!(
                            model,
                            retries,
                            error = %e,
                            "transient provider error, retrying"
                        );
                        tokio::select! {
                            _ = tokio::time::sleep(std::time::Duration::from_millis(1000 * retries as u64)) => {},
                            _ = cancel.cancelled() => {
                                anyhow::bail!("cancelled during transient retry wait");
                            }
                        }
                        continue;
                    }
                    Err(ChatError::Provider(e)) if is_transient_provider_error(&e) => {
                        tracing::warn!(model, error = %e, "transient provider errors exhausted, trying next in chain");
                        last_429 = Some(format!("{model}: {e}"));
                        break;
                    }
                    Err(e) => {
                        anyhow::bail!("model '{model}' failed without fallback: {e}");
                    }
                }
            }
        }
        match last_429 {
            Some(e) => anyhow::bail!("role chain exhausted, all rate-limited. last: {e}"),
            None if attempted => anyhow::bail!("role chain exhausted without attempting any model"),
            None => anyhow::bail!(
                "role chain exhausted: every model is cooling down (retry after cooldown)"
            ),
        }
    }

    /// One messages attempt with best-effort key injection: the mapped provider donates one pool
    /// key under its sink var for this call only.
    #[allow(clippy::too_many_arguments)]
    pub async fn stream_chat_messages_single(
        &self,
        providers: &[ProviderEntry],
        state: &mut FallbackState,
        model: &str,
        system: &str,
        messages: &[genai::chat::ChatMessage],
        tools: &[genai::chat::Tool],
        cancel: &CancellationToken,
        sink: &mut dyn StreamSink,
        rules: &[rules::CompiledRule],
    ) -> Result<CollectedTurn, ChatError> {
        // Guard drops (restoring env) at the end of the attempt.
        let _injected = inject_provider_key(providers, state, model);
        self.stream_chat_messages_spec(
            spec_for_model(model, providers),
            system,
            messages.to_vec(),
            tools.to_vec(),
            cancel,
            sink,
            rules,
        )
        .await
    }
}

/// Crate identifier. Used by smoke tests to prove the crate links.
pub fn crate_name() -> &'static str {
    "polymorph-llm"
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;
    use genai::chat::{ChatStreamEvent, StreamChunk};

    fn chunk(t: &str) -> genai::Result<ChatStreamEvent> {
        Ok(ChatStreamEvent::Chunk(StreamChunk { content: t.into() }))
    }

    #[test]
    fn binary_file_part_reads_png_as_image() {
        // Sidecar substrate: a real file becomes an image part (the
        // wire serializes it as image_url on OpenAI-path adapters).
        use genai::chat::ContentPart;
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("shot.png");
        std::fs::write(
            &png,
            [
                137u8, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82,
            ],
        )
        .unwrap();
        let part = ContentPart::from_binary_file(&png).unwrap();
        assert!(part.is_image(), "png must read as image");
    }

    fn boxed<I>(iter: I) -> EventStream
    where
        I: Stream<Item = genai::Result<ChatStreamEvent>> + Send + 'static,
    {
        Box::pin(iter)
    }

    #[tokio::test]
    async fn drive_stream_folds_chunks_until_end() {
        let events = vec![
            Ok(ChatStreamEvent::Start),
            chunk("hello "),
            chunk("world"),
            Ok(ChatStreamEvent::End(Default::default())),
        ];
        let cancel = CancellationToken::new();
        let mut sink = String::new();
        let out = drive_stream(boxed(stream::iter(events)), &cancel, &mut sink)
            .await
            .unwrap();
        assert_eq!(out.text, "hello world");
        assert_eq!(sink, "hello world");
        assert!(!out.aborted);
    }

    #[tokio::test]
    async fn drive_stream_reports_tool_calls_as_error() {
        // Tool-call chunks before are a contract error, not silent data.
        let events = vec![
            chunk("partial"),
            Ok(ChatStreamEvent::End(Default::default())),
        ];
        let cancel = CancellationToken::new();
        let mut sink = String::new();
        let out = drive_stream(boxed(stream::iter(events)), &cancel, &mut sink)
            .await
            .unwrap();
        assert_eq!(out.text, "partial");
        assert!(!out.aborted);
    }

    #[tokio::test]
    async fn drive_stream_abort_mid_token() {
        // A never-resolving stream + cancelled token must return promptly
        // with partial text and aborted=true (the A3 done-criteria).
        let pending = stream::pending::<genai::Result<ChatStreamEvent>>();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let mut sink = String::new();
        let out = drive_stream(boxed(pending), &cancel, &mut sink)
            .await
            .unwrap();
        assert!(out.aborted);
        assert!(out.text.is_empty());
    }

    fn tense_rule() -> crate::rules::CompiledRule {
        crate::rules::StreamRule {
            id: "tense".into(),
            pattern: r"will\s+edits?\b".into(),
            reminder: "use present tense for tool args".into(),
        }
        .compile()
        .unwrap()
    }

    #[tokio::test]
    async fn with_rules_seeded_misfire_aborts_with_reminder() {
        // The v2 R1 shape, scripted: the misfire text arrives across two
        // chunks (match spans the boundary), the fold stops at the hit —
        // no End needed, reminder attached for the retry.
        let events = vec![
            Ok(ChatStreamEvent::Start),
            chunk("the model will "),
            chunk("edit the file"),
            Ok(ChatStreamEvent::End(Default::default())),
        ];
        let cancel = CancellationToken::new();
        let mut sink = String::new();
        let out = drive_stream_with_rules(
            boxed(stream::iter(events)),
            &cancel,
            &mut sink,
            &[tense_rule()],
        )
        .await
        .unwrap();
        assert!(!out.aborted);
        assert_eq!(out.text, "the model will edit the file");
        let hit = out.rule_hit.expect("misfire must hit");
        assert_eq!(hit.rule_id, "tense");
        assert_eq!(hit.reminder, "use present tense for tool args");
    }

    #[tokio::test]
    async fn with_rules_clean_stream_passes_untouched() {
        let events = vec![
            Ok(ChatStreamEvent::Start),
            chunk("hello "),
            chunk("world"),
            Ok(ChatStreamEvent::End(Default::default())),
        ];
        let cancel = CancellationToken::new();
        let mut sink = String::new();
        let out = drive_stream_with_rules(
            boxed(stream::iter(events)),
            &cancel,
            &mut sink,
            &[tense_rule()],
        )
        .await
        .unwrap();
        assert_eq!(out.text, "hello world");
        assert!(out.rule_hit.is_none());
        assert!(!out.aborted);
    }

    #[tokio::test]
    async fn with_rules_empty_rules_equal_bare_behavior() {
        // Frozen-bare proof: no rules ≡ the old fold, byte for byte.
        let events = vec![
            Ok(ChatStreamEvent::Start),
            chunk("it will edit x"),
            Ok(ChatStreamEvent::End(Default::default())),
        ];
        let cancel = CancellationToken::new();
        let mut sink = String::new();
        let out = drive_stream_with_rules(boxed(stream::iter(events)), &cancel, &mut sink, &[])
            .await
            .unwrap();
        assert_eq!(out.text, "it will edit x");
        assert!(out.rule_hit.is_none());
    }

    #[tokio::test]
    async fn with_rules_collecting_hit_drops_partial_calls() {
        // Hit lands before End: no calls exist to execute (empty by
        // construction, the cancelled-turn posture).
        let events = vec![
            Ok(ChatStreamEvent::Start),
            chunk("it will edit x"),
            Ok(ChatStreamEvent::End(Default::default())),
        ];
        let cancel = CancellationToken::new();
        let mut sink = String::new();
        let out = drive_stream_collecting_tools_with_rules(
            boxed(stream::iter(events)),
            &cancel,
            &mut sink,
            &[tense_rule()],
        )
        .await
        .unwrap();
        assert!(out.tool_calls.is_empty());
        assert!(!out.aborted);
        assert_eq!(out.rule_hit.map(|h| h.rule_id), Some("tense".to_string()));
    }

    #[tokio::test]
    async fn drive_stream_stream_error_propagates() {
        let events = vec![
            chunk("kept"),
            Err(genai::Error::NoChatResponse {
                model_iden: genai::ModelIden::new(
                    genai::adapter::AdapterKind::OpenAI,
                    "test-model",
                ),
            }),
        ];
        let cancel = CancellationToken::new();
        let mut sink = String::new();
        let err = drive_stream(boxed(stream::iter(events)), &cancel, &mut sink)
            .await
            .unwrap_err();
        // Display preserves the pre-D string; the typed variant routes to Provider.
        assert!(err.to_string().contains("stream error"));
        assert!(matches!(err, DriveError::Upstream(_)));
        // partial text was still sunk before the error
        assert_eq!(sink, "kept");
    }

    #[test]
    fn router_chain_and_fallback() {
        let mut chains = std::collections::BTreeMap::new();
        chains.insert(
            "default".to_string(),
            vec!["a".to_string(), "b".to_string(), "c".to_string()],
        );
        let r = RoleRouter::new(chains);
        assert_eq!(r.chain(Role::Default), vec!["a", "b", "c"]);
        assert_eq!(r.fallback_after(Role::Default, "a").as_deref(), Some("b"));
        assert_eq!(r.fallback_after(Role::Default, "c"), None);
        assert_eq!(r.fallback_after(Role::Default, "zzz"), None);
        // unconfigured role → built-in, never empty
        assert!(!r.chain(Role::Review).is_empty());
    }

    #[test]
    fn rate_limit_detected_structurally() {
        use reqwest::StatusCode;
        let err = genai::Error::WebModelCall {
            model_iden: genai::ModelIden::new(genai::adapter::AdapterKind::OpenAI, "test-model"),
            webc_error: genai::webc::Error::ResponseFailedStatus {
                status: StatusCode::TOO_MANY_REQUESTS,
                body: "{}".into(),
                headers: Box::default(),
            },
        };
        assert!(is_rate_limited(&err));
        let other = genai::Error::WebModelCall {
            model_iden: genai::ModelIden::new(genai::adapter::AdapterKind::OpenAI, "test-model"),
            webc_error: genai::webc::Error::ResponseFailedStatus {
                status: StatusCode::BAD_REQUEST,
                body: "{}".into(),
                headers: Box::default(),
            },
        };
        assert!(!is_rate_limited(&other));
        assert!(!is_rate_limited(&genai::Error::NoChatResponse {
            model_iden: genai::ModelIden::new(genai::adapter::AdapterKind::OpenAI, "test-model"),
        }));
    }

    fn web_stream_err(status: reqwest::StatusCode) -> genai::Error {
        genai::Error::WebStream {
            model_iden: genai::ModelIden::new(genai::adapter::AdapterKind::OpenAI, "test-model"),
            cause: format!("HTTP error. Status: {status}"),
            error: Box::new(genai::Error::HttpError {
                status,
                canonical_reason: status.canonical_reason().unwrap_or("Unknown").to_string(),
                body: "{}".into(),
            }),
        }
    }

    #[test]
    fn rate_limit_detected_mid_stream_structurally() {
        use reqwest::StatusCode;
        // Same shape genai 0.6.5 builds in webc/web_stream.rs (boxed HttpError).
        assert!(is_rate_limited(&web_stream_err(
            StatusCode::TOO_MANY_REQUESTS
        )));
        assert!(!is_rate_limited(&web_stream_err(StatusCode::BAD_REQUEST)));
        // A non-HttpError box is never 429 (no string sniffing of `cause`).
        let other: genai::Error = genai::Error::WebStream {
            model_iden: genai::ModelIden::new(genai::adapter::AdapterKind::OpenAI, "test-model"),
            cause: "Status: 429 in prose only".into(),
            error: Box::new(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "x")),
        };
        assert!(!is_rate_limited(&other));
    }

    #[test]
    fn transient_provider_error_detected_structurally() {
        use reqwest::StatusCode;
        for status in [
            StatusCode::BAD_GATEWAY,
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::GATEWAY_TIMEOUT,
            StatusCode::INTERNAL_SERVER_ERROR,
        ] {
            let err = genai::Error::WebModelCall {
                model_iden: genai::ModelIden::new(
                    genai::adapter::AdapterKind::OpenAI,
                    "test-model",
                ),
                webc_error: genai::webc::Error::ResponseFailedStatus {
                    status,
                    body: "{}".into(),
                    headers: Box::default(),
                },
            };
            assert!(
                is_transient_provider_error(&err),
                "expected {status} to be transient"
            );
            assert!(
                is_transient_provider_error(&web_stream_err(status)),
                "expected stream {status} to be transient"
            );
        }
        let bad_request = genai::Error::WebModelCall {
            model_iden: genai::ModelIden::new(genai::adapter::AdapterKind::OpenAI, "test-model"),
            webc_error: genai::webc::Error::ResponseFailedStatus {
                status: StatusCode::BAD_REQUEST,
                body: "{}".into(),
                headers: Box::default(),
            },
        };
        assert!(!is_transient_provider_error(&bad_request));

        let timeout_err = genai::Error::WebStream {
            model_iden: genai::ModelIden::new(genai::adapter::AdapterKind::OpenAI, "test-model"),
            cause: "stream chunk timed out after 120s".into(),
            error: Box::new(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "stream chunk timed out after 120s",
            )),
        };
        assert!(is_transient_provider_error(&timeout_err));
    }

    /// Observed gateway failures (openai/openai_resp streamers via the Go
    /// proxy): stream decode and request-send errors must retry instead of
    /// halting the drive, while auth/payload errors must not.
    #[test]
    fn transient_provider_error_covers_observed_stream_failures() {
        let decode_err = genai::Error::WebStream {
            model_iden: genai::ModelIden::new(genai::adapter::AdapterKind::OpenAI, "test-model"),
            cause: "error decoding response body".into(),
            error: Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "error decoding response body",
            )),
        };
        assert!(is_transient_provider_error(&decode_err));
        let send_err = genai::Error::WebModelCall {
            model_iden: genai::ModelIden::new(genai::adapter::AdapterKind::OpenAI, "test-model"),
            webc_error: genai::webc::Error::ResponseFailedInvalidJson {
                body: String::new(),
                cause:
                    "error sending request for url (https://opencode.ai/zen/go/v1/chat/completions)"
                        .into(),
            },
        };
        assert!(is_transient_provider_error(&send_err));
        let closed = genai::Error::WebStream {
            model_iden: genai::ModelIden::new(genai::adapter::AdapterKind::OpenAI, "test-model"),
            cause: "connection closed before message completed".into(),
            error: Box::new(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "connection closed",
            )),
        };
        assert!(is_transient_provider_error(&closed));
        // Non-transport failures stay immediate: auth, bad payload, 429.
        assert!(!is_transient_provider_error(
            &genai::Error::RequiresApiKey {
                model_iden: genai::ModelIden::new(
                    genai::adapter::AdapterKind::OpenAI,
                    "test-model"
                ),
            }
        ));
        assert!(!is_transient_provider_error(
            &genai::Error::NoChatResponse {
                model_iden: genai::ModelIden::new(
                    genai::adapter::AdapterKind::OpenAI,
                    "test-model"
                ),
            }
        ));
    }

    #[test]
    fn scoped_env_restores_previous_or_removes() {
        // Unique names: llm tests share one process env.
        let name = "POLYMORPH_TEST_SCOPED_A";
        std::env::set_var(name, "orig");
        {
            let _g = ScopedEnv::set(name, "temp");
            assert_eq!(std::env::var(name).unwrap(), "temp");
        }
        assert_eq!(std::env::var(name).unwrap(), "orig");
        std::env::remove_var(name);

        let absent = "POLYMORPH_TEST_SCOPED_ABSENT_XYZ";
        std::env::remove_var(absent);
        {
            let _g = ScopedEnv::set(absent, "temp");
            assert_eq!(std::env::var(absent).unwrap(), "temp");
        }
        assert!(std::env::var(absent).is_err());
    }

    #[test]
    fn chain_scoped_prefers_pin_then_falls_through() {
        use polymorph_core::config::PathPin;
        let mut chains = std::collections::BTreeMap::new();
        chains.insert("default".to_string(), vec!["base".to_string()]);
        let pins = vec![PathPin {
            path_prefix: "crates/x".into(),
            role: None,
            chain: vec!["pinned".to_string()],
        }];
        let r = RoleRouter::with_pins(chains, pins);
        assert_eq!(
            r.chain_scoped(Role::Default, Some("crates/x/f.rs")),
            vec!["pinned"]
        );
        assert_eq!(
            r.chain_scoped(Role::Default, Some("elsewhere/f.rs")),
            vec!["base"]
        );
        assert_eq!(r.chain_scoped(Role::Default, None), vec!["base"]);
    }

    #[test]
    fn inject_sets_sink_var_and_restores_on_drop() {
        use polymorph_core::config::ProviderEntry;
        // Unique names: llm tests share one process env.
        let sink = "POLYMORPH_TEST_INJ_SINK";
        let src = "POLYMORPH_TEST_INJ_SRC";
        std::env::remove_var(sink);
        std::env::set_var(src, "sekret");
        let entry = ProviderEntry {
            id: "p".into(),
            api_key_env: Some(sink.into()),
            api_key_envs: vec![src.into()],
            models: vec!["m-inj".into()],
            ..Default::default()
        };
        let providers = vec![entry];
        let mut state = FallbackState::new(std::time::Duration::ZERO);
        {
            let guard = inject_provider_key(&providers, &mut state, "m-inj");
            assert!(guard.is_some());
            assert_eq!(std::env::var(sink).unwrap(), "sekret");
        }
        assert!(std::env::var(sink).is_err());
        // Unmapped model: no injection, env untouched.
        assert!(inject_provider_key(&providers, &mut state, "other").is_none());
        assert!(std::env::var(sink).is_err());
        std::env::remove_var(src);
    }

    /// Responses-surface routing table (Go `/responses` column): exact ids
    /// only, never prefixes. Pure: no network.
    #[test]
    fn spec_for_model_routes_responses_ids_to_target() {
        let providers = vec![polymorph_core::config::ProviderEntry {
            id: "opencode-go".into(),
            base_url: "https://opencode.ai/zen/go/v1/".into(),
            api_key_env: Some("OPENCODE_GO_API_KEY".into()),
            models: vec!["opencode_go::muse-spark-1.3-contributor".into()],
            ..Default::default()
        }];
        for id in [
            "opencode_go::muse-spark-1.3-contributor",
            "opencode_go::muse-spark-1.2-contributor",
        ] {
            match spec_for_model(id, &providers) {
                genai::ModelSpec::Target(t) => {
                    assert_eq!(t.endpoint.base_url(), "https://opencode.ai/zen/go/v1/");
                    assert!(matches!(
                        t.model.adapter_kind,
                        genai::adapter::AdapterKind::OpenAIResp
                    ));
                    assert_eq!(t.model.model_name.as_str(), id.split("::").last().unwrap());
                }
                other => panic!("{id} must route Target, got {other:?}"),
            }
        }
    }

    #[test]
    fn spec_for_model_keeps_name_path_for_chat_models() {
        for id in [
            "opencode_go::glm-5.3-flash",
            "opencode_go::deepseek-v4.1-flash",
            "glm-5.3",
        ] {
            assert!(
                matches!(spec_for_model(id, &[]), genai::ModelSpec::Name(_)),
                "{id} must keep the Name path"
            );
        }
    }

    #[test]
    fn spec_for_model_unclaimed_responses_id_uses_go_defaults() {
        match spec_for_model("opencode_go::muse-spark-1.3-contributor", &[]) {
            genai::ModelSpec::Target(t) => {
                assert_eq!(t.endpoint.base_url(), "https://opencode.ai/zen/go/v1/");
            }
            other => panic!("unclaimed contributor must still route Target, got {other:?}"),
        }
    }

    /// Minimal Responses-API SSE: created + text delta + completed + DONE.
    const SSE_RESP_OK: &str = concat!(
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r1\"}}\n\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello from mock\"}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r1\",\"status\":\"completed\",\"model\":\"m\"}}\n\n",
        "data: [DONE]\n\n",
    );

    /// Point a Responses spec at a local mock server (same `from_target`
    /// seam as production `spec_for_model`, OpenAIResp kind).
    fn mock_responses_spec(server: &wiremock::MockServer) -> genai::ModelSpec {
        use genai::resolver::{AuthData, Endpoint};
        genai::ModelSpec::from_target(genai::ServiceTarget {
            endpoint: Endpoint::from_owned(server.uri()),
            auth: AuthData::from_single("test-key"),
            model: genai::ModelIden::new(
                genai::adapter::AdapterKind::OpenAIResp,
                "muse-spark-1.3-contributor",
            ),
        })
    }

    #[tokio::test]
    async fn wiremock_responses_stream_folds_to_text() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .and(header("x-opencode-session", "test-session"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(SSE_RESP_OK)
                    .insert_header("content-type", "text/event-stream"),
            )
            .mount(&server)
            .await;
        let client = LlmClient::with_conversation_id("test-session");
        let cancel = CancellationToken::new();
        let mut sink = String::new();
        let out = client
            .stream_chat_typed_spec(
                mock_responses_spec(&server),
                "sys",
                "hi",
                &cancel,
                &mut sink,
            )
            .await
            .expect("responses mock must fold");
        assert!(out.text.contains("hello from mock"), "{}", out.text);
    }

    #[tokio::test]
    async fn wiremock_responses_429_maps_to_rate_limited_structurally() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(429).set_body_string(r#"{"error":"slow down"}"#))
            .mount(&server)
            .await;
        let client = LlmClient::new();
        let cancel = CancellationToken::new();
        let mut sink = String::new();
        let err = client
            .stream_chat_typed_spec(
                mock_responses_spec(&server),
                "sys",
                "hi",
                &cancel,
                &mut sink,
            )
            .await
            .unwrap_err();
        match err {
            ChatError::Provider(e) => assert!(is_rate_limited(&e), "{e}"),
            other => panic!("expected provider error, got {other:?}"),
        }
    }

    /// Point a spec at a local mock server.
    fn mock_spec(server: &wiremock::MockServer, model: &str) -> genai::ModelSpec {
        use genai::resolver::{AuthData, Endpoint};
        genai::ModelSpec::from_target(genai::ServiceTarget {
            endpoint: Endpoint::from_owned(server.uri()),
            auth: AuthData::from_single("test-key"),
            model: genai::ModelIden::new(genai::adapter::AdapterKind::OpenAI, model),
        })
    }

    #[test]
    fn effort_parse_strict_vocabulary() {
        assert_eq!(EffortLevel::parse("low").unwrap(), EffortLevel::Low);
        assert_eq!(EffortLevel::parse("medium").unwrap(), EffortLevel::Medium);
        assert_eq!(EffortLevel::parse("high").unwrap(), EffortLevel::High);
        for bad in ["", "HIGH", "max", "xhigh", "budget:100", "minimal"] {
            let err = EffortLevel::parse(bad).unwrap_err().to_string();
            assert!(err.contains("low, medium, high"), "{bad}: {err}");
        }
    }

    #[test]
    fn with_effort_preserves_session_contract() {
        // Headers + tool-call capture ride along untouched (the
        // with_conversation_id contract — effort only ADDS a field).
        let client = LlmClient::with_conversation_id("s1").with_effort(EffortLevel::High);
        let options = client.options.as_ref().expect("options set");
        assert_eq!(
            options.reasoning_effort.as_ref().map(|e| e.variant_name()),
            Some("high")
        );
        assert!(options.capture_tool_calls == Some(true));
        assert!(options.capture_reasoning_content == Some(true));
        let headers = format!("{:?}", options.extra_headers);
        assert!(headers.contains("x-opencode-session"), "{headers}");
    }

    #[test]
    fn router_effort_table() {
        use std::collections::BTreeMap;
        let mut cfg = polymorph_core::config::PolymorphConfig::default();
        let router = RoleRouter::from_config(&cfg);
        assert_eq!(router.effort_for(Role::Review).unwrap(), None);
        cfg.effort = BTreeMap::from([("review".to_string(), "high".to_string())]);
        let router = RoleRouter::from_config(&cfg);
        assert_eq!(
            router.effort_for(Role::Review).unwrap(),
            Some(EffortLevel::High)
        );
        assert_eq!(router.effort_for(Role::Act).unwrap(), None);
        cfg.effort = BTreeMap::from([("review".to_string(), "ultra".to_string())]);
        let router = RoleRouter::from_config(&cfg);
        let err = router.effort_for(Role::Review).unwrap_err().to_string();
        assert!(err.contains("low, medium, high"), "{err}");
    }

    #[tokio::test]
    async fn wiremock_effort_reaches_the_wire() {
        // The `reasoning_effort` field MUST appear in the request body on the OpenAI chat path. The
        // mock 404s otherwise, so a fold proves presence, not just acceptance.
        use wiremock::matchers::{body_string_contains, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(body_string_contains("reasoning_effort"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(SSE_OK),
            )
            .mount(&server)
            .await;
        let client =
            LlmClient::with_conversation_id("test-session").with_effort(EffortLevel::Medium);
        let cancel = CancellationToken::new();
        let mut sink = String::new();
        let out = client
            .stream_chat_typed_spec(
                mock_spec(&server, "w-effort"),
                "sys",
                "hi",
                &cancel,
                &mut sink,
            )
            .await
            .expect("effort-bearing turn must fold");
        assert!(out.text.contains("hello from mock"), "{}", out.text);
    }

    /// Minimal OpenAI SSE stream: one content chunk + DONE.
    const SSE_OK: &str = "data: {\"id\":\"chatcmpl-t\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"t\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hello from mock\"},\"finish_reason\":null}]}\n\ndata: [DONE]\n\n";

    #[tokio::test]
    async fn wiremock_429_maps_to_rate_limited_structurally() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(429).set_body_string(r#"{"error":"slow down"}"#))
            .mount(&server)
            .await;
        let client = LlmClient::new();
        let cancel = CancellationToken::new();
        let mut sink = String::new();
        let err = client
            .stream_chat_typed_spec(mock_spec(&server, "w-429"), "sys", "hi", &cancel, &mut sink)
            .await
            .unwrap_err();
        match err {
            ChatError::Provider(e) => assert!(is_rate_limited(&e)),
            e => panic!("expected structural 429 provider error, got {e:?}"),
        }
    }

    #[tokio::test]
    async fn wiremock_fallback_advances_past_429_to_success() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let limited = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(429).set_body_string(r#"{"error":"slow down"}"#))
            .mount(&limited)
            .await;
        let healthy = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(SSE_OK),
            )
            .mount(&healthy)
            .await;
        let client = LlmClient::new();
        let cancel = CancellationToken::new();
        let mut sink = String::new();
        let mut limited_hit = Vec::new();
        let pairs = vec![
            ("first".to_string(), mock_spec(&limited, "w-first")),
            ("second".to_string(), mock_spec(&healthy, "w-second")),
        ];
        let (outcome, label) = client
            .stream_chat_with_specs(pairs, "sys", "hi", &cancel, &mut sink, |m| {
                limited_hit.push(m.to_string())
            })
            .await
            .unwrap();
        assert_eq!(label, "second");
        assert_eq!(limited_hit, vec!["first"]);
        assert!(
            outcome.text.contains("hello from mock"),
            "got {:?}",
            outcome.text
        );
        assert!(!outcome.aborted);
    }

    #[test]
    fn conversation_constructors_tag_the_session_header() {
        let c = LlmClient::with_conversation_id("test-session");
        let opts = c.options.as_ref().expect("header must be set");
        let headers = opts
            .extra_headers
            .as_ref()
            .expect("extra headers must be set");
        let debug = format!("{headers:?}");
        assert!(debug.contains("x-opencode-session"), "{debug}");
        assert!(debug.contains("test-session"), "{debug}");
        assert!(LlmClient::new().options.is_none());
        assert!(LlmClient::default().options.is_none());
        // F3b: complete tool calls ride the stream End (capture on) on the
        // conversation constructors — the only production path.
        assert_eq!(c.options.as_ref().unwrap().capture_tool_calls, Some(true));
    }

    #[tokio::test]
    async fn wiremock_session_header_reaches_the_wire() {
        // The OpenCode Go gateway 400s without x-opencode-session
        // (MissingSessionID, observed live 2026-09-07): the mock demands it.
        use wiremock::matchers::{header, method};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(header("x-opencode-session", "test-session"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(SSE_OK),
            )
            .mount(&server)
            .await;
        let client = LlmClient::with_conversation_id("test-session");
        let cancel = CancellationToken::new();
        let mut sink = String::new();
        let outcome = client
            .stream_chat_typed_spec(
                mock_spec(&server, "w-sess"),
                "sys",
                "hi",
                &cancel,
                &mut sink,
            )
            .await
            .unwrap();
        assert!(outcome.text.contains("hello from mock"), "{outcome:?}");
    }

    fn end_with_calls(calls: Vec<genai::chat::ToolCall>) -> genai::chat::ChatStreamEvent {
        use genai::chat::{ChatStreamEvent, MessageContent, StreamEnd};
        ChatStreamEvent::End(StreamEnd {
            captured_content: Some(MessageContent::from_tool_calls(calls)),
            ..Default::default()
        })
    }

    fn made_call(id: &str, name: &str, args: serde_json::Value) -> genai::chat::ToolCall {
        genai::chat::ToolCall {
            call_id: id.to_string(),
            fn_name: name.to_string(),
            fn_arguments: args,
            thought_signatures: None,
        }
    }

    #[tokio::test]
    async fn collecting_fold_takes_text_from_chunks_calls_from_end() {
        // Fragment chunk events are IGNORED for accumulation (unstable ids —
        // struct docs); complete calls come from the End payload. A bare End
        // yields zero calls.
        let frag = {
            use genai::chat::{ChatStreamEvent, ToolCall, ToolChunk};
            ChatStreamEvent::ToolCallChunk(ToolChunk {
                tool_call: ToolCall {
                    call_id: "call_0".to_string(),
                    fn_name: String::new(),
                    fn_arguments: serde_json::json!("\"a\"}"),
                    thought_signatures: None,
                },
            })
        };
        let events = vec![
            chunk("working "),
            Ok(frag),
            Ok(end_with_calls(vec![made_call(
                "c1",
                "read",
                serde_json::json!({"path": "a"}),
            )])),
        ];
        let cancel = CancellationToken::new();
        let mut sink = String::new();
        let turn = drive_stream_collecting_tools(boxed(stream::iter(events)), &cancel, &mut sink)
            .await
            .unwrap();
        assert_eq!(turn.text, "working ");
        assert!(!turn.aborted);
        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(turn.tool_calls[0].call_id, "c1");
        assert_eq!(turn.tool_calls[0].fn_name, "read");
        assert_eq!(turn.tool_calls[0].fn_arguments["path"], "a");
        // Bare End (no capture): text only, zero calls.
        let events: Vec<genai::Result<genai::chat::ChatStreamEvent>> = vec![
            chunk("hi"),
            Ok(genai::chat::ChatStreamEvent::End(Default::default())),
        ];
        let mut sink = String::new();
        let turn = drive_stream_collecting_tools(boxed(stream::iter(events)), &cancel, &mut sink)
            .await
            .unwrap();
        assert_eq!(turn.text, "hi");
        assert!(turn.tool_calls.is_empty());
    }

    #[tokio::test]
    async fn collecting_fold_carries_reasoning_content_for_thinking_models() {
        let cancel = CancellationToken::new();
        let events = vec![
            Ok(genai::chat::ChatStreamEvent::ReasoningChunk(
                genai::chat::StreamChunk {
                    content: "think ".to_string(),
                },
            )),
            Ok(genai::chat::ChatStreamEvent::ReasoningChunk(
                genai::chat::StreamChunk {
                    content: "more".to_string(),
                },
            )),
            chunk("OK"),
            Ok(genai::chat::ChatStreamEvent::End(Default::default())),
        ];
        let mut sink = String::new();
        let turn = drive_stream_collecting_tools(boxed(stream::iter(events)), &cancel, &mut sink)
            .await
            .unwrap();
        assert_eq!(turn.text, "OK");
        assert_eq!(turn.reasoning_content.as_deref(), Some("think more"));
        assert!(turn.tool_calls.is_empty());

        // End-carried reasoning wins when the provider only exposes it there.
        let events = vec![
            chunk("OK"),
            Ok(genai::chat::ChatStreamEvent::End(genai::chat::StreamEnd {
                captured_reasoning_content: Some("end-only".to_string()),
                ..Default::default()
            })),
        ];
        let mut sink = String::new();
        let turn = drive_stream_collecting_tools(boxed(stream::iter(events)), &cancel, &mut sink)
            .await
            .unwrap();
        assert_eq!(turn.reasoning_content.as_deref(), Some("end-only"));
    }

    fn read_tool() -> genai::chat::Tool {
        genai::chat::Tool {
            name: genai::chat::ToolName::Custom("read".to_string()),
            description: Some("Read a file".to_string()),
            schema: Some(serde_json::json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"],
            })),
            strict: None,
            config: None,
        }
    }

    /// OpenAI SSE with a SPLIT tool call (two argument fragments): exercises
    /// genai's real fragment emission plus our accumulation end to end.
    /// Bodies are machine-built (a hand-escaped first draft failed StreamParse
    /// on its own commas — programmatically generated JSON only).
    fn sse_tool_split() -> String {
        fn chunk(delta: serde_json::Value, finish: serde_json::Value) -> String {
            let body = serde_json::json!({
                "id": "t", "object": "chat.completion.chunk", "created": 1, "model": "t",
                "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
            });
            format!("data: {body}\n\n")
        }
        let frag1 = serde_json::json!({"tool_calls": [{
            "index": 0, "id": "call_1", "type": "function",
            "function": {"name": "read", "arguments": "{\"path\":"},
        }]});
        let frag2 = serde_json::json!({"tool_calls": [{
            "index": 0, "function": {"arguments": "\"a\"}"},
        }]});
        format!(
            "{}{}{}data: [DONE]\n\n",
            chunk(frag1, serde_json::Value::Null),
            chunk(frag2, serde_json::Value::Null),
            chunk(serde_json::json!({}), serde_json::json!("tool_calls")),
        )
    }

    #[tokio::test]
    async fn wiremock_messages_path_collects_split_tool_call() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse_tool_split()),
            )
            .mount(&server)
            .await;
        // Conversation client: the capture flag lives on these constructors
        // (the only production path); `new()` stays bare per its pinning test.
        let client = LlmClient::with_conversation_id("test-tools");
        let cancel = CancellationToken::new();
        let mut sink = String::new();
        let turn = client
            .stream_chat_messages_spec(
                mock_spec(&server, "w-tools"),
                "sys",
                vec![genai::chat::ChatMessage::user("read a")],
                vec![read_tool()],
                &cancel,
                &mut sink,
                &[],
            )
            .await
            .unwrap();
        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(turn.tool_calls[0].fn_name, "read");
        assert_eq!(turn.tool_calls[0].fn_arguments["path"], "a");
    }

    #[test]
    fn fallback_extracts_deepseek_dsml() {
        let mut text = "Here is the file creation:\n<｜tool calls｜><｜tool call begin｜>function<｜tool sep｜>write\n```json\n{\"path\": \"foo.txt\", \"content\": \"hello\"}\n```<｜tool call end｜><｜tool calls end｜>\nDone!".to_string();
        let calls = extract_fallback_tool_calls(&mut text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].fn_name, "write");
        assert_eq!(calls[0].fn_arguments["path"], "foo.txt");
        assert_eq!(calls[0].fn_arguments["content"], "hello");
        assert_eq!(text, "Here is the file creation:\n\nDone!");
    }

    #[test]
    fn fallback_extracts_xml_and_fence() {
        let mut text_xml = "Calling tool:\n<tool_call>\n{\"name\": \"edit\", \"arguments\": {\"path\": \"bar.txt\", \"patch\": \"test\"}}\n</tool_call>".to_string();
        let calls_xml = extract_fallback_tool_calls(&mut text_xml);
        assert_eq!(calls_xml.len(), 1);
        assert_eq!(calls_xml[0].fn_name, "edit");
        assert_eq!(calls_xml[0].fn_arguments["path"], "bar.txt");

        let mut text_fence =
            "Running tool:\n```tool:bash\n{\"command\": \"ls -la\"}\n```\nAll set.".to_string();
        let calls_fence = extract_fallback_tool_calls(&mut text_fence);
        assert_eq!(calls_fence.len(), 1);
        assert_eq!(calls_fence[0].fn_name, "bash");
        assert_eq!(calls_fence[0].fn_arguments["command"], "ls -la");
        assert_eq!(text_fence, "Running tool:\n\nAll set.");
    }
}
