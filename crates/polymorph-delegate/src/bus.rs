//! Spawn/steer/stop bus.
//!
//! The bus owns child LIFECYCLE, never child MINDS: each turn is executed by
//! a pluggable [`TurnRunner`] (wires the real tools+LLM loop; tests
//! and `delegate run` use fakes).
//!
//! Rules:
//! - Fresh context per child: the runner gets ONLY `brief` + steer messages.
//! - Steerable mid-run (mpsc, delivered next turn, resets stall).
//! - Stoppable via [`CancellationToken`](tokio_util::sync::CancellationToken)
//!   (uniform async pattern). Runners MUST observe `ctx.cancel` — the bus
//!   never kills a hung runner any other way.
//! - NO wall-clock kills: budgets are turns/tool-calls/stall.
//!   `wall_secs` is recorded for observability only.
//! - Flat by default: `spawn` refuses `depth > max_depth` (default 1).

use async_trait::async_trait;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::Instant;
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

pub use crate::budget::Budget;
pub use crate::result::{FinishStatus, StopReason, TypedResult};
use crate::worktree::{self, WorktreeMode};

/// Process-unique child sequence (`c1`, `c2`, …). Ordering across the process
/// is all it promises; tests assert prefix/shape, never exact numbers.
static CHILD_SEQ: AtomicU64 = AtomicU64::new(1);

/// What the runner receives each turn. Everything the child may know.
pub struct TurnContext {
    pub child_id: String,
    pub role: polymorph_core::Role,
    pub brief: String,
    pub workdir: PathBuf,
    /// 1-based turn number.
    pub turn: u32,
    /// Steer messages queued since last turn, oldest first (drained).
    pub steer: Vec<String>,
    /// Runner MUST observe this (select/abort mid-turn on cancel).
    pub cancel: CancellationToken,
}

/// What the runner returns each turn.
pub struct TurnOutput {
    /// Transcript text for this turn (may be empty on tool-only turns).
    pub text: String,
    /// One-line outcome for the typed summary when `done`.
    pub summary: String,
    /// Tool calls issued this turn (counts toward the tool budget).
    pub tool_calls: u32,
    /// True = work finished; `data` carries the typed payload.
    pub done: bool,
    /// Typed payload (meaningful when `done`; validated by the caller).
    pub data: Option<serde_json::Value>,
}

impl TurnOutput {
    /// Progress = anything observable (done, text, or tool use). Blank idle
    /// turns advance the stall counter.
    fn progress(&self) -> bool {
        self.done || !self.text.trim().is_empty() || self.tool_calls > 0
    }
}

/// One turn of child thought. Implementors: the live tools+LLM loop, tests,
/// `delegate run` demos. Must be prompt: observe `ctx.cancel`.
#[async_trait]
pub trait TurnRunner: Send + Sync {
    async fn run_turn(&self, ctx: TurnContext) -> anyhow::Result<TurnOutput>;
}

/// Where a child works.
#[derive(Debug, Clone)]
pub enum WorkdirSpec {
    /// Existing directory (caller-owned; bus never deletes).
    InPlace(PathBuf),
    /// Fresh `git worktree` under `parent` (auto-removed unless kept).
    Worktree {
        repo: PathBuf,
        parent: PathBuf,
        mode: WorktreeMode,
    },
}

/// Everything needed to spawn a child.
pub struct SpawnSpec {
    pub role: polymorph_core::Role,
    pub brief: String,
    pub workdir: WorkdirSpec,
    pub budget: Budget,
    /// Allowed tool NAMES (scoping record; enforced by the runner/loop in F,
    /// which owns the registry — the bus never imports tools).
    pub tools: Vec<String>,
    /// 1 = child of main. Bus refuses `depth > max_depth`.
    pub depth: u32,
    /// Keep the worktree after the run (F sets this for fixers whose branch
    /// merges back; default false = auto-remove, no litter).
    pub keep_worktree: bool,
}

impl SpawnSpec {
    pub fn new(role: polymorph_core::Role, brief: impl Into<String>, workdir: WorkdirSpec) -> Self {
        let budget = Budget::for_role(role);
        Self {
            role,
            brief: brief.into(),
            workdir,
            budget,
            tools: Vec::new(),
            depth: 1,
            keep_worktree: false,
        }
    }
}

