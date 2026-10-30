//! Typed child results.
//!
//! "No prose to parse": every child turn-loop ends in a [`TypedResult`] — a status, a human
//! summary, and machine `data`. Callers check shape with [`validate_data`] (required top-level
//! keys; no new dep). Full verdict-schema enforcement (fixed findings shape, confidence) stays —
//! this is the transport the verdict will ride in.

/// How a child's drive loop ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinishStatus {
    /// Runner reported `done` with usable output.
    Completed,
    /// Loop stopped by policy/operator: budget, stall, cancel, depth.
    Stopped { reason: StopReason },
    /// The runner itself errored (tool crash, provider failure in F,...).
    Failed { error: String },
}

/// Why a child stopped short of completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// `max_turns` consumed.
    BudgetExhausted,
    /// `max_tool_calls` consumed.
    ToolBudgetExhausted,
    /// No progress for `stall_turns` turns (steer resets the counter).
    Stalled,
    /// Operator (or parent) cancelled.
    Cancelled,
}

impl std::fmt::Display for StopReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StopReason::BudgetExhausted => write!(f, "turn budget exhausted"),
            StopReason::ToolBudgetExhausted => write!(f, "tool-call budget exhausted"),
            StopReason::Stalled => write!(f, "stalled (no progress)"),
            StopReason::Cancelled => write!(f, "cancelled"),
        }
    }
}

/// The typed object a child hands back to its parent.
#[derive(Debug, Clone, PartialEq)]
pub struct TypedResult {
    pub status: FinishStatus,
    pub summary: String,
    pub data: serde_json::Value,
}

impl TypedResult {
    pub fn completed(summary: impl Into<String>, data: serde_json::Value) -> Self {
        Self {
            status: FinishStatus::Completed,
            summary: summary.into(),
            data,
        }
    }

    pub fn stopped(reason: StopReason, summary: impl Into<String>) -> Self {
        Self {
            status: FinishStatus::Stopped { reason },
            summary: summary.into(),
            data: serde_json::Value::Null,
        }
    }

    pub fn failed(error: impl Into<String>) -> Self {
        Self {
            status: FinishStatus::Failed {
                error: error.into(),
            },
            summary: "child runner failed".into(),
            data: serde_json::Value::Null,
        }
    }

    pub fn is_completed(&self) -> bool {
        matches!(self.status, FinishStatus::Completed)
    }
}

/// Shape-check `data`: every `required` key must exist at the top level of
/// an object. Missing keys / non-object data name the offender (refuse,
/// don't guess — the parent decides retry vs escalate).
pub fn validate_data(data: &serde_json::Value, required: &[&str]) -> anyhow::Result<()> {
    let obj = data
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("typed data must be an object for keys {required:?}"))?;
    for key in required {
        if !obj.contains_key(*key) {
            anyhow::bail!("typed data missing required key {key:?}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_data_accepts_superset_rejects_gaps() {
        let data = serde_json::json!({ "verdict": "pass", "extra": 1 });
        assert!(validate_data(&data, &["verdict"]).is_ok());
        let err = validate_data(&data, &["verdict", "findings"]).unwrap_err();
        assert!(err.to_string().contains("\"findings\""));
    }

    #[test]
    fn validate_data_refuses_non_objects() {
        for data in [
            serde_json::Value::Null,
            serde_json::json!("prose, not data"),
            serde_json::json!([1, 2]),
        ] {
            assert!(validate_data(&data, &["k"]).is_err());
        }
        // Vacuous truth: no requirements, any object passes.
        assert!(validate_data(&serde_json::json!({}), &[]).is_ok());
    }

    #[test]
    fn constructors_report_status_honestly() {
        let c = TypedResult::completed("ok", serde_json::json!({}));
        assert!(c.is_completed());
        let s = TypedResult::stopped(StopReason::Stalled, "quiet");
        assert!(!s.is_completed());
        assert!(matches!(
            s.status,
            FinishStatus::Stopped {
                reason: StopReason::Stalled
            }
        ));
        let f = TypedResult::failed("boom");
        assert!(matches!(f.status, FinishStatus::Failed { .. }));
    }

    #[test]
    fn stop_reasons_display_for_logs() {
        assert_eq!(StopReason::Cancelled.to_string(), "cancelled");
        assert!(StopReason::BudgetExhausted.to_string().contains("budget"));
    }
}
