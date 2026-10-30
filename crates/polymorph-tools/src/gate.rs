//! Permission gate (Epic B5): allow/deny lists over tool-call strings.
//!
//! Semantics (contract, tested): `deny` beats `allow` on the same prefix.
//! Matching is prefix-on-segments: rule `bash:rm` matches `bash:rm`,
//! `bash:rm -rf /`, and `bash:rm:forever`, but NOT `bash:rmrf`.
//! An empty allow-list means "allow all" (deny still applies).

use polymorph_core::config::Permissions;

/// Denied tool call.
#[derive(Debug, PartialEq, thiserror::Error)]
#[error("permission denied for tool call '{call}' (matched deny rule '{rule}')")]
pub struct Denied {
    pub call: String,
    pub rule: String,
}

/// Compiled gate. Cheap to clone; build once per session from config.
#[derive(Debug, Clone)]
pub struct Gate {
    allow: Vec<String>,
    deny: Vec<String>,
}

impl Gate {
    pub fn new(allow: Vec<String>, deny: Vec<String>) -> Self {
        Self { allow, deny }
    }

    pub fn from_config(p: &Permissions) -> Self {
        Self::new(p.allow.clone(), p.deny.clone())
    }

    /// Allow-list open (no rules): everything not denied passes.
    pub fn check(&self, tool_call: &str) -> Result<(), Denied> {
        for d in &self.deny {
            if rule_matches(d, tool_call) {
                return Err(Denied {
                    call: tool_call.into(),
                    rule: d.clone(),
                });
            }
        }
        if self.allow.is_empty() {
            return Ok(());
        }
        if self.allow.iter().any(|a| rule_matches(a, tool_call)) {
            return Ok(());
        }
        Err(Denied {
            call: tool_call.into(),
            rule: String::new(), // denied by absence of allow, not by a rule
        })
    }
}

fn rule_matches(rule: &str, call: &str) -> bool {
    call == rule || call.starts_with(&format!("{rule}:")) || call.starts_with(&format!("{rule} "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate() -> Gate {
        Gate::new(vec!["bash".into(), "read".into()], vec!["bash:rm".into()])
    }

    #[test]
    fn deny_beats_allow() {
        assert!(gate().check("bash:rm -rf /").is_err());
        assert!(gate().check("bash:ls").is_ok());
    }

    #[test]
    fn no_prefix_confusion() {
        // `bash:rmrf` shares a string prefix with deny rule `bash:rm` but is
        // a different segment — it must NOT match the deny rule. (It passes
        // the `bash` allow rule on the `gate()` fixture, so assert on the
        // open gate where allow-all isolates deny behavior.)
        let open = Gate::new(vec![], vec!["bash:rm".into()]);
        assert!(open.check("bash:rmrf").is_ok());
        assert!(open.check("bash:rm").is_err());
        assert!(open.check("bash:rm -rf /").is_err());
        // ...while the same call is legitimately allowed by a broad rule.
        assert!(gate().check("bash:rmrf").is_ok());
    }

    #[test]
    fn absent_allow_rule_denies_with_empty_rule() {
        let err = gate().check("write:foo").unwrap_err();
        assert_eq!(err.rule, "");
    }

    #[test]
    fn default_permissions_gate() {
        let g = Gate::from_config(&Permissions::default());
        assert!(g.check("read:src/main.rs").is_ok());
        assert!(g.check("bash:git push origin").is_err());
    }
}
