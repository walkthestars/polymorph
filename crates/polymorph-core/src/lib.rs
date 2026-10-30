//! `polymorph-core` — agent loop, plan/act/review state machine.
//!
//! Role routing types, verdict schema (fixed JSON), and loop config. No LLM calls
//! yet — that is A3.

use serde::{Deserialize, Serialize};

pub mod config;
pub mod sessions;

/// Agent role. Per-role prompts live in `prompts/{model}/{role}.md`
/// and per-role model routing is configured in `polymorph-llm`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Default,
    Smol,
    Slow,
    Plan,
    Act,
    Review,
    Commit,
    Vision,
    Task,
    Advisor,
    Tiny,
}

impl Role {
    /// All roles, for config validation and prompt-manifest checks.
    pub const ALL: &'static [Role] = &[
        Role::Default,
        Role::Smol,
        Role::Slow,
        Role::Plan,
        Role::Act,
        Role::Review,
        Role::Commit,
        Role::Vision,
        Role::Task,
        Role::Advisor,
        Role::Tiny,
    ];

    /// Parse a lowercase variant name (`"review"` → `Role::Review`).
    /// Same convention as role chains and pins (single source of truth for
    /// CLI `--role` and config keys). `None` = unknown name.
    pub fn parse(name: &str) -> Option<Role> {
        Role::ALL
            .iter()
            .find(|r| format!("{r:?}").to_lowercase() == name)
            .copied()
    }

    /// Filename stem for the prompt pack, e.g. `review.md`.
    pub fn prompt_file(&self) -> &'static str {
        match self {
            Role::Default => "default.md",
            Role::Smol => "smol.md",
            Role::Slow => "slow.md",
            Role::Plan => "plan.md",
            Role::Act => "act.md",
            Role::Review => "review.md",
            Role::Commit => "commit.md",
            Role::Vision => "vision.md",
            Role::Task => "task.md",
            Role::Advisor => "advisor.md",
            Role::Tiny => "tiny.md",
        }
    }
}

/// Reviewer verdict. Fixed JSON shape — never parse prose.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Verdict {
    pub verdict: VerdictKind,
    pub findings: Vec<Finding>,
    pub confidence: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerdictKind {
    Pass,
    Fail,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub severity: Severity,
    pub file: String,
    pub line: Option<u32>,
    pub issue: String,
    pub proposed_fix: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Severity {
    P0,
    P1,
    P2,
    P3,
}

/// Reviewer-loop convergence config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopConfig {
    /// Max reviewer→executor rounds before escalation.
    pub max_iterations: u32,
    /// Same finding surviving this many rounds triggers user escalation.
    pub surviving_finding_escalation: u32,
}

impl Default for LoopConfig {
    fn default() -> Self {
        Self {
            max_iterations: 5,
            surviving_finding_escalation: 3,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdict_round_trips_through_json() {
        let v = Verdict {
            verdict: VerdictKind::Fail,
            findings: vec![Finding {
                severity: Severity::P1,
                file: "src/main.rs".into(),
                line: Some(10),
                issue: "unchecked unwrap".into(),
                proposed_fix: "use context".into(),
            }],
            confidence: 0.8,
        };
        let s = serde_json::to_string(&v).unwrap();
        let back: Verdict = serde_json::from_str(&s).unwrap();
        assert_eq!(v, back);
    }

    #[test]
    fn every_role_has_prompt_file() {
        for r in Role::ALL {
            assert!(r.prompt_file().ends_with(".md"));
        }
    }

    #[test]
    fn role_parse_round_trips_all_variants() {
        for r in Role::ALL {
            let name = format!("{r:?}").to_lowercase();
            assert_eq!(Role::parse(&name), Some(*r));
        }
        assert_eq!(Role::parse("frobnicate"), None);
        assert_eq!(Role::parse("Review"), None); // lowercase only, by convention.
    }
}
