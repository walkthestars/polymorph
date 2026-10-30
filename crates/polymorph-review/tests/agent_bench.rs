//! Agent-bench seeded-corpus driver.
//!
//! Runs every task of `agent_bench/agent_bench_corpus.toml` from a failing start to a passing finish
//! through the shared [`polymorph_review::bench`] API (loader, tempdir
//! materialization, deadline-bound `cargo test`): write initial files → suite
//! must FAIL naming every fail_to_pass (pass_to_pass must read `ok`) → write
//! fix files → suite exit 0. Prints a per-task board plus
//! `AGENT-BENCH SCORE: n/10`; the gate is 100% (B6 precedent).

use polymorph_review::bench::{cargo_test, load_corpus, write_files, CARGO_DEADLINE};

#[test]
fn agent_bench_corpus_red_then_green() {
    let src = include_str!("agent_bench/agent_bench_corpus.toml");
    let corpus = load_corpus(src).expect("corpus TOML must parse");
    assert_eq!(corpus.task.len(), 10, "corpus pins 10 tasks");
    let mut passed = 0u32;
    for task in &corpus.task {
        assert!(
            !task.instruction.trim().is_empty(),
            "{}: instruction must be non-empty (agent-facing text)",
            task.id
        );
        assert!(
            !task.fail_to_pass.is_empty(),
            "{}: needs at least one fail_to_pass test",
            task.id
        );
        let dir = tempfile::tempdir().unwrap();
        write_files(dir.path(), &task.files).unwrap();
        let (red_out, red_ok) = cargo_test(dir.path(), CARGO_DEADLINE);
        let mut red_good = !red_ok && red_out.contains("test result: FAILED");
        for name in &task.fail_to_pass {
            // `test tests::<name>... FAILED` — substring on the bare name.
            if !(red_out.contains(name.as_str()) && red_out.contains("FAILED")) {
                red_good = false;
            }
        }
        for name in &task.pass_to_pass {
            // `test tests::<name>... ok` — the passing tests really passed.
            if !red_out.contains(&format!("{name} ... ok")) {
                red_good = false;
            }
        }
        write_files(dir.path(), &task.fix.files).unwrap();
        let (green_out, green_ok) = cargo_test(dir.path(), CARGO_DEADLINE);
        let green_good = green_ok && green_out.contains("test result: ok");
        if red_good && green_good {
            passed += 1;
            println!("PASS {} ({})", task.id, task.description);
        } else {
            println!(
                "FAIL {} ({}) (red_ok={red_good} green_ok={green_good})",
                task.id, task.description
            );
            println!("--- red output ---\n{red_out}\n--- green output ---\n{green_out}");
        }
    }
    println!("AGENT-BENCH SCORE: {passed}/{}", corpus.task.len());
    assert_eq!(
        passed,
        corpus.task.len() as u32,
        "agent-bench gate is 100%: every task must admit red-then-green"
    );
}
