//! `polymorph-acp::session_loop` — text and tool turn binding.
//!
//! Drives ONE model turn per ACP prompt over the CLI recipe verbatim:
//! fresh `LlmClient` + `Managed` act role + act-pack system
//! through `FixTurnRunner` (text) or `AgenticFixRunner` (tools), called
//! directly (no bus spawn for text; tools run InPlace on the serve cwd —
//! worktree isolation stays `review run`'s `--repo`). History bounds the
//! brief ([`HISTORY_TURNS`]); the full text returns as one notification
//! (token streaming deferred with reason — streamer plumbing).

use std::sync::Arc;

use polymorph_core::sessions::Message;

/// History rows kept in the brief (cost bug refused — full dumps grow the
/// context without bound; G-plan resumes already summarize long arcs).
pub const HISTORY_TURNS: usize = 20;

/// Render the model brief: last [`HISTORY_TURNS`] rows role-labeled,
/// oldest-first (store order), current prompt last. Pure (unit-tested).
#[must_use]
pub fn render_brief(history: &[Message], prompt: &str) -> String {
    let mut out = String::new();
    let take = history.len().min(HISTORY_TURNS);
    for m in &history[history.len() - take..] {
        out.push_str(&format!("{}: {}\n", m.role, m.content));
    }
    out.push_str(&format!("user: {prompt}\n"));
    out
}

/// Live text-loop pieces (CLI-owned; built once per `acp` serve).
/// The per-turn client stays FRESH (clean context per turn — per-call
/// fresh-children precedent), so this holds everything EXCEPT the client.
pub struct LiveTextLoop {
    pub router: polymorph_llm::RoleRouter,
    pub providers: Vec<polymorph_core::config::ProviderEntry>,
    pub state: Arc<tokio::sync::Mutex<polymorph_llm::FallbackState>>,
    pub system: String,
}

