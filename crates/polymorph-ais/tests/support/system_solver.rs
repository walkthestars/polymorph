//! The system `z3` for tests that need the real solver.
//!
//! A machine without `z3` skips these tests (an early return with a `SKIPPED`
//! line on stderr) instead of panicking. CI sets `POLYMORPH_REQUIRE_Z3=1`, so
//! a broken solver provisioning there is a failure, never a silent skip.

#![allow(dead_code)]

use std::time::Duration;

use polymorph_ais::solver::{Checker, SolverSpec};

/// The resolved system `z3`, or `None` (with a `SKIPPED` note) when it is
/// missing and `POLYMORPH_REQUIRE_Z3` is unset.
pub fn z3_or_skip(test: &str) -> Option<Checker> {
    match Checker::resolve(SolverSpec::z3(Duration::from_secs(20))) {
        Ok(c) => Some(c),
        Err(e) => {
            if std::env::var_os("POLYMORPH_REQUIRE_Z3").is_some() {
                panic!("{test}: POLYMORPH_REQUIRE_Z3 is set but no `z3` resolved: {e}");
            }
            eprintln!(
                "SKIPPED {test}: no `z3` on PATH ({e}); set POLYMORPH_REQUIRE_Z3=1 to make this a failure"
            );
            None
        }
    }
}
