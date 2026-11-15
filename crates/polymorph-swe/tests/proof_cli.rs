//! `polymorph proof run` exit codes: 0 only on `proved`.
//!
//! Drives the real binary against the fake solver from `polymorph-ais`, with
//! `--deterministic` so no model is called. Each run uses a scratch working
//! directory so the CLI's logs land there, not in the crate.

#[path = "../../polymorph-ais/tests/support/fake_solver.rs"]
mod fake_solver;

use std::path::Path;
use std::process::{Command, Output};

const ID: &str = "fn id(n: int) -> int\n  ensures result == n\n{\n  return n;\n}\n";

fn request_json(source: &str) -> String {
    serde_json::json!({
        "source": {
            "language": "ais-core",
            "files": [{ "path": "unit.ais", "text": source }]
        },
        "property": { "kind": "postcondition" },
        "backend": { "name": "smtlib2" },
        "budget": {
            "max_rounds": 1,
            "solver_timeout_s": 5,
            "token_budget": 0,
            "total_wall_s": 60
        }
    })
    .to_string()
}

fn proof_run(dir: &Path, request: &str, solver_flags: &[&str], extra: &[&str]) -> Output {
    let path = dir.join("request.json");
    std::fs::write(&path, request).unwrap();
    Command::new(env!("CARGO_BIN_EXE_polymorph"))
        .current_dir(dir)
        .args(["proof", "run", "--deterministic", "--request"])
        .arg(&path)
        .arg("--solver")
        .arg(fake_solver::command_line(solver_flags))
        .args(extra)
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run the polymorph binary")
}

fn verdict(out: &Output) -> String {
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not the response JSON ({e}): {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    });
    v["verdict"].as_str().unwrap_or_default().to_string()
}

#[test]
fn proof_run_exits_zero_only_on_proved() {
    let dir = tempfile::tempdir().unwrap();
    let cert = dir.path().join("cert.json");
    let out = proof_run(
        dir.path(),
        &request_json(ID),
        &["--answer", "unsat"],
        &["--json", "--cert-out", cert.to_str().unwrap()],
    );
    assert_eq!(verdict(&out), "proved");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(cert.is_file(), "a proof writes its certificate");
}

#[test]
fn proof_run_exits_nonzero_on_every_other_verdict() {
    let cases: &[(&[&str], &str)] = &[
        (&["--answer", "unknown"], "timeout"),
        (&["--answer", "garbage"], "unsupported"),
        (&["--answer", "sat"], "budget_exhausted"),
    ];
    for (flags, expected) in cases {
        let dir = tempfile::tempdir().unwrap();
        let cert = dir.path().join("cert.json");
        let out = proof_run(
            dir.path(),
            &request_json(ID),
            flags,
            &["--json", "--cert-out", cert.to_str().unwrap()],
        );
        assert_eq!(&verdict(&out), expected, "flags {flags:?}");
        assert_ne!(
            out.status.code(),
            Some(0),
            "flags {flags:?} must not exit 0"
        );
        assert!(
            !cert.exists(),
            "flags {flags:?}: no certificate without a proof"
        );
    }
}

#[test]
fn proof_run_exits_nonzero_on_an_invalid_request() {
    let dir = tempfile::tempdir().unwrap();
    let out = proof_run(dir.path(), "{ not json", &["--answer", "unsat"], &[]);
    assert_ne!(out.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("not valid AIS JSON"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