impl LiveTextLoop {
    /// One live text turn: fresh conversation client + `FixTurnRunner`,
    /// driven directly (no bus). Transport/auth failures propagate LOUD
    /// (a dead model is an error, never a silent turn).
    pub async fn drive_live(
        &self,
        brief: &str,
        workdir: std::path::PathBuf,
        cancel: tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<String> {
        let runner = polymorph_review::FixTurnRunner {
            client: polymorph_llm::LlmClient::with_fresh_conversation(),
            target: polymorph_review::LlmTarget::Managed {
                router: self.router.clone(),
                providers: self.providers.clone(),
                state: self.state.clone(),
                role: polymorph_core::Role::Act,
                scope: None,
            },
            system: self.system.clone(),
            pinned: Default::default(),
        };
        drive_turn(&runner, brief, workdir, cancel).await
    }
}

/// Live tool-loop pieces: everything `AgenticFixRunner::live`
/// needs except the per-turn client. CLI-owned; built once per serve when
/// `--tools` is on (registry includes MCP servers then — fail-soft).
pub struct LiveToolLoop {
    pub router: polymorph_llm::RoleRouter,
    pub providers: Vec<polymorph_core::config::ProviderEntry>,
    pub state: Arc<tokio::sync::Mutex<polymorph_llm::FallbackState>>,
    pub system: String,
    pub registry: Arc<polymorph_tools::Registry>,
    pub gate: polymorph_tools::Gate,
    pub tools: Vec<String>,
    pub max_tool_rounds: u32,
    /// Watching reviewer: project toggle carried to
    /// the fixer (off = pre-S behavior). Bench paths never set this.
    pub advisor: bool,
}

impl LiveToolLoop {
    /// One live agentic turn: fresh conversation client +
    /// `AgenticFixRunner::live`, driven directly. Allowlist + Gate enforce
    /// every call at runner call time; denials feed back as
    /// text (recoverable, never a bail). Transport/auth failures
    /// propagate LOUD.
    pub async fn drive_live_tools(
        &self,
        brief: &str,
        workdir: std::path::PathBuf,
        cancel: tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<String> {
        let mut runner = polymorph_review::AgenticFixRunner::live(
            polymorph_llm::LlmClient::with_fresh_conversation(),
            polymorph_review::LlmTarget::Managed {
                router: self.router.clone(),
                providers: self.providers.clone(),
                state: self.state.clone(),
                role: polymorph_core::Role::Act,
                scope: None,
            },
            self.system.clone(),
            self.registry.clone(),
            self.gate.clone(),
            self.tools.clone(),
            self.max_tool_rounds,
        );
        // Only the turn text is used here: no ledger, attachment or
        // rollback reads command change detection, so skip its walks.
        runner.command_changes = 0;
        // S advisor: same project toggle as review-run.
        if self.advisor {
            runner.advisor = Some(polymorph_review::AdvisorRunner::managed(
                self.router.clone(),
                self.providers.clone(),
                self.state.clone(),
            ));
        }
        drive_turn(&runner, brief, workdir, cancel).await
    }
}

/// Drive one turn through an injected runner (the F1 seam): builds the
/// `TurnContext` and returns the turn text. Offline tests script the runner;
/// live path passes `FixTurnRunner` (above). `scope` stays `None`
/// (unpinned pins are `review run`'s `--scope`; `acp` has no flag).
pub async fn drive_turn(
    runner: &dyn polymorph_delegate::TurnRunner,
    brief: &str,
    workdir: std::path::PathBuf,
    cancel: tokio_util::sync::CancellationToken,
) -> anyhow::Result<String> {
    let ctx = polymorph_delegate::TurnContext {
        child_id: "acp-turn".into(),
        role: polymorph_core::Role::Act,
        brief: brief.into(),
        workdir,
        turn: 1,
        steer: Vec::new(),
        cancel,
    };
    let out = runner.run_turn(ctx).await?;
    Ok(out.text)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scripted {
        text: String,
        fail: bool,
    }

    #[async_trait::async_trait]
    impl polymorph_delegate::TurnRunner for Scripted {
        async fn run_turn(
            &self,
            ctx: polymorph_delegate::TurnContext,
        ) -> anyhow::Result<polymorph_delegate::TurnOutput> {
            assert_eq!(ctx.role, polymorph_core::Role::Act);
            assert_eq!(ctx.turn, 1);
            if self.fail {
                anyhow::bail!("scripted transport failure");
            }
            Ok(polymorph_delegate::TurnOutput {
                text: self.text.clone(),
                summary: "scripted".into(),
                tool_calls: 0,
                done: true,
                data: None,
            })
        }
    }

    fn msg(role: &str, content: &str) -> Message {
        Message {
            id: 0,
            session_id: "s".into(),
            role: role.parse().unwrap(),
            content: content.into(),
            circuit_log: None,
            created_at: 0,
        }
    }

    #[test]
    fn brief_caps_labels_and_order() {
        let hist = vec![msg("user", "a"), msg("assistant", "b"), msg("user", "c")];
        let brief = super::render_brief(&hist, "d?");
        assert_eq!(brief, "user: a\nassistant: b\nuser: c\nuser: d?\n");
        assert_eq!(super::HISTORY_TURNS, 20);
    }

    #[test]
    fn brief_empty_history_is_prompt_only() {
        assert_eq!(super::render_brief(&[], "hi"), "user: hi\n");
    }

    #[tokio::test]
    async fn drive_turn_returns_scripted_text() {
        let text = super::drive_turn(
            &Scripted {
                text: "hello-live".into(),
                fail: false,
            },
            "brief",
            std::env::temp_dir(),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(text, "hello-live");
    }

    #[tokio::test]
    async fn drive_turn_failure_propagates_loud() {
        let err = super::drive_turn(
            &Scripted {
                text: String::new(),
                fail: true,
            },
            "brief",
            std::env::temp_dir(),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect_err("runner failure must propagate");
        assert!(err.to_string().contains("scripted transport failure"));
    }

    /// Scripted model turns (binding.rs `StubAgenticTurn` precedent):
    /// preloaded batches served in order, attempts counted.
    struct StubAgenticTurn {
        script: std::sync::Mutex<std::collections::VecDeque<polymorph_llm::CollectedTurn>>,
    }

    impl StubAgenticTurn {
        fn tool_call(id: &str, name: &str, args: serde_json::Value) -> genai::chat::ToolCall {
            genai::chat::ToolCall {
                call_id: id.to_string(),
                fn_name: name.to_string(),
                fn_arguments: args,
                thought_signatures: None,
            }
        }

        fn turn_text(text: &str) -> polymorph_llm::CollectedTurn {
            polymorph_llm::CollectedTurn {
                text: text.into(),
                tool_calls: Vec::new(),
                reasoning_content: None,
                aborted: false,
                rule_hit: None,
            }
        }
    }

    #[async_trait::async_trait]
    impl polymorph_review::AgenticTurnFn for StubAgenticTurn {
        async fn turn(
            &self,
            _messages: Vec<genai::chat::ChatMessage>,
            _tools: Vec<genai::chat::Tool>,
            _cancel: &tokio_util::sync::CancellationToken,
        ) -> anyhow::Result<polymorph_llm::CollectedTurn> {
            Ok(self
                .script
                .lock()
                .unwrap()
                .pop_front()
                .expect("script empty"))
        }
    }

    fn tool_runner(
        script: Vec<polymorph_llm::CollectedTurn>,
        gate: polymorph_tools::Gate,
    ) -> polymorph_review::AgenticFixRunner {
        polymorph_review::AgenticFixRunner {
            turn: std::sync::Arc::new(StubAgenticTurn {
                script: std::sync::Mutex::new(script.into()),
            }),
            registry: std::sync::Arc::new(polymorph_tools::registry::v1_registry()),
            gate,
            tools: vec!["read".into()],
            max_tool_rounds: 4,
            max_bonus_tool_rounds: polymorph_review::DEFAULT_MAX_BONUS_TOOL_ROUNDS,
            advisor: None,
            restore: None,
            history: Default::default(),
            command_changes: 0,
        }
    }

    /// Allowlist + gate pass: `read` executes a temp file, text follows.
    #[tokio::test]
    async fn tools_read_executes_temp_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let file = dir.path().join("note.txt");
        std::fs::write(&file, "temp-contents").unwrap();
        let call = StubAgenticTurn::tool_call(
            "c1",
            "read",
            serde_json::json!({"path": file.to_string_lossy()}),
        );
        let runner = tool_runner(
            vec![
                polymorph_llm::CollectedTurn {
                    text: "reading".into(),
                    tool_calls: vec![call],
                    reasoning_content: None,
                    aborted: false,
                    rule_hit: None,
                },
                StubAgenticTurn::turn_text("saw-it"),
            ],
            polymorph_tools::Gate::new(Vec::new(), Vec::new()),
        );
        let text = super::drive_turn(
            &runner,
            "brief",
            dir.path().into(),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(text, "saw-it");
    }

    /// Deny feeds back as TEXT (recoverable) — the loop continues,
    /// never bails.
    #[tokio::test]
    async fn tools_deny_feeds_text_not_bail() {
        let dir = tempfile::TempDir::new().unwrap();
        let call =
            StubAgenticTurn::tool_call("c1", "read", serde_json::json!({"path": "note.txt"}));
        let runner = tool_runner(
            vec![
                polymorph_llm::CollectedTurn {
                    text: "trying".into(),
                    tool_calls: vec![call],
                    reasoning_content: None,
                    aborted: false,
                    rule_hit: None,
                },
                StubAgenticTurn::turn_text("adjusted"),
            ],
            polymorph_tools::Gate::new(Vec::new(), vec!["read".into()]),
        );
        let text = super::drive_turn(
            &runner,
            "brief",
            dir.path().into(),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(text, "adjusted");
    }

    /// Unknown tools refuse LOUDLY into the feed — still no bail.
    #[tokio::test]
    async fn tools_unknown_refused_still_completes() {
        let dir = tempfile::TempDir::new().unwrap();
        let call = StubAgenticTurn::tool_call("c1", "nope_tool", serde_json::json!({}));
        let runner = tool_runner(
            vec![
                polymorph_llm::CollectedTurn {
                    text: "oops".into(),
                    tool_calls: vec![call],
                    reasoning_content: None,
                    aborted: false,
                    rule_hit: None,
                },
                StubAgenticTurn::turn_text("moved-on"),
            ],
            polymorph_tools::Gate::new(Vec::new(), Vec::new()),
        );
        let text = super::drive_turn(
            &runner,
            "brief",
            dir.path().into(),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(text, "moved-on");
    }
}
