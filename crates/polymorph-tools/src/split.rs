//! Atomic-split planner.
//!
//! Pure planning over CLI-shaped text plus ONE spawn edge:
//! [`plan_commits`] (offline: status + diff strings in, ordered groups
//! out) and [`plan_live`] (spawns `git status --porcelain=v1` +
//! `git diff --unified=0` in `repo`, then plans). Outputs are ordered
//! [`SplitGroup`]s the caller can commit one per atomic commit —
//! propose-only, this module never writes history.
//!
//! Content-signal floor (v2 T1, stated): same-file clustering — every
//! hunk lands in its own file's group, so zero cross-pollution holds by
//! construction. Cross-file dependency edges enter explicitly via
//! `deps` (`before -> after` on file paths); import-graph inference is
//! NOT guessed here. Cycles refuse via [`SplitError::Cycle`] BEFORE any
//! write.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// One changed path from `status --porcelain=v1`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedFile {
    pub path: String,
    pub staged: bool,
    pub kind: FileKind,
}

/// Change kind (porcelain XY collapsed to the interesting cases).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Modified,
    Added,
    Deleted,
    Renamed,
    Untracked,
}

/// One `--unified=0` hunk header with its file attached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub file: String,
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
}

/// One atomic-commit unit: v1 always single-file (see module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitGroup {
    pub id: String,
    pub files: Vec<String>,
    pub hunks: Vec<Hunk>,
}

/// Planner refusal. Every variant names the fault (line/file/cycle).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SplitError {
    #[error("bad porcelain line {line}: {detail}")]
    BadPorcelain { line: usize, detail: String },
    #[error("bad diff line {line}: {detail}")]
    BadDiff { line: usize, detail: String },
    #[error("diff has context lines (pass --unified=0): first at line {line}")]
    NonZeroContext { line: usize },
    #[error("dependency cycle: {names:?}")]
    Cycle { names: Vec<String> },
    #[error("dependency on unknown file: {file}")]
    UnknownDep { file: String },
}

/// Parse `git status --porcelain=v1` output.
///
/// Rename lines (`R old -> new`) take the NEW path. `??` paths are
/// untracked. Anything shorter than `XY␣path` refuses naming the line.
pub fn parse_porcelain(status: &str) -> Result<Vec<ChangedFile>, SplitError> {
    let mut out = Vec::new();
    for (i, raw) in status.lines().enumerate() {
        let line_no = i + 1;
        if raw.trim().is_empty() {
            continue;
        }
        if raw.len() < 4 {
            return Err(SplitError::BadPorcelain {
                line: line_no,
                detail: raw.to_string(),
            });
        }
        let x = raw.as_bytes()[0] as char;
        let y = raw.as_bytes()[1] as char;
        let rest = &raw[3..];
        if x == '?' && y == '?' {
            out.push(ChangedFile {
                path: rest.to_string(),
                staged: false,
                kind: FileKind::Untracked,
            });
            continue;
        }
        if x == 'R' || y == 'R' {
            let new_path = rest.split(" -> ").last().unwrap_or(rest).trim();
            if new_path.is_empty() {
                return Err(SplitError::BadPorcelain {
                    line: line_no,
                    detail: raw.to_string(),
                });
            }
            out.push(ChangedFile {
                path: new_path.to_string(),
                staged: x != ' ',
                kind: FileKind::Renamed,
            });
            continue;
        }
        let kind = match (x, y) {
            ('A', _) | (_, 'A') => FileKind::Added,
            ('D', _) | (_, 'D') => FileKind::Deleted,
            ('M', _) | (_, 'M') => FileKind::Modified,
            _ => {
                return Err(SplitError::BadPorcelain {
                    line: line_no,
                    detail: raw.to_string(),
                });
            }
        };
        out.push(ChangedFile {
            path: rest.to_string(),
            staged: x != ' ',
            kind,
        });
    }
    Ok(out)
}

fn strip_prefix(path: &str) -> String {
    for p in ["a/", "b/"] {
        if let Some(s) = path.strip_prefix(p) {
            return s.to_string();
        }
    }
    // Quoted paths (`"a/sp ace.rs"`) — strip quotes, then prefixes.
    let unquoted = path.trim_matches('"');
    for p in ["a/", "b/"] {
        if let Some(s) = unquoted.strip_prefix(p) {
            return s.to_string();
        }
    }
    unquoted.to_string()
}