/// A finished child: typed result + spend accounting.
pub struct ChildOutcome {
    pub id: String,
    pub result: TypedResult,
    pub turns_used: u32,
    pub tool_calls: u64,
    /// Observed only — never a stop condition (master rule).
    pub wall_secs: f64,
    /// Kept worktree for merge-back: `Some` exactly when
    /// the child ran `keep_worktree` (the guard survived instead of
    /// auto-removing). `None` everywhere else — callers must not infer
    /// mergeability from anything else.
    pub kept: Option<crate::worktree::KeptWorktree>,
}

/// Live child: steer, stop, await.
pub struct ChildHandle {
    id: String,
    cancel: CancellationToken,
    steer_tx: mpsc::UnboundedSender<String>,
    join: tokio::task::JoinHandle<ChildOutcome>,
}

impl ChildHandle {
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Queue a steer message (delivered next turn; resets stall). Errors when
    /// the drive loop is already gone (names the child).
    pub fn steer(&self, msg: impl Into<String>) -> anyhow::Result<()> {
        self.steer_tx
            .send(msg.into())
            .map_err(|_| anyhow::anyhow!("child {} is gone — steer undeliverable", self.id))?;
        Ok(())
    }

    /// Request stop (cancellation). Returns immediately; [`Self::join`]
    /// awaits the typed outcome.
    pub fn stop(&self) {
        self.cancel.cancel();
    }

    /// Await the outcome. A panicked drive loop becomes `Failed`, never a
    /// panic in the caller (the bus never panics outward).
    pub async fn join(self) -> ChildOutcome {
        match self.join.await {
            Ok(outcome) => outcome,
            Err(e) => ChildOutcome {
                id: self.id.clone(),
                result: TypedResult::failed(format!("child task ended without outcome: {e}")),
                turns_used: 0,
                tool_calls: 0,
                wall_secs: 0.0,
                kept: None,
            },
        }
    }

    /// Stop + await.
    pub async fn shutdown(self) -> ChildOutcome {
        self.stop();
        self.join().await
    }
}

/// Turn transcript sink (E1c). Failures are warned-over, never fatal:
/// a dead transcript must not kill a live child.
pub type Transcript = Arc<dyn TranscriptSink>;

#[async_trait]
pub trait TranscriptSink: Send + Sync {
    async fn record(&self, child_id: &str, turn: u32, role: &str, text: &str)
        -> anyhow::Result<()>;
}

/// [`TranscriptSink`] over the session store (one session per fan-out or per
/// parent turn — the caller owns that choice; the bus just appends).
/// `Store` is `!Clone`, so the sink holds it behind `Arc<Mutex<..>>`.
pub struct StoreTranscript {
    store: Arc<Mutex<polymorph_core::sessions::Store>>,
    session_id: String,
}

impl StoreTranscript {
    pub fn new(
        store: Arc<Mutex<polymorph_core::sessions::Store>>,
        session_id: impl Into<String>,
    ) -> Self {
        Self {
            store,
            session_id: session_id.into(),
        }
    }
}

#[async_trait]
impl TranscriptSink for StoreTranscript {
    async fn record(
        &self,
        child_id: &str,
        turn: u32,
        role: &str,
        text: &str,
    ) -> anyhow::Result<()> {
        let line = format!("[{child_id} t{turn} {role}] {text}");
        self.store
            .lock()
            .await
            .append_message(
                &self.session_id,
                polymorph_core::sessions::MessageRole::Assistant,
                &line,
            )
            .await
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("transcript append failed: {e}"))
    }
}

/// Deterministic demo/test runner: idle until `turns_to_done`, then done.
/// Records every steer payload it ever saw (steer-delivery proof).
pub struct EchoRunner {
    turns_to_done: u32,
    summary: String,
    seen_steer: Mutex<Vec<String>>,
}

impl EchoRunner {
    pub fn new(turns_to_done: u32, summary: impl Into<String>) -> Self {
        Self {
            turns_to_done,
            summary: summary.into(),
            seen_steer: Mutex::new(Vec::new()),
        }
    }

    pub async fn seen_steer(&self) -> Vec<String> {
        self.seen_steer.lock().await.clone()
    }
}

#[async_trait]
impl TurnRunner for EchoRunner {
    async fn run_turn(&self, ctx: TurnContext) -> anyhow::Result<TurnOutput> {
        self.seen_steer
            .lock()
            .await
            .extend(ctx.steer.iter().cloned());
        let done = ctx.turn >= self.turns_to_done;
        Ok(TurnOutput {
            text: format!("echo t{}: {}", ctx.turn, ctx.brief),
            summary: self.summary.clone(),
            tool_calls: 0,
            done,
            data: done.then(|| serde_json::json!({ "echo": ctx.brief })),
        })
    }
}

