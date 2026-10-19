//! Offline A/B harness (Epic F4a).
//!
//! Compares two reviewer/executor arms on the SAME seeded case via
//! [`crate::run_loop`]. LLM-free: arms are injected [`crate::Reviewer`] /
//! [`crate::Executor`] traits (scripted fakes here, live Bus+LLM in F4b).
//! Cost proxy = turns (`rounds_log.len`); token/cost ledger stays TBM-09
//! open and the live-model half of TBM-15 stays open (F4b).

use polymorph_core::LoopConfig;

use crate::r#loop::{run_loop, Executor, LoopMode, LoopReport, Outcome, Reviewer, RoundVerdict};

/// Seeded case: goal + criteria + initial artifact. No models, no network.
#[derive(Debug, Clone)]
pub struct AbCase {
    pub id: String,
    pub goal: String,
    pub criteria: String,
    pub initial: String,
}

impl AbCase {
    pub fn new(id: &str, goal: &str, criteria: &str, initial: &str) -> Self {
        Self {
            id: id.to_string(),
            goal: goal.to_string(),
            criteria: criteria.to_string(),
            initial: initial.to_string(),
        }
    }
}

/// One arm's result. `turns` is the cost proxy (one reviewer round = one
/// turn); `findings_seen` sums per-round finding counts (Fail + Pass
/// advisories; Invalid rounds contribute 0 by construction).
#[derive(Debug, Clone)]
pub struct ArmReport {
    pub arm: String,
    pub report: LoopReport,
    pub turns: u32,
    pub invalid_rounds: u32,
    pub findings_seen: usize,
    pub passed: bool,
}

/// Paired comparison. `delta_rounds` = B turns minus A turns (+ means B
/// took more turns, 0 tied).
#[derive(Debug, Clone)]
pub struct AbReport {
    pub case_id: String,
    pub arm_a: ArmReport,
    pub arm_b: ArmReport,
    pub delta_rounds: i32,
}

fn summarize(arm: &str, report: LoopReport) -> ArmReport {
    let turns = report.rounds_log.len() as u32;
    let invalid_rounds = report
        .rounds_log
        .iter()
        .filter(|r| r.verdict == RoundVerdict::Invalid)
        .count() as u32;
    let findings_seen = report.rounds_log.iter().map(|r| r.findings).sum();
    let passed = matches!(report.outcome, Outcome::Passed { .. });
    ArmReport {
        arm: arm.to_string(),
        report,
        turns,
        invalid_rounds,
        findings_seen,
        passed,
    }
}

