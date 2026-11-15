//! The analyzer: turn checker results into the failure taxonomy.
//!
//! The point of this module is that "not proved" is never one undifferentiated
//! failure. A caller must be able to tell a wrong invariant (fixable
//! automatically) from a wrong property (escalate to a human) from a solver
//! limitation (lower the claim or raise the budget), because the *response* to
//! each is different.
//!
//! Determinism: the mapping is a pure function of the obligation kinds and the
//! checker outcomes. No LLM is consulted, and no message here is generated text.

use std::collections::BTreeMap;

use crate::types::{Counterexample, FailureClass, Obligation, Verdict};

/// What the checker said about one obligation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The obligation holds.
    Discharged,
    /// A counterexample exists.
    Refuted,
    /// The solver answered `unknown`.
    Unknown,
    /// The solver was killed on its wall clock.
    TimedOut,
    /// The checker could not run the query.
    Error(String),
}

/// The analyzer's verdict on one round.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assessment {
    /// Taxonomy class.
    pub class: FailureClass,
    /// Verdict the driver should report if the loop stops here.
    pub verdict: Verdict,
    /// Ids of the obligations that did not discharge, in program order.
    pub failed: Vec<String>,
    /// One actionable line.
    pub message: String,
}

impl Assessment {
    /// A clean run: everything discharged.
    ///
    /// The `class` field is meaningless for a pass (nothing failed); callers must
    /// report `None`, which the driver does for proved rounds.
    pub fn proved() -> Self {
        Self {
            class: FailureClass::InsufficientInvariant,
            verdict: Verdict::Proved,
            failed: Vec::new(),
            message: "all obligations discharged".into(),
        }
    }
}

/// Classify a round of invariant-based obligations.
///
/// Precedence is deliberate: a malformed or false invariant is reported before
/// a weak one (fix the thing that is wrong before strengthening it), and a
/// solver limitation is reported before any proof claim (an `unknown` is not a
/// proof).
pub fn assess(obligations: &[Obligation], outcomes: &BTreeMap<String, Outcome>) -> Assessment {
    let mut failed = Vec::new();
    let mut errors = Vec::new();
    let mut unknowns = Vec::new();
    let mut timeouts = Vec::new();
    let mut refuted_invariant = Vec::new();
    let mut refuted_weak = Vec::new();
    let mut refuted_assert = Vec::new();

    for o in obligations {
        let outcome = outcomes
            .get(&o.id)
            .cloned()
            .unwrap_or(Outcome::Error("no outcome recorded".into()));
        match outcome {
            Outcome::Discharged => {}
            Outcome::Error(e) => {
                failed.push(o.id.clone());
                errors.push(format!("{}: {e}", o.id));
            }
            Outcome::Unknown => {
                failed.push(o.id.clone());
                unknowns.push(o.id.clone());
            }
            Outcome::TimedOut => {
                failed.push(o.id.clone());
                timeouts.push(o.id.clone());
            }
            Outcome::Refuted => {
                failed.push(o.id.clone());
                match o.kind.as_str() {
                    "init" | "preserve" => refuted_invariant.push(o.id.clone()),
                    "assert" => refuted_assert.push(o.id.clone()),
                    _ => refuted_weak.push(o.id.clone()),
                }
            }
        }
    }

    if failed.is_empty() {
        return Assessment::proved();
    }
    if !errors.is_empty() {
        return Assessment {
            class: FailureClass::UnsupportedFeature,
            verdict: Verdict::Unsupported,
            failed,
            message: format!("the checker could not run: {}", errors.join("; ")),
        };
    }
    if !unknowns.is_empty() || !timeouts.is_empty() {
        let what = if unknowns.is_empty() {
            "timed out".to_string()
        } else {
            format!("answered `unknown` on {} obligation(s)", unknowns.len())
        };
        return Assessment {
            class: FailureClass::SolverIncompleteness,
            verdict: Verdict::Timeout,
            failed,
            message: format!(
                "the solver {what}; this is a backend limitation, not a proof and not a refutation"
            ),
        };
    }
    if !refuted_invariant.is_empty() {
        return Assessment {
            class: FailureClass::UnsoundInvariant,
            verdict: Verdict::BudgetExhausted,
            failed: failed.clone(),
            message: format!(
                "the candidate invariant is false ({} refuted at loop entry or by the body); \
                 strengthen or repair the invariant itself before adding conjuncts",
                refuted_invariant.join(", ")
            ),
        };
    }
    if !refuted_assert.is_empty() {
        return Assessment {
            class: FailureClass::SpecBug,
            verdict: Verdict::BudgetExhausted,
            failed: failed.clone(),
            message: format!(
                "the code violates the asserted property ({}); the bounded check decides whether \
                 this is a real execution or an over-approximation",
                refuted_assert.join(", ")
            ),
        };
    }
    Assessment {
        class: FailureClass::InsufficientInvariant,
        verdict: Verdict::BudgetExhausted,
        failed,
        message: format!(
            "the property is not implied by the path conditions: {} obligation(s) refuted (the invariant is too weak, or there is no invariant to blame)",
            refuted_weak.len()
        ),
    }
}

