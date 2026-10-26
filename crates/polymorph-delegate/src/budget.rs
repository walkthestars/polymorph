//! Turn/stall budgets.
//!
//! Master rule: NO wall-clock kills — a child stops on turn count, tool-call
//! count, or stall (turns without progress). Wall-clock durations are
//! recorded on the outcome for observability only, never acted on.
//! Numbers below are starting points and are tuned against measurements.

use polymorph_core::Role;

/// Stop policy for one child's drive loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    /// Max turns before `StopReason::BudgetExhausted`.
    pub max_turns: u32,
    /// Max summed `tool_calls` before `ToolBudgetExhausted`.
    pub max_tool_calls: u32,
    /// Turns without progress before `Stalled` (steer resets to 0).
    pub stall_turns: u32,
}

impl Budget {
    /// Full-size worker (plan/act/review executor roles).
    pub fn standard() -> Self {
        Self {
            max_turns: 20,
            max_tool_calls: 100,
            stall_turns: 5,
        }
    }

    /// Cheap-role worker: fewer turns, tighter stall leash.
    pub fn smol() -> Self {
        Self {
            max_turns: 8,
            max_tool_calls: 30,
            stall_turns: 3,
        }
    }

    /// Role-routed budget (the roadmap "`smol` budgets" line made concrete):
    /// Smol|Tiny run smol, everything else runs standard.
    pub fn for_role(role: Role) -> Self {
        match role {
            Role::Smol | Role::Tiny => Self::smol(),
            _ => Self::standard(),
        }
    }
}

impl Default for Budget {
    fn default() -> Self {
        Self::standard()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smol_is_strictly_tighter_than_standard() {
        let (s, m) = (Budget::standard(), Budget::smol());
        assert!(m.max_turns < s.max_turns);
        assert!(m.max_tool_calls < s.max_tool_calls);
        assert!(m.stall_turns < s.stall_turns);
        // ...but still allows real work (nonzero everywhere).
        assert!(m.max_turns > 0 && m.max_tool_calls > 0 && m.stall_turns > 0);
    }

    #[test]
    fn for_role_routes_cheap_roles_to_smol() {
        assert_eq!(Budget::for_role(Role::Smol), Budget::smol());
        assert_eq!(Budget::for_role(Role::Tiny), Budget::smol());
        for role in [
            Role::Default,
            Role::Plan,
            Role::Act,
            Role::Review,
            Role::Commit,
        ] {
            assert_eq!(Budget::for_role(role), Budget::standard(), "{role:?}");
        }
        assert_eq!(Budget::default(), Budget::standard());
    }
}
