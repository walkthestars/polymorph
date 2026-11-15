//! Backend contract: what this crate's backend accepts and the limits it declares.

use crate::solver::Checker;

/// Where a call to the primitive is accepted.
pub struct Capabilities;

impl Capabilities {
    /// Backend family name.
    pub fn name(&self) -> &'static str {
        "smtlib2"
    }
    /// Accepted source languages.
    pub fn languages(&self) -> Vec<&'static str> {
        vec!["ais-core"]
    }
    /// Declared fragment limits, so a caller can fail fast.
    pub fn limits(&self) -> Vec<&'static str> {
        vec![
            "no concurrency",
            "no heap or aliasing beyond integer arrays",
            "no floating point",
            "no pointers or references",
            "no ghost code; `old(...)`, calls and `lemma` blocks parse but are refused as an expressiveness ceiling",
            "unbounded integer arithmetic (SMT `Int`), not machine words",
        ]
    }
}

/// A resolved backend: the checker plus its declared capabilities.
pub struct Backend {
    /// The pinned checker.
    pub checker: Checker,
    /// What it accepts.
    pub capabilities: Capabilities,
}

impl Backend {
    /// Wrap a resolved checker.
    pub fn new(checker: Checker) -> Self {
        Self {
            checker,
            capabilities: Capabilities,
        }
    }
}
