//! Stream rules (Epic R, chunk 123): regex misfire patterns that abort a
//! model stream mid-flight and attach the rule's reminder for the retry
//! (slice 2 re-issues the turn; this slice owns match + abort only).
//!
//! Rules are caller-owned (`&[CompiledRule]` per fold call — no store:
//! no compaction exists to survive, so persistence would be speculative).
//! Patterns compile at registration; a bad pattern refuses naming the
//! rule id, never at stream time. Matching runs per text chunk over the
//! full accumulated text (turns are KBs — stated bound, revisit only if
//! measured).

/// One stream rule as authored: an id, a Rust-regex pattern, and the
/// reminder text slice 2 injects as a system reminder on retry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamRule {
    pub id: String,
    pub pattern: String,
    pub reminder: String,
}

/// A compiled rule: the id + reminder ride along for the hit report.
#[derive(Debug, Clone)]
pub struct CompiledRule {
    pub id: String,
    pub reminder: String,
    matcher: regex::Regex,
}

/// A fired rule: which rule, what it matched, and what to inject.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleHit {
    pub rule_id: String,
    pub matched: String,
    pub reminder: String,
}

impl StreamRule {
    /// Compile the pattern now (fail-closed at registration, never
    /// mid-stream). Empty ids and patterns refuse naming the problem.
    pub fn compile(&self) -> anyhow::Result<CompiledRule> {
        if self.id.trim().is_empty() {
            anyhow::bail!("stream rule needs a non-empty `id`");
        }
        if self.pattern.trim().is_empty() {
            anyhow::bail!("stream rule `{}` needs a non-empty `pattern`", self.id);
        }
        let matcher = regex::Regex::new(&self.pattern)
            .map_err(|e| anyhow::anyhow!("stream rule `{}`: bad pattern: {e}", self.id))?;
        Ok(CompiledRule {
            id: self.id.clone(),
            reminder: self.reminder.clone(),
            matcher,
        })
    }
}

impl CompiledRule {
    /// First match of this rule in `text`, if any.
    pub fn find_in(&self, text: &str) -> Option<RuleHit> {
        self.matcher.find(text).map(|m| RuleHit {
            rule_id: self.id.clone(),
            matched: m.as_str().to_string(),
            reminder: self.reminder.clone(),
        })
    }
}

/// First firing rule in registration order, if any (deterministic —
/// overlapping patterns resolve by list position, never by length).
pub fn first_hit(rules: &[CompiledRule], text: &str) -> Option<RuleHit> {
    rules.iter().find_map(|r| r.find_in(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(id: &str, pattern: &str) -> CompiledRule {
        StreamRule {
            id: id.into(),
            pattern: pattern.into(),
            reminder: format!("reminder:{id}"),
        }
        .compile()
        .unwrap()
    }

    #[test]
    fn compile_refuses_empty_id_pattern_and_bad_regex() {
        assert!(StreamRule {
            id: String::new(),
            pattern: "x".into(),
            reminder: String::new()
        }
        .compile()
        .is_err());
        assert!(StreamRule {
            id: "r".into(),
            pattern: "  ".into(),
            reminder: String::new()
        }
        .compile()
        .is_err());
        let err = StreamRule {
            id: "bad".into(),
            pattern: "(unclosed".into(),
            reminder: String::new(),
        }
        .compile()
        .unwrap_err()
        .to_string();
        assert!(err.contains("bad"), "{err}");
    }

    #[test]
    fn first_hit_wins_by_order_and_reports_reminder() {
        let rules = vec![
            rule("tense", r"will\s+edits?\b"),
            rule("other", r"edits?\b"),
        ];
        let hit = first_hit(&rules, "it will edit x").unwrap();
        assert_eq!(hit.rule_id, "tense");
        assert_eq!(hit.matched, "will edit");
        assert_eq!(hit.reminder, "reminder:tense");
        assert!(first_hit(&rules, "clean text here").is_none());
        assert!(first_hit(&[], "it will edit x").is_none());
    }
}
