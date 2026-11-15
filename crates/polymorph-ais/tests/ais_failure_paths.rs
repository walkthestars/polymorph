//! The checker's failure paths, driven by a fake solver.
//!
//! `ais_core.rs` is the evidence against the real Z3; this file covers what Z3
//! will not do on demand — hang, answer `unknown`, print garbage, crash, flood
//! its pipes, interleave junk with batch markers — and shows each one lands on
//! the right verdict and class. The fake solver is trusted exactly as Z3 is
//! (the pinned backend binary is the trust boundary), so an all-`unsat` fake
//! *does* prove: that control is here too.
//! Nothing in this file needs a network, a model or a system solver.

#[path = "support/fake_solver.rs"]
mod fake_solver;

use std::time::{Duration, Instant};

use polymorph_ais::cert;
use polymorph_ais::driver::{self, DriverConfig};
use polymorph_ais::solver::{self, BatchItem, Checker, Sat};
use polymorph_ais::types::{
    BackendSpec, Budget, Determinism, Emit, FailureClass, FileSpec, Property, PropertyKind,
    Request, Response, Source, Verdict,
};

/// Loop-free, no `requires`: no precheck query, no consistency probes, one
/// postcondition obligation. Every solver call the driver makes is a round.
const ID: &str = "fn id(n: int) -> int\n  ensures result == n\n{\n  return n;\n}\n";

/// The same shape with a false postcondition: any model refutes it.
const WRONG: &str = "fn id(n: int) -> int\n  ensures result == n + 1\n{\n  return n;\n}\n";

/// A `requires` clause, so the driver issues a precondition query first.
const GUARDED: &str =
    "fn id(n: int) -> int\n  requires n >= 0\n  ensures result == n\n{\n  return n;\n}\n";

fn req(source: &str) -> Request {
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
        budget: Budget {
            max_rounds: 2,
            solver_timeout_s: 10,
            token_budget: 0,
            total_wall_s: 120,
        },
        determinism: Determinism::default(),
        emit: vec![Emit::Certificate, Emit::Ledger, Emit::Counterexamples],
    }
}

/// No pool and no Houdini: the checker is called only for the precheck and
/// the round, so the verdict is the fake's answer and nothing else.
fn cfg(checker: Checker) -> DriverConfig {
    let mut cfg = DriverConfig::deterministic(checker);
    cfg.use_pool = false;
    cfg.houdini = false;
    cfg
}

async fn drive(source: &str, flags: &[&str]) -> Response {
    let checker = fake_solver::checker(flags, Duration::from_secs(10));
    driver::run(req(source), cfg(checker)).await.response
}

fn class(r: &Response) -> Option<FailureClass> {
    r.failure.as_ref().map(|f| f.class)
}

fn item(script: &str) -> BatchItem {
    BatchItem {
        script: script.into(),
        probes: Vec::new(),
    }
}

// ---- Sat::Unsat: the control

#[tokio::test]
async fn an_unsat_answer_proves_with_a_certificate() {
    let checker = fake_solver::checker(&["--answer", "unsat"], Duration::from_secs(10));
    let out = driver::run(req(ID), cfg(checker)).await;
    assert_eq!(
        out.response.verdict,
        Verdict::Proved,
        "{:?}",
        out.response.failure
    );
    assert!(out.response.failure.is_none());
    let certificate = out.certificate.expect("a proof carries a certificate");
    assert!(certificate.digest_ok());
    assert_eq!(out.response.rounds.len(), 1);
}

// ---- Sat::Timeout

#[tokio::test]
async fn a_hung_solver_is_killed_and_reported_as_solver_incompleteness() {
    let checker = fake_solver::checker(
        &["--sleep-ms", "60000", "--answer", "unsat"],
        Duration::from_secs(1),
    );
    let started = Instant::now();
    let r = driver::run(req(ID), cfg(checker)).await.response;
    let took = started.elapsed();
    assert_eq!(r.verdict, Verdict::Timeout, "{:?}", r.failure);
    assert_eq!(class(&r), Some(FailureClass::SolverIncompleteness));
    assert!(
        r.failure.as_ref().unwrap().message.contains("timed out"),
        "{:?}",
        r.failure
    );
    // One kill per call (the round only: `ID` has no precheck), not the
    // fake's sixty seconds.
    assert!(took < Duration::from_secs(15), "took {took:?}");
}

