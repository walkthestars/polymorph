//! End-to-end tests of the AIS primitive against a real SMT solver.
//!
//! These tests are the primitive's evidence: the checker is the real pinned Z3
//! binary, the proposals are either deterministic or scripted, and no model is
//! called. A test that needs a model would be wrong here — the point is that the
//! verdict does not depend on one.

#[path = "support/system_solver.rs"]
mod system_solver;

use std::time::Duration;

use polymorph_ais::cert::{self, Certificate};
use polymorph_ais::driver::{self, DriverConfig};
use polymorph_ais::propose::{Proposal, ScriptedProposer};
use polymorph_ais::solver::{Checker, SolverSpec};
use polymorph_ais::types::{
    Annotation, BackendSpec, Determinism, Emit, FailureClass, FileSpec, Property, PropertyKind,
    Request, Source, Verdict,
};

fn checker() -> Checker {
    let spec = SolverSpec::z3(Duration::from_secs(20));
    match Checker::resolve(spec) {
        Ok(c) => c,
        Err(e) => panic!("this test needs a `z3` binary on PATH: {e}"),
    }
}

fn req(source: &str, kind: PropertyKind, expr: Option<&str>) -> Request {
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
        budget: polymorph_ais::types::Budget {
            max_rounds: 3,
            solver_timeout_s: 20,
            token_budget: 0,
            total_wall_s: 120,
        },
        determinism: Determinism::default(),
        emit: vec![Emit::Certificate, Emit::Ledger, Emit::Counterexamples],
    }
}

const SUM: &str = r#"
fn sum(n: int) -> int
  requires n >= 0
  ensures result == n * (n + 1) / 2
{
  var r: int = 0;
  var i: int = 0;
  while (i < n)
    invariant i >= 0 && i <= n && r == i * (i + 1) / 2
  {
    i = i + 1;
    r = r + i;
  }
  return r;
}
"#;

#[tokio::test]
async fn proves_a_sum_loop_and_emits_a_recheckable_certificate() {
    let Some(_) =
        system_solver::z3_or_skip("proves_a_sum_loop_and_emits_a_recheckable_certificate")
    else {
        return;
    };
    let out = driver::run(
        req(SUM, PropertyKind::Postcondition, None),
        DriverConfig::deterministic(checker()),
    )
    .await;
    assert_eq!(
        out.response.verdict,
        Verdict::Proved,
        "{:?}",
        out.response.failure
    );
    let certificate = out.certificate.expect("a proof carries a certificate");
    assert!(certificate.digest_ok(), "certificate self-digest must hold");
    assert_eq!(certificate.format, "ais-cert-v1");
    assert!(
        certificate
            .body
            .obligations
            .iter()
            .all(|o| o.status == polymorph_ais::types::ObligationStatus::Discharged),
        "every obligation must be discharged on a proof"
    );
    // The certificate replays through the same pinned solver.
    let outcome = cert::recheck(&certificate, Some(SUM.as_bytes()), &checker());
    assert!(outcome.ok, "recheck failed: {:?}", outcome.first_failure());
}

#[tokio::test]
async fn a_missing_invariant_is_a_classified_failure_not_a_proof() {
    let Some(_) =
        system_solver::z3_or_skip("a_missing_invariant_is_a_classified_failure_not_a_proof")
    else {
        return;
    };
    let bare = SUM.replace(
        "\n    invariant i >= 0 && i <= n && r == i * (i + 1) / 2",
        "",
    );
    let mut cfg = DriverConfig::deterministic(checker());
    // No pool and no pruning: this is the honest "nothing was proposed" case.
    cfg.use_pool = false;
    cfg.houdini = false;
    let out = driver::run(req(&bare, PropertyKind::Postcondition, None), cfg).await;
    assert_ne!(out.response.verdict, Verdict::Proved);
    let failure = out.response.failure.expect("a failure must be classified");
    assert_eq!(
        failure.class,
        FailureClass::InsufficientInvariant,
        "{failure:?}"
    );
    assert!(out.response.certificate.is_none());
}

#[tokio::test]
async fn a_false_invariant_is_reported_as_unsound() {
    let Some(_) = system_solver::z3_or_skip("a_false_invariant_is_reported_as_unsound") else {
        return;
    };
    let wrong = SUM.replace(
        "invariant i >= 0 && i <= n && r == i * (i + 1) / 2",
        "invariant i >= 0 && i <= n && r == i * i",
    );
    let mut cfg = DriverConfig::deterministic(checker());
    // Houdini would prune the false conjunct; the claim here is that a verbatim
    // proposal that is false is *reported* as unsound, never silently dropped.
    cfg.houdini = false;
    let out = driver::run(req(&wrong, PropertyKind::Postcondition, None), cfg).await;
    let failure = out.response.failure.expect("classified failure");
    assert_eq!(failure.class, FailureClass::UnsoundInvariant, "{failure:?}");
}

