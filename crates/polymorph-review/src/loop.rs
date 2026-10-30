//! Plan/act loop driver.
//!
//! Both modes are the same reviewer→executor machine over FRESH reviewer
//! context each round: plan mode critiques the plan, act mode
//! critiques the artifact. The reviewer and executor are injected traits —
//! scripted fakes here, Bus + LLM bindings in F2.

use async_trait::async_trait;
use polymorph_core::{Finding, LoopConfig, Role, Verdict};

use crate::converge::{FindingId, Tracker};

/// Which loop runs. Same machine; different roles and artifact meaning
/// (plan text vs worktree diff/test output). G owns the PLAN.md artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopMode {
    Plan,
    Act,
}

impl LoopMode {
    /// Executor role per mode. The reviewer is ALWAYS [`Role::Review`]:
    /// review quality is role-routed, never mode-routed.
    pub fn executor_role(&self) -> Role {
        match self {
            LoopMode::Plan => Role::Plan,
            LoopMode::Act => Role::Act,
        }
    }

    pub fn reviewer_role(&self) -> Role {
        Role::Review
    }

    pub fn parse(name: &str) -> Option<LoopMode> {
        match name {
            "plan" => Some(LoopMode::Plan),
            "act" => Some(LoopMode::Act),
            _ => None,
        }
    }
}

/// Everything a reviewer round may know. There is deliberately NO
/// history/findings/transcript field — a fresh reviewer sees goal +
/// criteria + CURRENT artifact and nothing else (§2.6 refinement #1).
/// The type system, not discipline, enforces clean context.
#[derive(Debug, Clone)]
pub struct ReviewerInput {
    pub goal: String,
    pub criteria: String,
    pub artifact: String,
    pub mode: LoopMode,
}

/// Executor context: the goal plus the artifact plus the findings to fix. The goal rides along
/// because the fixer must know the DELIVERABLE (create vs repair, which files) — the reviewer brief
/// always had it, the fix brief never did, so creation jobs could never converge. The criteria ride
/// along for the same reason: a CREATE job's only finding is the reviewer's one-paragraph summary
/// of the spec, so a fixer without the criteria rebuilt the rest from repo docs and never saw the
/// verification commands.
#[derive(Debug, Clone)]
pub struct ExecutorInput {
    pub goal: String,
    pub criteria: String,
    pub artifact: String,
    pub findings: Vec<Finding>,
    pub mode: LoopMode,
}

/// Reviewer failure: malformed verdict (F2 maps [`crate::VerdictError`] here).
/// Consumes the round; the loop continues to the cap, then escalates.
#[derive(Debug, Clone, PartialEq)]
pub enum ReviewerError {
    Invalid(String),
}

impl std::fmt::Display for ReviewerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReviewerError::Invalid(d) => write!(f, "reviewer verdict invalid: {d}"),
        }
    }
}

impl std::error::Error for ReviewerError {}

#[async_trait]
pub trait Reviewer: Send + Sync {
    async fn review(&self, input: ReviewerInput) -> Result<Verdict, ReviewerError>;
}

#[async_trait]
pub trait Executor: Send + Sync {
    async fn execute(&self, input: ExecutorInput) -> anyhow::Result<String>;
}

/// Why the loop escalated. Priority: Survivor > final-round-invalid > MaxIterations (plan-review
/// correction — invalid-driven caps must not misreport as plain caps).
#[derive(Debug, Clone, PartialEq)]
pub enum EscalateReason {
    Survivor(Finding),
    FinalInvalid(String),
    ExecutorFailed(String),
    MaxIterations,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Passed { rounds: u32 },
    Escalated { reason: EscalateReason, rounds: u32 },
}

/// One round for the transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoundRecord {
    pub round: u32,
    pub verdict: RoundVerdict,
    pub findings: usize,
    /// Machine-readable finding one-liners (`P1 file:line issue`, capped
    /// [`MAX_ISSUES_PER_ROUND`]) for bench taxonomy (F6): counts flow to
    /// cost proxies, these flow to failure classification.
    pub issues: Vec<String>,
    /// `Some` when the verdict came from the caller's seed, not a reviewer call — carrying which
    /// seed rule fired.
    pub synthesized: Option<SeedKind>,
    /// Why the reviewer produced no usable verdict (the
    /// [`ReviewerError::Invalid`] detail): `Some` exactly for
    /// [`RoundVerdict::Invalid`] rounds, so their reports can say why.
    pub invalid: Option<String>,
}

