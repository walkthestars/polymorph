//! Command change detection: what a fixer `bash`/`exec` call did to the
//! workspace (follow-up to dockets #1, #3, #6 and #8).
//!
//! Only `write`/`edit` targets used to count as mutations, so a `sed -i`,
//! a code generator, a formatter or an `mv` run through `bash` changed
//! files no reviewer attachment, ledger, LSP snapshot or rollback knew
//! about. [`AgenticFixRunner`](crate::AgenticFixRunner) now stamps the
//! workdir (size + mtime of every workspace file, no file opened) right
//! before and right after each command and diffs the two.
//!
//! One approach for git and non-git trees alike: the gitignore-aware stamp
//! walk ([`polymorph_tools::fs::stamp_tree`]). `git status` alone cannot
//! see a command rewrite an already-dirty file (its status line does not
//! change) and reports index-only operations (`git add`) as changes, so it
//! would need per-file stamps anyway — and its untracked scan stats the
//! same tree. Git still matters for rollback, which needs a provable
//! original ([`crate::restore`]).
//!
//! Rules:
//! - Bounded: more than the cap's files (`[agent] command_change_max_files`)
//!   and detection is skipped for the rest of the fix turn, with an
//!   explicit note on every command's ledger entry.
//! - Ignored files (`.gitignore`, also outside git), VCS metadata, harness
//!   state (`.polymorph/`, drive event ledgers) and the registry's
//!   scan-hidden files are never reported. Dotfiles are.
//! - Detection reports, it never blocks or reverts anything by itself.

use std::path::{Path, PathBuf};

use polymorph_tools::fs::{FileStamp, TreeStamps};

/// Default `[agent] command_change_max_files`: files a stamp walk may see
/// before detection gives up for the turn (a 20,000-file walk measured
/// ~90 ms in a release build on a Windows NTFS checkout).
pub const DEFAULT_COMMAND_CHANGE_MAX_FILES: usize = 20_000;

/// Changed paths one command's ledger entry lists (the rest are counted).
pub const MAX_CHANGED_PATHS_RECORDED: usize = 200;

/// Changed files per command the LSP pool is consulted on (a formatter
/// over the tree must not cost a pull per file); the rest are named
/// unconsulted.
pub const MAX_LSP_CONSULTS_PER_COMMAND: usize = 8;

/// How one command changed one path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    /// Absent before the command, present after.
    Created,
    /// Present before and after, stamp changed.
    Modified,
    /// Present before, absent after.
    Deleted,
}

/// One path a command changed, as the detector saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandChange {
    /// `/`-separated workdir-relative spelling.
    pub rel: String,
    pub kind: ChangeKind,
    /// Stamp right before the command (`None` = absent).
    pub before: Option<FileStamp>,
}

/// Everything one command did to the walked tree.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TreeDiff {
    /// Changed files, sorted by path.
    pub changes: Vec<CommandChange>,
    /// Workdir-relative directories that appeared (empty or not), sorted:
    /// removable on rollback once empty.
    pub created_dirs: Vec<String>,
}

/// One stamp walk's answer.
#[derive(Debug)]
pub(crate) enum Probe {
    Stamps(TreeStamps),
    /// Over the cap: the note every later command of the turn carries.
    Skipped(String),
}

/// Stamp `workdir` (at most `max_files` files).
pub(crate) fn probe(workdir: &Path, hidden: &[PathBuf], max_files: usize) -> Probe {
    match polymorph_tools::fs::stamp_tree(workdir, hidden, max_files) {
        Some(stamps) => Probe::Stamps(stamps),
        None => Probe::Skipped(format!(
            "skipped: tree too large (over the {max_files}-file change-detection cap)"
        )),
    }
}

