//! Tool-call hooks (Epic J1, chunk 79): PreToolUse / PostToolUse / Stop.
//!
//! In-process handler registry consulted at the `Registry::call` boundary
//! (pre → resolve → per-tool gate → execute → post). Hooks are an
//! ADDITIONAL layer — the distributed per-tool gates stay exactly where
//! they are (observed, not centralized). Handlers are sync and must be
//! fast/non-blocking (no sleeps, no spawns); script-command handlers ride
//! a later slice with their own timeout policy (stated, not silent).
//!
//! Semantics: PreToolUse consults handlers in registration order, FIRST
//! `Deny` wins (mapped to typed `CallError::Denied`, before even
//! resolve — hooks see probe/unknown calls too). PostToolUse/Stop
//! broadcast to every handler; their decisions are ignored (observation
//! only — a post-hook cannot resurrect a failed call).

use std::fmt;

/// What happened (Pre) or is happening (Post/Stop) at the call boundary.
/// Owned data throughout — no borrows cross the registry's awaits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookEvent {
    /// Before anything (incl. resolve):deny here leaves zero side effects.
    PreToolUse {
        name: String,
        args: serde_json::Value,
    },
    /// After execution (or denial/failure): `ok` names the branch, `detail`
    /// carries the shape (result keys on ok, error text on failure).
    /// NEVER carries secret values (same invariant as the tracing layers).
    PostToolUse {
        name: String,
        ok: bool,
        detail: String,
    },
    /// An agentic tool loop terminated (`run_turn` end): `reason` is the
    /// rounds/calls/dropped summary. Observation only.
    Stop { reason: String },
}

/// One hook handler: sync, fast, non-blocking (no sleeps, no spawns).
type HookHandler = Box<dyn Fn(&HookEvent) -> HookDecision + Send + Sync>;

/// Handler verdict. Only `PreToolUse` decisions act (first `Deny` wins);
/// Post/Stop verdicts are ignored by construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookDecision {
    Allow,
    Deny { reason: String },
}

/// In-process handler table. Empty by default (pure passthrough — every
/// pre-79 caller behaves identically, proven by the unchanged suite).
#[derive(Default)]
pub struct HookRegistry {
    handlers: Vec<HookHandler>,
}

impl fmt::Debug for HookRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Handler COUNT only: closures have no stable names worth printing.
        f.debug_struct("HookRegistry")
            .field("handlers", &self.handlers.len())
            .finish()
    }
}

impl HookRegistry {
    /// Register one handler (consulted in registration order).
    pub fn add_hook<F>(&mut self, handler: F)
    where
        F: Fn(&HookEvent) -> HookDecision + Send + Sync + 'static,
    {
        self.handlers.push(Box::new(handler));
    }

    /// PreToolUse consult: first `Deny` wins, else `Allow`. Empty table =
    /// `Allow` (passthrough).
    pub fn consult_pre(&self, name: &str, args: &serde_json::Value) -> HookDecision {
        let event = HookEvent::PreToolUse {
            name: name.to_string(),
            args: args.clone(),
        };
        for handler in &self.handlers {
            match handler(&event) {
                decision @ HookDecision::Deny { .. } => return decision,
                HookDecision::Allow => {}
            }
        }
        HookDecision::Allow
    }

    /// PostToolUse/Stop broadcast: every handler runs, all verdicts ignored.
    pub fn notify(&self, event: HookEvent) {
        for handler in &self.handlers {
            let _ = handler(&event);
        }
    }
}

/// One ask-user question (Epic J1, chunk 89): which tool wants to run and
/// the handler-supplied question. Owned throughout; the prompter answers
/// Allow/Deny — there is deliberately NO third sync variant (D1: the sync
/// boundary never blocks on a human).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptRequest {
    pub tool: String,
    pub question: String,
}