/// The bus: spawn policy (depth) + fan-out. State-free otherwise (children
/// own their tasks; transcripts own their stores).
#[derive(Debug)]
pub struct Bus {
    max_depth: u32,
}

impl Bus {
    pub fn new(max_depth: u32) -> Self {
        Self { max_depth }
    }

    /// Flat by default: children may not spawn (depth 1 only).
    pub fn flat() -> Self {
        Self::new(1)
    }

    pub fn max_depth(&self) -> u32 {
        self.max_depth
    }

    /// Spawn one child. Refuses `spec.depth > max_depth` (named error).
    /// Worktree creation happens INSIDE the drive task (async), so spawn
    /// itself never blocks on git.
    pub async fn spawn(
        &self,
        spec: SpawnSpec,
        runner: Arc<dyn TurnRunner>,
        transcript: Option<Transcript>,
    ) -> anyhow::Result<ChildHandle> {
        if spec.depth > self.max_depth {
            anyhow::bail!(
                "child depth {} exceeds bus max_depth {} (flat by default — raise for orchestrators)",
                spec.depth,
                self.max_depth
            );
        }
        let id = format!("c{}", CHILD_SEQ.fetch_add(1, Ordering::Relaxed));
        let cancel = CancellationToken::new();
        let (steer_tx, steer_rx) = mpsc::unbounded_channel();
        let join = tokio::spawn(drive(
            id.clone(),
            spec,
            runner,
            transcript,
            cancel.clone(),
            steer_rx,
        ));
        Ok(ChildHandle {
            id,
            cancel,
            steer_tx,
            join,
        })
    }

    /// Fan out N children, await ALL, return outcomes in SPAWN order.
    /// A sibling's failure never cancels the rest (caller sets policy on the
    /// collected outcomes). Depth violations fail fast BEFORE any spawn.
    pub async fn fan_out(
        &self,
        specs: Vec<SpawnSpec>,
        runner: Arc<dyn TurnRunner>,
        transcript: Option<Transcript>,
    ) -> anyhow::Result<Vec<(String, ChildOutcome)>> {
        for (i, spec) in specs.iter().enumerate() {
            if spec.depth > self.max_depth {
                anyhow::bail!(
                    "fan-out child {i} depth {} exceeds bus max_depth {}",
                    spec.depth,
                    self.max_depth
                );
            }
        }
        let mut set: JoinSet<(usize, String, ChildOutcome)> = JoinSet::new();
        for (i, spec) in specs.into_iter().enumerate() {
            let handle = self.spawn(spec, runner.clone(), transcript.clone()).await?;
            set.spawn(async move {
                let id = handle.id().to_string();
                let outcome = handle.join().await;
                (i, id, outcome)
            });
        }
        let mut out = Vec::new();
        while let Some(joined) = set.join_next().await {
            let (i, id, outcome) =
                joined.map_err(|e| anyhow::anyhow!("fan-out task ended without outcome: {e}"))?;
            out.push((i, id, outcome));
        }
        out.sort_by_key(|(i, _, _)| *i);
        Ok(out.into_iter().map(|(_, id, o)| (id, o)).collect())
    }
}