#[tokio::test]
async fn the_deterministic_pool_finds_the_invariant_with_no_model() {
    let Some(_) =
        system_solver::z3_or_skip("the_deterministic_pool_finds_the_invariant_with_no_model")
    else {
        return;
    };
    let bare = SUM.replace(
        "\n    invariant i >= 0 && i <= n && r == i * (i + 1) / 2",
        "",
    );
    let out = driver::run(
        req(&bare, PropertyKind::Postcondition, None),
        DriverConfig::deterministic(checker()),
    )
    .await;
    assert_eq!(
        out.response.verdict,
        Verdict::Proved,
        "pool failed: {:?} rounds={:?}",
        out.response.failure,
        out.response.rounds.last()
    );
    assert_eq!(out.response.provenance.proposer, "heuristic");
    assert_eq!(out.response.cost.tokens, 0);
}

#[tokio::test]
async fn a_wrong_postcondition_is_disproved_with_a_real_counterexample() {
    let Some(_) =
        system_solver::z3_or_skip("a_wrong_postcondition_is_disproved_with_a_real_counterexample")
    else {
        return;
    };
    // `result` is the sum 0..n, never 2n for n >= 3, so a bounded execution
    // refutes the property outright — this is a bad property, not a weak
    // invariant, and the primitive must not pretend otherwise.
    let no_contract = SUM.replace(
        "  ensures result == n * (n + 1) / 2
",
        "",
    );
    let out = driver::run(
        req(
            &no_contract,
            PropertyKind::Postcondition,
            Some("result == 2 * n"),
        ),
        DriverConfig::deterministic(checker()),
    )
    .await;
    assert_eq!(
        out.response.verdict,
        Verdict::Disproved,
        "{:?}",
        out.response.failure
    );
    let failure = out.response.failure.expect("classified");
    assert_eq!(failure.class, FailureClass::SpecBug);
    assert!(failure.needs_human, "spec_bug escalates to a human");
    assert!(
        !out.response.counterexamples.is_empty(),
        "a disproved verdict must carry a validated counterexample"
    );
    let witnessed: Vec<_> = out
        .response
        .counterexamples
        .iter()
        .filter(|c| c.witnessed)
        .collect();
    assert_eq!(witnessed.len(), 1, "exactly one witnessed counterexample");
    assert_eq!(witnessed[0].class, FailureClass::SpecBug);
}

#[tokio::test]
async fn a_vacuous_specification_is_spec_inconsistent() {
    let Some(_) = system_solver::z3_or_skip("a_vacuous_specification_is_spec_inconsistent") else {
        return;
    };
    let vacuous = SUM.replace("requires n >= 0", "requires n < 0 && n > 5");
    let out = driver::run(
        req(&vacuous, PropertyKind::Postcondition, None),
        DriverConfig::deterministic(checker()),
    )
    .await;
    assert_eq!(out.response.verdict, Verdict::SpecInconsistent);
    assert_eq!(
        out.response.failure.expect("classified").class,
        FailureClass::SpecBug
    );
}

#[tokio::test]
async fn an_unsupported_language_is_refused_at_admission() {
    let Some(_) = system_solver::z3_or_skip("an_unsupported_language_is_refused_at_admission")
    else {
        return;
    };
    let mut r = req(SUM, PropertyKind::Postcondition, None);
    r.source.language = "rust".into();
    let out = driver::run(r, DriverConfig::deterministic(checker())).await;
    assert_eq!(out.response.verdict, Verdict::Unsupported);
    assert_eq!(
        out.response.failure.expect("classified").class,
        FailureClass::UnsupportedFeature
    );
}

#[tokio::test]
async fn a_malformed_annotation_is_a_proposer_fault_that_the_next_round_repairs() {
    let Some(_) = system_solver::z3_or_skip(
        "a_malformed_annotation_is_a_proposer_fault_that_the_next_round_repairs",
    ) else {
        return;
    };
    let bare = SUM.replace(
        "\n    invariant i >= 0 && i <= n && r == i * (i + 1) / 2",
        "",
    );
    let bad = ScriptedProposer::new(
        "scripted-bad-then-good",
        vec![Proposal {
            annotations: vec![Annotation {
                target: "loop1".into(),
                kind: "invariant".into(),
                text: "i >< n".into(),
            }],
            tokens: 7,
            tokens_estimated: false,
            label: "scripted-bad-then-good".into(),
        }],
    )
    .with_refinement(Proposal {
        annotations: vec![
            Annotation {
                target: "loop1".into(),
                kind: "invariant".into(),
                text: "i >= 0".into(),
            },
            Annotation {
                target: "loop1".into(),
                kind: "invariant".into(),
                text: "i <= n".into(),
            },
            Annotation {
                target: "loop1".into(),
                kind: "invariant".into(),
                text: "r == i * (i + 1) / 2".into(),
            },
        ],
        tokens: 11,
        tokens_estimated: false,
        label: "scripted-bad-then-good".into(),
    });

    let mut cfg = DriverConfig::deterministic(checker());
    cfg.proposer = Some(std::sync::Arc::new(bad));
    let out = driver::run(req(&bare, PropertyKind::Postcondition, None), cfg).await;
    assert_eq!(
        out.response.verdict,
        Verdict::Proved,
        "{:?}",
        out.response.failure
    );
    assert_eq!(
        out.response.rounds.len(),
        2,
        "the first round must be recorded as malformed"
    );
    assert_eq!(
        out.response.rounds[0].class,
        Some(FailureClass::ProposerMalformed)
    );
    assert_eq!(out.response.cost.tokens, 18);
}

