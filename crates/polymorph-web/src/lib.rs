//! `polymorph-web` — serica-backed web tools + REST server.

/// REST + SSE server.
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