#[test]
fn the_timeout_kill_answers_every_item_of_the_batch() {
    let checker = fake_solver::checker(&["--sleep-ms", "60000"], Duration::from_secs(1));
    let runs = checker.run_batch(&[item("(check-sat)"), item("(check-sat)")]);
    assert_eq!(runs.len(), 2);
    assert!(runs.iter().all(|r| r.sat == Sat::Timeout), "{runs:?}");
    assert!(runs.iter().all(|r| r.exit_code.is_none()));
}

// ---- Sat::Unknown

#[tokio::test]
async fn an_unknown_answer_is_solver_incompleteness_not_a_proof() {
    let r = drive(ID, &["--answer", "unknown"]).await;
    assert_eq!(r.verdict, Verdict::Timeout, "{:?}", r.failure);
    assert_eq!(class(&r), Some(FailureClass::SolverIncompleteness));
    let message = &r.failure.as_ref().unwrap().message;
    assert!(message.contains("unknown"), "{message}");
    // The round ran: this is the round path, not the precheck.
    assert_eq!(r.rounds.len(), 1);
    assert_eq!(r.rounds[0].obligations_discharged, 0);
}

#[tokio::test]
async fn an_unknown_precondition_is_solver_incompleteness_before_any_round() {
    let r = drive(GUARDED, &["--answer", "unknown"]).await;
    assert_eq!(r.verdict, Verdict::Timeout, "{:?}", r.failure);
    assert_eq!(class(&r), Some(FailureClass::SolverIncompleteness));
    assert!(r.rounds.is_empty());
}

// ---- Sat::Error

#[tokio::test]
async fn garbage_output_is_unsupported_never_proved() {
    let r = drive(ID, &["--answer", "garbage"]).await;
    assert_eq!(r.verdict, Verdict::Unsupported, "{:?}", r.failure);
    assert_eq!(class(&r), Some(FailureClass::UnsupportedFeature));
    let message = &r.failure.as_ref().unwrap().message;
    assert!(message.contains("no verdict"), "{message}");
}

#[tokio::test]
async fn a_solver_that_exits_without_a_verdict_is_unsupported() {
    let r = drive(ID, &["--answer", "silent", "--exit", "3"]).await;
    assert_eq!(r.verdict, Verdict::Unsupported, "{:?}", r.failure);
    assert_eq!(class(&r), Some(FailureClass::UnsupportedFeature));
}

#[tokio::test]
async fn a_solver_that_cannot_start_is_unsupported() {
    let mut checker = fake_solver::checker(&[], Duration::from_secs(10));
    checker.binary = checker.binary.with_file_name("no-such-solver.exe");
    let r = driver::run(req(ID), cfg(checker)).await.response;
    assert_eq!(r.verdict, Verdict::Unsupported, "{:?}", r.failure);
    let message = &r.failure.as_ref().unwrap().message;
    assert!(message.contains("cannot start solver"), "{message}");
}

#[tokio::test]
async fn a_garbage_precondition_answer_is_unsupported_before_any_round() {
    let r = drive(GUARDED, &["--answer", "garbage"]).await;
    assert_eq!(r.verdict, Verdict::Unsupported, "{:?}", r.failure);
    assert!(r.rounds.is_empty());
}

// ---- Sat::Sat

#[tokio::test]
async fn a_sat_answer_on_a_false_property_is_disproved_with_a_witness() {
    let r = drive(WRONG, &["--answer", "sat"]).await;
    assert_eq!(r.verdict, Verdict::Disproved, "{:?}", r.failure);
    assert_eq!(class(&r), Some(FailureClass::SpecBug));
    assert!(
        r.counterexamples.iter().any(|c| c.witnessed),
        "{:?}",
        r.counterexamples
    );
}

