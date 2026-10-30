//! Hashline edit format (Epic B4, ADR-003).
//!
//! A line-anchored, content-hash-keyed patch language for LLM-driven edits,
//! modeled on omp's `@oh-my-pi/hashline` (OBSERVED via DeepWiki's source index
//! of `can1357/oh-my-pi`, 2026-09-07: `[PATH#TAG]` file-version binding with
//! 4-hex snapshot tags, verb ops `PUT`/`CUT`/`REM`/`MV`, single `+` payload
//! sigil, all-or-nothing in-memory preflight, original-state line numbering,
//! 100k-line range cap, no-op diagnostics, LF/BOM normalization).
//!
//! Deliberate v1 divergences from upstream (documented, not accidental):
//! - STRICT applier, no boundary repair (no echo-dropping, delimiter balance,
//!   or indentation recovery). Prompt packs teach exactness instead. Upstream
//!   parity gap, recorded in plan-chunk4.
//! - No `N*` syntactic-block ops (needs tree-sitter — Epic L). The parser
//!   recognizes the form and returns `Unsupported`, never a mis-apply.
//! - No seen-lines guard yet (needs the read ledger — session store, Epic C).
//! - Registers (`CUT ... @name` / `PUT >N @name`) work IN-patch only.
//!   Cross-patch registers need session state (Epic E).
//! - Tag hash is SHA-256 (first 4 hex as the TAG, full hex as the binding).
//!   Collision handling is therefore exact-hash comparison, strictly stronger
//!   than upstream's prefer-most-recent.
//!
//! Format (v1 grammar):
//! ```text
//! [src/main.rs#a1b2]
//! PUT 12.=15:
//! +new line one
//! +new line two
//! PUT >20:
//! +inserted after line 20
//! CUT 30.=32
//! ```
//! - Section header: `[PATH#TAG]`, TAG = exactly 4 lowercase hex.
//! - `PUT <target>:` + `+`-sigil body (replace range / block).
//! - `PUT <N:` / `PUT >N:` + body (insert before / after line N).
//! - `PUT >N @name` (no body, no colon): paste in-patch register.
//! - `CUT <target> [@name]`: delete range, optionally into a register.
//! - `REM`: delete the whole file. `MV <dest>`: rename.
//! - Targets: `N`, `N.=M`, `N=M`, `N-M`, `N..M`, `N…M` (inclusive).
//!   `N*` parses but is rejected as `Unsupported`.
//! - Body lines start with exactly one `+` sigil (`+` alone = empty line;
//!   `++x` = content `+x`). A PUT with a body-target and zero body lines is
//!   rejected (use CUT for deletes).
//! - All line numbers refer to the ORIGINAL file state and never shift across
//!   hunks in one patch. Overlapping affected ranges are rejected.

pub mod apply;
pub mod parse;
pub mod store;

pub use apply::{
    apply_text_to_disk, apply_to_disk, apply_to_strings, ApplyFileResult, DiskReport, FileAction,
};
pub use parse::{parse, serialize};
pub use store::{content_hash, SnapshotStore};

/// Hard cap on one range's span, mirroring upstream (DoS guard).
pub const MAX_RANGE_SPAN: u32 = 100_000;

/// One file section of a patch.
#[derive(Debug, Clone, PartialEq)]
pub struct FileSection {
    pub path: String,
    pub tag: String,
    pub ops: Vec<Op>,
}

/// A whole patch: one or more file sections, applied atomically.
#[derive(Debug, Clone, PartialEq)]
pub struct Patch {
    pub sections: Vec<FileSection>,
}

/// Edit operations. Line numbers are 1-based positions in the ORIGINAL file.
#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    /// `PUT N:` / `PUT N.=M:` + body — replace original lines start..=end.
    Replace {
        start: u32,
        end: u32,
        body: Vec<String>,
    },
    /// `PUT <N:` + body — insert before original line N (1..=len+1).
    InsertBefore { at: u32, body: Vec<String> },
    /// `PUT >N:` + body — insert after original line N (1..=len).
    InsertAfter { at: u32, body: Vec<String> },
    /// `PUT >N @name` — paste in-patch register after line N (1..=len).
    Paste { after: u32, register: String },
    /// `CUT N.=M [@name]` — delete lines, optionally into a register.
    Cut {
        start: u32,
        end: u32,
        register: Option<String>,
    },
    /// `REM` — delete the whole file.
    RemoveFile,
    /// `MV DEST` — rename the file.
    MoveFile { dest: String },
}

/// Failure modes. `StaleTag` is the core differentiator: the file moved under
/// the model, so the patch is refused BEFORE any mutation (never half-applied:
/// validation of every section precedes every write).
#[derive(Debug, PartialEq, thiserror::Error)]
pub enum HashlineError {
    #[error("parse error at patch line {line}: {msg}")]
    Parse { line: usize, msg: String },
    #[error("unsupported op (needs Epic L tree-sitter): {0}")]
    Unsupported(String),
    #[error("stale tag for '{path}': expected #{tag} but content changed")]
    StaleTag { path: String, tag: String },
    #[error("line range out of bounds in '{path}': {msg}")]
    OutOfBounds { path: String, msg: String },
    #[error("overlapping ops in '{path}': {msg}")]
    Overlap { path: String, msg: String },
    #[error("PUT with body-target in '{path}' has an empty body (use CUT)")]
    EmptyBody { path: String },
    #[error("unknown register '@{0}' (registers are in-patch only)")]
    MissingRegister(String),
    #[error("path escapes the workspace root: {0}")]
    Traversal(String),
    #[error("range span exceeds {MAX_RANGE_SPAN} lines: {0}")]
    RangeTooLarge(String),
    #[error("I/O error on {path}: {detail}")]
    Io { path: String, detail: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_display_is_model_readable() {
        // Error strings surface to the model in fixer loops — they must name
        // the file and the reason, never a bare code.
        let e = HashlineError::StaleTag {
            path: "a.rs".into(),
            tag: "ab12".into(),
        };
        let s = e.to_string();
        assert!(s.contains("a.rs") && s.contains("ab12"));
    }
}