/// Which harness seed answered round 1: `Create` — the artifact path(s) are missing, so the work is
/// creating them; `Start` — a pure `--diff` set on a clean tree, so the GOAL is not implemented
/// yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeedKind {
    Create,
    Start,
}

impl SeedKind {
    /// Wire name (`--events-json` verdict `synthesized` field).
    pub fn as_str(self) -> &'static str {
        match self {
            SeedKind::Create => "create",
            SeedKind::Start => "start",
        }
    }

    /// Board label (`harness-synthesized {label} finding`), matching the
    /// seed finding's own `Harness-synthesized {label} finding` wording.
    pub fn label(self) -> &'static str {
        match self {
            SeedKind::Create => "CREATE",
            SeedKind::Start => "START",
        }
    }
}

/// A round-1 seed for [`run_loop_seeded`]: the harness-authored findings
/// and the seed rule that produced them.
#[derive(Debug, Clone, PartialEq)]
pub struct RoundSeed {
    pub kind: SeedKind,
    pub findings: Vec<Finding>,
}

/// Transcript tightness bound: full findings stay in executor inputs and
/// `LoopReport.surviving`; the per-round log carries the head only.
pub const MAX_ISSUES_PER_ROUND: usize = 3;

/// Render finding one-liners, head-capped and length-bounded (transcript
/// hygiene — the CLI prints these verbatim).
pub fn finding_issues(findings: &[Finding]) -> Vec<String> {
    findings
        .iter()
        .take(MAX_ISSUES_PER_ROUND)
        .map(|f| {
            let loc = match f.line {
                Some(l) => format!("{}:{}", f.file, l),
                None => f.file.clone(),
            };
            let mut issue: String = f.issue.chars().take(160).collect();
            if f.issue.chars().count() > 160 {
                issue.push('\u{2026}');
            }
            format!("{:?} {} {}", f.severity, loc, issue)
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoundVerdict {
    Pass,
    Fail,
    Invalid,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LoopReport {
    pub outcome: Outcome,
    /// Per-round transcript, oldest first.
    pub rounds_log: Vec<RoundRecord>,
    /// Findings that blocked convergence (survivor / cap rounds), else empty.
    pub surviving: Vec<Finding>,
}

/// Drive reviewer→executor rounds until pass or escalation.
pub async fn run_loop<R: Reviewer, E: Executor>(
    cfg: &LoopConfig,
    goal: &str,
    criteria: &str,
    initial: &str,
    mode: LoopMode,
    reviewer: &R,
    executor: &E,
) -> LoopReport {
    run_loop_seeded(cfg, goal, criteria, initial, mode, reviewer, executor, None).await
}

/// [`run_loop`] with an optional round-1 seed: non-empty `seed` findings stand in for the round-1
/// reviewer verdict (a FAIL carrying exactly them; the reviewer is not called that round) and the
/// record is marked `synthesized` with the seed kind. Everything else is unchanged: the seeded
/// round counts toward `max_iterations` like any round (so a cap of 1 escalates on it without a fix
/// turn — a final-round verdict is never fixed unreviewed), feeds the tracker and the stall check,
/// and its findings go to the executor. `None` or an empty seed (a failing verdict with no findings
/// is schema-invalid) is exactly [`run_loop`].
#[allow(clippy::too_many_arguments)] // run_loop + the seed (one optional input)
pub async fn run_loop_seeded<R: Reviewer, E: Executor>(
    cfg: &LoopConfig,
    goal: &str,
    criteria: &str,
    initial: &str,
    mode: LoopMode,
    reviewer: &R,
    executor: &E,
    seed: Option<RoundSeed>,
) -> LoopReport {
    let mut seed = seed.filter(|s| !s.findings.is_empty());
    let mut report = LoopReport {
        outcome: Outcome::Escalated {
            reason: EscalateReason::MaxIterations,
            rounds: 0,
        },
        rounds_log: Vec::new(),
        surviving: Vec::new(),
    };
    if cfg.max_iterations == 0 {
        return report; // reviewer never consulted.
    }
    let mut tracker = Tracker::new(cfg.clone());
    let mut artifact = initial.to_string();
    // Findings of the most recent failed round — reported as `surviving`
    // if the loop exhausts `max_iterations` without a terminal verdict.
    let mut last_findings: Vec<Finding> = Vec::new();
    // Stalled-loop signal: finding identity + reviewed artifact of the previous failed round. A
    // verbatim repeat against an executor-unchanged artifact means the fix turn was a no-op.
    let mut last_ids: Option<Vec<FindingId>> = None;
    let mut last_artifact: Option<String> = None;
    for round in 1..=cfg.max_iterations {
        // The seed answers round 1 only (taken once); every later round,
        // and every unseeded loop, asks the reviewer.
        let synthesized = seed.as_ref().map(|s| s.kind);
        let reviewed = match seed.take() {
            Some(s) => Ok(Verdict {
                verdict: polymorph_core::VerdictKind::Fail,
                findings: s.findings,
                confidence: 1.0,
            }),
            None => {
                let input = ReviewerInput {
                    goal: goal.to_string(),
                    criteria: criteria.to_string(),
                    artifact: artifact.clone(),
                    mode,
                };
                reviewer.review(input).await
            }
        };
        match reviewed {
            Err(ReviewerError::Invalid(detail)) => {
                report.rounds_log.push(RoundRecord {
                    round,
                    verdict: RoundVerdict::Invalid,
                    findings: 0,
                    issues: Vec::new(),
                    synthesized: None,
                    invalid: Some(detail.clone()),
                });
                if round >= cfg.max_iterations {
                    report.surviving = last_findings.clone();
                    report.outcome = Outcome::Escalated {
                        reason: EscalateReason::FinalInvalid(detail),
                        rounds: round,
                    };
                    return report;
                }
            }
            Ok(verdict) => {
                let n = verdict.findings.len();
                if verdict.verdict == polymorph_core::VerdictKind::Fail {
                    // Stalled-loop early escalation: identical findings
                    // against an unchanged artifact escalate WITH the finding
                    // now (Survivor) instead of burning the remaining rounds
                    // to the same cap. Empty fails carry nothing to compare,
                    // so they still consume the round via the tracker below.
                    let ids: Vec<FindingId> = verdict.findings.iter().map(FindingId::of).collect();
                    if round > 1
                        && !ids.is_empty()
                        && last_ids.as_ref() == Some(&ids)
                        && last_artifact.as_deref() == Some(artifact.as_str())
                    {
                        let f = verdict.findings[0].clone();
                        report.rounds_log.push(RoundRecord {
                            round,
                            verdict: RoundVerdict::Fail,
                            findings: n,
                            issues: finding_issues(&verdict.findings),
                            synthesized,
                            invalid: None,
                        });
                        report.surviving = vec![f.clone()];
                        report.outcome = Outcome::Escalated {
                            reason: EscalateReason::Survivor(f),
                            rounds: round,
                        };
                        return report;
                    }
                    last_ids = Some(ids);
                    last_artifact = Some(artifact.clone());
                    last_findings = verdict.findings.clone();
                }
                match tracker.observe(&verdict) {
                    crate::ObserveOutcome::Pass => {
                        report.rounds_log.push(RoundRecord {
                            round,
                            verdict: RoundVerdict::Pass,
                            findings: n,
                            issues: finding_issues(&verdict.findings),
                            synthesized,
                            invalid: None,
                        });
                        report.outcome = Outcome::Passed { rounds: round };
                        return report;
                    }
                    crate::ObserveOutcome::Continue => {
                        report.rounds_log.push(RoundRecord {
                            round,
                            verdict: RoundVerdict::Fail,
                            findings: n,
                            issues: finding_issues(&verdict.findings),
                            synthesized,
                            invalid: None,
                        });
                        let exec_input = ExecutorInput {
                            goal: goal.to_string(),
                            criteria: criteria.to_string(),
                            artifact: artifact.clone(),
                            findings: verdict.findings.clone(),
                            mode,
                        };
                        match executor.execute(exec_input).await {
                            Ok(next) => artifact = next,
                            Err(e) => {
                                report.outcome = Outcome::Escalated {
                                    reason: EscalateReason::ExecutorFailed(e.to_string()),
                                    rounds: round,
                                };
                                return report;
                            }
                        }
                    }
                    crate::ObserveOutcome::EscalateSurvivor(f) => {
                        report.rounds_log.push(RoundRecord {
                            round,
                            verdict: RoundVerdict::Fail,
                            findings: n,
                            issues: finding_issues(&verdict.findings),
                            synthesized,
                            invalid: None,
                        });
                        report.surviving = vec![f.clone()];
                        report.outcome = Outcome::Escalated {
                            reason: EscalateReason::Survivor(f),
                            rounds: round,
                        };
                        return report;
                    }
                    crate::ObserveOutcome::EscalateCap => {
                        report.rounds_log.push(RoundRecord {
                            round,
                            verdict: RoundVerdict::Fail,
                            findings: n,
                            issues: finding_issues(&verdict.findings),
                            synthesized,
                            invalid: None,
                        });
                        report.surviving = verdict.findings.clone();
                        report.outcome = Outcome::Escalated {
                            reason: EscalateReason::MaxIterations,
                            rounds: round,
                        };
                        return report;
                    }
                }
            }
        }
    }
    // The loop bound (not the tracker) governs when invalid rounds interleave:
    // reaching here means rounds exhausted without a terminal verdict.
    // Every iteration pushes exactly one record, so the log length IS the count.
    report.surviving = last_findings;
    report.outcome = Outcome::Escalated {
        reason: EscalateReason::MaxIterations,
        rounds: report.rounds_log.len() as u32,
    };
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use polymorph_core::{Severity, VerdictKind};
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

    fn fail_verdict(issue: &str) -> Verdict {
        Verdict {
            verdict: VerdictKind::Fail,
            findings: vec![finding(issue)],
            confidence: 0.8,
        }
    }

    fn pass_verdict() -> Verdict {
        Verdict {
            verdict: VerdictKind::Pass,
            findings: vec![],
            confidence: 0.9,
        }
    }

    /// Scripted reviewer: pops one verdict per round; records artifacts seen.
    struct ScriptReviewer {
        script: Mutex<Vec<Result<Verdict, ReviewerError>>>,
        seen: Mutex<Vec<String>>,
    }

    impl ScriptReviewer {
        fn new(script: Vec<Result<Verdict, ReviewerError>>) -> Self {
            Self {
                script: Mutex::new(script.into_iter().rev().collect()),
                seen: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl Reviewer for ScriptReviewer {
        async fn review(&self, input: ReviewerInput) -> Result<Verdict, ReviewerError> {
            self.seen.lock().unwrap().push(input.artifact.clone());
            // A test double that panics on exhaustion instead of inventing a
            // verdict: running out of script is a test bug, and it must be loud.
            self.script.lock().unwrap().pop().expect("script exhausted")
        }
    }

    /// Executor double: applies the scripted fix, records what it got.
    struct ScriptExecutor {
        fix_to: String,
        got_artifacts: Mutex<Vec<String>>,
        got_counts: Mutex<Vec<usize>>,
        got_criteria: Mutex<Vec<String>>,
        fail: bool,
    }

    impl ScriptExecutor {
        fn fixing(fix_to: &str) -> Self {
            Self {
                fix_to: fix_to.to_string(),
                got_artifacts: Mutex::new(Vec::new()),
                got_counts: Mutex::new(Vec::new()),
                got_criteria: Mutex::new(Vec::new()),
                fail: false,
            }
        }

        fn failing() -> Self {
            Self {
                fix_to: String::new(),
                got_artifacts: Mutex::new(Vec::new()),
                got_counts: Mutex::new(Vec::new()),
                got_criteria: Mutex::new(Vec::new()),
                fail: true,
            }
        }
    }

    #[async_trait]
    impl Executor for ScriptExecutor {
        async fn execute(&self, input: ExecutorInput) -> anyhow::Result<String> {
            self.got_artifacts.lock().unwrap().push(input.artifact);
            self.got_counts.lock().unwrap().push(input.findings.len());
            self.got_criteria.lock().unwrap().push(input.criteria);
            if self.fail {
                anyhow::bail!("executor blew up");
            }
            Ok(self.fix_to.clone())
        }
    }

    fn cfg(max: u32, surviving: u32) -> LoopConfig {
        LoopConfig {
            max_iterations: max,
            surviving_finding_escalation: surviving,
        }
    }

    #[tokio::test]
    async fn plan_mode_pass_first_try() {
        let r = ScriptReviewer::new(vec![Ok(pass_verdict())]);
        let e = ScriptExecutor::fixing("unused");
        let rep = run_loop(
            &LoopConfig::default(),
            "goal",
            "crit",
            "plan-v1",
            LoopMode::Plan,
            &r,
            &e,
        )
        .await;
        assert_eq!(rep.outcome, Outcome::Passed { rounds: 1 });
        assert_eq!(rep.rounds_log.len(), 1);
        assert!(rep.surviving.is_empty());
        assert!(e.got_artifacts.lock().unwrap().is_empty()); // executor never ran.
    }

    #[tokio::test]
    async fn act_mode_fail_once_then_pass() {
        let r = ScriptReviewer::new(vec![Ok(fail_verdict("bug")), Ok(pass_verdict())]);
        let e = ScriptExecutor::fixing("fixed-artifact");
        let rep = run_loop(&cfg(5, 3), "goal", "crit", "v1", LoopMode::Act, &r, &e).await;
        assert_eq!(rep.outcome, Outcome::Passed { rounds: 2 });
        // Executor got v1 + the one finding; reviewer round 2 saw the fix.
        assert_eq!(*e.got_artifacts.lock().unwrap(), vec!["v1".to_string()]);
        assert_eq!(*e.got_counts.lock().unwrap(), vec![1]);
        assert_eq!(
            *r.seen.lock().unwrap(),
            vec!["v1".to_string(), "fixed-artifact".to_string()]
        );
    }

    #[tokio::test]
    async fn executor_receives_the_criteria_every_round() {
        // Regression: the fix turn got goal + artifact + findings but never the criteria, so a
        // CREATE job's fixer only saw the reviewer's one-finding summary of the spec.
        let criteria = "Files: a.rs
Verify: cargo test -p a --lib exact_name";
        let r = ScriptReviewer::new(vec![
            Ok(fail_verdict("missing")),
            Ok(fail_verdict("still wrong")),
            Ok(pass_verdict()),
        ]);
        let e = ScriptExecutor::fixing("fixed");
        let rep = run_loop(&cfg(5, 3), "goal", criteria, "", LoopMode::Act, &r, &e).await;
        assert_eq!(rep.outcome, Outcome::Passed { rounds: 3 });
        assert_eq!(
            *e.got_criteria.lock().unwrap(),
            vec![criteria.to_string(), criteria.to_string()]
        );
    }

    #[tokio::test]
    async fn round_records_carry_capped_issue_one_liners() {
        let long = "y".repeat(200);
        let r = ScriptReviewer::new(vec![Ok(Verdict {
            verdict: VerdictKind::Fail,
            findings: vec![
                finding("first"),
                finding("second"),
                finding(&long),
                finding("fourth"),
            ],
            confidence: 0.5,
        })]);
        let e = ScriptExecutor::fixing("fixed");
        // max_iterations 1: single Fail round, then the cap (executor idle).
        let rep = run_loop(&cfg(1, 3), "goal", "crit", "v1", LoopMode::Act, &r, &e).await;
        assert_eq!(rep.rounds_log.len(), 1);
        let rec = &rep.rounds_log[0];
        assert_eq!(rec.findings, 4);
        assert_eq!(rec.issues.len(), 3);
        assert_eq!(rec.issues[0], "P1 a.rs:1 first");
        assert_eq!(rec.issues[1], "P1 a.rs:1 second");
        // 160-char truncation + ellipsis: 10-char head + 160 + 1.
        assert_eq!(rec.issues[2].chars().count(), 171);
        assert!(rec.issues[2].ends_with('\u{2026}'));
    }

    #[tokio::test]
    async fn reviewer_sees_only_current_artifact_never_prior_findings() {
        // Round-2 input must equal the executor's output verbatim, and the
        // input TYPE carries no findings field (compile-time). Here: the
        // reviewer cannot distinguish a fixed artifact from a fresh one.
        let r = ScriptReviewer::new(vec![Ok(fail_verdict("bug-a")), Ok(pass_verdict())]);
        let e = ScriptExecutor::fixing("totally-new-text");
        let rep = run_loop(
            &cfg(5, 3),
            "goal",
            "crit",
            "original",
            LoopMode::Plan,
            &r,
            &e,
        )
        .await;
        assert_eq!(rep.outcome, Outcome::Passed { rounds: 2 });
        let seen = r.seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[1], "totally-new-text");
        assert_ne!(seen[1], seen[0]);
    }

    #[tokio::test]
    async fn never_converging_escalates_at_cap() {
        // Fresh issue per round so only the cap can fire.
        let mut script = Vec::new();
        for i in 0..5 {
            script.push(Ok(fail_verdict(&format!("fresh-{i}"))));
        }
        let r = ScriptReviewer::new(script);
        let e = ScriptExecutor::fixing("still-broken");
        let rep = run_loop(&cfg(5, 100), "goal", "crit", "v1", LoopMode::Act, &r, &e).await;
        assert_eq!(
            rep.outcome,
            Outcome::Escalated {
                reason: EscalateReason::MaxIterations,
                rounds: 5
            }
        );
        assert_eq!(rep.rounds_log.len(), 5);
        assert_eq!(rep.surviving.len(), 1); // what blocked convergence is kept.
    }

    #[tokio::test]
    async fn stalled_noop_fix_escalates_as_survivor_early() {
        // Verbatim findings against an executor-unchanged artifact mean the fix turn was a no-op —
        // Survivor at round 2 with the finding attached, not a bare cap at round 5.
        let r = ScriptReviewer::new(vec![Ok(fail_verdict("stuck")), Ok(fail_verdict("stuck"))]);
        let e = ScriptExecutor::fixing("v1"); // no-op: returns input text.
        let rep = run_loop(&cfg(5, 100), "goal", "crit", "v1", LoopMode::Act, &r, &e).await;
        assert_eq!(
            rep.outcome,
            Outcome::Escalated {
                reason: EscalateReason::Survivor(finding("stuck")),
                rounds: 2
            }
        );
        assert_eq!(rep.rounds_log.len(), 2);
        assert_eq!(rep.surviving.len(), 1);
        // Executor ran exactly once (round-1 fix); round 2 escalated pre-fix.
        assert_eq!(e.got_artifacts.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn survivor_escalates_early_with_reason() {
        let r = ScriptReviewer::new(vec![
            Ok(fail_verdict("stubborn")),
            Ok(fail_verdict("stubborn")),
            Ok(fail_verdict("stubborn")),
        ]);
        let e = ScriptExecutor::fixing("v2");
        let rep = run_loop(
            &LoopConfig::default(),
            "goal",
            "crit",
            "v1",
            LoopMode::Plan,
            &r,
            &e,
        )
        .await;
        assert_eq!(
            rep.outcome,
            Outcome::Escalated {
                reason: EscalateReason::Survivor(finding("stubborn")),
                rounds: 3
            }
        );
        assert_eq!(rep.surviving, vec![finding("stubborn")]);
    }

    #[tokio::test]
    async fn invalid_reviewer_escalates_as_invalid_not_plain_cap() {
        let r = ScriptReviewer::new(vec![
            Err(ReviewerError::Invalid("not json".into())),
            Err(ReviewerError::Invalid("not json".into())),
        ]);
        let e = ScriptExecutor::fixing("unused");
        let rep = run_loop(&cfg(2, 3), "goal", "crit", "v1", LoopMode::Act, &r, &e).await;
        assert_eq!(
            rep.outcome,
            Outcome::Escalated {
                reason: EscalateReason::FinalInvalid("not json".into()),
                rounds: 2
            }
        );
        assert!(rep
            .rounds_log
            .iter()
            .all(|r| r.verdict == RoundVerdict::Invalid));
    }

    #[tokio::test]
    async fn mixed_invalid_and_fail_exhausts_with_true_round_count() {
        // Invalid rounds don't touch the tracker, so the LOOP bound (not the
        // tracker cap) terminates: outcome rounds must equal rounds logged.
        // Case 1: final round invalid → FinalInvalid wins over plain cap.
        let r = ScriptReviewer::new(vec![
            Err(ReviewerError::Invalid("bad json".into())),
            Ok(fail_verdict("fresh-a")),
            Err(ReviewerError::Invalid("bad json".into())),
        ]);
        let e = ScriptExecutor::fixing("v2");
        let rep = run_loop(&cfg(3, 100), "goal", "crit", "v1", LoopMode::Act, &r, &e).await;
        assert_eq!(
            rep.outcome,
            Outcome::Escalated {
                reason: EscalateReason::FinalInvalid("bad json".into()),
                rounds: 3
            }
        );
        assert_eq!(rep.rounds_log.len(), 3);
        assert_eq!(rep.surviving, vec![finding("fresh-a")]);
        // Case 2: final round a valid fail → MaxIterations with true count.
        let r = ScriptReviewer::new(vec![
            Err(ReviewerError::Invalid("bad json".into())),
            Ok(fail_verdict("fresh-a")),
            Ok(fail_verdict("fresh-b")),
        ]);
        let e = ScriptExecutor::fixing("v2");
        let rep = run_loop(&cfg(3, 100), "goal", "crit", "v1", LoopMode::Act, &r, &e).await;
        assert_eq!(
            rep.outcome,
            Outcome::Escalated {
                reason: EscalateReason::MaxIterations,
                rounds: 3
            }
        );
        assert_eq!(rep.rounds_log.len(), 3);
        assert_eq!(rep.surviving, vec![finding("fresh-b")]);
    }

    #[tokio::test]
    async fn executor_failure_escalates_immediately() {
        let r = ScriptReviewer::new(vec![Ok(fail_verdict("bug"))]);
        let e = ScriptExecutor::failing();
        let rep = run_loop(&cfg(5, 3), "goal", "crit", "v1", LoopMode::Act, &r, &e).await;
        assert!(
            matches!(
                rep.outcome,
                Outcome::Escalated {
                    reason: EscalateReason::ExecutorFailed(_),
                    rounds: 1
                }
            ),
            "{:?}",
            rep.outcome
        );
    }

    #[tokio::test]
    async fn zero_max_iterations_never_consults_reviewer() {
        let r = ScriptReviewer::new(vec![]);
        let e = ScriptExecutor::fixing("unused");
        let rep = run_loop(&cfg(0, 3), "goal", "crit", "v1", LoopMode::Plan, &r, &e).await;
        assert_eq!(
            rep.outcome,
            Outcome::Escalated {
                reason: EscalateReason::MaxIterations,
                rounds: 0
            }
        );
        assert!(r.seen.lock().unwrap().is_empty());
        assert!(rep.rounds_log.is_empty());
    }

    #[tokio::test]
    async fn seed_answers_round_one_without_a_reviewer_call() {
        // The seed is round 1's verdict — the reviewer is first consulted in round 2, on the
        // executor's output.
        let r = ScriptReviewer::new(vec![Ok(pass_verdict())]);
        let e = ScriptExecutor::fixing("created");
        let seed = RoundSeed {
            kind: SeedKind::Create,
            findings: vec![finding("create a.rs"), finding("and its test")],
        };
        let rep = run_loop_seeded(
            &cfg(3, 3),
            "goal",
            "crit",
            "",
            LoopMode::Act,
            &r,
            &e,
            Some(seed),
        )
        .await;
        assert_eq!(rep.outcome, Outcome::Passed { rounds: 2 });
        assert_eq!(*r.seen.lock().unwrap(), vec!["created".to_string()]);
        assert_eq!(*e.got_artifacts.lock().unwrap(), vec![String::new()]);
        assert_eq!(*e.got_counts.lock().unwrap(), vec![2]);
        assert_eq!(rep.rounds_log.len(), 2);
        assert_eq!(rep.rounds_log[0].synthesized, Some(SeedKind::Create));
        assert_eq!(rep.rounds_log[0].verdict, RoundVerdict::Fail);
        assert_eq!(rep.rounds_log[0].findings, 2);
        assert_eq!(rep.rounds_log[0].issues[0], "P1 a.rs:1 create a.rs");
        assert_eq!(rep.rounds_log[1].synthesized, None);
    }

    #[tokio::test]
    async fn seeded_round_counts_toward_max_iterations() {
        // Cap 1: the seeded round IS the final round — escalate on it
        // with the seed surviving; no model call, no unreviewed fix.
        let r = ScriptReviewer::new(vec![]);
        let e = ScriptExecutor::fixing("unused");
        let seed = RoundSeed {
            kind: SeedKind::Start,
            findings: vec![finding("create a.rs")],
        };
        let rep = run_loop_seeded(
            &cfg(1, 3),
            "goal",
            "crit",
            "",
            LoopMode::Act,
            &r,
            &e,
            Some(seed.clone()),
        )
        .await;
        assert_eq!(
            rep.outcome,
            Outcome::Escalated {
                reason: EscalateReason::MaxIterations,
                rounds: 1
            }
        );
        assert_eq!(rep.surviving, seed.findings);
        assert!(r.seen.lock().unwrap().is_empty());
        assert!(e.got_artifacts.lock().unwrap().is_empty());
        assert_eq!(rep.rounds_log[0].synthesized, Some(SeedKind::Start));
        // Cap 2: seed + fix, then one reviewer round that fails → cap at 2.
        let r = ScriptReviewer::new(vec![Ok(fail_verdict("still missing"))]);
        let e = ScriptExecutor::fixing("v2");
        let rep = run_loop_seeded(
            &cfg(2, 3),
            "goal",
            "crit",
            "",
            LoopMode::Act,
            &r,
            &e,
            Some(seed.clone()),
        )
        .await;
        assert_eq!(
            rep.outcome,
            Outcome::Escalated {
                reason: EscalateReason::MaxIterations,
                rounds: 2
            }
        );
        assert_eq!(r.seen.lock().unwrap().len(), 1);
        assert_eq!(e.got_artifacts.lock().unwrap().len(), 1);
        assert_eq!(rep.rounds_log.len(), 2);
        // Cap 0: nothing runs, the seed is never consumed.
        let r = ScriptReviewer::new(vec![]);
        let e = ScriptExecutor::fixing("unused");
        let rep = run_loop_seeded(
            &cfg(0, 3),
            "goal",
            "crit",
            "",
            LoopMode::Act,
            &r,
            &e,
            Some(seed),
        )
        .await;
        assert!(rep.rounds_log.is_empty());
        assert!(e.got_artifacts.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn no_or_empty_seed_is_exactly_run_loop() {
        // Unseeded loops (every non-CREATE drive, and the opt-out) must be
        // indistinguishable from `run_loop`, records included.
        let script = || {
            vec![
                Err(ReviewerError::Invalid("bad json".into())),
                Ok(fail_verdict("bug")),
                Ok(pass_verdict()),
            ]
        };
        let r = ScriptReviewer::new(script());
        let e = ScriptExecutor::fixing("fixed");
        let base = run_loop(&cfg(5, 3), "goal", "crit", "v1", LoopMode::Act, &r, &e).await;
        let base_seen = r.seen.lock().unwrap().clone();
        assert!(base.rounds_log.iter().all(|rec| rec.synthesized.is_none()));
        // Only the Invalid round carries its reason.
        assert_eq!(base.rounds_log[0].verdict, RoundVerdict::Invalid);
        assert_eq!(base.rounds_log[0].invalid.as_deref(), Some("bad json"));
        assert!(base.rounds_log[1..].iter().all(|rec| rec.invalid.is_none()));
        for seed in [
            None,
            Some(RoundSeed {
                kind: SeedKind::Create,
                findings: Vec::new(),
            }),
        ] {
            let r = ScriptReviewer::new(script());
            let e = ScriptExecutor::fixing("fixed");
            let rep = run_loop_seeded(
                &cfg(5, 3),
                "goal",
                "crit",
                "v1",
                LoopMode::Act,
                &r,
                &e,
                seed,
            )
            .await;
            assert_eq!(rep, base);
            assert_eq!(*r.seen.lock().unwrap(), base_seen);
        }
    }

    #[test]
    fn mode_role_routing() {
        assert_eq!(LoopMode::Plan.executor_role(), Role::Plan);
        assert_eq!(LoopMode::Act.executor_role(), Role::Act);
        assert_eq!(LoopMode::Plan.reviewer_role(), Role::Review);
        assert_eq!(LoopMode::Act.reviewer_role(), Role::Review);
        assert_eq!(LoopMode::parse("plan"), Some(LoopMode::Plan));
        assert_eq!(LoopMode::parse("act"), Some(LoopMode::Act));
        assert_eq!(LoopMode::parse("frobnicate"), None);
    }
}