#[tokio::test]
async fn a_sat_answer_whose_model_does_not_refute_is_not_a_counterexample() {
    // The fake claims `sat` with an all-zero model, which satisfies `ID`'s
    // postcondition. The independent evaluator discards that model, so no
    // counterexample is reported and nothing is disproved — and a `sat` is
    // still never a proof.
    let r = drive(ID, &["--answer", "sat"]).await;
    assert_ne!(r.verdict, Verdict::Proved);
    assert_ne!(r.verdict, Verdict::Disproved, "{:?}", r.failure);
    assert_eq!(class(&r), Some(FailureClass::InsufficientInvariant));
    assert!(r.counterexamples.is_empty(), "{:?}", r.counterexamples);
}

#[tokio::test]
async fn an_unsat_precondition_is_spec_inconsistent() {
    let r = drive(GUARDED, &["--answer", "unsat"]).await;
    assert_eq!(r.verdict, Verdict::SpecInconsistent, "{:?}", r.failure);
    assert_eq!(class(&r), Some(FailureClass::SpecBug));
}

// ---- the sandbox

#[test]
fn the_solver_environment_is_scrubbed() {
    // Edition 2021: `set_var` is safe. No other test reads this variable.
    std::env::set_var("AIS_PLANTED_SECRET", "hunter2-planted");
    let checker = fake_solver::checker(
        &[
            "--answer",
            "unknown",
            "--echo-env",
            "AIS_PLANTED_SECRET",
            "--echo-env",
            "PATH",
        ],
        Duration::from_secs(10),
    );
    let run = checker.run_script("(check-sat)\n", &[]);
    assert_eq!(run.sat, Sat::Unknown, "{}", run.stdout);
    assert!(
        run.stdout.contains("(:echo AIS_PLANTED_SECRET <unset>)"),
        "{}",
        run.stdout
    );
    assert!(!run.stdout.contains("hunter2-planted"), "{}", run.stdout);
    // Control: the allow-listed variables do reach the child.
    assert!(
        !run.stdout.contains("(:echo PATH <unset>)"),
        "{}",
        run.stdout
    );
    std::env::remove_var("AIS_PLANTED_SECRET");
}

#[test]
fn the_solver_runs_in_a_scratch_directory_that_is_removed() {
    let checker = fake_solver::checker(
        &["--answer", "unsat", "--echo-cwd"],
        Duration::from_secs(10),
    );
    let run = checker.run_script(
        "(check-sat)
",
        &[],
    );
    assert_eq!(run.sat, Sat::Unsat, "{}", run.stdout);
    let cwd = run
        .stdout
        .lines()
        .find_map(|l| l.strip_prefix("(:cwd \"")?.strip_suffix("\")"))
        .unwrap_or_else(|| panic!("the fake reported no cwd: {}", run.stdout));
    let cwd = std::path::Path::new(cwd);
    assert!(
        cwd.file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with("ais-check-")),
        "the solver ran outside its scratch directory: {}",
        cwd.display()
    );
    assert!(
        !cwd.exists(),
        "the scratch directory leaked: {}",
        cwd.display()
    );
}

// ---- batch parsing

#[test]
fn batch_verdicts_map_by_marker_index_through_junk() {
    let checker = fake_solver::checker(&["--answer", "cycle", "--junk"], Duration::from_secs(10));
    let items: Vec<BatchItem> = (0..5).map(|_| item("(check-sat)")).collect();
    let runs = checker.run_batch(&items);
    let got: Vec<Sat> = runs.iter().map(|r| r.sat.clone()).collect();
    assert_eq!(
        got,
        vec![Sat::Unsat, Sat::Sat, Sat::Unknown, Sat::Unsat, Sat::Sat],
        "{runs:?}"
    );
    // The `@@ais 9999` marker the fake prints after each item closes the
    // segment, so the trailing `sat` junk is attributed to no item.
    assert!(runs.iter().all(|r| !r.stdout.contains("9999")));
}

