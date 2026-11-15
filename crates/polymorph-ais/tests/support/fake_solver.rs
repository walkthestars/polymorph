//! Build and drive the fake solver (`tests/fake_solver/fake_solver.rs`).
//!
//! The source is embedded with `include_str!` and compiled once per test
//! binary with the `rustc` that is on `PATH` (or `$RUSTC`), into Cargo's
//! per-target scratch directory (the system temp directory outside
//! integration tests). Shared by `polymorph-ais` and `polymorph-swe`
//! tests through `#[path]`.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

use polymorph_ais::solver::{Checker, SolverSpec};

const SOURCE: &str = include_str!("../fake_solver/fake_solver.rs");

/// The compiled fake solver, built on first use.
pub fn binary() -> &'static Path {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        // One directory per test binary: two test binaries may build at once.
        let exe_stem = std::env::current_exe()
            .ok()
            .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "tests".into());
        // `CARGO_TARGET_TMPDIR` exists for integration tests only.
        let root = option_env!("CARGO_TARGET_TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let dir = root.join("fake_solver").join(exe_stem);
        std::fs::create_dir_all(&dir).expect("create the fake solver directory");
        let src = dir.join("fake_solver.rs");
        std::fs::write(&src, SOURCE).expect("write the fake solver source");
        let out = dir.join(format!("fake_solver{}", std::env::consts::EXE_SUFFIX));
        let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
        let status = Command::new(rustc)
            .args(["--edition", "2021", "--crate-name", "fake_solver", "-o"])
            .arg(&out)
            .arg(&src)
            .status()
            .expect("run rustc to build the fake solver");
        assert!(status.success(), "rustc failed to build the fake solver");
        out
    })
}

/// A checker that runs the fake solver with `flags`, killed after `timeout`.
pub fn checker(flags: &[&str], timeout: Duration) -> Checker {
    Checker::resolve(spec(flags, timeout)).expect("the fake solver resolves")
}

/// The solver spec for `flags`, for callers that resolve it themselves.
pub fn spec(flags: &[&str], timeout: Duration) -> SolverSpec {
    let mut command = vec![binary().to_string_lossy().into_owned()];
    command.extend(flags.iter().map(|f| f.to_string()));
    command.push("{input}".into());
    SolverSpec { command, timeout }
}

/// The same command as one `--solver` line, quoted for paths with spaces.
pub fn command_line(flags: &[&str]) -> String {
    let mut line = format!("\"{}\"", binary().display());
    for f in flags {
        line.push(' ');
        line.push_str(f);
    }
    line.push_str(" {input}");
    line
}