/// A bounded model check's finding: a counterexample that is a *real* execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BmcResult {
    /// Depth the check used.
    pub depth: usize,
    /// A validated counterexample, if one was found.
    pub violation: Option<Counterexample>,
    /// True when a bounded obligation could not be decided.
    pub undecided: bool,
}

impl BmcResult {
    /// A check that was not run.
    pub fn not_run() -> Self {
        Self {
            depth: 0,
            violation: None,
            undecided: false,
        }
    }
}

/// Upgrade an `insufficient_invariant` assessment when the bounded check found a
/// real execution that violates the property: that is a disproved property (and
/// an escalation), not a synthesis failure.
pub fn upgrade_with_bmc(assessment: Assessment, bmc: &BmcResult) -> Assessment {
    if assessment.class != FailureClass::InsufficientInvariant
        && assessment.class != FailureClass::SpecBug
    {
        return assessment;
    }
    let Some(ce) = &bmc.violation else {
        // A refuted `assert` is only a *candidate* spec bug: behind a loop the
        // obligation assumes the invariant, not the real state, so a weak
        // invariant refutes a true assertion. `spec_bug` (needs a human, never
        // repaired) requires a witnessed execution; without one the honest
        // class is a missing invariant. A check that did not run (depth 0)
        // changes nothing.
        if assessment.class == FailureClass::SpecBug && bmc.depth > 0 {
            let undecided = if bmc.undecided {
                " (some bounded obligations were undecided)"
            } else {
                ""
            };
            return Assessment {
                class: FailureClass::InsufficientInvariant,
                verdict: Verdict::BudgetExhausted,
                message: format!(
                    "the assertion is not implied by the current invariants ({}), but no \
                     execution of depth ≤ {} violates it{undecided}: this is a missing or \
                     too-weak invariant, not a demonstrated bug",
                    assessment.failed.join(", "),
                    bmc.depth
                ),
                failed: assessment.failed,
            };
        }
        return assessment;
    };
    Assessment {
        class: FailureClass::SpecBug,
        verdict: Verdict::Disproved,
        failed: assessment.failed,
        message: format!(
            "a real execution of depth ≤ {} violates the property ({}); the property or the code \
             is wrong — this is not a missing invariant, and AIS will not weaken the claim",
            bmc.depth, ce.obligation
        ),
    }
}