#[tokio::test]
async fn termination_needs_a_variant_and_a_bad_one_is_caught() {
    let Some(_) = system_solver::z3_or_skip("termination_needs_a_variant_and_a_bad_one_is_caught")
    else {
        return;
    };
    let source = r#"
fn down(n: int) -> int
  requires n >= 0
  ensures result == 0
{
  var i: int = n;
  while (i > 0)
    invariant i >= 0
    variant i
  {
    i = i - 1;
  }
  return 0;
}
"#;
    let out = driver::run(
        req(source, PropertyKind::Termination, None),
        DriverConfig::deterministic(checker()),
    )
    .await;
    assert_eq!(
        out.response.verdict,
        Verdict::Proved,
        "{:?}",
        out.response.failure
    );

    let bad = source.replace("variant i\n", "");
    let out = driver::run(
        req(&bad, PropertyKind::Termination, None),
        DriverConfig::deterministic(checker()),
    )
    .await;
    assert_ne!(out.response.verdict, Verdict::Proved);
    assert_eq!(
        out.response.failure.expect("classified").class,
        FailureClass::InsufficientInvariant
    );
}

#[tokio::test]
async fn a_tampered_certificate_fails_recheck() {
    let Some(_) = system_solver::z3_or_skip("a_tampered_certificate_fails_recheck") else {
        return;
    };
    let out = driver::run(
        req(SUM, PropertyKind::Postcondition, None),
        DriverConfig::deterministic(checker()),
    )
    .await;
    let original = out.certificate.clone().expect("certificate");
    let mut certificate: Certificate = original.clone();
    // Flip one status: the digest no longer matches, so a third party must
    // reject the artifact outright.
    certificate.body.obligations[0].status = polymorph_ais::types::ObligationStatus::Refuted;
    let outcome = cert::recheck(&certificate, Some(SUM.as_bytes()), &checker());
    assert!(!outcome.ok);
    assert!(outcome
        .checks
        .iter()
        .any(|c| c.name == "self_digest" && !c.ok));

    // And a certificate bound to different source bytes must fail too.
    let outcome = cert::recheck(&original, Some(b"fn other() { }"), &checker());
    assert!(!outcome.ok);
    assert!(outcome
        .checks
        .iter()
        .any(|c| c.name == "source_digest" && !c.ok));
}

#[tokio::test]
async fn zero_obligation_units_are_refused_not_proved() {
    let Some(_) = system_solver::z3_or_skip("zero_obligation_units_are_refused_not_proved") else {
        return;
    };
    // `fn f() { }` with a default postcondition property (no `ensures` in the
    // unit, no expression in the request) elaborates to zero obligations. Zero
    // obligations vacuously discharge, which must read as a refusal, never as a
    // proof with a certificate over nothing.
    let out = driver::run(
        req("fn f() { }", PropertyKind::Postcondition, None),
        DriverConfig::deterministic(checker()),
    )
    .await;
    assert_ne!(out.response.verdict, Verdict::Proved);
    let failure = out.response.failure.expect("a refusal must be classified");
    assert_eq!(
        failure.class,
        FailureClass::UnsupportedFeature,
        "{failure:?}"
    );
    assert!(failure.message.contains("nothing to prove"), "{failure:?}");
    assert!(out.certificate.is_none());
}