#[test]
fn an_item_with_no_marker_in_the_output_is_an_error_not_a_verdict() {
    // `silent` prints nothing: every item gets an empty segment.
    let checker = fake_solver::checker(&["--answer", "silent"], Duration::from_secs(10));
    let runs = checker.run_batch(&[item("(check-sat)"), item("(check-sat)")]);
    assert_eq!(runs.len(), 2);
    assert!(
        runs.iter().all(|r| matches!(r.sat, Sat::Error(_))),
        "{runs:?}"
    );
}

#[test]
fn a_solver_that_floods_its_pipes_still_answers() {
    // Far more than any OS pipe buffer, on stdout and stderr both: the checker
    // must drain the pipes while it waits, or the child blocks on write and
    // the batch is misreported as a timeout.
    let checker = fake_solver::checker(
        &["--flood-bytes", "2000000", "--answer", "unsat"],
        Duration::from_secs(20),
    );
    let runs = checker.run_batch(&[item("(check-sat)"), item("(check-sat)")]);
    let got: Vec<Sat> = runs.iter().map(|r| r.sat.clone()).collect();
    assert_eq!(got, vec![Sat::Unsat, Sat::Unsat]);
}

// ---- emit filtering

#[tokio::test]
async fn emit_drops_the_ledger_and_counterexamples_unless_asked() {
    let checker = || fake_solver::checker(&["--answer", "sat"], Duration::from_secs(10));

    let mut full = req(WRONG);
    full.emit = vec![Emit::Ledger, Emit::Counterexamples];
    let r = driver::run(full, cfg(checker())).await.response;
    assert_eq!(r.verdict, Verdict::Disproved, "{:?}", r.failure);
    assert!(!r.rounds.is_empty());
    assert!(!r.counterexamples.is_empty());

    let mut bare = req(WRONG);
    bare.emit = vec![Emit::Certificate];
    let r = driver::run(bare, cfg(checker())).await.response;
    assert_eq!(r.verdict, Verdict::Disproved, "{:?}", r.failure);
    assert!(r.rounds.is_empty(), "{:?}", r.rounds);
    assert!(r.counterexamples.is_empty(), "{:?}", r.counterexamples);
    // The failure itself is never filtered.
    assert_eq!(class(&r), Some(FailureClass::SpecBug));

    let mut ledger_only = req(WRONG);
    ledger_only.emit = vec![Emit::Ledger];
    let r = driver::run(ledger_only, cfg(checker())).await.response;
    assert!(!r.rounds.is_empty());
    assert!(r.counterexamples.is_empty());

    // An empty `emit` means "everything".
    let mut everything = req(WRONG);
    everything.emit = Vec::new();
    let r = driver::run(everything, cfg(checker())).await.response;
    assert!(!r.rounds.is_empty());
    assert!(!r.counterexamples.is_empty());
}

// ---- wall clock

#[tokio::test]
async fn the_wall_clock_budget_stops_the_loop_before_round_zero() {
    // The precheck alone outlasts `total_wall_s = 1`: the loop must stop at
    // its first wall-clock check, with no round run and no verdict claimed.
    let checker = fake_solver::checker(
        &["--sleep-ms", "1500", "--answer", "sat"],
        Duration::from_secs(10),
    );
    let mut request = req(GUARDED);
    request.budget.total_wall_s = 1;
    let r = driver::run(request, cfg(checker)).await.response;
    assert_eq!(r.verdict, Verdict::BudgetExhausted, "{:?}", r.failure);
    assert_eq!(class(&r), Some(FailureClass::BudgetExhausted));
    let message = &r.failure.as_ref().unwrap().message;
    assert!(message.contains("wall-clock"), "{message}");
    assert!(r.rounds.is_empty());
}

// ---- certificate replay