fn parse_range(s: &str) -> Option<(u32, u32)> {
    let s = s.trim_start_matches(['-', '+']);
    match s.split_once(',') {
        Some((a, b)) => Some((a.parse().ok()?, b.parse().ok()?)),
        None => Some((s.parse().ok()?, 1)),
    }
}

/// Parse `git diff --unified=0` output into hunks.
///
/// Only `---`/`+++` file headers and `@@` hunk headers are read; body
/// lines are skipped EXCEPT ` ` (space) context lines, which refuse with
/// [`SplitError::NonZeroContext`] (the caller must pass `--unified=0`).
pub fn parse_unified0(diff: &str) -> Result<Vec<Hunk>, SplitError> {
    let mut hunks = Vec::new();
    let mut a_path: Option<String> = None;
    let mut cur_file: Option<String> = None;
    for (i, raw) in diff.lines().enumerate() {
        let line_no = i + 1;
        if let Some(rest) = raw.strip_prefix("--- ") {
            let p = rest.split('\t').next().unwrap_or(rest).trim();
            a_path = Some(if p == "/dev/null" {
                String::new()
            } else {
                strip_prefix(p)
            });
            continue;
        }
        if let Some(rest) = raw.strip_prefix("+++ ") {
            let p = rest.split('\t').next().unwrap_or(rest).trim();
            if p == "/dev/null" {
                cur_file = a_path.clone().filter(|s| !s.is_empty());
            } else {
                cur_file = Some(strip_prefix(p));
            }
            continue;
        }
        if let Some(header) = raw.strip_prefix("@@") {
            let file = cur_file.clone().ok_or(SplitError::BadDiff {
                line: line_no,
                detail: "hunk before file header".into(),
            })?;
            let inner = header.trim_end_matches("@").trim();
            // `inner` is `-a[,b] +c[,d]` (trailing section headings after
            // the second `@@` were trimmed above with the header split).
            let mut parts = inner.split_whitespace();
            let old_s = parts.next().ok_or(SplitError::BadDiff {
                line: line_no,
                detail: raw.to_string(),
            })?;
            let new_s = parts.next().ok_or(SplitError::BadDiff {
                line: line_no,
                detail: raw.to_string(),
            })?;
            let (old_start, old_lines) = parse_range(old_s).ok_or(SplitError::BadDiff {
                line: line_no,
                detail: raw.to_string(),
            })?;
            let (new_start, new_lines) = parse_range(new_s).ok_or(SplitError::BadDiff {
                line: line_no,
                detail: raw.to_string(),
            })?;
            hunks.push(Hunk {
                file,
                old_start,
                old_lines,
                new_start,
                new_lines,
            });
            continue;
        }
        if raw.starts_with(' ') {
            return Err(SplitError::NonZeroContext { line: line_no });
        }
    }
    Ok(hunks)
}

/// Cluster hunks by file (deterministic file-sorted order).
///
/// Group ids are `split-01`, `split-02`, … in sorted-file order.
pub fn cluster_by_file(hunks: &[Hunk]) -> Vec<SplitGroup> {
    let mut map: BTreeMap<&str, Vec<Hunk>> = BTreeMap::new();
    for h in hunks {
        map.entry(h.file.as_str()).or_default().push(h.clone());
    }
    map.into_iter()
        .enumerate()
        .map(|(i, (file, hs))| SplitGroup {
            id: format!("split-{:02}", i + 1),
            files: vec![file.to_string()],
            hunks: hs,
        })
        .collect()
}

