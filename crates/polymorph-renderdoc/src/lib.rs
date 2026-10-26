//! `polymorph-renderdoc` — graphics-debugging tool.

/// Crate identifier. Used by smoke tests to prove the crate links.
pub fn crate_name() -> &'static str {
    "polymorph-renderdoc"
}

#[cfg(test)]
mod tests {
    #[test]
    fn links() {
        assert_eq!(super::crate_name(), "polymorph-renderdoc");
    }
}
