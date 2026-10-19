//! `polymorph-web` — serica-backed web tools (Epic P) + REST server (Epic W).

/// REST + SSE server (Epic W, chunk 135).
pub mod server;

/// Crate identifier. Used by smoke tests to prove the crate links.
pub fn crate_name() -> &'static str {
    "polymorph-web"
}

#[cfg(test)]
mod tests {
    #[test]
    fn links() {
        assert_eq!(super::crate_name(), "polymorph-web");
    }
}