/// Order groups over explicit file-level `deps` (`(before, after)`).
///
/// Stable Kahn sort: ready groups emit in file-sorted order. Unknown
/// dep files refuse; cycles refuse naming the cycle members (sorted).
pub fn order_groups(
    groups: Vec<SplitGroup>,
    deps: &[(String, String)],
) -> Result<Vec<SplitGroup>, SplitError> {
    if groups.is_empty() {
        return Ok(groups);
    }
    let file_to_group: BTreeMap<&str, usize> = groups
        .iter()
        .enumerate()
        .flat_map(|(gi, g)| g.files.iter().map(move |f| (f.as_str(), gi)))
        .collect();
    for (b, a) in deps {
        if !file_to_group.contains_key(b.as_str()) {
            return Err(SplitError::UnknownDep { file: b.clone() });
        }
        if !file_to_group.contains_key(a.as_str()) {
            return Err(SplitError::UnknownDep { file: a.clone() });
        }
    }
    // Group-level edges (deduped), ignoring self-edges within one group.
    let n = groups.len();
    let mut incoming: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); n];
    let mut outgoing: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); n];
    for (b, a) in deps {
        let gb = file_to_group[b.as_str()];
        let ga = file_to_group[a.as_str()];
        if gb == ga {
            continue;
        }
        if outgoing[gb].insert(ga) {
            incoming[ga].insert(gb);
        }
    }
    // Stable Kahn: ready set ordered by (first file, id).
    let mut ready: Vec<usize> = (0..n).filter(|g| incoming[*g].is_empty()).collect();
    let key = |g: &usize| {
        (
            groups[*g].files.first().cloned().unwrap_or_default(),
            groups[*g].id.clone(),
        )
    };
    ready.sort_by_key(|a| key(a));
    let mut order = Vec::with_capacity(n);
    while let Some(g) = ready.first().cloned() {
        ready.remove(0);
        order.push(g);
        let nexts: Vec<usize> = outgoing[g].iter().cloned().collect();
        for nx in nexts {
            incoming[nx].remove(&g);
            if incoming[nx].is_empty() {
                ready.push(nx);
            }
        }
        ready.sort_by_key(|a| key(a));
    }
    if order.len() != n {
        let mut names: Vec<String> = groups
            .iter()
            .enumerate()
            .filter(|(gi, _)| !order.contains(gi))
            .flat_map(|(_, g)| g.files.clone())
            .collect();
        names.sort();
        return Err(SplitError::Cycle { names });
    }
    Ok(order.into_iter().map(|gi| groups[gi].clone()).collect())
}

/// Full plan: parse status + diff, cluster hunks, add hunk-less changed
/// files as empty single-file groups, then order by `deps`.
pub fn plan_commits(
    status: &str,
    diff: &str,
    deps: &[(String, String)],
) -> Result<Vec<SplitGroup>, SplitError> {
    let files = parse_porcelain(status)?;
    let hunks = parse_unified0(diff)?;
    let mut groups = cluster_by_file(&hunks);
    let covered: BTreeSet<&str> = groups
        .iter()
        .flat_map(|g| g.files.iter().map(String::as_str))
        .collect();
    let mut missing: Vec<&str> = files
        .iter()
        .map(|f| f.path.as_str())
        .filter(|p| !covered.contains(*p))
        .collect();
    missing.sort();
    missing.dedup();
    for path in missing {
        groups.push(SplitGroup {
            id: String::new(), // re-numbered below
            files: vec![path.to_string()],
            hunks: Vec::new(),
        });
    }
    // Re-number deterministically after the merge (file-sorted).
    groups.sort_by(|a, b| a.files.cmp(&b.files));
    for (i, g) in groups.iter_mut().enumerate() {
        g.id = format!("split-{:02}", i + 1);
    }
    order_groups(groups, deps)
}

