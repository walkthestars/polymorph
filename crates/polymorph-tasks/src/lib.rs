//! `polymorph-tasks` — task model + dependency DAG + PLAN.md.
//!
//! The plan-mode substrate. [`graph`] owns the model (tasks, statuses, edges, ready-
//! order, merge ordering, checkpoints); [`plan`] owns the observable artifact (PLAN.md
//! render/parse). No LLM, no bus, no network — the reviewer loop drives these types without
//! touching them (F1/F2 seam precedent).
//!
//! Rules: - The plan is a FILE (rendered, reviewed, rewritten), never a black-box transcript. -
//! `Done` is terminal: resume never resurrects completed work. - Multi-child merges run in
//! dependency order; the caller stops at the first conflict (no partial merges) —
//! [`graph::TaskGraph::merge_order`] only orders, it never merges.

pub mod graph;
pub mod plan;

pub use graph::{GraphError, Task, TaskGraph, TaskStatus};
pub use plan::{apply_section, render, render_section, PlanError};

/// Crate identifier. Used by smoke tests to prove the crate links.
pub fn crate_name() -> &'static str {
    "polymorph-tasks"
}

#[cfg(test)]
mod tests {
    #[test]
    fn links() {
        assert_eq!(super::crate_name(), "polymorph-tasks");
    }
}