/// Run the same case through two arms sequentially (deterministic; no
/// JoinSet — F4b may parallelize live arms later without touching this).
#[allow(clippy::too_many_arguments)]
pub async fn run_ab_pair<R1, E1, R2, E2>(
    cfg: &LoopConfig,
    case: &AbCase,
    mode: LoopMode,
    arm_a_name: &str,
    reviewer_a: &R1,
    executor_a: &E1,
    arm_b_name: &str,
    reviewer_b: &R2,
    executor_b: &E2,
) -> AbReport
where
    R1: Reviewer,
    E1: Executor,
    R2: Reviewer,
    E2: Executor,
{
    let rep_a = run_loop(
        cfg,
        &case.goal,
        &case.criteria,
        &case.initial,
        mode,
        reviewer_a,
        executor_a,
    )
    .await;
    let rep_b = run_loop(
        cfg,
        &case.goal,
        &case.criteria,
        &case.initial,
        mode,
        reviewer_b,
        executor_b,
    )
    .await;
    let arm_a = summarize(arm_a_name, rep_a);
    let arm_b = summarize(arm_b_name, rep_b);
    let delta_rounds = arm_b.turns as i32 - arm_a.turns as i32;
    AbReport {
        case_id: case.id.clone(),
        arm_a,
        arm_b,
        delta_rounds,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ReviewerError, ReviewerInput};
    use async_trait::async_trait;
    use polymorph_core::{Finding, Severity, Verdict, VerdictKind};
    use std::sync::Mutex;

    fn finding(issue: &str) -> Finding {
        Finding {
            severity: Severity::P1,
            file: "a.rs".into(),
            line: Some(1),
            issue: issue.into(),
            proposed_fix: "fix".into(),
        }
    }

    fn pass() -> Verdict {
        Verdict {
            verdict: VerdictKind::Pass,
            findings: vec![],
            confidence: 0.9,
        }
    }

    fn fail(issues: &[&str]) -> Verdict {
        Verdict {
            verdict: VerdictKind::Fail,
            findings: issues.iter().map(|i| finding(i)).collect(),
            confidence: 0.8,
        }
    }

    struct ScriptReviewer {
        script: Mutex<Vec<Result<Verdict, ReviewerError>>>,
    }

    impl ScriptReviewer {
        fn new(script: Vec<Result<Verdict, ReviewerError>>) -> Self {
            Self {
                script: Mutex::new(script.into_iter().rev().collect()),
            }
        }
    }

    #[async_trait]
    impl Reviewer for ScriptReviewer {
        async fn review(&self, _input: ReviewerInput) -> Result<Verdict, ReviewerError> {
            self.script.lock().unwrap().pop().expect("script exhausted")
        }
    }

    struct ScriptExecutor {
        fix_to: String,
    }

    impl ScriptExecutor {
        fn fixing(fix_to: &str) -> Self {
            Self {
                fix_to: fix_to.to_string(),
            }
        }
    }

    #[async_trait]
    impl Executor for ScriptExecutor {
        async fn execute(&self, _input: crate::ExecutorInput) -> anyhow::Result<String> {
            Ok(self.fix_to.clone())
        }
    }

    fn cfg() -> LoopConfig {
        LoopConfig {
            max_iterations: 5,
            surviving_finding_escalation: 3,
        }
    }

    fn case() -> AbCase {
        AbCase::new("seeded-1", "goal", "crit", "v1")
    }

    #[tokio::test]
    async fn both_pass_first_try_ties_at_one_turn() {
        let rep = run_ab_pair(
            &cfg(),
            &case(),
            LoopMode::Act,
            "same",
            &ScriptReviewer::new(vec![Ok(pass())]),
            &ScriptExecutor::fixing("unused"),
            "diverse",
            &ScriptReviewer::new(vec![Ok(pass())]),
            &ScriptExecutor::fixing("unused"),
        )
        .await;
        assert_eq!(rep.case_id, "seeded-1");
        assert!(rep.arm_a.passed && rep.arm_b.passed);
        assert_eq!((rep.arm_a.turns, rep.arm_b.turns), (1, 1));
        assert_eq!(rep.delta_rounds, 0);
        assert_eq!((rep.arm_a.findings_seen, rep.arm_b.findings_seen), (0, 0));
    }

    #[tokio::test]
    async fn fail_once_costs_one_extra_turn() {
        let rep = run_ab_pair(
            &cfg(),
            &case(),
            LoopMode::Act,
            "clean",
            &ScriptReviewer::new(vec![Ok(pass())]),
            &ScriptExecutor::fixing("unused"),
            "picky",
            &ScriptReviewer::new(vec![Ok(fail(&["bug"])), Ok(pass())]),
            &ScriptExecutor::fixing("fixed"),
        )
        .await;
        assert!(rep.arm_a.passed && rep.arm_b.passed);
        assert_eq!((rep.arm_a.turns, rep.arm_b.turns), (1, 2));
        assert_eq!(rep.delta_rounds, 1);
        assert_eq!(rep.arm_b.findings_seen, 1);
    }

    #[tokio::test]
    async fn diverse_finds_more_but_converges() {
        let rep = run_ab_pair(
            &cfg(),
            &case(),
            LoopMode::Plan,
            "same",
            &ScriptReviewer::new(vec![Ok(fail(&["bug-a"])), Ok(pass())]),
            &ScriptExecutor::fixing("fixed"),
            "diverse",
            &ScriptReviewer::new(vec![Ok(fail(&["bug-a", "bug-b"])), Ok(pass())]),
            &ScriptExecutor::fixing("fixed"),
        )
        .await;
        assert!(rep.arm_a.passed && rep.arm_b.passed);
        assert_eq!((rep.arm_a.turns, rep.arm_b.turns), (2, 2));
        assert_eq!(rep.delta_rounds, 0);
        assert_eq!(rep.arm_a.findings_seen, 1);
        assert_eq!(rep.arm_b.findings_seen, 2);
    }

    #[tokio::test]
    async fn invalid_round_counted_costs_a_turn() {
        let rep = run_ab_pair(
            &cfg(),
            &case(),
            LoopMode::Act,
            "clean",
            &ScriptReviewer::new(vec![Ok(pass())]),
            &ScriptExecutor::fixing("unused"),
            "flaky",
            &ScriptReviewer::new(vec![
                Err(ReviewerError::Invalid("bad json".into())),
                Ok(pass()),
            ]),
            &ScriptExecutor::fixing("unused"),
        )
        .await;
        assert!(rep.arm_a.passed && rep.arm_b.passed);
        assert_eq!(rep.arm_b.invalid_rounds, 1);
        assert_eq!((rep.arm_a.turns, rep.arm_b.turns), (1, 2));
        assert_eq!(rep.delta_rounds, 1);
    }

    #[tokio::test]
    async fn cap_on_one_arm_reports_honestly() {
        let mut never = Vec::new();
        for i in 0..5 {
            never.push(Ok(Verdict {
                verdict: VerdictKind::Fail,
                findings: vec![finding(&format!("fresh-{i}"))],
                confidence: 0.8,
            }));
        }
        let rep = run_ab_pair(
            &cfg(),
            &case(),
            LoopMode::Act,
            "converges",
            &ScriptReviewer::new(vec![Ok(pass())]),
            &ScriptExecutor::fixing("unused"),
            "stuck",
            &ScriptReviewer::new(never),
            &ScriptExecutor::fixing("still-broken"),
        )
        .await;
        assert!(rep.arm_a.passed);
        assert!(!rep.arm_b.passed);
        assert_eq!(rep.arm_b.turns, 5);
        assert_eq!(rep.delta_rounds, 4);
    }
}