#[tokio::test]
async fn a_false_function_beside_a_true_entry_is_not_proved() {
    let Some(_) = system_solver::z3_or_skip("a_false_function_beside_a_true_entry_is_not_proved")
    else {
        return;
    };
    // Every function in the unit is proved, not only the entry (the last
    // function, by default): a false function beside a true entry must not
    // report `proved`; one false function fails the whole unit.
    let false_beside_true = r#"
fn bad() -> int
  ensures result == 2
{
  return 1;
}

fn good() -> int
  ensures result == 1
{
  return 1;
}
"#;
    let out = driver::run(
        req(false_beside_true, PropertyKind::Postcondition, None),
        DriverConfig::deterministic(checker()),
    )
    .await;
    assert_ne!(
        out.response.verdict,
        Verdict::Proved,
        "{:?}",
        out.response.failure
    );
    // The bounded check covers the non-entry function too, so its refuted
    // obligation is upgraded into a witnessed disproof of the unit.
    assert_eq!(
        out.response.verdict,
        Verdict::Disproved,
        "{:?}",
        out.response.failure
    );
    let failure = out
        .response
        .failure
        .expect("a false function must be classified");
    assert!(
        failure.obligations.iter().any(|o| o.starts_with("bad."))
            || failure.message.contains("bad"),
        "the failure must name the false function `bad`: {failure:?}"
    );
    let witnessed: Vec<_> = out
        .response
        .counterexamples
        .iter()
        .filter(|c| c.witnessed)
        .collect();
    assert!(
        witnessed.iter().any(|c| c.function == "bad"),
        "a refuted non-entry obligation must upgrade with a real witnessed \
         execution: {:?}",
        out.response.counterexamples
    );

    // The both-true variant proves, and its certificate shows that both
    // functions were elaborated, not only the entry.
    let both_true = false_beside_true.replace("ensures result == 2", "ensures result == 1");
    let out = driver::run(
        req(&both_true, PropertyKind::Postcondition, None),
        DriverConfig::deterministic(checker()),
    )
    .await;
    assert_eq!(
        out.response.verdict,
        Verdict::Proved,
        "{:?}",
        out.response.failure
    );
    let certificate = out.certificate.expect("a proof carries a certificate");
    assert!(certificate.digest_ok(), "certificate self-digest must hold");
    let elaborated: Vec<&str> = certificate
        .body
        .obligations
        .iter()
        .map(|o| o.function.as_str())
        .collect();
    assert!(
        elaborated.contains(&"bad"),
        "the non-entry function `bad` must appear in the certificate: {elaborated:?}"
    );
    assert!(
        elaborated.contains(&"good"),
        "the entry function `good` must appear in the certificate: {elaborated:?}"
    );
}

#[tokio::test]
async fn a_doctored_property_fails_full_recheck_but_passes_digest_only() {
    let Some(_) =
        system_solver::z3_or_skip("a_doctored_property_fails_full_recheck_but_passes_digest_only")
    else {
        return;
    };
    // Replaying a payload says nothing about *why* those queries exist. A
    // forger who rewrites the property and recomputes the plain body digest
    // produces an internally consistent certificate that pure replay accepts;
    // a full re-check must re-derive the obligations from the source and catch
    // the swap, while digest-only explicitly declines to re-derive and says so.
    let checker = checker();
    let out = driver::run(
        req(SUM, PropertyKind::Postcondition, None),
        DriverConfig::deterministic(checker.clone()),
    )
    .await;
    assert_eq!(
        out.response.verdict,
        Verdict::Proved,
        "{:?}",
        out.response.failure
    );
    let mut certificate = out.certificate.expect("a proof carries a certificate");
    certificate.body.property.expr = Some("result == 2 * n".into());
    certificate.digest = cert::sha256_tag(&certificate.canonical_body().unwrap());
    assert!(
        certificate.digest_ok(),
        "the forger's artifact is self-consistent"
    );

    let outcome = cert::recheck(&certificate, Some(SUM.as_bytes()), &checker);
    assert!(!outcome.ok, "a doctored property must fail a full re-check");
    assert_eq!(
        outcome.first_failure().expect("a failing check").name,
        "payload_rederived",
        "checks: {:?}",
        outcome.checks
    );

    let sources = vec![("unit.ais".to_string(), SUM.as_bytes().to_vec())];
    let outcome = cert::recheck_sources(
        &certificate,
        Some(&sources),
        &checker,
        cert::RecheckMode::DigestOnly,
    );
    assert!(
        outcome.ok,
        "digest-only declines to re-derive: {:?}",
        outcome.first_failure()
    );
    let mode = outcome
        .checks
        .iter()
        .find(|c| c.name == "mode")
        .expect("digest-only reports its mode");
    assert!(mode.ok);
    assert!(mode.detail.contains("not re-derived"), "{}", mode.detail);
}