/// Interactive prompter: answers one question asynchronously. Implemented
/// by surfaces with a human (TUI modal); headless loops attach none and
/// fail closed (D2). The `Debug` bound keeps `Registry`'s derive working.
#[async_trait::async_trait]
pub trait Prompter: Send + Sync + std::fmt::Debug {
    async fn ask(&self, req: PromptRequest) -> HookDecision;

    /// Structured pick (Epic Q, chunk 122): choose among options.
    /// Default body dismisses (backward compatible — pre-122
    /// implementors keep compiling; surfaces override when they can
    /// render a picker, TUI/ACP arms docketed as Q-followup).
    async fn pick(&self, req: PickRequest) -> PickOutcome {
        let _ = req;
        PickOutcome::Dismissed {
            reason: "no pick UI attached".to_string(),
        }
    }
}

/// Cap on options per pick (Epic Q, chunk 122): overlay space + model
/// discipline (chunk-98 caps precedent); TUI slice inherits it.
pub const MAX_ASK_OPTIONS: usize = 8;

/// One pickable option (Epic Q, chunk 122): a stable label the model
/// gets back verbatim, an optional hint for the human, at most one
/// `recommended` per request (validated at the tool, not here).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickOption {
    pub label: String,
    pub hint: Option<String>,
    pub recommended: bool,
}

/// One structured pick question (Epic Q, chunk 122): owned throughout.
/// `multi` allows several labels; `allow_other` admits free text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickRequest {
    pub tool: String,
    pub question: String,
    pub options: Vec<PickOption>,
    pub multi: bool,
    pub allow_other: bool,
}

/// Structured pick outcome (Epic Q, chunk 122): labels (stable under
/// renderer reorder — never indices), free text, or dismissal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickOutcome {
    Selected(Vec<String>),
    Other(String),
    Dismissed { reason: String },
}

/// One ask hook: returns the question to put to the user, or `None` to
/// pass. Consulted in registration order; the first question wins (D1).
type AskHandler = Box<dyn Fn(&str, &serde_json::Value) -> Option<String> + Send + Sync>;

/// Ask-user table + optional prompter. Empty = never asks (all pre-89
/// behavior unchanged); fires without a prompter = fail-closed Deny (D2).
#[derive(Default)]
pub struct AskRegistry {
    handlers: Vec<AskHandler>,
    prompter: Option<std::sync::Arc<dyn Prompter>>,
}

impl std::fmt::Debug for AskRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Handler COUNT + prompter presence only (same precedent as
        // `HookRegistry`: closures have no stable names worth printing).
        f.debug_struct("AskRegistry")
            .field("handlers", &self.handlers.len())
            .field("has_prompter", &self.prompter.is_some())
            .finish()
    }
}

impl AskRegistry {
    /// Register one ask hook (consulted in registration order).
    pub fn add_ask_hook<F>(&mut self, handler: F)
    where
        F: Fn(&str, &serde_json::Value) -> Option<String> + Send + Sync + 'static,
    {
        self.handlers.push(Box::new(handler));
    }

    /// Attach the interactive prompter (runtime-attached like `add_hook`,
    /// never via `[[hooks]]` config — scripts cannot Ask, D5).
    pub fn set_prompter(&mut self, prompter: std::sync::Arc<dyn Prompter>) {
        self.prompter = Some(prompter);
    }

    /// First question wins, else `None` (pass-through).
    pub fn question_for(&self, name: &str, args: &serde_json::Value) -> Option<String> {
        for handler in &self.handlers {
            if let Some(question) = handler(name, args) {
                return Some(question);
            }
        }
        None
    }

    /// Resolve one pick through the prompter, or dismiss fail-closed
    /// without one (Q posture: denial the loop can see, never a stall).
    pub async fn resolve_pick(&self, req: PickRequest) -> PickOutcome {
        match &self.prompter {
            Some(prompter) => prompter.pick(req).await,
            None => PickOutcome::Dismissed {
                reason: format!("ask-no-prompter: {}", req.tool),
            },
        }
    }

