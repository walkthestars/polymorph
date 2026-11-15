//! The suite that must never prove: one unit per line of attack on the
//! verdict, each checked against the real solver for a non-`proved` verdict
//! *and* the class a caller would act on. A green run here is evidence that
//! the primitive refuses; a single `proved` is a soundness bug.
//!
//! Proposals are off (no pool, no model): each unit's own annotations are the
//! attack, so the verdict cannot come from a lucky search.

#[path = "support/system_solver.rs"]
mod system_solver;

use std::time::Duration;

use polymorph_ais::driver::{self, DriverConfig};
use polymorph_ais::solver::{Checker, SolverSpec};
use polymorph_ais::types::{
    BackendSpec, Budget, Determinism, Emit, FailureClass, FileSpec, Property, PropertyKind,
    Request, Response, Source, Verdict,
};

fn req(source: &str, kind: PropertyKind, expr: Option<&str>, timeout_s: u64) -> Request {
    Request {
        source: Source {
            language: "ais-core".into(),
            files: vec![FileSpec {
                path: "unit.ais".into(),
                text: Some(source.to_string()),
            }],
            entry: None,
        },
        property: Property {
            kind,
            expr: expr.map(str::to_string),
            assumptions: Vec::new(),
            at: None,
        },
        backend: BackendSpec {
            name: "smtlib2".into(),
            binary: None,
            version: None,
        },
        budget: Budget {
            max_rounds: 2,
            solver_timeout_s: timeout_s,
            token_budget: 0,
            total_wall_s: 120,
        },
        determinism: Determinism::default(),
        emit: vec![Emit::Ledger, Emit::Counterexamples],
    }
}

/// Run one attack. `None` = no `z3` here (the test skips).
async fn attack(
    name: &str,
    source: &str,
    kind: PropertyKind,
    expr: Option<&str>,
    timeout_s: u64,
) -> Option<Response> {
    system_solver::z3_or_skip(name)?;
    let checker = Checker::resolve(SolverSpec::z3(Duration::from_secs(timeout_s)))
        .expect("z3 resolved a moment ago");
    let mut cfg = DriverConfig::deterministic(checker);
    cfg.use_pool = false;
    let out = driver::run(req(source, kind, expr, timeout_s), cfg).await;
    assert!(
        out.certificate.is_none(),
        "{name}: a refused unit must carry no certificate"
    );
    Some(out.response)
}

/// The two facts every case asserts: not proved, and the expected class.
fn refused(name: &str, r: &Response, verdict: Verdict, class: FailureClass) {
    assert_ne!(r.verdict, Verdict::Proved, "{name}: PROVED — soundness bug");
    let failure = r
        .failure
        .as_ref()
        .unwrap_or_else(|| panic!("{name}: a refusal must be classified"));
    assert_eq!(
        (r.verdict, failure.class),
        (verdict, class),
        "{name}: {}",
        failure.message
    );
}

// 1
#[tokio::test]
async fn a_false_postcondition_is_disproved() {
    let src = "fn f(x: int) -> int\n  ensures result == x + 1\n{\n  return x + 2;\n}\n";
    let name = "false_postcondition";
    let Some(r) = attack(name, src, PropertyKind::Postcondition, None, 20).await else {
        return;
    };
    refused(name, &r, Verdict::Disproved, FailureClass::SpecBug);
    assert!(r.counterexamples.iter().any(|c| c.witnessed));
}

// 2
#[tokio::test]
async fn a_contradictory_invariant_set_is_unsound_not_free() {
    // `i >= 0 && i < 0` would discharge every obligation if assumed.
    let src = "fn f(n: int) -> int\n  requires n >= 0\n  ensures result == 12345\n{\n  var i: int = 0;\n  while (i < n)\n    invariant i >= 0\n    invariant i < 0\n  {\n    i = i + 1;\n  }\n  return i;\n}\n";
    let name = "contradictory_invariants";
    let Some(r) = attack(name, src, PropertyKind::Postcondition, None, 20).await else {
        return;
    };
    refused(
        name,
        &r,
        Verdict::BudgetExhausted,
        FailureClass::UnsoundInvariant,
    );
}

// 3
#[tokio::test]
async fn a_vacuous_precondition_is_spec_inconsistent() {
    let src = "fn f(n: int) -> int\n  requires n > 0 && n < 0\n  ensures result == 7\n{\n  return n;\n}\n";
    let name = "vacuous_precondition";
    let Some(r) = attack(name, src, PropertyKind::Postcondition, None, 20).await else {
        return;
    };
    refused(name, &r, Verdict::SpecInconsistent, FailureClass::SpecBug);
}

// 4
#[tokio::test]
async fn zero_obligations_are_refused() {
    let src = "fn f(n: int) -> int\n{\n  return n;\n}\n";
    let name = "zero_obligations";
    let Some(r) = attack(name, src, PropertyKind::Postcondition, None, 20).await else {
        return;
    };
    refused(
        name,
        &r,
        Verdict::Unsupported,
        FailureClass::UnsupportedFeature,
    );
}

// 5
#[tokio::test]
async fn an_invariant_over_a_body_local_is_refused() {
    // `t` is declared inside the body: it has no value at the loop head.
    let src = "fn f(n: int) -> int\n  requires n >= 0\n  ensures result == n\n{\n  var i: int = 0;\n  while (i < n)\n    invariant t == i\n  {\n    var t: int = i;\n    i = i + 1;\n  }\n  return i;\n}\n";
    let name = "body_local_invariant";
    let Some(r) = attack(name, src, PropertyKind::Postcondition, None, 20).await else {
        return;
    };
    refused(
        name,
        &r,
        Verdict::Unsupported,
        FailureClass::UnsupportedFeature,
    );
}