#[tokio::test]
async fn a_backend_digest_mismatch_fails_recheck() {
    let Some(_) = system_solver::z3_or_skip("a_backend_digest_mismatch_fails_recheck") else {
        return;
    };
    // A certificate is pinned to the solver binary that proved it. A
    // re-check under a different binary must fail at the `backend` check —
    // naming both digests — not silently replay the queries through some other
    // solver.
    let checker = checker();
    let out = driver::run(
        req(SUM, PropertyKind::Postcondition, None),
        DriverConfig::deterministic(checker.clone()),
    )
    .await;
    assert_eq!(
        out.response.verdict,
        Verdict::Proved,
        "{:?}",
        out.response.failure
    );
    let mut certificate = out.certificate.expect("a proof carries a certificate");
    certificate.body.backend.binary_digest = format!("sha256:{}", "0".repeat(64));
    certificate.digest = cert::sha256_tag(&certificate.canonical_body().unwrap());
    assert!(
        certificate.digest_ok(),
        "the doctored body is self-consistent"
    );

    let outcome = cert::recheck(&certificate, Some(SUM.as_bytes()), &checker);
    assert!(
        !outcome.ok,
        "a different backend pin must fail the re-check"
    );
    let failure = outcome.first_failure().expect("a failing check");
    assert_eq!(failure.name, "backend", "checks: {:?}", outcome.checks);
    assert!(
        failure
            .detail
            .contains(&certificate.body.backend.binary_digest),
        "the detail must name the pinned digest: {}",
        failure.detail
    );
    assert!(
        failure.detail.contains(&checker.binary_digest),
        "the detail must name the local checker's digest: {}",
        failure.detail
    );
}

#[tokio::test]
async fn a_multi_file_certificate_rechecks_from_its_files() {
    let Some(_) = system_solver::z3_or_skip("a_multi_file_certificate_rechecks_from_its_files")
    else {
        return;
    };
    // A certificate over a multi-file unit re-checks from the files it
    // pins — each pin's digest and length verified individually, the files
    // joined in pin order — and one wrong file fails the `source_files` check
    // instead of panicking.
    let file_a = r#"fn left() -> int
  ensures result == 1
{
  return 1;
}
"#;
    let file_b = r#"fn right() -> int
  ensures result == 2
{
  return 2;
}
"#;
    let request = Request {
        source: Source {
            language: "ais-core".into(),
            files: vec![
                FileSpec {
                    path: "left.ais".into(),
                    text: Some(file_a.to_string()),
                },
                FileSpec {
                    path: "right.ais".into(),
                    text: Some(file_b.to_string()),
                },
            ],
            entry: None,
        },
        property: Property {
            kind: PropertyKind::Postcondition,
            expr: None,
            assumptions: Vec::new(),
            at: None,
        },
        backend: BackendSpec {
            name: "smtlib2".into(),
            binary: None,
            version: None,
        },
        budget: polymorph_ais::types::Budget {
            max_rounds: 3,
            solver_timeout_s: 20,
            token_budget: 0,
            total_wall_s: 120,
        },
        determinism: Determinism::default(),
        emit: vec![Emit::Certificate],
    };
    let out = driver::run(request, DriverConfig::deterministic(checker())).await;
    assert_eq!(
        out.response.verdict,
        Verdict::Proved,
        "{:?}",
        out.response.failure
    );
    let certificate = out.certificate.expect("a proof carries a certificate");
    assert_eq!(certificate.body.source_files.len(), 2);
    assert_eq!(certificate.body.entry, "right");

    let sources = vec![
        ("left.ais".to_string(), file_a.as_bytes().to_vec()),
        ("right.ais".to_string(), file_b.as_bytes().to_vec()),
    ];
    let outcome = cert::recheck_sources(
        &certificate,
        Some(&sources),
        &checker(),
        cert::RecheckMode::Full,
    );
    assert!(outcome.ok, "recheck failed: {:?}", outcome.first_failure());
    assert!(
        outcome
            .checks
            .iter()
            .any(|c| c.name == "source_files" && c.ok),
        "per-file pins must be verified individually: {:?}",
        outcome.checks
    );

    // Paths are matched to the pins, so supplying the files in the other order
    // still joins them in the certificate's pin order.
    let swapped = vec![sources[1].clone(), sources[0].clone()];
    let outcome = cert::recheck_sources(
        &certificate,
        Some(&swapped),
        &checker(),
        cert::RecheckMode::Full,
    );
    assert!(
        outcome.ok,
        "pin-order join failed: {:?}",
        outcome.first_failure()
    );

    // One wrong file: the per-file pin check must fail, by name.
    let mut doctored = sources.clone();
    doctored[1] = (
        "right.ais".to_string(),
        file_b
            .replace("ensures result == 2", "ensures result == 3")
            .into_bytes(),
    );
    let outcome = cert::recheck_sources(
        &certificate,
        Some(&doctored),
        &checker(),
        cert::RecheckMode::Full,
    );
    assert!(!outcome.ok, "a wrong file must fail the re-check");
    assert!(
        outcome
            .checks
            .iter()
            .any(|c| c.name == "source_files" && !c.ok),
        "the per-file pin check must fail: {:?}",
        outcome.checks
    );
}

