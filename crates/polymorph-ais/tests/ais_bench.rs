//! The benchmark as a regression test: the corpus's intended outcomes are the
//! assertions. A mismatch is a real finding and fails the build.

#[path = "support/system_solver.rs"]
mod system_solver;

use polymorph_ais::bench;

#[tokio::test]
async fn benchmark_matches_its_declared_expectations() {
    if system_solver::z3_or_skip("benchmark_matches_its_declared_expectations").is_none() {
        return;
    }
    let checker = match bench::default_checker() {
        Ok(c) => c,
        Err(e) => panic!("the benchmark checker did not resolve: {e}"),
    };
    let report = bench::run(checker).await;
    println!("{}", report.table());
    let mismatched: Vec<String> = report
        .results
        .iter()
        .filter(|r| !r.matched)
        .map(|r| {
            format!(
                "{}: expected {} got {:?}{}",
                r.case.name,
                r.case.expect.label(),
                r.verdict,
                r.class.map(|c| format!(" ({c:?})")).unwrap_or_default()
            )
        })
        .collect();
    assert!(
        mismatched.is_empty(),
        "benchmark mismatches:\n{}\n\n{}",
        mismatched.join("\n"),
        report.table()
    );
    // The proof-rate claim is a floor, not a target: at least 60% of the cases
    // that should be proved are proved by the deterministic pool alone.
    let rate = report.proved() as f64 / report.provable().max(1) as f64;
    assert!(
        rate >= 0.6,
        "deterministic proof rate {rate:.2} is below the floor\n{}",
        report.table()
    );
}
