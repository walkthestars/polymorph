//! Differential replay with real solvers: every certificate the benchmark
//! proves under z3 is replayed under cvc5, obligation by obligation. Skips
//! without either solver; this is evidence, not a CI gate.

#[path = "support/system_solver.rs"]
mod system_solver;

use std::time::Duration;

use polymorph_ais::bench::{self, Expect};
use polymorph_ais::cert;
use polymorph_ais::driver::{self, DriverConfig};
use polymorph_ais::solver::{Checker, SolverSpec};
use polymorph_ais::types::{
    BackendSpec, Budget, Determinism, Emit, FileSpec, Property, Request, Source, Verdict,
};

fn cvc5_or_skip(test: &str) -> Option<Checker> {
    let spec = SolverSpec::from_command_line("cvc5 {input}", Duration::from_secs(20)).ok()?;
    match Checker::resolve(spec) {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("SKIPPED {test}: no `cvc5` on PATH ({e})");
            None
        }
    }
}

fn request(case: &bench::Case) -> Request {
    Request {
        source: Source {
            language: case.language.into(),
            files: vec![FileSpec {
                path: format!("{}.ais", case.name),
                text: Some(case.source.to_string()),
            }],
            entry: None,
        },
        property: Property {
            kind: case.kind,
            expr: case.expr.map(str::to_string),
            assumptions: Vec::new(),
            at: None,
        },
        backend: BackendSpec {
            name: "smtlib2".into(),
            binary: None,
            version: None,
        },
        budget: Budget {
            max_rounds: 3,
            solver_timeout_s: 20,
            token_budget: 0,
            total_wall_s: 120,
        },
        determinism: Determinism::default(),
        emit: vec![Emit::Certificate],
    }
}

#[tokio::test]
async fn every_benchmark_proof_agrees_under_cvc5() {
    let name = "every_benchmark_proof_agrees_under_cvc5";
    let Some(z3) = system_solver::z3_or_skip(name) else {
        return;
    };
    let Some(cvc5) = cvc5_or_skip(name) else {
        return;
    };
    let mut proved = 0;
    let mut rows = Vec::new();
    for case in bench::corpus() {
        if !matches!(case.expect, Expect::Proved) {
            continue;
        }
        let out = driver::run(request(&case), DriverConfig::deterministic(z3.clone())).await;
        assert_eq!(out.response.verdict, Verdict::Proved, "{}", case.name);
        let certificate = out.certificate.expect("a proof carries a certificate");
        let row = cert::differential(&certificate, &cvc5);
        println!("{:<28} {}", case.name, row.detail);
        rows.push((case.name, row));
        proved += 1;
    }
    assert!(proved > 0, "the corpus proved nothing");
    let disagreements: Vec<String> = rows
        .iter()
        .filter(|(_, r)| !r.ok)
        .map(|(n, r)| format!("{n}: {}", r.detail))
        .collect();
    assert!(
        disagreements.is_empty(),
        "z3 and cvc5 disagree:\n{}",
        disagreements.join("\n")
    );
}
