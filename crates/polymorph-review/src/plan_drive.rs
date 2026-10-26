//! Plan-mode DAG driver.
//!
//! Drives [`run_loop`] per ready task in
//! dependency order over PLAN.md sections, marks tasks Done/Failed, and
//! exposes the merge sequence. `run_loop` itself is untouched
//! (the core loop never changes for a consumer).
//!
//! Policies (explicit, tested):
//! - One task per drive step: snapshot [`TaskGraph::ready`], drive each in
//!   order, recompute. Every step consumes exactly one Pending task, so the
//!   drive always terminates in ≤ N steps.
//! - Escalated tasks mark Failed and the drive CONTINUES with siblings
//!   (independent work proceeds); dependents stay blocked structurally
//!   (their deps never all reach Done) and land in `unfinished`.
//! - [`drive_plan`] ignores executor rewrites (contract, locked by
//!   test); [`drive_plan_with_writeback`] applies them via
//!   [`apply_section`](polymorph_tasks::plan::apply_section) — strict shape,
//!   atomic, status never applied (the verdict owns lifecycle). Apply
//!   failures keep the old text and mark the task Failed (never
//!   half-applied, never guessed).
//! - [`PlanDriveReport::merge_sequence`] orders Done tasks for merging; the
//!   MERGER stops at the first conflict (no partial merges) —
//!   stopping is caller policy, ordering is this driver's.

use async_trait::async_trait;
use std::sync::Mutex;

use crate::{run_loop, Executor, ExecutorInput, LoopMode, LoopReport, Outcome, Reviewer};
use polymorph_tasks::{apply_section, render_section, TaskGraph, TaskStatus};

/// One task's convergence transcript, in drive order.
#[derive(Debug, Clone)]
pub struct PlanTaskReport {
    pub id: String,
    pub report: LoopReport,
    /// Write-back fault text when the executor's rewrite failed to apply
    /// (task marked Failed, old text kept). None when write-back is off or
    /// applied clean — absence is data, not an error.
    pub writeback_error: Option<String>,
}

/// Whole-drive outcome: per-task transcripts + merge order + leftovers.
#[derive(Debug, Clone)]
pub struct PlanDriveReport {
    /// Per-task reports in the order driven (dependency order).
    pub tasks: Vec<PlanTaskReport>,
    /// Done-task ids in dependency order for merging (empty when nothing
    /// completed). The merger stops at the first conflict — never partial.
    pub merge_sequence: Vec<String>,
    /// Tasks still Pending/Running/Failed at the end (blocked or failed).
    pub unfinished: Vec<String>,
    /// The graph with updated statuses (checkpoint-ready).
    pub graph: TaskGraph,
}

/// Drive a plan graph to quiescence: while tasks are ready, review each
/// task's PLAN.md section and mark it Done (Passed) or Failed (Escalated).
/// Hand-built graphs are validated first (fail-fast — cycles from disk die
/// here, never mid-drive). Executor rewrites are IGNORED (see
/// [`drive_plan_with_writeback`] for the applying twin).
pub async fn drive_plan<R: Reviewer, E: Executor>(
    cfg: &polymorph_core::LoopConfig,
    plan_title: &str,
    graph: TaskGraph,
    reviewer: &R,
    executor: &E,
) -> anyhow::Result<PlanDriveReport> {
    drive_inner(
        cfg,
        plan_title,
        graph,
        reviewer,
        LastWrite::new(executor),
        false,
    )
    .await
}

/// Drive like [`drive_plan`], but apply each Passed task's final executor
/// rewrite back into its section ([`apply_section`](polymorph_tasks::plan::apply_section):
/// strict shape, atomic, title/criteria/deps only — never status). A task
/// that passes review but whose rewrite fails to apply marks Failed with
/// the fault in [`PlanTaskReport::writeback_error`] and the old text kept.
/// A task that passes on the first review (executor never ran) has no
/// rewrite — nothing to apply, still Done.
pub async fn drive_plan_with_writeback<R: Reviewer, E: Executor>(
    cfg: &polymorph_core::LoopConfig,
    plan_title: &str,
    graph: TaskGraph,
    reviewer: &R,
    executor: &E,
) -> anyhow::Result<PlanDriveReport> {
    drive_inner(
        cfg,
        plan_title,
        graph,
        reviewer,
        LastWrite::new(executor),
        true,
    )
    .await
}