/// One child's drive loop: turns until done / budget / stall / cancel.
async fn drive(
    id: String,
    spec: SpawnSpec,
    runner: Arc<dyn TurnRunner>,
    transcript: Option<Transcript>,
    cancel: CancellationToken,
    steer_rx: mpsc::UnboundedReceiver<String>,
) -> ChildOutcome {
    let t0 = Instant::now();
    let finish = |result: TypedResult, turns_used: u32, tool_calls: u64| ChildOutcome {
        id: id.clone(),
        result,
        turns_used,
        tool_calls,
        wall_secs: t0.elapsed().as_secs_f64(),
        kept: None,
    };

    // Workdir setup (async, inside the task — spawn never blocks on git).
    // The live guard rides into drive_in: auto-removed at every exit, or
    // handed back as `kept` when `keep_worktree` is set (merge-back).
    // `repo` is captured for the kept handoff (`spec` moves into drive_in).
    let repo_opt: Option<PathBuf> = match &spec.workdir {
        WorkdirSpec::InPlace(_) => None,
        WorkdirSpec::Worktree { repo, .. } => Some(repo.clone()),
    };
    // Only Branch trees are committable; Detached keeps hand back
    // uncommitted (read-only children — merge_back refuses them).
    // (Branch-ness is re-derived from the guard at handoff time.)
    let (workdir, guard): (PathBuf, Option<worktree::Worktree>) = match &spec.workdir {
        WorkdirSpec::InPlace(p) => (p.clone(), None),
        WorkdirSpec::Worktree { repo, parent, mode } => {
            match worktree::create(repo, parent, &id, mode).await {
                Ok(w) => (w.path().to_path_buf(), Some(w)),
                Err(e) => {
                    return finish(
                        TypedResult::failed(format!("worktree setup failed: {e}")),
                        0,
                        0,
                    )
                }
            }
        }
    };
    let keep = spec.keep_worktree && guard.is_some();
    let (mut outcome, guard) = drive_in(
        id.clone(),
        spec,
        runner,
        transcript,
        cancel,
        steer_rx,
        workdir,
        guard,
        keep,
        t0,
    )
    .await;
    // Kept handoff: commit Branch+Completed trees deterministically,
    // then hand path/branch/repo to the caller. Commit trouble warns and
    // flags `committed: false` (merge_back refuses dirty trees) — the
    // Completed result itself is never rewritten.
    if let (true, Some(w)) = (keep, guard) {
        let path = w.path().to_path_buf();
        let branch = w.branch().map(str::to_string);
        let mut committed = false;
        if outcome.result.is_completed() && branch.is_some() {
            match worktree::commit_all(&path, &format!("polymorph {id} done")).await {
                Ok(c) => committed = c,
                Err(e) => tracing::warn!(child = id, "keep commit failed: {e:#}"),
            }
        }
        std::mem::forget(w); // caller-owned now (merge_back or discard).
        if let Some(repo) = repo_opt {
            outcome.kept = Some(worktree::KeptWorktree::new(path, branch, repo, committed));
        }
    }
    outcome
}

