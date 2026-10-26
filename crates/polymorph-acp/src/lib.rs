//! `polymorph-acp` — Agent Client Protocol (ACP) server over stdio.
//!
//! v1 surface: initialize / new session / prompt.
//! `agent` module — initialize/new/prompt-echo over
//! `Channel::duplex`, no production callers yet.

pub mod agent;
/// Text-only live loop binding: brief render +
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