#[tokio::test]
async fn a_hung_replay_is_killed_and_fails_recheck() {
    let prover = fake_solver::checker(&["--answer", "unsat"], Duration::from_secs(10));
    let certificate = driver::run(req(ID), cfg(prover.clone()))
        .await
        .certificate
        .expect("the control proves");
    // Control: the same binary replays `unsat` and the recheck passes.
    let outcome = cert::recheck(&certificate, Some(ID.as_bytes()), &prover);
    assert!(outcome.ok, "{:?}", outcome.first_failure());

    // Same binary (so the backend pin holds), but it never answers: the
    // replay is killed on the checker's deadline instead of hanging recheck.
    let hung = fake_solver::checker(&["--sleep-ms", "60000"], Duration::from_secs(1));
    let started = Instant::now();
    let outcome = cert::recheck(&certificate, Some(ID.as_bytes()), &hung);
    assert!(started.elapsed() < Duration::from_secs(15));
    assert!(!outcome.ok);
    let failure = outcome.first_failure().expect("a failing check");
    assert_eq!(failure.name, "replay", "{failure:?}");
    assert!(failure.detail.contains("Timeout"), "{failure:?}");
}

#[test]
fn the_replay_environment_is_scrubbed() {
    std::env::set_var("AIS_PLANTED_REPLAY_SECRET", "hunter2-replay");
    let checker = fake_solver::checker(
        &[
            "--answer",
            "unsat",
            "--echo-env",
            "AIS_PLANTED_REPLAY_SECRET",
        ],
        Duration::from_secs(10),
    );
    let run = solver::run_verbatim(
        &checker,
        "(check-sat)
",
    )
    .expect("a replay run");
    assert_eq!(run.sat, Sat::Unsat, "{}", run.stdout);
    assert!(
        run.stdout
            .contains("(:echo AIS_PLANTED_REPLAY_SECRET <unset>)"),
        "{}",
        run.stdout
    );
    std::env::remove_var("AIS_PLANTED_REPLAY_SECRET");
}

// ---- parser nesting

/// A unit as deep as the parser allows: blocks nested to the limit, then a
/// return whose right-nested sum is a deep expression tree (bare parentheses
/// build no nodes; a binary operator inside each one does).
fn deepest_unit() -> String {
    use polymorph_ais::ir::{BLOCK_NESTING_COST, MAX_NESTING};
    let blocks = (MAX_NESTING - 1) / BLOCK_NESTING_COST - 1;
    // The `return` statement itself costs a block level.
    let terms = MAX_NESTING - BLOCK_NESTING_COST - 2;
    let sum = format!("{}0{}", "x + (".repeat(terms), ")".repeat(terms));
    format!(
        "fn f(x: int) -> int
  ensures result == {sum}
{{
{}x = x;
{}return {sum};
}}
",
        "if (true) {
"
        .repeat(blocks),
        "}
"
        .repeat(blocks)
    )
}

#[test]
fn a_unit_at_the_nesting_limit_drives_on_a_small_stack() {
    // The CLI polls the driver on the main thread: 1 MiB on Windows. Parsing,
    // elaboration, rendering and the certificate must all fit there.
    let unit = deepest_unit();
    let handle = std::thread::Builder::new()
        .stack_size(1 << 20)
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            let checker = fake_solver::checker(&["--answer", "unsat"], Duration::from_secs(10));
            runtime.block_on(driver::run(req(&unit), cfg(checker)))
        })
        .unwrap();
    let out = handle.join().expect("the deepest legal unit fits in 1 MiB");
    assert_eq!(
        out.response.verdict,
        Verdict::Proved,
        "{:?}",
        out.response.failure
    );
}

#[tokio::test]
async fn a_unit_past_the_nesting_limit_is_refused_by_name() {
    let depth = 100_000;
    let unit = format!(
        "fn f(x: int) -> int\n  ensures result == {}x{}\n{{\n  return x;\n}}\n",
        "-".repeat(depth),
        ""
    );
    let r = drive(&unit, &["--answer", "unsat"]).await;
    assert_eq!(r.verdict, Verdict::Unsupported, "{:?}", r.failure);
    let message = &r.failure.as_ref().unwrap().message;
    assert!(
        message.contains(polymorph_ais::ir::NESTING_FAULT),
        "{message}"
    );
}

// ---- standard non-verdict replies and differential replay