/// Display words for one driven task: derived from the FINAL graph status,
/// not the loop outcome — a task that passes review but whose rewrite is
/// refused ends Failed, and the board must say so (live catch:
/// printing the loop verdict here reported Done for a Failed task). Pure
/// fn, unit-tested (callers print these verbatim).
pub fn task_words(status: TaskStatus, outcome: &Outcome) -> String {
    match (status, outcome) {
        (TaskStatus::Done, Outcome::Passed { rounds }) => format!("Done-{rounds}"),
        (_, Outcome::Escalated { reason, rounds }) => {
            format!("Failed-{reason:?}-{rounds}")
        }
        // Passed review but the rewrite failed to apply (write-back
        // refusal): Failed with the round count, detail rides alongside.
        (_, Outcome::Passed { rounds }) => format!("Failed-writeback-{rounds}"),
    }
}

/// Recording decorator: remembers the last Ok executor text so the driver
/// can write it back after a Passed verdict. Transparent otherwise.
struct LastWrite<E> {
    inner: E,
    last: Mutex<Option<String>>,
}

impl<E> LastWrite<E> {
    fn new(inner: E) -> Self {
        Self {
            inner,
            last: Mutex::new(None),
        }
    }
}

#[async_trait]
#[async_trait]
impl<E: Executor> Executor for LastWrite<E> {
    async fn execute(&self, input: ExecutorInput) -> anyhow::Result<String> {
        let text = self.inner.execute(input).await?;
        *self.last.lock().unwrap() = Some(text.clone());
        Ok(text)
    }
}

#[async_trait]
impl<E: Executor> Executor for &E {
    async fn execute(&self, input: ExecutorInput) -> anyhow::Result<String> {
        (*self).execute(input).await
    }
}