#[tokio::test]
async fn infer_is_reported_as_not_inferred() {
    let Some(_) = system_solver::z3_or_skip("infer_is_reported_as_not_inferred") else {
        return;
    };
    // `kind: infer` does not synthesize a postcondition. It proves the
    // unit's own `ensures` exactly like `postcondition`, and the response must
    // say so: `inferred: false`, never a silent claim of inference.
    let source = "fn f(n: int) -> int\n  ensures result == n\n{\n  return n;\n}\n";
    let out = driver::run(
        req(source, PropertyKind::Infer, None),
        DriverConfig::deterministic(checker()),
    )
    .await;
    assert_eq!(
        out.response.verdict,
        Verdict::Proved,
        "{:?}",
        out.response.failure
    );
    assert!(out.certificate.is_some(), "a proof carries a certificate");
    assert!(
        !out.response.inferred,
        "infer must be reported as not inferred"
    );
}

#[tokio::test]
async fn old_in_a_postcondition_reports_the_expressiveness_ceiling() {
    let Some(_) =
        system_solver::z3_or_skip("old_in_a_postcondition_reports_the_expressiveness_ceiling")
    else {
        return;
    };
    // `old(...)` parses, so it is refused at elaboration as an
    // expressiveness ceiling — the construct and function named — instead of
    // misreporting a parse error as `unsupported_feature`.
    let source = "fn f(x: int) -> int\n  ensures result == old(x)\n{\n  return x;\n}\n";
    let out = driver::run(
        req(source, PropertyKind::Postcondition, None),
        DriverConfig::deterministic(checker()),
    )
    .await;
    assert_eq!(
        out.response.verdict,
        Verdict::Unsupported,
        "{:?}",
        out.response.failure
    );
    let failure = out
        .response
        .failure
        .expect("the ceiling must be classified");
    assert_eq!(
        failure.class,
        FailureClass::ExpressivenessCeiling,
        "{failure:?}"
    );
    assert!(failure.needs_human, "a ceiling escalates to a human");
    assert!(
        failure.message.contains("old") && failure.message.contains("`f`"),
        "the refusal must name the construct and the function: {failure:?}"
    );
    assert!(out.certificate.is_none());
}

#[tokio::test]
async fn a_call_expression_reports_the_expressiveness_ceiling() {
    let Some(_) = system_solver::z3_or_skip("a_call_expression_reports_the_expressiveness_ceiling")
    else {
        return;
    };
    // A call expression parses, so it is refused at elaboration as an
    // expressiveness ceiling instead of a parse error.
    let source =
        "fn f(x: int) -> int\n{\n  return g(x);\n}\nfn g(x: int) -> int\n{\n  return x;\n}\n";
    let out = driver::run(
        req(source, PropertyKind::Postcondition, None),
        DriverConfig::deterministic(checker()),
    )
    .await;
    assert_eq!(
        out.response.verdict,
        Verdict::Unsupported,
        "{:?}",
        out.response.failure
    );
    let failure = out
        .response
        .failure
        .expect("the ceiling must be classified");
    assert_eq!(
        failure.class,
        FailureClass::ExpressivenessCeiling,
        "{failure:?}"
    );
    assert!(failure.needs_human, "a ceiling escalates to a human");
    assert!(
        failure.message.contains("g") && failure.message.contains("`f`"),
        "the refusal must name the construct and the function: {failure:?}"
    );
    assert!(out.certificate.is_none());
}

#[tokio::test]
async fn a_call_statement_reports_the_expressiveness_ceiling() {
    let Some(_) = system_solver::z3_or_skip("a_call_statement_reports_the_expressiveness_ceiling")
    else {
        return;
    };
    // A call statement parses, so it is refused at elaboration as an
    // expressiveness ceiling instead of a parse error.
    let source = "fn f() { helper(); }\nfn helper() { }\n";
    let out = driver::run(
        req(source, PropertyKind::Postcondition, None),
        DriverConfig::deterministic(checker()),
    )
    .await;
    assert_eq!(
        out.response.verdict,
        Verdict::Unsupported,
        "{:?}",
        out.response.failure
    );
    let failure = out
        .response
        .failure
        .expect("the ceiling must be classified");
    assert_eq!(
        failure.class,
        FailureClass::ExpressivenessCeiling,
        "{failure:?}"
    );
    assert!(failure.needs_human, "a ceiling escalates to a human");
    assert!(
        failure.message.contains("helper") && failure.message.contains("`f`"),
        "the refusal must name the construct and the function: {failure:?}"
    );
    assert!(out.certificate.is_none());
}

#[tokio::test]
async fn a_lemma_block_reports_the_expressiveness_ceiling() {
    let Some(_) = system_solver::z3_or_skip("a_lemma_block_reports_the_expressiveness_ceiling")
    else {
        return;
    };
    // A `lemma` block parses in `fn`'s shape, and is refused at elaboration
    // as an expressiveness ceiling instead of a parse error.
    let source = "lemma helper() { }\nfn f() -> int\n  ensures result == 1\n{\n  return 1;\n}\n";
    let out = driver::run(
        req(source, PropertyKind::Postcondition, None),
        DriverConfig::deterministic(checker()),
    )
    .await;
    assert_eq!(
        out.response.verdict,
        Verdict::Unsupported,
        "{:?}",
        out.response.failure
    );
    let failure = out
        .response
        .failure
        .expect("the ceiling must be classified");
    assert_eq!(
        failure.class,
        FailureClass::ExpressivenessCeiling,
        "{failure:?}"
    );
    assert!(failure.needs_human, "a ceiling escalates to a human");
    assert!(
        failure.message.contains("lemma") && failure.message.contains("helper"),
        "the refusal must name the construct and the function: {failure:?}"
    );
    assert!(out.certificate.is_none());
}

