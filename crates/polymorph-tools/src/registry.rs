//! Registry module — the canonical registration point.
//!
//! CONVENTION: tools self-register HERE (`polymorph-tools/src/registry.rs`);
//! nothing registers from `core`. `lib.rs` re-exports this module so both
//! `polymorph_tools::registry::Registry` and `polymorph_tools::Registry` work.

pub use crate::{is_allowed, v1_tool_floor, Registry, ToolContext, ToolDef};

/// Build the V1 tool floor as a live registry. Registration order is the priority order from
/// Tool inventory.
pub fn v1_registry() -> crate::Registry {
    let mut r = crate::Registry::new();
    for def in v1_tool_floor() {
        // Descriptors are unique by construction; a duplicate here is a
        // programming error, so panic (not silent skip) — covered by test.
        r.register(def)
            .expect("v1 floor descriptors must be unique");
    }
    r
}

#[cfg(test)]
mod tests {
    #[test]
    fn v1_registry_holds_twentythree_tools() {
        let r = super::v1_registry();
        assert_eq!(r.len(), 23);
        for name in [
            "read",
            "write",
            "edit",
            "grep",
            "glob",
            "bash",
            "lsp_hover",
            "lsp_definition",
            "lsp_references",
            "dap_start",
            "dap_continue",
            "dap_inspect",
            "dap_evaluate",
            "dap_disconnect",
            "rdoc_capture",
            "rdoc_draws",
            "rdoc_state",
            "rdoc_diff",
            "rdoc_script",
            "web_search",
            "web_extract",
            "ask",
            "split_plan",
        ] {
            assert!(r.get(name).is_some(), "{name} registered");
        }
    }
}