/// What changed between two walks of one tree. A directory neither walk
/// could see (ignored, VCS metadata, harness state) is never listed.
pub(crate) fn diff(before: &TreeStamps, after: &TreeStamps) -> TreeDiff {
    let mut changes: Vec<CommandChange> = Vec::new();
    for (rel, stamp) in &after.files {
        match before.files.get(rel) {
            Some(old) if old == stamp => {}
            Some(old) => changes.push(CommandChange {
                rel: rel.clone(),
                kind: ChangeKind::Modified,
                before: Some(*old),
            }),
            None => changes.push(CommandChange {
                rel: rel.clone(),
                kind: ChangeKind::Created,
                before: None,
            }),
        }
    }
    for (rel, old) in &before.files {
        if !after.files.contains_key(rel) {
            changes.push(CommandChange {
                rel: rel.clone(),
                kind: ChangeKind::Deleted,
                before: Some(*old),
            });
        }
    }
    changes.sort_by(|a, b| a.rel.cmp(&b.rel));
    let mut created_dirs: Vec<String> = after.dirs.difference(&before.dirs).cloned().collect();
    created_dirs.sort();
    TreeDiff {
        changes,
        created_dirs,
    }
}

/// The ledger fields of one command's detected changes (merged into its
/// `tool_outcomes` entry): `changed_paths` (sorted, capped), and when
/// non-empty `deleted_paths` (the subset absent after it) and
/// `changed_omitted` (count past the cap). Empty map = nothing changed,
/// so the entry stays byte-identical to an undetected one.
pub(crate) fn ledger_fields(
    changes: &[CommandChange],
) -> serde_json::Map<String, serde_json::Value> {
    use serde_json::Value;
    let mut out = serde_json::Map::new();
    if changes.is_empty() {
        return out;
    }
    let kept = &changes[..changes.len().min(MAX_CHANGED_PATHS_RECORDED)];
    out.insert(
        "changed_paths".into(),
        Value::Array(kept.iter().map(|c| Value::String(c.rel.clone())).collect()),
    );
    let deleted: Vec<Value> = kept
        .iter()
        .filter(|c| c.kind == ChangeKind::Deleted)
        .map(|c| Value::String(c.rel.clone()))
        .collect();
    if !deleted.is_empty() {
        out.insert("deleted_paths".into(), Value::Array(deleted));
    }
    if changes.len() > kept.len() {
        out.insert(
            "changed_omitted".into(),
            Value::from(changes.len() - kept.len()),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamps(files: &[(&str, u64)], dirs: &[&str]) -> TreeStamps {
        TreeStamps {
            files: files
                .iter()
                .map(|(p, n)| (p.to_string(), (*n, None)))
                .collect(),
            dirs: dirs.iter().map(|d| d.to_string()).collect(),
        }
    }

    #[test]
    fn diff_classifies_and_names_created_dirs() {
        let before = stamps(&[("a.rs", 1), ("src/b.rs", 2), ("gone.rs", 3)], &["src"]);
        let after = stamps(
            &[
                ("a.rs", 1),
                ("src/b.rs", 5),
                ("src/gen/deep/c.rs", 1),
                ("new.rs", 1),
            ],
            &["src", "src/gen", "src/gen/deep"],
        );
        let d = diff(&before, &after);
        let kinds: Vec<(&str, ChangeKind)> =
            d.changes.iter().map(|c| (c.rel.as_str(), c.kind)).collect();
        assert_eq!(
            kinds,
            [
                ("gone.rs", ChangeKind::Deleted),
                ("new.rs", ChangeKind::Created),
                ("src/b.rs", ChangeKind::Modified),
                ("src/gen/deep/c.rs", ChangeKind::Created),
            ]
        );
        assert_eq!(d.created_dirs, ["src/gen", "src/gen/deep"]);
        assert_eq!(d.changes[2].before, Some((2, None)));
        assert_eq!(diff(&after, &after), TreeDiff::default());
    }

    #[test]
    fn ledger_fields_cap_and_mark_deletions() {
        assert!(ledger_fields(&[]).is_empty());
        let change = |rel: String, kind| CommandChange {
            rel,
            kind,
            before: None,
        };
        let mut many: Vec<CommandChange> = (0..MAX_CHANGED_PATHS_RECORDED + 3)
            .map(|i| change(format!("f{i:04}"), ChangeKind::Modified))
            .collect();
        many[1].kind = ChangeKind::Deleted;
        let f = ledger_fields(&many);
        assert_eq!(
            f["changed_paths"].as_array().unwrap().len(),
            MAX_CHANGED_PATHS_RECORDED
        );
        assert_eq!(f["deleted_paths"], serde_json::json!(["f0001"]));
        assert_eq!(f["changed_omitted"], 3);
    }
}