#[tokio::test]
async fn a_genuinely_malformed_unit_is_still_a_parse_error() {
    let Some(_) = system_solver::z3_or_skip("a_genuinely_malformed_unit_is_still_a_parse_error")
    else {
        return;
    };
    // The grammar admits `old(...)`, calls and lemma blocks; genuinely
    // malformed syntax must remain a parse error reported as
    // `unsupported_feature`, distinct from the ceiling class.
    let out = driver::run(
        req(
            "fn f(x: int) -> int { return x }\n",
            PropertyKind::Postcondition,
            None,
        ),
        DriverConfig::deterministic(checker()),
    )
    .await;
    assert_eq!(
        out.response.verdict,
        Verdict::Unsupported,
        "{:?}",
        out.response.failure
    );
    let failure = out
        .response
        .failure
        .expect("a parse fault must be classified");
    assert_eq!(
        failure.class,
        FailureClass::UnsupportedFeature,
        "{failure:?}"
    );
    assert!(failure.message.contains("did not parse"), "{failure:?}");
    assert!(!failure.needs_human, "a parse error does not escalate");
    assert!(out.certificate.is_none());
}

/// Forge a certificate the way a forger would: elaborate the unit directly with
/// the public API, mark every obligation discharged without ever running a
/// checker round, and pin the local checker's backend digest so the replay side
/// of the re-check passes. The forgery is not in the digests — the body is
/// self-consistent and the payload re-derives — it is in what the certificate
/// claims, so only the re-check's own guards can catch it.
fn forged_certificate(
    source: &str,
    entry: &str,
    annotations: &[Annotation],
    program: &polymorph_ais::ir::Program,
    checker: &Checker,
) -> Certificate {
    let set =
        polymorph_ais::vc::elaborate(program, entry, &polymorph_ais::vc::Goal::default(), 20, 0)
            .expect("the forged unit elaborates");
    let statuses: std::collections::BTreeMap<String, polymorph_ais::types::ObligationStatus> = set
        .obligations
        .iter()
        .map(|o| {
            (
                o.id.clone(),
                polymorph_ais::types::ObligationStatus::Discharged,
            )
        })
        .collect();
    Certificate::build(
        Property {
            kind: PropertyKind::Postcondition,
            expr: None,
            assumptions: Vec::new(),
            at: None,
        },
        entry.to_string(),
        0,
        20,
        cert::sha256_tag(source),
        vec![cert::SourcePin {
            path: "unit.ais".into(),
            digest: cert::sha256_tag(source),
            bytes: u64::try_from(source.len()).unwrap_or(u64::MAX),
        }],
        annotations.to_vec(),
        cert::BackendPin {
            name: "smtlib2".into(),
            version: checker.version.clone(),
            binary_digest: checker.binary_digest.clone(),
            command: checker.spec.command.clone(),
        },
        &set.obligations,
        &statuses,
        "forged transcript",
        "polymorph proof recheck --cert <certificate.json>",
    )
    .expect("the forged certificate assembles")
}

#[tokio::test]
async fn a_contradictory_invariant_certificate_fails_recheck() {
    let Some(_) = system_solver::z3_or_skip("a_contradictory_invariant_certificate_fails_recheck")
    else {
        return;
    };
    // A contradictory invariant set discharges every obligation for free,
    // so a forged certificate over one re-derives cleanly and every obligation
    // it carries replays `unsat` — replay alone would accept it.
    // The Full re-check must re-run the driver's invariant-consistency guard
    // under the pinned checker and refuse.
    let checker = checker();
    let source = SUM.replace(
        "invariant i >= 0 && i <= n && r == i * (i + 1) / 2",
        "invariant i >= 0 && i <= n && r == i * (i + 1) / 2\n    invariant i < 0",
    );
    let annotations = vec![
        Annotation {
            target: "loop1".into(),
            kind: "invariant".into(),
            text: "i >= 0 && i <= n && r == i * (i + 1) / 2".into(),
        },
        Annotation {
            target: "loop1".into(),
            kind: "invariant".into(),
            text: "i < 0".into(),
        },
    ];
    let mut program = polymorph_ais::ir::Program::parse(&source).expect("the unit parses");
    let forged: Vec<polymorph_ais::ir::Expr> = annotations
        .iter()
        .map(|a| polymorph_ais::vc::parse_annotation(&a.text).expect("annotation parses"))
        .collect();
    assert!(
        program.set_invariants("loop1", forged),
        "SUM's only loop is loop1"
    );
    let certificate = forged_certificate(&source, "sum", &annotations, &program, &checker);

    let outcome = cert::recheck(&certificate, Some(source.as_bytes()), &checker);
    assert!(
        !outcome.ok,
        "a contradictory invariant set must fail a full re-check: {:?}",
        outcome.first_failure()
    );
    let row = outcome
        .checks
        .iter()
        .find(|c| c.name == "consistency")
        .expect("a full re-check reports a consistency row");
    assert!(!row.ok, "the consistency row must fail: {row:?}");
    assert!(
        row.detail.contains("contradictory invariant set"),
        "the detail must name the fault: {row:?}"
    );
    assert!(row.detail.contains("loop1"), "the loop is named: {row:?}");

    // The same unit is refused at the driver layer too: the round check flags
    // the contradiction as an unsound invariant before any certificate exists.
    let out = driver::run(
        req(&source, PropertyKind::Postcondition, None),
        DriverConfig::deterministic(checker),
    )
    .await;
    assert_ne!(
        out.response.verdict,
        Verdict::Proved,
        "{:?}",
        out.response.failure
    );
    assert!(out.response.certificate.is_none());
}