/// Drive-loop body. `workdir` is resolved (in-place path or fresh worktree);
/// `worktree_guard` (Some) is auto-removed at every exit — unless `keep` is
/// set, in which case the guard rides back out for the caller's merge-back
/// (teardown still warns instead of failing; litter is for `prune_orphans`).
#[allow(clippy::too_many_arguments)]
async fn drive_in(
    id: String,
    spec: SpawnSpec,
    runner: Arc<dyn TurnRunner>,
    transcript: Option<Transcript>,
    cancel: CancellationToken,
    mut steer_rx: mpsc::UnboundedReceiver<String>,
    workdir: PathBuf,
    worktree_guard: Option<worktree::Worktree>,
    keep: bool,
    t0: Instant,
) -> (ChildOutcome, Option<worktree::Worktree>) {
    let id_f = id.clone();
    let finish = move |result: TypedResult, turns_used: u32, tool_calls: u64| ChildOutcome {
        id: id_f.clone(),
        result,
        turns_used,
        tool_calls,
        wall_secs: t0.elapsed().as_secs_f64(),
        kept: None,
    };
    // Best-effort teardown at every exit: the outcome is already built, so a
    // failed remove warns (litter for prune_orphans) instead of failing.
    // With `keep`, the guard survives for merge-back instead.
    let id_t = id.clone();
    let teardown = move |guard: Option<worktree::Worktree>| {
        let id_t = id_t.clone();
        async move {
            if keep {
                return guard;
            }
            if let Some(w) = guard {
                let path = w.path().to_path_buf();
                if let Err(e) = w.remove().await {
                    tracing::warn!(child = id_t, path = %path.display(), "worktree auto-remove failed: {e}");
                }
            }
            None
        }
    };

    let role_name = format!("{:?}", spec.role).to_lowercase();
    let mut turns_used: u32 = 0;
    let mut tool_calls: u64 = 0;
    let mut stall: u32 = 0;
    let mut pending_steer: VecDeque<String> = VecDeque::new();

    if spec.budget.max_turns == 0 {
        let outcome = finish(
            TypedResult::stopped(StopReason::BudgetExhausted, "zero-turn budget"),
            0,
            0,
        );
        let guard = teardown(worktree_guard).await;
        return (outcome, guard);
    }

    loop {
        if cancel.is_cancelled() {
            let outcome = finish(
                TypedResult::stopped(StopReason::Cancelled, "cancelled by operator"),
                turns_used,
                tool_calls,
            );
            let guard = teardown(worktree_guard).await;
            return (outcome, guard);
        }
        while let Ok(msg) = steer_rx.try_recv() {
            pending_steer.push_back(msg);
        }
        let steered = !pending_steer.is_empty();
        let steer: Vec<String> = pending_steer.drain(..).collect();
        if steered {
            stall = 0; // external input is progress by definition.
        }
        let ctx = TurnContext {
            child_id: id.clone(),
            role: spec.role,
            brief: spec.brief.clone(),
            workdir: workdir.clone(),
            turn: turns_used + 1,
            steer,
            cancel: cancel.clone(),
        };
        let out = match runner.run_turn(ctx).await {
            Ok(out) => out,
            Err(e) => {
                let outcome = finish(
                    TypedResult::failed(format!("{e:#}")),
                    turns_used,
                    tool_calls,
                );
                let guard = teardown(worktree_guard).await;
                return (outcome, guard);
            }
        };
        turns_used += 1;
        tool_calls += u64::from(out.tool_calls);
        if let Some(t) = &transcript {
            if let Err(e) = t.record(&id, turns_used, &role_name, &out.text).await {
                tracing::warn!(
                    child = id,
                    "transcript record failed (child continues): {e}"
                );
            }
        }
        if out.done {
            let summary = if out.summary.trim().is_empty() {
                out.text.chars().take(200).collect()
            } else {
                out.summary.clone()
            };
            let outcome = finish(
                TypedResult {
                    status: crate::result::FinishStatus::Completed,
                    summary,
                    data: out.data.unwrap_or(serde_json::Value::Null),
                },
                turns_used,
                tool_calls,
            );
            let guard = teardown(worktree_guard).await;
            return (outcome, guard);
        }
        if out.progress() {
            stall = 0;
        } else {
            stall += 1;
        }
        if tool_calls > u64::from(spec.budget.max_tool_calls) {
            let outcome = finish(
                TypedResult::stopped(
                    StopReason::ToolBudgetExhausted,
                    format!(
                        "{tool_calls} tool calls over budget {}",
                        spec.budget.max_tool_calls
                    ),
                ),
                turns_used,
                tool_calls,
            );
            let guard = teardown(worktree_guard).await;
            return (outcome, guard);
        }
        if turns_used >= spec.budget.max_turns {
            let outcome = finish(
                TypedResult::stopped(
                    StopReason::BudgetExhausted,
                    format!("{} turns over budget {}", turns_used, spec.budget.max_turns),
                ),
                turns_used,
                tool_calls,
            );
            let guard = teardown(worktree_guard).await;
            return (outcome, guard);
        }
        if stall >= spec.budget.stall_turns {
            let outcome = finish(
                TypedResult::stopped(
                    StopReason::Stalled,
                    format!("no progress for {stall} turns"),
                ),
                turns_used,
                tool_calls,
            );
            let guard = teardown(worktree_guard).await;
            return (outcome, guard);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::result::validate_data;

    fn test_spec(brief: &str) -> SpawnSpec {
        SpawnSpec::new(
            polymorph_core::Role::Default,
            brief,
            WorkdirSpec::InPlace(std::env::temp_dir()),
        )
    }

    /// Runner that never finishes and produces no progress — but observes
    /// cancel (the contract F's real runner must follow).
    struct StallForever;
    #[async_trait]
    impl TurnRunner for StallForever {
        async fn run_turn(&self, ctx: TurnContext) -> anyhow::Result<TurnOutput> {
            ctx.cancel.cancelled().await;
            Ok(TurnOutput {
                text: String::new(),
                summary: String::new(),
                tool_calls: 0,
                done: false,
                data: None,
            })
        }
    }

    struct FailRunner(&'static str);
    #[async_trait]
    impl TurnRunner for FailRunner {
        async fn run_turn(&self, _ctx: TurnContext) -> anyhow::Result<TurnOutput> {
            anyhow::bail!("{}", self.0)
        }
    }

    struct EmptyRunner;
    #[async_trait]
    impl TurnRunner for EmptyRunner {
        async fn run_turn(&self, ctx: TurnContext) -> anyhow::Result<TurnOutput> {
            let _ = ctx.turn;
            Ok(TurnOutput {
                text: String::new(),
                summary: String::new(),
                tool_calls: 0,
                done: false,
                data: None,
            })
        }
    }

    struct ToolSpender(u32);
    #[async_trait]
    impl TurnRunner for ToolSpender {
        async fn run_turn(&self, ctx: TurnContext) -> anyhow::Result<TurnOutput> {
            let _ = ctx.turn;
            Ok(TurnOutput {
                text: "working".into(),
                summary: String::new(),
                tool_calls: self.0,
                done: false,
                data: None,
            })
        }
    }

    #[derive(Default)]
    struct RecordingSink {
        lines: Mutex<Vec<(String, u32, String)>>,
    }
    #[async_trait]
    impl TranscriptSink for RecordingSink {
        async fn record(
            &self,
            child: &str,
            turn: u32,
            role: &str,
            text: &str,
        ) -> anyhow::Result<()> {
            self.lines
                .lock()
                .await
                .push((child.into(), turn, format!("{role}:{text}")));
            Ok(())
        }
    }

    #[tokio::test]
    async fn spawn_done_reports_typed_result() {
        let bus = Bus::flat();
        let runner = Arc::new(EchoRunner::new(2, "did the thing"));
        let rec = Arc::new(RecordingSink::default());
        let sink: Transcript = rec.clone();
        let h = bus
            .spawn(test_spec("brief-a"), runner, Some(sink))
            .await
            .unwrap();
        let id = h.id().to_string();
        let out = h.join().await;
        assert_eq!(out.id, id);
        assert!(out.result.is_completed());
        assert_eq!(out.result.summary, "did the thing");
        assert_eq!(out.turns_used, 2);
        assert!(out.wall_secs >= 0.0);
        validate_data(&out.result.data, &["echo"]).unwrap();
        let lines = rec.lines.lock().await;
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].1, 1);
        assert_eq!(lines[1].1, 2);
    }

    #[tokio::test]
    async fn steer_is_delivered_next_turn() {
        let bus = Bus::flat();
        let runner = Arc::new(EchoRunner::new(3, "steered"));
        let h = bus
            .spawn(test_spec("brief-b"), runner.clone(), None)
            .await
            .unwrap();
        h.steer("go left").unwrap();
        let out = h.join().await;
        assert!(out.result.is_completed());
        assert_eq!(runner.seen_steer().await, vec!["go left".to_string()]);
    }

    #[tokio::test]
    async fn stop_cancels_a_stuck_child() {
        let bus = Bus::flat();
        let h = bus
            .spawn(test_spec("stuck"), Arc::new(StallForever), None)
            .await
            .unwrap();
        h.stop();
        let out = h.join().await;
        assert!(matches!(
            out.result.status,
            FinishStatus::Stopped {
                reason: StopReason::Cancelled
            }
        ));
    }

    #[tokio::test]
    async fn turn_budget_exhausts_honestly() {
        let bus = Bus::flat();
        let mut spec = test_spec("long");
        spec.budget = Budget {
            max_turns: 3,
            max_tool_calls: 1000,
            stall_turns: 100,
        };
        let out = bus
            .spawn(spec, Arc::new(EchoRunner::new(100, "never")), None)
            .await
            .unwrap()
            .join()
            .await;
        assert!(matches!(
            out.result.status,
            FinishStatus::Stopped {
                reason: StopReason::BudgetExhausted
            }
        ));
        assert_eq!(out.turns_used, 3);
    }

    #[tokio::test]
    async fn stall_and_tool_budgets_fire() {
        let bus = Bus::flat();
        let mut spec = test_spec("quiet");
        spec.budget = Budget {
            max_turns: 100,
            max_tool_calls: 1000,
            stall_turns: 2,
        };
        let out = bus
            .spawn(spec, Arc::new(EmptyRunner), None)
            .await
            .unwrap()
            .join()
            .await;
        assert!(matches!(
            out.result.status,
            FinishStatus::Stopped {
                reason: StopReason::Stalled
            }
        ));
        assert_eq!(out.turns_used, 2);

        let mut spec = test_spec("spendy");
        spec.budget = Budget {
            max_turns: 100,
            max_tool_calls: 15,
            stall_turns: 100,
        };
        let out = bus
            .spawn(spec, Arc::new(ToolSpender(10)), None)
            .await
            .unwrap()
            .join()
            .await;
        assert!(matches!(
            out.result.status,
            FinishStatus::Stopped {
                reason: StopReason::ToolBudgetExhausted
            }
        ));
        // 10 + 10 = 20 > 15 after the second turn.
        assert_eq!(out.turns_used, 2);
    }

    #[tokio::test]
    async fn runner_errors_become_failed_not_panics() {
        let bus = Bus::flat();
        let out = bus
            .spawn(test_spec("doomed"), Arc::new(FailRunner("kaput")), None)
            .await
            .unwrap()
            .join()
            .await;
        assert!(matches!(out.result.status, FinishStatus::Failed { .. }));
    }

    #[tokio::test]
    async fn depth_over_max_is_refused() {
        let bus = Bus::flat();
        let mut spec = test_spec("deep");
        spec.depth = 2;
        // unwrap_err needs T: Debug (ChildHandle isn't) — match instead.
        let err = match bus.spawn(spec, Arc::new(EmptyRunner), None).await {
            Ok(_) => panic!("depth-2 spawn must be refused"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("max_depth"));
        // And fan-out fails fast before spawning anything.
        let mut ok_spec = test_spec("ok");
        ok_spec.depth = 1;
        let mut deep = test_spec("deep");
        deep.depth = 2;
        assert!(bus
            .fan_out(vec![ok_spec, deep], Arc::new(EmptyRunner), None)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn fan_out_collects_mixed_outcomes_in_spawn_order() {
        let bus = Bus::flat();
        let specs = vec![test_spec("one"), test_spec("two"), test_spec("three")];
        // Mixed runners need per-child runners — fan_out takes one runner, so
        // use a dispatching runner keyed by brief (test-only seam).
        struct Dispatch;
        #[async_trait]
        impl TurnRunner for Dispatch {
            async fn run_turn(&self, ctx: TurnContext) -> anyhow::Result<TurnOutput> {
                match ctx.brief.as_str() {
                    "one" => Ok(TurnOutput {
                        text: "t1".into(),
                        summary: "first".into(),
                        tool_calls: 0,
                        done: true,
                        data: Some(serde_json::json!({"n": 1})),
                    }),
                    "two" => anyhow::bail!("second child blew up"),
                    _ => {
                        if ctx.turn >= 2 {
                            Ok(TurnOutput {
                                text: "slow".into(),
                                summary: "third".into(),
                                tool_calls: 0,
                                done: true,
                                data: Some(serde_json::json!({"n": 3})),
                            })
                        } else {
                            Ok(TurnOutput {
                                text: "warming".into(),
                                summary: String::new(),
                                tool_calls: 0,
                                done: false,
                                data: None,
                            })
                        }
                    }
                }
            }
        }
        let out = bus.fan_out(specs, Arc::new(Dispatch), None).await.unwrap();
        assert_eq!(out.len(), 3);
        // Spawn order preserved despite completion order differing.
        let briefs: Vec<&str> = out
            .iter()
            .map(|(_, o)| match &o.result.data {
                v if v.get("n") == Some(&serde_json::json!(1)) => "one",
                _ if matches!(o.result.status, FinishStatus::Failed { .. }) => "two",
                _ => "three",
            })
            .collect();
        assert_eq!(briefs, vec!["one", "two", "three"]);
        assert!(out[0].1.result.is_completed());
        assert!(matches!(
            out[1].1.result.status,
            FinishStatus::Failed { .. }
        ));
        assert!(out[2].1.result.is_completed());
    }

    #[tokio::test]
    async fn zero_turn_budget_stops_without_running() {
        let bus = Bus::flat();
        let mut spec = test_spec("none");
        spec.budget = Budget {
            max_turns: 0,
            max_tool_calls: 0,
            stall_turns: 0,
        };
        struct Counting;
        #[async_trait]
        impl TurnRunner for Counting {
            async fn run_turn(&self, _ctx: TurnContext) -> anyhow::Result<TurnOutput> {
                panic!("must never run");
            }
        }
        let out = bus
            .spawn(spec, Arc::new(Counting), None)
            .await
            .unwrap()
            .join()
            .await;
        assert!(matches!(
            out.result.status,
            FinishStatus::Stopped {
                reason: StopReason::BudgetExhausted
            }
        ));
        assert_eq!(out.turns_used, 0);
    }

    #[tokio::test]
    async fn store_transcript_records_ordered_turns() {
        use polymorph_core::sessions::Store;
        let store = Store::open_in_memory().await.unwrap();
        let session = store.create_session("bus-test").await.unwrap();
        let shared = Arc::new(Mutex::new(store));
        let sink: Transcript = Arc::new(StoreTranscript::new(shared.clone(), session.id.clone()));
        let bus = Bus::flat();
        let out = bus
            .spawn(
                test_spec("logged"),
                Arc::new(EchoRunner::new(2, "s")),
                Some(sink),
            )
            .await
            .unwrap()
            .join()
            .await;
        assert!(out.result.is_completed());
        // Same store handle reads back both turns, in order.
        let history = shared.lock().await.history(&session.id, 10).await.unwrap();
        assert_eq!(history.len(), 2);
        assert!(history[0].content.contains("t1"));
        assert!(history[1].content.contains("t2"));
        assert!(history[0].content.contains(&out.id));
    }

    /// Fixture git repo (sync setup; mirrors worktree.rs init_repo +
    /// byte-deterministic autocrlf). Local-only, tempdir-owned.
    fn init_repo(dir: &std::path::Path) -> std::path::PathBuf {
        use std::process::{Command, Stdio};
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let run = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(&repo)
                    .args(args)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .unwrap()
                    .success(),
                "{args:?}"
            );
        };
        run(&["init", "-b", "main"]);
        run(&["config", "core.autocrlf", "false"]);
        std::fs::write(repo.join("f.txt"), "v1\n").unwrap();
        run(&["add", "."]);
        run(&[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-m",
            "seed",
            "--quiet",
        ]);
        repo
    }

    /// Runner that writes one file into its workdir, then reports done.
    struct FileWriter;
    #[async_trait]
    impl TurnRunner for FileWriter {
        async fn run_turn(&self, ctx: TurnContext) -> anyhow::Result<TurnOutput> {
            std::fs::write(ctx.workdir.join("out.txt"), "work\n").unwrap();
            Ok(TurnOutput {
                text: "wrote".into(),
                summary: "wrote out.txt".into(),
                tool_calls: 0,
                done: true,
                data: None,
            })
        }
    }

    fn branch_spec(repo: std::path::PathBuf, parent: std::path::PathBuf, keep: bool) -> SpawnSpec {
        let mut spec = SpawnSpec::new(
            polymorph_core::Role::Act,
            "write out.txt",
            WorkdirSpec::Worktree {
                repo,
                parent,
                mode: crate::worktree::WorktreeMode::Branch(None),
            },
        );
        spec.keep_worktree = keep;
        spec
    }

    #[tokio::test]
    async fn keep_branch_hands_back_committed_tree() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path());
        let parent = dir.path().join("wt");
        std::fs::create_dir_all(&parent).unwrap();
        let bus = Bus::flat();
        let out = bus
            .spawn(
                branch_spec(repo.clone(), parent.clone(), true),
                Arc::new(FileWriter),
                None,
            )
            .await
            .unwrap()
            .join()
            .await;
        assert!(out.result.is_completed());
        let kept = out.kept.expect("keep must hand back the tree");
        assert!(kept.branch().is_some_and(|b| b.starts_with("polymorph/")));
        assert!(kept.committed());
        assert_eq!(kept.repo(), repo.as_path());
        // Tree survives on disk with the work (nothing auto-removed).
        assert_eq!(
            std::fs::read_to_string(kept.path().join("out.txt")).unwrap(),
            "work\n"
        );
        // Caller-owned cleanup works through discard.
        kept.discard().await.unwrap();
    }

    #[tokio::test]
    async fn default_branch_still_auto_removes_with_no_kept() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path());
        let parent = dir.path().join("wt");
        std::fs::create_dir_all(&parent).unwrap();
        let bus = Bus::flat();
        let out = bus
            .spawn(
                branch_spec(repo, parent.clone(), false),
                Arc::new(FileWriter),
                None,
            )
            .await
            .unwrap()
            .join()
            .await;
        assert!(out.result.is_completed());
        assert!(out.kept.is_none());
        // Worktree gone (only the parent dir remains, empty).
        assert!(parent.read_dir().unwrap().next().is_none());
    }

    #[tokio::test]
    async fn keep_detached_hands_back_uncommitted_branchless() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path());
        let parent = dir.path().join("wt");
        std::fs::create_dir_all(&parent).unwrap();
        let mut spec = SpawnSpec::new(
            polymorph_core::Role::Review,
            "look",
            WorkdirSpec::Worktree {
                repo,
                parent: parent.clone(),
                mode: crate::worktree::WorktreeMode::Detached,
            },
        );
        spec.keep_worktree = true;
        let bus = Bus::flat();
        let out = bus
            .spawn(spec, Arc::new(FileWriter), None)
            .await
            .unwrap()
            .join()
            .await;
        assert!(out.result.is_completed());
        let kept = out.kept.expect("keep must hand back the tree");
        assert!(kept.branch().is_none());
        assert!(!kept.committed());
        kept.discard().await.unwrap();
    }

    #[tokio::test]
    async fn keep_failed_child_hands_back_uncommitted() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path());
        let parent = dir.path().join("wt");
        std::fs::create_dir_all(&parent).unwrap();
        let bus = Bus::flat();
        let out = bus
            .spawn(
                branch_spec(repo, parent, true),
                Arc::new(FailRunner("nope")),
                None,
            )
            .await
            .unwrap()
            .join()
            .await;
        assert!(!out.result.is_completed());
        let kept = out
            .kept
            .expect("keep must hand back the tree even on failure");
        assert!(!kept.committed());
        kept.discard().await.unwrap();
    }
}
