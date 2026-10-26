//! Cassette replay for MCP transcripts.
//!
//! `FramePlayer` implements rmcp's `Transport<RoleClient>` over the `in`
//! frames of `tests/cassettes/mcp_probe.json` (recorded): each
//! `receive()` pops the next recorded server frame, each `send()` logs its
//! method. The replay test serves a real client over the player, enrolls,
//! and drives `Registry::call` — results and the sent spine must equal the
//! recording exactly. No child process, no python, fully deterministic.

use polymorph_tools::mcp::{enroll_mcp_tools, McpToolInfo};
use polymorph_tools::{Gate, Registry, ToolCallContext};
use std::future::Future;
use std::path::PathBuf;

/// A scripted `Transport<RoleClient>`: canned server frames in, method log
/// out. Release is send-gated: the service pump drains an
/// always-ready transport BEFORE any request is pending, so `receive()`
/// parks until the matching send happened — exactly the causality a real
/// server provides. Plan entries pair 1:1 with sends in recorded order;
/// notifications pair with `None` (skipped, never delivered). Divergence
/// (unexpected send) reads as EOF: the service fails `TransportClosed`
/// and the test goes red naming the send log — never a hang, never silent.
pub struct FramePlayer {
    state: std::sync::Arc<std::sync::Mutex<PlayerState>>,
    notify: std::sync::Arc<tokio::sync::Notify>,
    sent: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

struct PlayerState {
    /// (request method, response frame or `None` for notifications).
    plan: Vec<(
        String,
        Option<rmcp::service::RxJsonRpcMessage<rmcp::RoleClient>>,
    )>,
    /// Plan entries consumed (responses delivered + notifications skipped).
    released: usize,
    closed: bool,
}

impl FramePlayer {
    fn new(
        plan: Vec<(
            String,
            Option<rmcp::service::RxJsonRpcMessage<rmcp::RoleClient>>,
        )>,
    ) -> (Self, SentLog) {
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        (
            Self {
                state: std::sync::Arc::new(std::sync::Mutex::new(PlayerState {
                    plan,
                    released: 0,
                    closed: false,
                })),
                notify: std::sync::Arc::new(tokio::sync::Notify::new()),
                sent: log.clone(),
            },
            SentLog(log),
        )
    }
}

/// Read handle on the player's send log.
pub struct SentLog(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

impl SentLog {
    fn methods(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

fn sent_method(item: &rmcp::service::TxJsonRpcMessage<rmcp::RoleClient>) -> String {
    serde_json::to_value(item)
        .ok()
        .and_then(|v| v.get("method")?.as_str().map(str::to_string))
        .unwrap_or_else(|| "(response)".to_string())
}

impl rmcp::transport::Transport<rmcp::RoleClient> for FramePlayer {
    type Error = std::io::Error;

    fn send(
        &mut self,
        item: rmcp::service::TxJsonRpcMessage<rmcp::RoleClient>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let log = self.sent.clone();
        let notify = self.notify.clone();
        let method = sent_method(&item);
        if std::env::var("MCP_REPLAY_DEBUG").is_ok() {
            eprintln!(
                "player send <- {}",
                serde_json::to_string(&item).unwrap_or_default()
            );
        }
        async move {
            log.lock().unwrap().push(method);
            notify.notify_waiters();
            Ok(())
        }
    }

    async fn receive(&mut self) -> Option<rmcp::service::RxJsonRpcMessage<rmcp::RoleClient>> {
        loop {
            let notify = self.notify.clone();
            let notified = notify.notified();
            tokio::pin!(notified);
            {
                let mut state = self.state.lock().unwrap();
                let log = self.sent.lock().unwrap();
                // Release plan entries up to the sends made: responses are
                // delivered, notifications skipped. A send beyond the plan
                // (or a method mismatch) reads as EOF — loud red downstream.
                while state.released < state.plan.len() && state.released < log.len() {
                    let idx = state.released;
                    state.released += 1;
                    let (want, resp) = state.plan[idx].clone();
                    if log[idx] != want {
                        eprintln!(
                            "player: replay diverged at send #{idx}: got {:?}, plan wants {want:?}",
                            log[idx]
                        );
                        return None;
                    }
                    if let Some(frame) = resp {
                        if std::env::var("MCP_REPLAY_DEBUG").is_ok() {
                            eprintln!(
                                "player receive -> {}",
                                serde_json::to_string(&frame).unwrap_or_default()
                            );
                        }
                        return Some(frame);
                    }
                }
                if state.closed || log.len() > state.plan.len() {
                    if log.len() > state.plan.len() {
                        eprintln!(
                            "player: {} sends exceed {} plan entries",
                            log.len(),
                            state.plan.len()
                        );
                    }
                    return None;
                }
            }
            notified.await;
        }
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        self.state.lock().unwrap().closed = true;
        self.notify.notify_waiters();
        Ok(())
    }
}

fn cassette() -> serde_json::Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("cassettes")
        .join("mcp_probe.json");
    let text = std::fs::read_to_string(&path).unwrap();
    serde_json::from_str(&text).unwrap()
}

/// Replay the committed transcript end to end: serve over the player,
/// enroll, call both tools through the registry, and prove results AND
/// the sent spine equal the recording (ids pair requests to responses).
#[tokio::test]
async fn replay_matches_recording() {
    use rmcp::ServiceExt;

    let tape = cassette();
    let frames = tape["frames"].as_array().unwrap();
    assert!(!frames.is_empty(), "cassette must hold frames");

    // Build the release plan in recorded order: every `out` request pairs
    // with its `in` response by id; notifications pair with `None`. Every
    // `in` line must parse as a client-side message (a corrupt cassette
    // fails HERE naming the frame, never mid-replay).
    let mut plan = Vec::new();
    let mut recorded_out_methods = Vec::new();
    for f in frames.iter() {
        if f["dir"] != "out" {
            continue;
        }
        let Some(method) = f["line"]["method"].as_str() else {
            continue;
        };
        recorded_out_methods.push(method.to_string());
        let response = f["line"].get("id").and_then(|id| {
            frames.iter().find_map(|r| {
                (r["dir"] == "in" && r["line"].get("id") == Some(id)).then(|| {
                    serde_json::from_value(r["line"].clone()).unwrap_or_else(|e| {
                        panic!("cassette response for id {id} does not parse: {e}")
                    })
                })
            })
        });
        plan.push((method.to_string(), response));
    }
    assert!(!plan.is_empty(), "cassette must hold requests");

    // Pair call requests to their recorded results by id (strict: every
    // recorded call must have exactly one recorded result).
    let mut expected: std::collections::BTreeMap<String, String> = Default::default();
    for f in frames {
        if f["dir"] == "out" && f["line"]["method"] == "tools/call" {
            let id = f["line"]["id"].clone();
            let request_name = f["line"]["params"]["name"].as_str().unwrap().to_string();
            let result = frames
                .iter()
                .find(|r| r["dir"] == "in" && r["line"]["id"] == id)
                .unwrap_or_else(|| panic!("no recorded result for call id {id}"));
            let text = result["line"]["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .to_string();
            expected.insert(request_name, text);
        }
    }
    assert_eq!(expected.len(), 2, "cassette holds echo + fail calls");

    let (player, sent) = FramePlayer::new(plan);
    let client = ().serve(player).await.unwrap();
    // `serve` moved the player; replays assert through results + send log.

    let infos: Vec<McpToolInfo> = client
        .list_tools(None)
        .await
        .unwrap()
        .tools
        .iter()
        .map(McpToolInfo::from_rmcp)
        .collect();
    // Reuse the production mapping (the same fn the live path calls).
    let mut r = Registry::new();
    enroll_mcp_tools(&mut r, "probe", &infos).unwrap();
    r.inject_mcp_client("probe", std::sync::Arc::new(client))
        .await;

    let d = tempfile::tempdir().unwrap();
    let ctx = ToolCallContext::new(
        d.path().to_path_buf(),
        Gate::new(vec![], vec![]),
        tokio_util::sync::CancellationToken::new(),
    );
    let echo = r
        .call("mcp__probe__probe_echo", serde_json::json!({}), &ctx)
        .await
        .unwrap();
    assert_eq!(
        echo.to_string(),
        serde_json::json!({"content": [{"type": "text", "text": expected["probe_echo"]}], "is_error": false}).to_string()
    );
    let err = r
        .call("mcp__probe__probe_fail", serde_json::json!({}), &ctx)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains(&expected["probe_fail"]), "{err}");

    // Sent spine equals the recording exactly (order + membership).
    assert_eq!(sent.methods(), recorded_out_methods);
}