/// Build a counterexample record, validating it with the independent evaluator.
///
/// `None` means the model did **not** actually refute the obligation — the
/// record is discarded rather than handed to a refiner as if it were evidence.
pub fn counterexample(
    obligation: &Obligation,
    check: &crate::vc::IrCheck,
    class: FailureClass,
    values: &[(String, String)],
    witnessed: bool,
) -> Option<Counterexample> {
    let mut model = BTreeMap::new();
    let mut rendered = BTreeMap::new();
    for (sym, text) in values {
        let v = crate::eval::parse_literal(text).ok()?;
        model.insert(sym.clone(), v.clone());
        rendered.insert(sym.clone(), v.render());
    }
    match crate::eval::refutes(&check.assumptions, &check.goal, &model) {
        Ok(true) => Some(Counterexample {
            obligation: obligation.id.clone(),
            class,
            values: rendered,
            function: obligation.function.clone(),
            method: "get-value on the negated obligation, independently re-evaluated".into(),
            witnessed,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Obligation;
    use std::collections::BTreeMap;

    fn ob(id: &str, kind: &str) -> Obligation {
        Obligation {
            id: id.into(),
            function: "f".into(),
            kind: kind.into(),
            smt: String::new(),
            probes: Vec::new(),
        }
    }

    fn outcomes(pairs: &[(&str, Outcome)]) -> BTreeMap<String, Outcome> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn all_discharged_is_the_only_proved() {
        let obs = vec![ob("loop1.init0", "init"), ob("f.post0", "exit")];
        let o = outcomes(&[
            ("loop1.init0", Outcome::Discharged),
            ("f.post0", Outcome::Discharged),
        ]);
        let a = assess(&obs, &o);
        assert_eq!(a.verdict, Verdict::Proved);
        assert!(a.failed.is_empty());
    }

    #[test]
    fn a_false_invariant_outranks_a_weak_one() {
        let obs = vec![ob("loop1.init0", "init"), ob("f.post0", "exit")];
        let o = outcomes(&[
            ("loop1.init0", Outcome::Refuted),
            ("f.post0", Outcome::Refuted),
        ]);
        let a = assess(&obs, &o);
        assert_eq!(a.class, FailureClass::UnsoundInvariant);
        assert_ne!(a.verdict, Verdict::Proved);
    }

    #[test]
    fn a_weak_invariant_is_insufficient() {
        let obs = vec![ob("loop1.init0", "init"), ob("f.post0", "exit")];
        let o = outcomes(&[
            ("loop1.init0", Outcome::Discharged),
            ("f.post0", Outcome::Refuted),
        ]);
        assert_eq!(assess(&obs, &o).class, FailureClass::InsufficientInvariant);
    }

    #[test]
    fn unknown_and_timeout_are_solver_limitations_never_proofs() {
        let obs = vec![ob("f.post0", "exit")];
        for outcome in [Outcome::Unknown, Outcome::TimedOut] {
            let a = assess(&obs, &outcomes(&[("f.post0", outcome)]));
            assert_eq!(a.class, FailureClass::SolverIncompleteness);
            assert_eq!(a.verdict, Verdict::Timeout);
        }
    }

    #[test]
    fn a_checker_that_cannot_run_is_unsupported() {
        let obs = vec![ob("f.post0", "exit")];
        let a = assess(
            &obs,
            &outcomes(&[("f.post0", Outcome::Error("no z3".into()))]),
        );
        assert_eq!(a.verdict, Verdict::Unsupported);
        assert_eq!(a.class, FailureClass::UnsupportedFeature);
    }

    #[test]
    fn a_refuted_assertion_is_a_spec_bug_and_escalates() {
        let obs = vec![ob("f.assert1", "assert")];
        let a = assess(&obs, &outcomes(&[("f.assert1", Outcome::Refuted)]));
        assert_eq!(a.class, FailureClass::SpecBug);
        assert!(a.class.needs_human());
    }

    #[test]
    fn a_witnessed_violation_upgrades_a_weakness_into_a_disproof() {
        let a = Assessment {
            class: FailureClass::InsufficientInvariant,
            verdict: Verdict::BudgetExhausted,
            failed: vec!["f.post0".into()],
            message: String::new(),
        };
        let bmc = BmcResult {
            depth: 6,
            violation: Some(Counterexample {
                obligation: "bmc.f.post0".into(),
                class: FailureClass::SpecBug,
                values: BTreeMap::new(),
                function: "f".into(),
                method: "test".into(),
                witnessed: true,
            }),
            undecided: false,
        };
        let up = upgrade_with_bmc(a, &bmc);
        assert_eq!(up.verdict, Verdict::Disproved);
        assert_eq!(up.class, FailureClass::SpecBug);
        let a = Assessment {
            class: FailureClass::InsufficientInvariant,
            verdict: Verdict::BudgetExhausted,
            failed: Vec::new(),
            message: String::new(),
        };
        assert_eq!(
            upgrade_with_bmc(a.clone(), &BmcResult::not_run()).verdict,
            a.verdict
        );
    }

    #[test]
    fn an_unwitnessed_refuted_assertion_is_a_missing_invariant() {
        let obs = vec![ob("f.assert1", "assert")];
        let a = assess(&obs, &outcomes(&[("f.assert1", Outcome::Refuted)]));
        assert_eq!(a.class, FailureClass::SpecBug);
        let clean = BmcResult {
            depth: 6,
            violation: None,
            undecided: false,
        };
        let down = upgrade_with_bmc(a.clone(), &clean);
        assert_eq!(down.class, FailureClass::InsufficientInvariant);
        assert_eq!(down.verdict, Verdict::BudgetExhausted);
        assert!(!down.class.needs_human());
        assert!(down.message.contains("depth ≤ 6"), "{}", down.message);
        // Without a bounded check there is nothing to downgrade on.
        assert_eq!(
            upgrade_with_bmc(a, &BmcResult::not_run()).class,
            FailureClass::SpecBug
        );
    }
}