// 6
#[tokio::test]
async fn an_invariant_true_only_at_entry_is_unsound() {
    let src = "fn f(n: int) -> int\n  requires n >= 0\n  ensures result == n\n{\n  var i: int = 0;\n  while (i < n)\n    invariant i == 0\n  {\n    i = i + 1;\n  }\n  return i;\n}\n";
    let name = "entry_only_invariant";
    let Some(r) = attack(name, src, PropertyKind::Postcondition, None, 20).await else {
        return;
    };
    refused(
        name,
        &r,
        Verdict::BudgetExhausted,
        FailureClass::UnsoundInvariant,
    );
}

// 7
#[tokio::test]
async fn a_preserved_but_too_weak_invariant_is_insufficient() {
    // True and inductive, but says nothing about `r`.
    let src = "fn sum(n: int) -> int\n  requires n >= 0\n  ensures result == n * (n + 1) / 2\n{\n  var r: int = 0;\n  var i: int = 0;\n  while (i < n)\n    invariant i >= 0 && i <= n\n  {\n    i = i + 1;\n    r = r + i;\n  }\n  return r;\n}\n";
    let name = "too_weak_invariant";
    let Some(r) = attack(name, src, PropertyKind::Postcondition, None, 20).await else {
        return;
    };
    refused(
        name,
        &r,
        Verdict::BudgetExhausted,
        FailureClass::InsufficientInvariant,
    );
}

// 8
#[tokio::test]
async fn an_assert_false_on_one_branch_is_disproved() {
    let src = "fn f(n: int) -> int\n  ensures result == n\n{\n  if (n > 5) {\n    assert n > 10;\n  }\n  return n;\n}\n";
    let name = "one_branch_assert";
    let Some(r) = attack(name, src, PropertyKind::Postcondition, None, 20).await else {
        return;
    };
    refused(name, &r, Verdict::Disproved, FailureClass::SpecBug);
}

// 9
#[tokio::test]
async fn termination_without_a_variant_is_not_proved() {
    let src = "fn f(n: int) -> int\n  requires n >= 0\n  ensures result == n\n{\n  var i: int = 0;\n  while (i < n)\n    invariant i >= 0 && i <= n\n  {\n    i = i + 1;\n  }\n  return i;\n}\n";
    let name = "termination_without_variant";
    let Some(r) = attack(name, src, PropertyKind::Termination, None, 20).await else {
        return;
    };
    refused(
        name,
        &r,
        Verdict::BudgetExhausted,
        FailureClass::InsufficientInvariant,
    );
}

// 10
#[tokio::test]
async fn division_by_a_symbolic_zero_does_not_prove() {
    // `(div n 0)` is a total but unspecified term in SMT-LIB2. The property
    // holds for every `d != 0` (Euclid), so the only refuting models divide by
    // zero: the solver says `sat`, and the result must not be a proof. The
    // independent evaluator refuses to evaluate a division by zero, so the
    // model is not confirmed as a real execution either — the unit lands on
    // `insufficient_invariant` ("or there is no invariant to blame"), not
    // `disproved`. Conservative on both sides.
    let src =
        "fn f(n: int, d: int) -> int\n  ensures result * d == n - n % d\n{\n  return n / d;\n}\n";
    let name = "division_by_zero";
    let Some(r) = attack(name, src, PropertyKind::Postcondition, None, 20).await else {
        return;
    };
    refused(
        name,
        &r,
        Verdict::BudgetExhausted,
        FailureClass::InsufficientInvariant,
    );
    assert!(r.counterexamples.is_empty(), "{:?}", r.counterexamples);
}

// 11
#[tokio::test]
async fn an_undecided_quantified_property_is_incompleteness() {
    // Nonlinear arithmetic under a quantifier: z3 answers `unknown` or runs
    // out its one-second budget. Either way it is a backend limit, never a
    // proof and never a refutation.
    let src = "fn f(n: int) -> int\n  requires n > 1\n  ensures forall x: int :: forall y: int :: forall z: int :: (x > 0 && y > 0 && z > 0) => x * x * x + y * y * y != z * z * z + n - n\n{\n  return n;\n}\n";
    let name = "undecided_quantified";
    let Some(r) = attack(name, src, PropertyKind::Postcondition, None, 1).await else {
        return;
    };
    refused(
        name,
        &r,
        Verdict::Timeout,
        FailureClass::SolverIncompleteness,
    );
}

// 12
#[tokio::test]
async fn a_false_second_function_fails_the_unit() {
    let src = "fn bad(x: int) -> int\n  ensures result == x + 1\n{\n  return x;\n}\n\nfn good(x: int) -> int\n  ensures result == x\n{\n  return x;\n}\n";
    let name = "false_second_function";
    let Some(r) = attack(name, src, PropertyKind::Postcondition, None, 20).await else {
        return;
    };
    refused(name, &r, Verdict::Disproved, FailureClass::SpecBug);
    let failed = &r.failure.as_ref().unwrap().obligations;
    assert!(
        failed.iter().any(|o| o.contains("bad")),
        "the false function must be named: {failed:?}"
    );
}
