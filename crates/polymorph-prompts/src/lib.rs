//! Built-in default prompt packs for Polymorph.
//!
//! A `cargo install`ed binary ships without a checkout, so the prompt packs the
//! review/act loops default to (`prompts/{vendor}/{model}/{role}.md`) would
//! otherwise be missing on disk. This crate compiles the committed packs into
//! the binary with `include_str!` and exposes them under their manifest keys —
//! the same relative paths, minus the `prompts/` prefix.
//!
//! These are the *built-in defaults*, not an override: a real `prompts/`
//! directory on disk still wins in `load_pack`, which probes the on-disk
//! candidates first and only falls back to [`embedded`] when every candidate
//! misses. Each embedded pack is byte-identical to the file under
//! `crates/polymorph-prompts/prompts/...` that `prompts.toml` pins by sha256.

/// Every embedded pack key, in manifest order.
///
/// Keys are relative paths without the `prompts/` prefix, so they can be
/// compared directly against a normalized runtime path.
pub const PACK_PATHS: &[&str] = &[
    "muse/muse-spark-1.3/review.md",
    "glm/glm-5.3/review.md",
    "glm/glm-5.3/act.md",
    "glm/glm-5.3/plan.md",
    "glm/glm-5.3/edit.md",
    "glm/glm-5.3/str_replace.md",
    "deepseek/deepseek-v4.1-flash/act.md",
    "deepseek/deepseek-v4.1-flash/edit.md",
];

/// Contents of an embedded pack, or `None` when `path` names no built-in.
///
/// The input is normalized before matching, so a runtime key such as
/// `prompts/glm/glm-5.3/act.md` (the CLI default) or a Windows-style
/// `prompts\glm\glm-5.3\act.md` resolves to the same pack as the bare
/// `glm/glm-5.3/act.md`:
///
/// 1. leading/trailing whitespace is trimmed,
/// 2. one leading `prompts/` or `prompts\` is stripped,
/// 3. every backslash becomes `/`.
///
/// Anything else returns `None`; this function never panics.
pub fn embedded(path: &str) -> Option<&'static str> {
    let trimmed = path.trim();
    let stripped = trimmed
        .strip_prefix("prompts/")
        .or_else(|| trimmed.strip_prefix("prompts\\"))
        .unwrap_or(trimmed);
    let key = stripped.replace('\\', "/");
    match key.as_str() {
        "muse/muse-spark-1.3/review.md" => {
            Some(include_str!("../prompts/muse/muse-spark-1.3/review.md"))
        }
        "glm/glm-5.3/review.md" => Some(include_str!("../prompts/glm/glm-5.3/review.md")),
        "glm/glm-5.3/act.md" => Some(include_str!("../prompts/glm/glm-5.3/act.md")),
        "glm/glm-5.3/plan.md" => Some(include_str!("../prompts/glm/glm-5.3/plan.md")),
        "glm/glm-5.3/edit.md" => Some(include_str!("../prompts/glm/glm-5.3/edit.md")),
        "glm/glm-5.3/str_replace.md" => Some(include_str!("../prompts/glm/glm-5.3/str_replace.md")),
        "deepseek/deepseek-v4.1-flash/act.md" => Some(include_str!(
            "../prompts/deepseek/deepseek-v4.1-flash/act.md"
        )),
        "deepseek/deepseek-v4.1-flash/edit.md" => Some(include_str!(
            "../prompts/deepseek/deepseek-v4.1-flash/edit.md"
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_paths_is_the_full_manifest() {
        assert_eq!(PACK_PATHS.len(), 8);
        for key in PACK_PATHS {
            assert!(!key.is_empty(), "empty key in PACK_PATHS");
            assert!(!key.starts_with("prompts/"), "{key} keeps the prefix");
            assert!(key.ends_with(".md"), "{key} is not a markdown pack");
        }
    }

    #[test]
    fn every_pack_path_resolves_to_a_non_empty_pack() {
        for &key in PACK_PATHS {
            let body = embedded(key).unwrap_or_else(|| panic!("{key} is not embedded"));
            assert!(!body.is_empty(), "{key} embeds an empty file");
            assert!(!body.trim().is_empty(), "{key} embeds a blank file");
        }
    }

    #[test]
    fn prompts_prefix_is_stripped() {
        for &key in PACK_PATHS {
            assert_eq!(embedded(&format!("prompts/{key}")), embedded(key), "{key}");
            assert_eq!(
                embedded(&format!(" prompts/{key}\n")),
                embedded(key),
                "{key}"
            );
        }
    }

    #[test]
    fn backslashes_are_normalized() {
        for &key in PACK_PATHS {
            let windows_key = key.replace('/', "\\");
            assert_eq!(
                embedded(&format!("prompts\\{windows_key}")),
                embedded(key),
                "{key}"
            );
            assert_eq!(embedded(&windows_key), embedded(key), "{key}");
        }
    }

    #[test]
    fn unknown_paths_return_none() {
        assert!(embedded("unknown.md").is_none());
        assert!(embedded("glm/glm-5.3/nope.md").is_none());
        assert!(embedded("prompts/unknown.md").is_none());
        assert!(embedded("prompts\\unknown.md").is_none());
        assert!(embedded("").is_none());
        assert!(embedded("prompts/").is_none());
    }
}
