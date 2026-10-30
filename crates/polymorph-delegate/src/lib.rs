//! `polymorph-delegate` — subagent engine (Epic E).
//!
//! Chunk 8 (E1 bus core): spawn/steer/stop lifecycle, turn/stall budgets,
//! typed JSON results, JoinSet fan-out, worktree isolation. Turn execution
//! is a pluggable [`TurnRunner`](bus::TurnRunner) — Epic F wires the real
//! tools+LLM loop; this crate depends on core only, never tools/llm.

pub mod budget;
pub mod bus;
pub mod result;
pub mod worktree;

pub use budget::Budget;
pub use bus::{
    Bus, ChildHandle, ChildOutcome, EchoRunner, SpawnSpec, StoreTranscript, Transcript,
    TranscriptSink, TurnContext, TurnOutput, TurnRunner, WorkdirSpec,
};
pub use result::{validate_data, FinishStatus, StopReason, TypedResult};
pub use worktree::{
    branch_name, commit_all, create as create_worktree, merge_back, prune_orphans, DriveWorktree,
    KeptWorktree, MergeError, MergeReport, Worktree, WorktreeMode, MARKER as WORKTREE_MARKER,
};

/// Crate identifier. Used by smoke tests to prove the crate links.
pub fn crate_name() -> &'static str {
    "polymorph-delegate"
}

#[cfg(test)]
mod tests {
    #[test]
    fn links() {
        assert_eq!(super::crate_name(), "polymorph-delegate");
    }
}
