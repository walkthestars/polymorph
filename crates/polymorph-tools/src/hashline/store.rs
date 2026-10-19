//! Snapshot store: file-version binding for `[PATH#TAG]` headers.
//!
//! The agent's `read` mints a tag for the content it showed the model; `apply`
//! refuses any section whose tag no longer binds the current file content
//! (`StaleTag`). Tags are 4 lowercase hex chars (16-bit, mirroring upstream),
//! but the BINDING is the full SHA-256 hex — collisions resolve by exact
//! comparison, strictly stronger than upstream's prefer-most-recent.

use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// SHA-256 hex of normalized text (`Normalized.text`: LF, no trailing
/// terminator, BOM stripped). Callers MUST pass `normalize().text` (or an
/// equivalent convention) — hashing raw bytes forks the binding across
/// identical visible content.
pub fn content_hash(normalized_text: &str) -> String {
    format!("{:x}", Sha256::digest(normalized_text.as_bytes()))
}

/// 4-hex snapshot tag for a content hash.
pub fn tag_for_hash(hash_hex: &str) -> String {
    hash_hex[..4].to_string()
}

#[derive(Debug, Default)]
pub struct SnapshotStore {
    /// (path, tag) -> full content hash. Tags are meaningful only with their
    /// path: two identical files legitimately share one tag. A 4-hex
    /// collision across DIFFERENT content for the same path resolves
    /// prefer-most-recent (mirrors upstream); the older patch then correctly
    /// fails verification since hashes differ.
    tags: HashMap<(PathBuf, String), String>,
}

impl SnapshotStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mint a tag for the content the agent just showed the model.
    /// `normalized_content` is `Normalized.text` (terminator-stripped).
    /// Returns the 4-hex tag for the `[PATH#TAG]` header / prompt pack.
    pub fn mint(&mut self, path: &Path, normalized_content: &str) -> String {
        let hash = content_hash(normalized_content);
        let tag = tag_for_hash(&hash);
        self.tags.insert((path.to_path_buf(), tag.clone()), hash);
        tag
    }

    /// Verify a section header against current file content.
    pub fn verify(
        &self,
        path: &Path,
        tag: &str,
        current_normalized_content: &str,
    ) -> Result<(), super::HashlineError> {
        use super::HashlineError;
        let stale = || HashlineError::StaleTag {
            path: path.display().to_string(),
            tag: tag.into(),
        };
        match self.tags.get(&(path.to_path_buf(), tag.to_string())) {
            Some(bound_hash) if &content_hash(current_normalized_content) == bound_hash => Ok(()),
            // Unknown (path, tag), or content moved since minting: either way
            // the section does not bind THIS file's shown version — stale.
            _ => Err(stale()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mint_verify_roundtrip() {
        let mut s = SnapshotStore::new();
        let tag = s.mint(Path::new("a.rs"), "fn a() {}");
        assert_eq!(tag.len(), 4);
        assert!(s.verify(Path::new("a.rs"), &tag, "fn a() {}").is_ok());
    }

    #[test]
    fn changed_content_is_stale() {
        let mut s = SnapshotStore::new();
        let tag = s.mint(Path::new("a.rs"), "v1");
        assert!(s.verify(Path::new("a.rs"), &tag, "v2").is_err());
    }

    #[test]
    fn tag_bound_to_other_path_is_stale() {
        let mut s = SnapshotStore::new();
        let tag = s.mint(Path::new("a.rs"), "same\n");
        assert!(s.verify(Path::new("b.rs"), &tag, "same\n").is_err());
    }

    #[test]
    fn identical_files_share_a_tag() {
        // Regression pin: two files with byte-identical content mint the same
        // tag, and BOTH must verify (the store used to key tags globally, so
        // the second mint stole the first file's binding).
        let mut s = SnapshotStore::new();
        let ta = s.mint(Path::new("a"), "same");
        let tb = s.mint(Path::new("b"), "same");
        assert_eq!(ta, tb);
        assert!(s.verify(Path::new("a"), &ta, "same").is_ok());
        assert!(s.verify(Path::new("b"), &tb, "same").is_ok());
    }

    #[test]
    fn unknown_tag_is_stale() {
        let s = SnapshotStore::new();
        assert!(s.verify(Path::new("a.rs"), "ffff", "x\n").is_err());
    }
}