async fn drive_inner<R: Reviewer, E: Executor>(
    cfg: &polymorph_core::LoopConfig,
    plan_title: &str,
    mut graph: TaskGraph,
    reviewer: &R,
    recorder: LastWrite<E>,
    writeback: bool,
) -> anyhow::Result<PlanDriveReport> {
    graph
        .validate()
        .map_err(|e| anyhow::anyhow!("drive_plan: invalid graph: {e}"))?;
    let mut tasks = Vec::new();
    loop {
        let ready: Vec<String> = graph.ready().iter().map(|t| t.id.clone()).collect();
        if ready.is_empty() {
            break;
        }
        for id in ready {
            let task = graph
                .get(&id)
                .expect("ready() ids come from the graph")
                .clone();
            graph
                .set_status(&id, TaskStatus::Running)
                .expect("known id");
            // Clear any stale recording: only this task's final rewrite may
            // apply (an ExecutorFailed round must never donate older text).
            *recorder.last.lock().unwrap() = None;
            let report = run_loop(
                cfg,
                &format!("{plan_title} — task {}: {}", task.id, task.title),
                &task.criteria,
                &render_section(&task),
                LoopMode::Plan,
                reviewer,
                &recorder,
            )
            .await;
            let mut done = matches!(report.outcome, Outcome::Passed { .. });
            let mut writeback_error = None;
            if writeback && done {
                match recorder.last.lock().unwrap().take() {
                    // Immediate pass: the executor never ran, so no rewrite
                    // exists — nothing to apply, still Done.
                    None => {}
                    Some(text) => {
                        if let Err(e) = apply_section(&mut graph, &id, &text) {
                            done = false;
                            writeback_error = Some(e.to_string());
                        }
                    }
                }
            }
            graph
                .set_status(
                    &id,
                    if done {
                        TaskStatus::Done
                    } else {
                        TaskStatus::Failed
                    },
                )
                .expect("known id, non-terminal source");
            tasks.push(PlanTaskReport {
                id,
                report,
                writeback_error,
            });
        }
    }
    // All Done tasks — including pre-completed ones from a resumed graph,
    // which never drove this session but still merge in order.
    let done_ids: Vec<String> = graph
        .ordered()
        .iter()
        .filter(|t| t.status == TaskStatus::Done)
        .map(|t| t.id.clone())
        .collect();
    // Validated at entry and only statuses changed since — cannot fail, but
    // a merge order that silently dropped work would be worse than an error.
    let merge_sequence = graph.merge_order(&done_ids).unwrap_or_default();
    let unfinished: Vec<String> = graph
        .ordered()
        .iter()
        .filter(|t| t.status != TaskStatus::Done)
        .map(|t| t.id.clone())
        .collect();
    Ok(PlanDriveReport {
        tasks,
        merge_sequence,
        unfinished,
        graph,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ExecutorInput, Outcome, ReviewerError, ReviewerInput};
    use async_trait::async_trait;
    use polymorph_core::{LoopConfig, Verdict, VerdictKind};
    use polymorph_tasks::Task;
    use std::sync::Mutex;

    fn pass() -> Verdict {
        Verdict {
            verdict: VerdictKind::Pass,
            findings: vec![],
            confidence: 0.9,
        }
    }

    fn fail() -> Verdict {
        Verdict {
            verdict: VerdictKind::Fail,
            findings: vec![],
            confidence: 0.8,
        }
    }

    /// Reviewer double: pops one verdict per review; records artifacts.
    struct ScriptReviewer {
        script: Mutex<Vec<Result<Verdict, ReviewerError>>>,
        seen: Mutex<Vec<String>>,
    }

    impl ScriptReviewer {
        fn passes(n: usize) -> Self {
            Self {
                script: Mutex::new(vec![Ok(pass()); n]),
                seen: Mutex::new(Vec::new()),
            }
        }

        /// Fail once (driving exactly one fix turn) then pass.
        fn fail_once_then_pass() -> Self {
            Self {
                script: Mutex::new(vec![Ok(pass()), Ok(fail())]),
                seen: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl Reviewer for ScriptReviewer {
        async fn review(&self, input: ReviewerInput) -> Result<Verdict, ReviewerError> {
            self.seen.lock().unwrap().push(input.artifact.clone());
            self.script.lock().unwrap().pop().expect("script exhausted")
        }
    }

    /// Executor double: echoes the artifact (no rewrite); counts calls.
    struct EchoExecutor {
        calls: Mutex<u32>,
    }

    #[async_trait]
    impl Executor for EchoExecutor {
        async fn execute(&self, input: ExecutorInput) -> anyhow::Result<String> {
            *self.calls.lock().unwrap() += 1;
            Ok(input.artifact)
        }
    }

    fn task(id: &str, deps: &[&str]) -> Task {
        Task::new(
            id,
            format!("title {id}"),
            format!("criteria {id}"),
            deps.iter().map(|s| s.to_string()).collect(),
        )
    }

    fn cfg() -> LoopConfig {
        LoopConfig {
            max_iterations: 2,
            ..Default::default()
        }
    }

    fn chain() -> TaskGraph {
        let mut g = TaskGraph::new();
        g.add_task(task("a", &[])).unwrap();
        g.add_task(task("b", &["a"])).unwrap();
        g.add_task(task("c", &["b"])).unwrap();
        g
    }

    #[tokio::test]
    async fn chain_drives_in_order_all_done() {
        let reviewer = ScriptReviewer::passes(3);
        let executor = EchoExecutor {
            calls: Mutex::new(0),
        };
        let rep = drive_plan(&cfg(), "goal", chain(), &reviewer, &executor)
            .await
            .unwrap();
        assert_eq!(
            rep.tasks.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            vec!["a", "b", "c"]
        );
        assert!(rep
            .tasks
            .iter()
            .all(|t| matches!(t.report.outcome, Outcome::Passed { .. })));
        assert_eq!(rep.merge_sequence, vec!["a", "b", "c"]);
        assert!(rep.unfinished.is_empty());
        assert!(rep
            .graph
            .ordered()
            .iter()
            .all(|t| t.status == TaskStatus::Done));
        // Every reviewer saw a PLAN.md section for its own task.
        let seen = reviewer.seen.lock().unwrap();
        assert_eq!(seen.len(), 3);
        assert!(seen[0].contains("## [a]"), "{}", seen[0]);
        assert!(seen[1].contains("## [b]"), "{}", seen[1]);
        assert!(seen[2].contains("## [c]"), "{}", seen[2]);
    }

    #[tokio::test]
    async fn failed_task_blocks_dependents_siblings_continue() {
        // Diamond a -> {b, c} -> d; b fails twice (cap 2 → Escalated).
        let mut g = TaskGraph::new();
        for (id, deps) in [
            ("a", &[][..]),
            ("b", &["a"][..]),
            ("c", &["a"][..]),
            ("d", &["b", "c"][..]),
        ] {
            g.add_task(task(id, deps)).unwrap();
        }
        let reviewer = ScriptReviewer {
            // a passes; b fails every round (2 rounds × cap); c passes.
            script: Mutex::new(
                vec![Ok(pass()), Ok(fail()), Ok(fail()), Ok(pass())]
                    .into_iter()
                    .rev()
                    .collect(),
            ),
            seen: Mutex::new(Vec::new()),
        };
        let executor = EchoExecutor {
            calls: Mutex::new(0),
        };
        let rep = drive_plan(&cfg(), "goal", g, &reviewer, &executor)
            .await
            .unwrap();
        // a Done, b Failed, c Done (sibling continued), d Pending (blocked).
        assert_eq!(rep.graph.get("a").unwrap().status, TaskStatus::Done);
        assert_eq!(rep.graph.get("b").unwrap().status, TaskStatus::Failed);
        assert_eq!(rep.graph.get("c").unwrap().status, TaskStatus::Done);
        assert_eq!(rep.graph.get("d").unwrap().status, TaskStatus::Pending);
        assert_eq!(rep.unfinished, vec!["b".to_string(), "d".to_string()]);
        // Merge order covers Done tasks only, in dependency order.
        assert_eq!(rep.merge_sequence, vec!["a".to_string(), "c".to_string()]);
    }

    #[tokio::test]
    async fn invalid_reviewer_fails_task_without_hang() {
        let reviewer = ScriptReviewer {
            script: Mutex::new(vec![Err(ReviewerError::Invalid("prose".into())); 2]),
            seen: Mutex::new(Vec::new()),
        };
        let executor = EchoExecutor {
            calls: Mutex::new(0),
        };
        // max 1: first Invalid is final → Escalated → Failed; b blocked.
        let one = LoopConfig {
            max_iterations: 1,
            ..Default::default()
        };
        let rep = drive_plan(&one, "goal", chain(), &reviewer, &executor)
            .await
            .unwrap();
        assert_eq!(rep.tasks.len(), 1);
        assert_eq!(rep.graph.get("a").unwrap().status, TaskStatus::Failed);
        assert_eq!(rep.graph.get("b").unwrap().status, TaskStatus::Pending);
        assert!(rep.merge_sequence.is_empty());
    }

    #[tokio::test]
    async fn done_tasks_never_redriven() {
        let mut g = chain();
        g.set_status("a", TaskStatus::Done).unwrap();
        let reviewer = ScriptReviewer::passes(2);
        let executor = EchoExecutor {
            calls: Mutex::new(0),
        };
        let rep = drive_plan(&cfg(), "goal", g, &reviewer, &executor)
            .await
            .unwrap();
        assert_eq!(
            rep.tasks.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            vec!["b", "c"]
        );
        // No executor call for `a` (it passed with zero rounds each here —
        // reviewer passed immediately, so calls stay 0 throughout).
        assert_eq!(*executor.calls.lock().unwrap(), 0);
        assert_eq!(rep.merge_sequence, vec!["a", "b", "c"]);
    }

    #[tokio::test]
    async fn empty_graph_drives_nothing() {
        let reviewer = ScriptReviewer::passes(0);
        let executor = EchoExecutor {
            calls: Mutex::new(0),
        };
        let rep = drive_plan(&cfg(), "goal", TaskGraph::new(), &reviewer, &executor)
            .await
            .unwrap();
        assert!(rep.tasks.is_empty());
        assert!(rep.merge_sequence.is_empty());
        assert!(rep.unfinished.is_empty());
    }

    #[tokio::test]
    async fn cyclic_graph_fails_before_driving() {
        // Cycles are unconstructible via add_task (forward refs refused),
        // so they arrive through whole-graph input — raw JSON here (PLAN.md
        // parse refuses them the same way, tested in the tasks crate).
        let g: TaskGraph = serde_json::from_str(
            r#"{"tasks":[
                {"id":"a","title":"t","criteria":"c","status":"pending","deps":["b"]},
                {"id":"b","title":"t","criteria":"c","status":"pending","deps":["a"]}
            ]}"#,
        )
        .unwrap();
        let reviewer = ScriptReviewer::passes(10);
        let executor = EchoExecutor {
            calls: Mutex::new(0),
        };
        let err = drive_plan(&cfg(), "goal", g, &reviewer, &executor)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("cycle"), "{err}");
        // Nothing drove (reviewer never consulted).
        assert!(reviewer.seen.lock().unwrap().is_empty());
    }

    /// Executor double: always replies with one fixed rewrite fragment.
    struct RewriteExecutor {
        reply: String,
        calls: Mutex<u32>,
    }

    #[async_trait]
    impl Executor for RewriteExecutor {
        async fn execute(&self, _input: ExecutorInput) -> anyhow::Result<String> {
            *self.calls.lock().unwrap() += 1;
            Ok(self.reply.clone())
        }
    }

    fn rewrite_for(id: &str, title: &str, criteria: &str) -> String {
        format!("## [{id}] {title}\nStatus: pending\nDeps: none\nCriteria:\n{criteria}\n")
    }

    #[tokio::test]
    async fn writeback_applies_rewrite_on_pass() {
        let mut g = TaskGraph::new();
        g.add_task(task("a", &[])).unwrap();
        let reviewer = ScriptReviewer::fail_once_then_pass();
        let executor = RewriteExecutor {
            reply: rewrite_for("a", "A, revised", "new words"),
            calls: Mutex::new(0),
        };
        let rep = drive_plan_with_writeback(&cfg(), "goal", g, &reviewer, &executor)
            .await
            .unwrap();
        assert_eq!(*executor.calls.lock().unwrap(), 1);
        assert_eq!(rep.graph.get("a").unwrap().title, "A, revised");
        assert_eq!(rep.graph.get("a").unwrap().criteria, "new words\n");
        assert_eq!(rep.graph.get("a").unwrap().status, TaskStatus::Done);
        assert_eq!(rep.tasks[0].writeback_error, None);
        assert_eq!(rep.merge_sequence, vec!["a".to_string()]);
    }

    #[tokio::test]
    async fn immediate_pass_has_nothing_to_apply() {
        // Reviewer passes round one: the executor never ran, so no rewrite
        // exists — still Done, still no error (absence is data).
        let mut g = TaskGraph::new();
        g.add_task(task("a", &[])).unwrap();
        let reviewer = ScriptReviewer::passes(1);
        let executor = RewriteExecutor {
            reply: rewrite_for("a", "A, revised", "new words"),
            calls: Mutex::new(0),
        };
        let rep = drive_plan_with_writeback(&cfg(), "goal", g, &reviewer, &executor)
            .await
            .unwrap();
        assert_eq!(*executor.calls.lock().unwrap(), 0);
        assert_eq!(rep.graph.get("a").unwrap().title, "title a");
        assert_eq!(rep.graph.get("a").unwrap().status, TaskStatus::Done);
        assert_eq!(rep.tasks[0].writeback_error, None);
    }

    #[tokio::test]
    async fn task_words_reports_final_status_not_loop_verdict() {
        // Passed + refused rewrite ends Failed — the
        // words must say Failed-writeback, never Done.
        assert_eq!(
            task_words(TaskStatus::Done, &Outcome::Passed { rounds: 2 }),
            "Done-2"
        );
        assert_eq!(
            task_words(TaskStatus::Failed, &Outcome::Passed { rounds: 2 }),
            "Failed-writeback-2"
        );
        assert!(task_words(
            TaskStatus::Failed,
            &Outcome::Escalated {
                reason: crate::EscalateReason::MaxIterations,
                rounds: 3
            }
        )
        .starts_with("Failed-"));
    }

    #[tokio::test]
    async fn writeback_failure_marks_failed_and_keeps_old() {
        let mut g = TaskGraph::new();
        g.add_task(task("a", &[])).unwrap();
        g.add_task(task("b", &["a"])).unwrap();
        let reviewer = ScriptReviewer::fail_once_then_pass();
        // Prose, not a section: strict shape refuses it. (One fix turn runs
        // before the round-2 pass, so the prose reaches the applier.)
        let executor = RewriteExecutor {
            reply: "looks good, ship it".to_string(),
            calls: Mutex::new(0),
        };
        let rep = drive_plan_with_writeback(&cfg(), "goal", g, &reviewer, &executor)
            .await
            .unwrap();
        // Passed review but rewrite refused → Failed, old text kept.
        assert_eq!(*executor.calls.lock().unwrap(), 1);
        assert_eq!(rep.graph.get("a").unwrap().status, TaskStatus::Failed);
        assert_eq!(rep.graph.get("a").unwrap().title, "title a");
        assert!(rep.tasks[0].writeback_error.is_some());
        // Dependent structurally blocked (never drove — total stays 1).
        assert_eq!(rep.graph.get("b").unwrap().status, TaskStatus::Pending);
        assert_eq!(*executor.calls.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn plain_drive_ignores_rewrites() {
        // Contract, locked: without write-back the executor's
        // rewrite never touches the graph, even when well-formed and
        // actually produced (fail-then-pass forces the fix turn to run).
        let mut g = TaskGraph::new();
        g.add_task(task("a", &[])).unwrap();
        let reviewer = ScriptReviewer::fail_once_then_pass();
        let executor = RewriteExecutor {
            reply: rewrite_for("a", "A, revised", "new words"),
            calls: Mutex::new(0),
        };
        let rep = drive_plan(&cfg(), "goal", g, &reviewer, &executor)
            .await
            .unwrap();
        assert_eq!(rep.graph.get("a").unwrap().title, "title a");
        assert_eq!(rep.graph.get("a").unwrap().status, TaskStatus::Done);
        assert_eq!(rep.tasks[0].writeback_error, None);
    }
}
