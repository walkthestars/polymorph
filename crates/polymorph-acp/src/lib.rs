//! `polymorph-acp` — Full Deck epic I (see docs/architecture.md).
//!
//! Chunk 58 (design): stdio-first v1 agent settled (see docs/plan-chunk58.md).
//! Chunk 59 (substrate): `agent` module — initialize/new/prompt-echo over
//! `Channel::duplex`, no prod callers yet (CLI serve rides c60).

pub mod agent;
/// Text-only live loop binding (docket 63b, chunk 67): brief render +
/// injected-runner drive + live act-turn pieces. Wire stays in `agent`.
pub mod session_loop;

/// Crate identifier. Used by smoke tests to prove the crate links.
pub fn crate_name() -> &'static str {
    "polymorph-acp"
}

#[cfg(test)]
mod tests {
    #[test]
    fn links() {
        assert_eq!(super::crate_name(), "polymorph-acp");
    }
}
