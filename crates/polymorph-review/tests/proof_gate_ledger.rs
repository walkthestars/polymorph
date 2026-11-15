//! What the proof gate records for the drive ledger: one summary per
//! invocation, and whether the last certificate's source bytes are still on
//! disk at drive end. Driven by the fake solver, so no `z3` is needed.

#[path = "../../polymorph-ais/tests/support/fake_solver.rs"]
mod fake_solver;

use std::time::Duration;

use polymorph_ais::types::{
    BackendSpec, Budget, Determinism, FileSpec, Property, PropertyKind, Request, Source,
};
use polymorph_review::ProofGate;

const UNIT: &str = "fn inc(x: int) -> int\n  ensures result == x + 1\n{\n  return x + 1;\n}\n";

/// A request that reads `unit.ais` from the drive's workdir, as `--proof-request`
/// does for a file the fixer edits.
fn request(file: FileSpec) -> Request {
    Request {
        source: Source {
            language: "ais-core".into(),
            files: vec![file],
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
            max_rounds: 1,
            solver_timeout_s: 5,
            token_budget: 0,
            total_wall_s: 60,
        },
        determinism: Determinism::default(),
        emit: Vec::new(),
    }
}

fn on_disk() -> FileSpec {
    FileSpec {
        path: "unit.ais".into(),
        text: None,
    }
}

fn gate(file: FileSpec, flags: &[&str]) -> ProofGate {
    ProofGate::new(
        request(file),
        fake_solver::checker(flags, Duration::from_secs(10)),
        4,
    )
}

#[tokio::test]
async fn every_invocation_is_recorded_in_order() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("unit.ais"), UNIT).unwrap();
    let g = gate(on_disk(), &["--answer", "unsat"]);
    assert!(g.history().is_empty());
    assert!(g.summary().is_none());
    assert_eq!(g.certificate_source_present(), None);

    g.check(dir.path()).await;
    g.check(dir.path()).await;
    let history = g.history();
    assert_eq!(history.len(), 2);
    assert!(history.iter().all(|s| s.verdict == "proved"));
    assert!(history.iter().all(|s| s.certificate_digest.is_some()));
    assert_eq!(g.summary().as_ref(), history.last());
}

#[tokio::test]
async fn the_certificate_source_is_present_until_the_bytes_change() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("unit.ais");
    std::fs::write(&path, UNIT).unwrap();
    let g = gate(on_disk(), &["--answer", "unsat"]);
    assert!(g.check(dir.path()).await.is_empty());
    assert_eq!(g.certificate_source_present(), Some(true));

    // A rollback that restores different bytes: the certificate no longer
    // describes the tree.
    std::fs::write(&path, UNIT.replace("x + 1;", "x + 1 ;")).unwrap();
    assert_eq!(g.certificate_source_present(), Some(false));

    // A discarded worktree: the file is gone.
    std::fs::remove_file(&path).unwrap();
    assert_eq!(g.certificate_source_present(), Some(false));

    // The original bytes back: present again (the check re-reads, it does
    // not latch).
    std::fs::write(&path, UNIT).unwrap();
    assert_eq!(g.certificate_source_present(), Some(true));
}

#[tokio::test]
async fn no_certificate_means_no_presence_claim() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("unit.ais"), UNIT).unwrap();

    // Proved first, then a failing invocation: the last word wins.
    let g = gate(on_disk(), &["--answer", "unsat"]);
    g.check(dir.path()).await;
    assert_eq!(g.certificate_source_present(), Some(true));

    let g = gate(on_disk(), &["--answer", "unknown"]);
    assert_eq!(g.check(dir.path()).await.len(), 1);
    assert_eq!(g.certificate_source_present(), None);
    let last = g.summary().unwrap();
    assert_eq!(last.verdict, "timeout");
    assert_eq!(last.class.as_deref(), Some("solver_incompleteness"));
}

#[tokio::test]
async fn inline_source_is_carried_by_the_request_and_always_present() {
    let dir = tempfile::tempdir().unwrap();
    let g = gate(
        FileSpec {
            path: "unit.ais".into(),
            text: Some(UNIT.into()),
        },
        &["--answer", "unsat"],
    );
    assert!(g.check(dir.path()).await.is_empty());
    assert_eq!(g.certificate_source_present(), Some(true));
}
