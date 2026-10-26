//! Closure matrix: seeded reviewer→executor runs over the
//! public [`polymorph_review`] API proving convergence AND every escalation
//! kind terminates with a typed reason. No LLM, no subprocess — scripted
//! fakes through the F1 `Reviewer`/`Executor` seam (offline by construction).

use std::collections::VecDeque;
use std::sync::Mutex;

use polymorph_core::{Finding, LoopConfig, Severity, Verdict, VerdictKind};
use polymorph_review::{
    EscalateReason, Executor, ExecutorInput, LoopMode, Outcome, Reviewer, ReviewerError,
    ReviewerInput, RoundVerdict,
};

fn finding(issue: &str) -> Finding {
    Finding {
        severity: Severity::P1,
        file: "a.rs".into(),
        line: Some(1),
        issue: issue.into(),
        proposed_fix: "fix it".into(),
    }
}

fn pass() -> Verdict {
    Verdict {
        verdict: VerdictKind::Pass,
        findings: Vec::new(),
        confidence: 1.0,
    }
}

fn fail(issues: &[&str]) -> Verdict {
    Verdict {
        verdict: VerdictKind::Fail,
        findings: issues.iter().map(|i| finding(i)).collect(),
        confidence: 0.9,
    }
}

/// Scripted reviewer: queued verdicts (or Invalid errors) served in order.
struct ScriptReviewer {
    script: Mutex<VecDeque<Result<Verdict, String>>>,
    calls: Mutex<u32>,
}

impl ScriptReviewer {
    fn new(script: Vec<Result<Verdict, String>>) -> Self {
        Self {
            script: Mutex::new(script.into()),
            calls: Mutex::new(0),
        }
    }
}

#[async_trait::async_trait]
impl Reviewer for ScriptReviewer {
    async fn review(&self, _input: ReviewerInput) -> Result<Verdict, ReviewerError> {
        *self.calls.lock().unwrap() += 1;
        match self
            .script
            .lock()
            .unwrap()
            .pop_front()
            .expect("script empty")
        {
            Ok(v) => Ok(v),
            Err(d) => Err(ReviewerError::Invalid(d)),
        }
    }
}

/// Scripted executor: queued rewrites served in order; calls counted.
struct ScriptExecutor {
    script: Mutex<VecDeque<String>>,
    calls: Mutex<u32>,
}

impl ScriptExecutor {
    fn new(script: Vec<&str>) -> Self {
        Self {
            script: Mutex::new(script.into_iter().map(str::to_string).collect()),
            calls: Mutex::new(0),
        }
    }
}

#[async_trait::async_trait]
impl Executor for ScriptExecutor {
    async fn execute(&self, _input: ExecutorInput) -> anyhow::Result<String> {
        *self.calls.lock().unwrap() += 1;
        Ok(self
            .script
            .lock()
            .unwrap()
            .pop_front()
            .expect("script empty"))
    }
}

fn cfg(max_iterations: u32) -> LoopConfig {
    LoopConfig {
        max_iterations,
        surviving_finding_escalation: 3,
    }
}

async fn drive(
    cfg: &LoopConfig,
    reviewer: ScriptReviewer,
    executor: ScriptExecutor,
) -> (polymorph_review::LoopReport, u32, u32) {
    let report = polymorph_review::run_loop(
        cfg,
        "goal",
        "criteria",
        "artifact-0",
        LoopMode::Act,
        &reviewer,
        &executor,
    )
    .await;
    let r = *reviewer.calls.lock().unwrap();
    let e = *executor.calls.lock().unwrap();
    (report, r, e)
}

#[tokio::test]
async fn pass_first_try_never_consults_executor() {
    let (report, r, e) = drive(
        &cfg(3),
        ScriptReviewer::new(vec![Ok(pass())]),
        ScriptExecutor::new(vec![]),
    )
    .await;
    assert_eq!(report.outcome, Outcome::Passed { rounds: 1 });
    assert_eq!((r, e), (1, 0));
    assert_eq!(report.rounds_log.len(), 1);
    assert_eq!(report.rounds_log[0].verdict, RoundVerdict::Pass);
    assert!(report.surviving.is_empty());
}

#[tokio::test]
async fn refine_then_pass_converges() {
    let (report, r, e) = drive(
        &cfg(3),
        ScriptReviewer::new(vec![Ok(fail(&["off-by-one"])), Ok(pass())]),
        ScriptExecutor::new(vec!["artifact-1"]),
    )
    .await;
    assert_eq!(report.outcome, Outcome::Passed { rounds: 2 });
    assert_eq!((r, e), (2, 1));
    assert!(report.surviving.is_empty());
}

#[tokio::test]
async fn surviving_finding_escalates_at_three_strikes() {
    let f = || Ok(fail(&["stubborn"]));
    let (report, r, e) = drive(
        &cfg(10),
        ScriptReviewer::new(vec![f(), f(), f(), f()]),
        ScriptExecutor::new(vec!["a1", "a2", "a3", "a4"]),
    )
    .await;
    assert_eq!((r, e), (3, 2), "third strike escalates BEFORE a third fix");
    match &report.outcome {
        Outcome::Escalated {
            reason: EscalateReason::Survivor(found),
            rounds: 3,
        } => assert_eq!(found.issue, "stubborn"),
        other => panic!("expected Survivor@3, got {other:?}"),
    }
    assert_eq!(report.surviving.len(), 1);
}

#[tokio::test]
async fn novel_failures_exhaust_to_max_iterations() {
    let (report, r, e) = drive(
        &cfg(3),
        ScriptReviewer::new(vec![
            Ok(fail(&["f1"])),
            Ok(fail(&["f2"])),
            Ok(fail(&["f3"])),
        ]),
        ScriptExecutor::new(vec!["a1", "a2"]),
    )
    .await;
    assert_eq!((r, e), (3, 2));
    assert_eq!(
        report.outcome,
        Outcome::Escalated {
            reason: EscalateReason::MaxIterations,
            rounds: 3
        }
    );
    assert_eq!(report.surviving.len(), 1);
    assert_eq!(report.surviving[0].issue, "f3");
}

#[tokio::test]
async fn final_round_invalid_escalates_naming_detail() {
    let (report, r, e) = drive(
        &cfg(2),
        ScriptReviewer::new(vec![Ok(fail(&["f1"])), Err("prose-not-json".into())]),
        ScriptExecutor::new(vec!["a1"]),
    )
    .await;
    assert_eq!((r, e), (2, 1));
    assert_eq!(
        report.outcome,
        Outcome::Escalated {
            reason: EscalateReason::FinalInvalid("prose-not-json".into()),
            rounds: 2
        }
    );
    assert_eq!(report.rounds_log[1].verdict, RoundVerdict::Invalid);
}

struct FailingExecutor;

#[async_trait::async_trait]
impl Executor for FailingExecutor {
    async fn execute(&self, _input: ExecutorInput) -> anyhow::Result<String> {
        anyhow::bail!("fixer transport died")
    }
}

#[tokio::test]
async fn executor_failure_escalates_loud() {
    let reviewer = ScriptReviewer::new(vec![Ok(fail(&["f1"]))]);
    let report = polymorph_review::run_loop(
        &cfg(3),
        "goal",
        "criteria",
        "artifact-0",
        LoopMode::Act,
        &reviewer,
        &FailingExecutor,
    )
    .await;
    match &report.outcome {
        Outcome::Escalated {
            reason: EscalateReason::ExecutorFailed(detail),
            rounds: 1,
        } => assert!(detail.contains("fixer transport died"), "{detail}"),
        other => panic!("expected ExecutorFailed@1, got {other:?}"),
    }
}
