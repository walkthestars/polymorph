//! `polymorph-review` — clean-context reviewer loop (offline core).
//!
//! Verdict text boundary, convergence state machine, and the
//! mode-parameterized plan/act loop driver over injected async
//! [`Reviewer`]/[`Executor`] traits. No LLM, no Bus, no network binds
//! these traits to the delegate bus + D1-managed LLM path without touching F1.
//!
//! Rules (4 "Plan/Act/Review/Delegate"):
//! - Reviewer input carries goal + criteria + CURRENT artifact only. There is
//!   deliberately NO history/findings field on [`ReviewerInput`] — reviewer
//!   independence is enforced by construction, not by discipline.
//! - Verdicts are typed [`polymorph_core::Verdict`]; raw model text crosses
//!   ONLY via [`parse_verdict`](verdict::parse_verdict) (strict JSON, never
//!   prose-sniffed).
//! - Escalate-reason priority: Survivor > final-round-invalid > MaxIterations.
//! - This crate depends on core ONLY (delegate precedent).

pub mod ab;
pub mod artifact_set;
pub mod bench;
pub mod binding;
pub mod command_changes;
pub mod converge;
pub mod r#loop;
pub mod plan_drive;
pub mod restore;
pub mod review_tools;
pub mod verdict;

pub use ab::{run_ab_pair, AbCase, AbReport, ArmReport};
pub use artifact_set::{
    is_artifact_set, ArtifactSet, Change, RenderedSet, ARTIFACT_SET_HEADER, DIFF_SET_FINDING_FILE,
};
pub use bench::{
    bench_gate, bench_gate_shell, cargo_test, check_record, load_corpus, load_record,
    loop_worktree_record_profile, materialize_repo, parse_bench_arms, parse_preamble,
    preamble_record_suffix, profile_shell_rule, score_dir, single_record_profile, single_tool_list,
    snapshot_files, worktree_record_profile, write_files, write_record, BenchArm, BenchRecord,
    Corpus, Fix, NamedFile, Task, BENCH_DENY_EXTRA, CARGO_DEADLINE, SCOPED_BASH_PROFILE,
    SCOPED_PROFILE,
};
pub use binding::{
    create_job_seed, load_pack, read_attached, render_file_attachments, select_preamble,
    to_genai_tools, AdvisorNote, AdvisorRunner, AdvisorSeverity, AgenticFixRunner, AgenticTurnFn,
    AttachedFile, AttachedState, BusExecutor, BusReviewer, FixTurnRunner, HarnessGate,
    HarnessGateReviewer, LiveAgenticTurn, LlmTarget, LoopMergeConfig, LspGateRunner,
    PeripheralFiles, ReviewTurnRunner, ToolHistoryBudget, VerifyGateRunner,
    DEFAULT_MAX_BONUS_TOOL_ROUNDS, DEFAULT_TOOL_HISTORY_BUDGET_BYTES,
    DEFAULT_TOOL_HISTORY_KEEP_ROUNDS, MAX_CALLS_PER_TURN, PERIPHERAL_HEADER, TOOLS_PREAMBLE,
    TOOLS_PREAMBLE_MIN,
};
pub use command_changes::{ChangeKind, CommandChange, TreeDiff, DEFAULT_COMMAND_CHANGE_MAX_FILES};
pub use converge::{FindingId, ObserveOutcome, Tracker};
pub use plan_drive::{
    drive_plan, drive_plan_with_writeback, task_words, PlanDriveReport, PlanTaskReport,
};
pub use r#loop::{
    run_loop, run_loop_seeded, EscalateReason, Executor, ExecutorInput, LoopMode, LoopReport,
    Outcome, Reviewer, ReviewerError, ReviewerInput, RoundRecord, RoundSeed, RoundVerdict,
    SeedKind,
};
pub use restore::{
    git_status_lines, OnFail, RestoreLedger, RestoreRefreshRunner, RollbackGuard, RollbackReport,
    RESTORE_MAX_FILE_BYTES, RESTORE_MAX_TOTAL_BYTES,
};
pub use review_tools::{
    reviewer_gate, reviewer_tool_names, ReviewToolReport, ToolReviewTurnRunner,
    DEFAULT_REVIEWER_TOOL_ROUNDS,
};
pub use verdict::{parse_verdict, VerdictError};

/// Crate identifier. Used by smoke tests to prove the crate links.
pub fn crate_name() -> &'static str {
    "polymorph-review"
}

#[cfg(test)]
mod tests {
    #[test]
    fn links() {
        assert_eq!(super::crate_name(), "polymorph-review");
    }
}