    /// Resolve one question through the prompter, or fail closed (D2).
    pub async fn resolve(&self, tool: &str, question: String) -> HookDecision {
        match &self.prompter {
            Some(prompter) => {
                prompter
                    .ask(PromptRequest {
                        tool: tool.to_string(),
                        question,
                    })
                    .await
            }
            None => HookDecision::Deny {
                reason: format!("ask-no-prompter: {tool}"),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn recorder() -> (HookRegistry, Arc<Mutex<Vec<HookEvent>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut registry = HookRegistry::default();
        let seen_c = Arc::clone(&seen);
        registry.add_hook(move |event| {
            seen_c.lock().unwrap().push(event.clone());
            HookDecision::Allow
        });
        (registry, seen)
    }

    #[test]
    fn empty_registry_allows_and_notifies_nothing() {
        let registry = HookRegistry::default();
        assert_eq!(
            registry.consult_pre("read", &serde_json::json!({})),
            HookDecision::Allow
        );
        registry.notify(HookEvent::Stop {
            reason: "x".to_string(),
        });
    }

    #[test]
    fn first_deny_wins_later_handlers_skipped() {
        let mut registry = HookRegistry::default();
        let calls = Arc::new(Mutex::new(0u32));
        let calls_c = Arc::clone(&calls);
        registry.add_hook(move |_| {
            *calls_c.lock().unwrap() += 1;
            HookDecision::Deny {
                reason: "first".to_string(),
            }
        });
        registry.add_hook(|_| HookDecision::Allow);
        let decision = registry.consult_pre("write", &serde_json::json!({}));
        assert_eq!(
            decision,
            HookDecision::Deny {
                reason: "first".to_string()
            }
        );
        // The denying handler ran once; the Allow handler never ran.
        assert_eq!(*calls.lock().unwrap(), 1);
    }

    #[test]
    fn notify_broadcasts_to_every_handler() {
        let (registry, seen) = recorder();
        // Second handler proves broadcast (both run, verdicts ignored).
        let seen_c = Arc::clone(&seen);
        let mut registry = registry;
        registry.add_hook(move |event| {
            seen_c.lock().unwrap().push(event.clone());
            HookDecision::Deny {
                reason: "ignored".to_string(),
            }
        });
        registry.notify(HookEvent::PostToolUse {
            name: "read".to_string(),
            ok: true,
            detail: "read".to_string(),
        });
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    fn pick_req() -> PickRequest {
        PickRequest {
            tool: "ask".to_string(),
            question: "Ship it?".to_string(),
            options: vec![PickOption {
                label: "yes".to_string(),
                hint: None,
                recommended: true,
            }],
            multi: false,
            allow_other: true,
        }
    }

    #[tokio::test]
    async fn resolve_pick_without_prompter_dismisses_fail_closed() {
        let reg = AskRegistry::default();
        assert!(matches!(
            reg.resolve_pick(pick_req()).await,
            PickOutcome::Dismissed { .. }
        ));
        let err = reg.resolve_pick(pick_req()).await;
        assert!(format!("{err:?}").contains("ask-no-prompter"));
    }

    #[tokio::test]
    async fn default_pick_impl_dismisses() {
        // A pre-122 prompter (ask-only) keeps compiling AND dismisses
        // picks — backward compatibility with a stated behavior.
        #[derive(Debug)]
        struct AskOnly;
        #[async_trait::async_trait]
        impl Prompter for AskOnly {
            async fn ask(&self, _req: PromptRequest) -> HookDecision {
                HookDecision::Allow
            }
        }
        let mut reg = AskRegistry::default();
        reg.set_prompter(std::sync::Arc::new(AskOnly));
        assert!(matches!(
            reg.resolve_pick(pick_req()).await,
            PickOutcome::Dismissed { .. }
        ));
    }
}