#[tokio::test]
async fn a_vacuous_precondition_certificate_fails_recheck() {
    let Some(_) = system_solver::z3_or_skip("a_vacuous_precondition_certificate_fails_recheck")
    else {
        return;
    };
    // A vacuous specification proves nothing, but a forged certificate over
    // one re-derives cleanly and replays `unsat` on every obligation — the
    // preconditions make them all hold for free. The Full re-check must re-run
    // the precondition satisfiability guard under the pinned checker.
    let checker = checker();
    let source = "fn f(n: int) -> int\n  requires n < 0 && n > 5\n  ensures result == n\n{\n  return n;\n}\n";
    let program = polymorph_ais::ir::Program::parse(source).expect("the unit parses");
    let certificate = forged_certificate(source, "f", &[], &program, &checker);

    let outcome = cert::recheck(&certificate, Some(source.as_bytes()), &checker);
    assert!(
        !outcome.ok,
        "a vacuous specification must fail a full re-check: {:?}",
        outcome.first_failure()
    );
    let row = outcome
        .checks
        .iter()
        .find(|c| c.name == "precondition")
        .expect("a full re-check reports a precondition row");
    assert!(!row.ok, "the precondition row must fail: {row:?}");
    assert!(
        row.detail.contains("vacuous specification"),
        "the detail must name the fault: {row:?}"
    );
    assert!(row.detail.contains("f"), "the function is named: {row:?}");

    // The driver layer refuses the same unit at precheck, before any round.
    let out = driver::run(
        req(source, PropertyKind::Postcondition, None),
        DriverConfig::deterministic(checker),
    )
    .await;
    assert_ne!(
        out.response.verdict,
        Verdict::Proved,
        "{:?}",
        out.response.failure
    );
    assert_eq!(out.response.verdict, Verdict::SpecInconsistent);
    assert!(out.response.certificate.is_none());
}

#[tokio::test]
async fn a_zero_obligation_certificate_fails_recheck() {
    let Some(_) = system_solver::z3_or_skip("a_zero_obligation_certificate_fails_recheck") else {
        return;
    };
    // A certificate over an empty obligation set is not a proof — the
    // empty set discharges vacuously, and the driver already refuses to *prove*
    // such a unit. The re-check must refuse the artifact for the same reason,
    // even though nothing replays and everything else about it is consistent.
    let checker = checker();
    let source = "fn f() { }";
    let program = polymorph_ais::ir::Program::parse(source).expect("the unit parses");
    let certificate = forged_certificate(source, "f", &[], &program, &checker);
    assert!(
        certificate.body.obligations.is_empty(),
        "the fixture really is a zero-obligation certificate"
    );

    let outcome = cert::recheck(&certificate, Some(source.as_bytes()), &checker);
    assert!(
        !outcome.ok,
        "a zero-obligation certificate must fail a full re-check: {:?}",
        outcome.first_failure()
    );
    let row = outcome
        .checks
        .iter()
        .find(|c| c.name == "obligations")
        .expect("the re-check reports an obligations row");
    assert!(!row.ok, "the obligations row must fail: {row:?}");
    assert!(
        row.detail.contains("no obligations"),
        "the detail must say so: {row:?}"
    );

    // The driver layer refuses the same unit: never proved, no artifact.
    let out = driver::run(
        req(source, PropertyKind::Postcondition, None),
        DriverConfig::deterministic(checker),
    )
    .await;
    assert_ne!(
        out.response.verdict,
        Verdict::Proved,
        "{:?}",
        out.response.failure
    );
    assert!(out.certificate.is_none());
}