/// One `git -C <repo> <args>` call: piped stdio, stderr folded into the error. Paths only, no
/// secrets exist here (mirrors `delegate::worktree::git`; not imported — delegate is not a tools
/// dep, stated).
async fn git_out(repo: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("split: cannot spawn git for {}: {e}", repo.display()))?;
    if !out.status.success() {
        anyhow::bail!(
            "split: git {} failed for {} ({}): {} {}",
            args.join(" "),
            repo.display(),
            out.status,
            String::from_utf8_lossy(&out.stdout).trim(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Live plan for `repo`: spawn status + unified-0 diff, then
/// [`plan_commits`]. Propose-only — no commit is created. Errors name
/// the repo (non-repos fail here, never as empty plans).
pub async fn plan_live(repo: &Path, deps: &[(String, String)]) -> anyhow::Result<Vec<SplitGroup>> {
    let status = git_out(repo, &["status", "--porcelain=v1"]).await?;
    let diff = git_out(repo, &["diff", "--unified=0"]).await?;
    plan_commits(&status, &diff, deps)
        .map_err(|e| anyhow::anyhow!("split: {}: {e}", repo.display()))
}

/// Canonical JSON shape for tool/read surfaces (kept here so both
/// surfaces emit byte-identical previews).
pub fn groups_json(repo: &Path, groups: &[SplitGroup]) -> serde_json::Value {
    serde_json::json!({
        "repo": repo.display().to_string(),
        "groups": groups.iter().map(|g| serde_json::json!({
            "id": g.id,
            "files": g.files,
            "hunks": g.hunks.iter().map(|h| serde_json::json!({
                "file": h.file,
                "old_start": h.old_start,
                "old_lines": h.old_lines,
                "new_start": h.new_start,
                "new_lines": h.new_lines,
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porcelain_matrix() {
        let got = parse_porcelain(" M src/a.rs\nM  src/b.rs\n?? new.txt\n").unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].kind, FileKind::Modified);
        assert!(!got[0].staged);
        assert!(got[1].staged);
        assert_eq!(got[2].kind, FileKind::Untracked);
    }

    #[test]
    fn porcelain_rename_takes_new_path() {
        let got = parse_porcelain("R  old.rs -> new.rs\n").unwrap();
        assert_eq!(got[0].path, "new.rs");
        assert_eq!(got[0].kind, FileKind::Renamed);
    }

    #[test]
    fn porcelain_malformed_names_line() {
        let err = parse_porcelain("M\n").unwrap_err();
        assert!(matches!(err, SplitError::BadPorcelain { line: 1, .. }));
    }

    #[test]
    fn diff_multi_file_hunks() {
        let diff = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,0 +2,2 @@\n+x\n+y\n\
             diff --git a/b.rs b/b.rs\n--- a/b.rs\n+++ b/b.rs\n@@ -5,0 +6,1 @@\n+z\n";
        let hunks = parse_unified0(diff).unwrap();
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0].file, "a.rs");
        assert_eq!(hunks[1].file, "b.rs");
        assert_eq!(hunks[0].new_start, 2);
        assert_eq!(hunks[0].new_lines, 2);
    }

    #[test]
    fn diff_context_refused() {
        let diff =
            "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,3 +1,3 @@\n ctx\n-x\n+y\n";
        let err = parse_unified0(diff).unwrap_err();
        assert!(matches!(err, SplitError::NonZeroContext { .. }));
    }

    #[test]
    fn cluster_no_cross_pollution() {
        let hunks = vec![
            Hunk {
                file: "b.rs".into(),
                old_start: 1,
                old_lines: 0,
                new_start: 2,
                new_lines: 1,
            },
            Hunk {
                file: "a.rs".into(),
                old_start: 1,
                old_lines: 0,
                new_start: 1,
                new_lines: 1,
            },
            Hunk {
                file: "a.rs".into(),
                old_start: 9,
                old_lines: 0,
                new_start: 9,
                new_lines: 1,
            },
        ];
        let groups = cluster_by_file(&hunks);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].files, vec!["a.rs".to_string()]);
        assert_eq!(groups[0].hunks.len(), 2);
        assert_eq!(groups[1].files, vec!["b.rs".to_string()]);
        assert!(groups[0].hunks.iter().all(|h| h.file == "a.rs"));
    }

    #[test]
    fn order_respects_deps() {
        let groups = cluster_by_file(&[
            Hunk {
                file: "b.rs".into(),
                old_start: 1,
                old_lines: 0,
                new_start: 1,
                new_lines: 1,
            },
            Hunk {
                file: "a.rs".into(),
                old_start: 1,
                old_lines: 0,
                new_start: 1,
                new_lines: 1,
            },
        ]);
        let ordered = order_groups(groups, &[("a.rs".to_string(), "b.rs".to_string())]).unwrap();
        assert_eq!(ordered[0].files, vec!["a.rs".to_string()]);
        assert_eq!(ordered[1].files, vec!["b.rs".to_string()]);
    }

    #[test]
    fn unknown_dep_names_file() {
        let groups = cluster_by_file(&[Hunk {
            file: "a.rs".into(),
            old_start: 1,
            old_lines: 0,
            new_start: 1,
            new_lines: 1,
        }]);
        let err =
            order_groups(groups, &[("a.rs".to_string(), "ghost.rs".to_string())]).unwrap_err();
        assert_eq!(
            err,
            SplitError::UnknownDep {
                file: "ghost.rs".to_string()
            }
        );
    }

    #[test]
    fn cycle_names_members() {
        let groups = cluster_by_file(&[
            Hunk {
                file: "a.rs".into(),
                old_start: 1,
                old_lines: 0,
                new_start: 1,
                new_lines: 1,
            },
            Hunk {
                file: "b.rs".into(),
                old_start: 1,
                old_lines: 0,
                new_start: 1,
                new_lines: 1,
            },
        ]);
        let err = order_groups(
            groups,
            &[
                ("a.rs".to_string(), "b.rs".to_string()),
                ("b.rs".to_string(), "a.rs".to_string()),
            ],
        )
        .unwrap_err();
        assert_eq!(
            err,
            SplitError::Cycle {
                names: vec!["a.rs".to_string(), "b.rs".to_string()]
            }
        );
    }

    #[test]
    fn plan_seeded_pr_splits_two() {
        let status = " M src/a.rs\n M src/b.rs\n";
        let diff = "diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1,0 +2,1 @@\n+x\n\
             diff --git a/src/b.rs b/src/b.rs\n--- a/src/b.rs\n+++ b/src/b.rs\n@@ -3,0 +4,1 @@\n+y\n";
        let plan = plan_commits(status, diff, &[]).unwrap();
        assert_eq!(plan.len(), 2);
        assert!(plan.iter().all(|g| g.files.len() == 1));
        let total: usize = plan.iter().map(|g| g.hunks.len()).sum();
        assert_eq!(total, 2);
    }

    /// Fresh repo with one commit (mirrors worktree.rs `init_repo`:
    /// `init -b main`, autocrlf off, `-c` identity — never the
    /// operator's gitconfig). Sync setup; the live fns stay async.
    fn init_repo(dir: &Path) -> std::path::PathBuf {
        use std::process::{Command, Stdio};
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let run = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(&repo)
                    .args(args)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .unwrap()
                    .success(),
                "{args:?}"
            );
        };
        run(&["init", "-b", "main"]);
        run(&["config", "core.autocrlf", "false"]);
        std::fs::write(repo.join("a.txt"), "one\n").unwrap();
        std::fs::write(repo.join("b.txt"), "uno\n").unwrap();
        run(&["add", "."]);
        run(&[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-m",
            "seed",
        ]);
        repo
    }

    #[tokio::test]
    async fn live_two_dirty_files_split_two() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path());
        std::fs::write(repo.join("a.txt"), "one\ntwo\n").unwrap();
        std::fs::write(repo.join("b.txt"), "uno\ndos\n").unwrap();
        let groups = plan_live(&repo, &[]).await.unwrap();
        assert_eq!(groups.len(), 2);
        assert!(groups.iter().all(|g| g.files.len() == 1));
        let total: usize = groups.iter().map(|g| g.hunks.len()).sum();
        assert_eq!(total, 2);
    }

    #[tokio::test]
    async fn live_clean_repo_plans_empty() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path());
        let groups = plan_live(&repo, &[]).await.unwrap();
        assert!(groups.is_empty());
    }

    #[tokio::test]
    async fn live_non_repo_names_path() {
        let dir = tempfile::tempdir().unwrap();
        let err = plan_live(dir.path(), &[]).await.unwrap_err().to_string();
        assert!(err.contains(&dir.path().display().to_string()), "{err}");
    }

    #[test]
    fn groups_json_shape() {
        let groups = cluster_by_file(&[Hunk {
            file: "a.txt".into(),
            old_start: 1,
            old_lines: 1,
            new_start: 1,
            new_lines: 2,
        }]);
        let v = groups_json(std::path::Path::new("r"), &groups);
        assert_eq!(v["repo"], serde_json::json!("r"));
        assert_eq!(v["groups"][0]["id"], serde_json::json!("split-01"));
        assert_eq!(v["groups"][0]["files"], serde_json::json!(["a.txt"]));
        assert_eq!(
            v["groups"][0]["hunks"][0]["new_lines"],
            serde_json::json!(2)
        );
    }
}
