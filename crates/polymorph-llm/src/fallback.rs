//! Fallback policy pieces.
//!
//! - [`Cooldowns`]: per-model 429 timestamps; a limited model is skipped
//!   until its cooldown expires, then automatically restored. Time is an
//!   explicit `now: Instant` parameter (never read inside) so tests are
//!   deterministic — no sleeps, no test-only hooks.
//! - [`FallbackState`]: the per-process managed-loop state: cooldowns plus
//!   one [`CredentialPool`](crate::config::CredentialPool) per provider id.
//!   Pools live in the caller (one `exec` invocation = one state), so no
//!   shared mutable statics and no cross-thread credential handling.

use polymorph_core::config::CredentialPool;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// 429 cooldown tracker. `cooldown == Duration::ZERO` disables skipping
/// (every model always available — the pre-D delegation behavior).
#[derive(Debug, Clone)]
pub struct Cooldowns {
    cooldown: Duration,
    limited_until: HashMap<String, Instant>,
}

/// Default = disabled (zero cooldown): everything always available.
/// (Pre-D call paths construct this; managed paths use `new`.)
impl Default for Cooldowns {
    fn default() -> Self {
        Self::disabled()
    }
}

impl Cooldowns {
    pub fn new(cooldown: Duration) -> Self {
        Self {
            cooldown,
            limited_until: HashMap::new(),
        }
    }

    /// Zero-cooldown tracker: [`Cooldowns::available`] always true.
    pub fn disabled() -> Self {
        Self::new(Duration::ZERO)
    }

    /// Record a 429 for `model` at `now`.
    pub fn mark_limited(&mut self, model: &str, now: Instant) {
        if let Some(until) = now.checked_add(self.cooldown) {
            self.limited_until.insert(model.to_string(), until);
        }
    }

    /// True when `model` may be attempted at `now` (never limited, or the
    /// cooldown has expired — expiry restores automatically, no reset call).
    pub fn available(&self, model: &str, now: Instant) -> bool {
        match self.limited_until.get(model) {
            Some(until) => now >= *until,
            None => true,
        }
    }

    /// First available entry of `chain` at `now`, or `None` if every entry
    /// is still cooling down (caller reports exhaustion, not fallback).
    pub fn next_available<'a>(&self, chain: &'a [String], now: Instant) -> Option<&'a str> {
        chain
            .iter()
            .find(|m| self.available(m.as_str(), now))
            .map(String::as_str)
    }

    /// Number of models currently cooling down (observability for tests/logs).
    pub fn limited_count(&self, now: Instant) -> usize {
        self.limited_until
            .values()
            .filter(|until| now < **until)
            .count()
    }
}

/// Managed-loop state for one process invocation.
#[derive(Debug, Default)]
pub struct FallbackState {
    pub cooldowns: Cooldowns,
    pools: HashMap<String, CredentialPool>,
}

impl FallbackState {
    pub fn new(cooldown: Duration) -> Self {
        Self {
            cooldowns: Cooldowns::new(cooldown),
            pools: HashMap::new(),
        }
    }

    /// Pre-D behavior: no skipping, no pools (ambient-env only).
    pub fn unmanaged() -> Self {
        Self {
            cooldowns: Cooldowns::disabled(),
            pools: HashMap::new(),
        }
    }

    /// Pool for `provider_id`, inserting `pool` on first use. Returns the
    /// live pool (mutations — cursor advances — persist in the state).
    pub fn pool_for(&mut self, provider_id: &str, pool: CredentialPool) -> &mut CredentialPool {
        self.pools.entry(provider_id.to_string()).or_insert(pool)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chain(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn disabled_never_skips() {
        let cd = Cooldowns::disabled();
        let now = Instant::now();
        let c = chain(&["a", "b"]);
        assert!(cd.available("a", now));
        assert_eq!(cd.next_available(&c, now), Some("a"));
    }

    #[test]
    fn limited_model_skipped_until_expiry_then_restored() {
        let mut cd = Cooldowns::new(Duration::from_secs(60));
        let t0 = Instant::now();
        let c = chain(&["a", "b", "c"]);
        cd.mark_limited("a", t0);
        // Still cooling: skipped, next available is b.
        assert!(!cd.available("a", t0));
        assert_eq!(cd.next_available(&c, t0), Some("b"));
        assert_eq!(cd.limited_count(t0), 1);
        // After expiry: automatic restore, no reset call.
        let later = t0 + Duration::from_secs(61);
        assert!(cd.available("a", later));
        assert_eq!(cd.next_available(&c, later), Some("a"));
        assert_eq!(cd.limited_count(later), 0);
    }

    #[test]
    fn all_cooling_down_reports_none() {
        let mut cd = Cooldowns::new(Duration::from_secs(60));
        let t0 = Instant::now();
        let c = chain(&["a", "b"]);
        cd.mark_limited("a", t0);
        cd.mark_limited("b", t0);
        assert_eq!(cd.next_available(&c, t0), None);
    }

    #[test]
    fn unknown_model_always_available() {
        let cd = Cooldowns::new(Duration::from_secs(60));
        assert!(cd.available("never-seen", Instant::now()));
    }

    #[test]
    fn pool_for_inserts_once_and_keeps_cursor() {
        let mut st = FallbackState::new(Duration::from_secs(60));
        st.pool_for("p", CredentialPool::new(vec!["k1".into(), "k2".into()]));
        assert_eq!(
            st.pool_for("p", CredentialPool::default()).next_key(),
            Some("k1")
        );
        // Second insert for the same id does NOT reset the cursor.
        assert_eq!(
            st.pool_for("p", CredentialPool::default()).next_key(),
            Some("k2")
        );
        assert_eq!(
            st.pool_for("p", CredentialPool::default()).next_key(),
            Some("k1")
        );
    }
}
