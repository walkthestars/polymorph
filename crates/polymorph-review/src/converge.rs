//! Convergence state machine.
//!
//! Tracks recurring findings across reviewer rounds: a finding that survives
//! `LoopConfig::surviving_finding_escalation` CONSECUTIVE failed rounds
//! escalates to the user instead of converging to a worse artifact (reviewers
//! without enforced disagreement agree too easily). Termination is on clean verdict or `max_iterations`.
//!
//! Identity is exact-after-trim on (file, line, issue): predictable and
//! documented. A reworded repeat resets its count — accepted limit (NOTES).

use polymorph_core::{Finding, LoopConfig, Verdict, VerdictKind};
use std::collections::HashMap;

/// Identity of a finding across rounds. Severity and proposed_fix are
/// deliberately EXCLUDED: a reprioritized repeat is still the same issue.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FindingId {
    pub file: String,
    pub line: Option<u32>,
    pub issue: String,
}

impl FindingId {
    pub fn of(f: &Finding) -> Self {
        Self {
            file: f.file.trim().to_string(),
            line: f.line,
            issue: f.issue.trim().to_string(),
        }
    }
}

/// What one observation means for the loop.
#[derive(Debug, Clone, PartialEq)]
pub enum ObserveOutcome {
    /// Clean verdict — loop passes (advisory findings, if any, are recorded
    /// by the caller from the verdict itself; they never block).
    Pass,
    /// Failed round, no escalation yet — run the executor, review again.
    Continue,
    /// A finding survived the escalation threshold — escalate WITH it.
    EscalateSurvivor(Finding),
    /// Rounds exhausted without convergence.
    EscalateCap,
}

/// Survival tracker. Owns the round count so the cap lives in one place.
pub struct Tracker {
    cfg: LoopConfig,
    rounds: u32,
    /// Consecutive-round sightings per finding id.
    seen: HashMap<FindingId, (u32, Finding)>,
}

impl Tracker {
    pub fn new(cfg: LoopConfig) -> Self {
        Self {
            cfg,
            rounds: 0,
            seen: HashMap::new(),
        }
    }

    pub fn rounds(&self) -> u32 {
        self.rounds
    }

    pub fn observe(&mut self, verdict: &Verdict) -> ObserveOutcome {
        self.rounds += 1;
        if verdict.verdict == VerdictKind::Pass {
            return ObserveOutcome::Pass;
        }
        // Fail round: consecutive counts; ids absent this round reset
        // (rebuild instead of retain — the reset IS the semantics).
        let mut next: HashMap<FindingId, (u32, Finding)> = HashMap::new();
        let mut survivor: Option<Finding> = None;
        for f in &verdict.findings {
            let id = FindingId::of(f);
            let count = self.seen.get(&id).map(|(c, _)| *c).unwrap_or(0) + 1;
            // Survivor beats the cap: the more informative reason wins.
            if survivor.is_none() && count >= self.cfg.surviving_finding_escalation {
                survivor = Some(f.clone());
            }
            next.insert(id, (count, f.clone()));
        }
        self.seen = next;
        if let Some(f) = survivor {
            return ObserveOutcome::EscalateSurvivor(f);
        }
        // An empty fail carries nothing to fix; it still consumes the round.
        if self.rounds >= self.cfg.max_iterations {
            return ObserveOutcome::EscalateCap;
        }
        ObserveOutcome::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polymorph_core::Severity;

    fn finding(issue: &str) -> Finding {
        Finding {
            severity: Severity::P1,
            file: "a.rs".into(),
            line: Some(1),
            issue: issue.into(),
            proposed_fix: "fix".into(),
        }
    }

    fn fail(findings: Vec<Finding>) -> Verdict {
        Verdict {
            verdict: VerdictKind::Fail,
            findings,
            confidence: 0.8,
        }
    }

    fn pass() -> Verdict {
        Verdict {
            verdict: VerdictKind::Pass,
            findings: vec![],
            confidence: 0.9,
        }
    }

    fn cfg(max: u32, surviving: u32) -> LoopConfig {
        LoopConfig {
            max_iterations: max,
            surviving_finding_escalation: surviving,
        }
    }

    #[test]
    fn pass_first_try() {
        let mut t = Tracker::new(LoopConfig::default());
        assert_eq!(t.observe(&pass()), ObserveOutcome::Pass);
        assert_eq!(t.rounds(), 1);
    }

    #[test]
    fn fail_once_then_pass_continues_then_passes() {
        let mut t = Tracker::new(LoopConfig::default());
        assert_eq!(
            t.observe(&fail(vec![finding("x")])),
            ObserveOutcome::Continue
        );
        assert_eq!(t.observe(&pass()), ObserveOutcome::Pass);
    }

    #[test]
    fn never_converging_hits_cap() {
        let mut t = Tracker::new(cfg(3, 100)); // survivor unreachable; cap governs.
                                               // Fresh issue per round so only the cap can fire.
        for i in 0..2 {
            assert_eq!(
                t.observe(&fail(vec![finding(&format!("issue-{i}"))])),
                ObserveOutcome::Continue
            );
        }
        assert_eq!(
            t.observe(&fail(vec![finding("issue-2")])),
            ObserveOutcome::EscalateCap
        );
        assert_eq!(t.rounds(), 3);
    }

    #[test]
    fn survivor_escalates_at_threshold_with_the_finding() {
        let mut t = Tracker::new(LoopConfig::default()); // 5 max, 3 to survive.
        assert_eq!(
            t.observe(&fail(vec![finding("stubborn")])),
            ObserveOutcome::Continue
        );
        assert_eq!(
            t.observe(&fail(vec![finding("stubborn")])),
            ObserveOutcome::Continue
        );
        let out = t.observe(&fail(vec![finding("stubborn")]));
        assert_eq!(out, ObserveOutcome::EscalateSurvivor(finding("stubborn")));
        assert_eq!(t.rounds(), 3); // early — before the cap of 5.
    }

    #[test]
    fn absent_id_resets_its_count() {
        let mut t = Tracker::new(LoopConfig::default());
        assert_eq!(
            t.observe(&fail(vec![finding("x")])),
            ObserveOutcome::Continue
        );
        // Round 2 omits "x" — its streak resets.
        assert_eq!(
            t.observe(&fail(vec![finding("y")])),
            ObserveOutcome::Continue
        );
        assert_eq!(
            t.observe(&fail(vec![finding("x")])),
            ObserveOutcome::Continue
        );
        // "x" seen in rounds 1 and 3 non-consecutively: count is 1, no survivor.
        assert_eq!(
            t.observe(&fail(vec![finding("x")])),
            ObserveOutcome::Continue
        );
    }

    #[test]
    fn survivor_beats_cap_when_both_fire() {
        // Threshold 2, max 2: round 2 completes a survival AND exhausts rounds.
        let mut t = Tracker::new(cfg(2, 2));
        assert_eq!(
            t.observe(&fail(vec![finding("x")])),
            ObserveOutcome::Continue
        );
        assert_eq!(
            t.observe(&fail(vec![finding("x")])),
            ObserveOutcome::EscalateSurvivor(finding("x"))
        );
    }

    #[test]
    fn pass_with_advisories_still_passes() {
        let mut t = Tracker::new(LoopConfig::default());
        let mut v = pass();
        v.findings = vec![finding("nit")];
        assert_eq!(t.observe(&v), ObserveOutcome::Pass);
    }
}