#[test]
fn unsupported_and_success_replies_are_not_read_as_the_verdict() {
    for line in ["unsupported", "success"] {
        let checker = fake_solver::checker(
            &["--answer", "unsat", "--preamble", line],
            Duration::from_secs(10),
        );
        let runs = checker.run_batch(&[item("(check-sat)"), item("(check-sat)")]);
        assert!(runs.iter().all(|r| r.sat == Sat::Unsat), "{line}: {runs:?}");
        let replay = solver::run_verbatim(
            &checker,
            "(check-sat)
",
        )
        .unwrap();
        assert_eq!(replay.sat, Sat::Unsat, "{line}");
    }
    // Only those two words are skipped: anything else is still no verdict.
    let checker = fake_solver::checker(
        &["--answer", "unsat", "--preamble", "warning: something"],
        Duration::from_secs(10),
    );
    assert!(matches!(
        checker
            .run_script(
                "(check-sat)
",
                &[]
            )
            .sat,
        Sat::Error(_)
    ));
}

#[tokio::test]
async fn differential_replay_agrees_or_fails_the_recheck() {
    let prover = fake_solver::checker(&["--answer", "unsat"], Duration::from_secs(10));
    let certificate = driver::run(req(ID), cfg(prover.clone()))
        .await
        .certificate
        .expect("the control proves");
    let n = certificate.body.obligations.len();

    let agree = cert::differential(&certificate, &prover);
    assert!(agree.ok, "{agree:?}");
    assert_eq!(agree.name, "differential");
    assert!(agree.detail.starts_with(&format!("{n}/{n} ")), "{agree:?}");

    for answer in ["sat", "unknown", "garbage"] {
        let second = fake_solver::checker(&["--answer", answer], Duration::from_secs(10));
        let row = cert::differential(&certificate, &second);
        assert!(!row.ok, "{answer}: {row:?}");
        assert!(row.detail.contains("disagreements"), "{row:?}");

        // A failing differential row fails an otherwise green recheck.
        let mut outcome = cert::recheck(&certificate, Some(ID.as_bytes()), &prover);
        assert!(outcome.ok);
        outcome.push_check(row);
        assert!(!outcome.ok);
        assert_eq!(outcome.verdict, "failed");
    }

    let hung = fake_solver::checker(&["--sleep-ms", "60000"], Duration::from_secs(1));
    let row = cert::differential(&certificate, &hung);
    assert!(!row.ok && row.detail.contains("Timeout"), "{row:?}");
}

// ---- launcher shims

#[test]
fn a_scoop_shim_is_pinned_and_run_as_its_target() {
    let dir = tempfile::tempdir().unwrap();
    let real = fake_solver::binary();
    // A launcher that is not the solver: any other bytes will do.
    let launcher = dir
        .path()
        .join(format!("solver{}", std::env::consts::EXE_SUFFIX));
    std::fs::write(&launcher, b"not the solver").unwrap();
    std::fs::write(
        dir.path().join("solver.shim"),
        format!(
            "path = \"{}\"
",
            real.display()
        ),
    )
    .unwrap();
    let spec = polymorph_ais::solver::SolverSpec {
        command: vec![
            launcher.to_string_lossy().into_owned(),
            "--answer".into(),
            "unsat".into(),
            "{input}".into(),
        ],
        timeout: Duration::from_secs(10),
    };
    let checker = Checker::resolve(spec).unwrap();
    assert_eq!(checker.binary, real);
    assert_eq!(
        checker.binary_digest,
        cert::sha256_bytes(&std::fs::read(real).unwrap())
    );
    // And it is the target that runs.
    assert_eq!(
        checker
            .run_script(
                "(check-sat)
",
                &[]
            )
            .sat,
        Sat::Unsat
    );

    // A shim that injects args is not followed: its target alone is not
    // the command the shim runs.
    std::fs::write(
        dir.path().join("solver.shim"),
        format!(
            "path = \"{}\"
args = --answer sat
",
            real.display()
        ),
    )
    .unwrap();
    assert_eq!(polymorph_ais::solver::follow_shim(&launcher), launcher);
}
