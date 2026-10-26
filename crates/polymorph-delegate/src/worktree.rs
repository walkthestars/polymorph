//! Worktree isolation, implemented over the `git` CLI.
//!
//! Each child works in its own `git worktree` so siblings never share a
//! working tree ("no merge conflicts between siblings").
//! Implemented over `git` CLI via `tokio::process` (porcelain completeness,
//! zero unsafe). Merge-back of branches lives in the review crate — here:
//! create (detached
//! or branch), consuming `remove`, and `prune_orphans` for leaked trees.
//!
//! Orphan criterion: a `.polymorph-child` marker file WE wrote inside the
//! worktree. Foreign worktrees (no marker) are never touched.

use std::path::{Path, PathBuf};
use std::process::Stdio;

/// Marker file written into every worktree we create (content: child id).
pub const MARKER: &str = ".polymorph-child";

/// Branch namespace for child branches (`polymorph/<child-id>` default).
pub const BRANCH_NS: &str = "polymorph";

/// How to attach the new worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorktreeMode {
    /// Detached HEAD (read-mostly children: reviewers, planners).
    Detached,
    /// New branch (children that commit: act/fix executors in F).
    /// `None` name → `{BRANCH_NS}/{child_id}`.
    Branch(Option<String>),
}

/// A live child worktree. `remove(self)` is the only way out (consuming —
/// a removed worktree cannot be double-removed by construction).
#[derive(Debug)]
pub struct Worktree {
    path: PathBuf,
    repo: PathBuf,
    branch: Option<String>,
}

impl Worktree {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn branch(&self) -> Option<&str> {
        self.branch.as_deref()
    }

    /// Tear down: `worktree remove --force`, then delete our branch (only
    /// when WE created it — detached trees have no branch to delete).
    pub async fn remove(self) -> anyhow::Result<()> {
        git(
            &self.repo,
            &[
                "worktree",
                "remove",
                "--force",
                &self.path.to_string_lossy(),
            ],
        )
        .await?;
        if let Some(branch) = &self.branch {
            git(&self.repo, &["branch", "-D", branch]).await?;
        }
        Ok(())
    }
}

/// One detached worktree that lives for a whole review drive (`review
/// run|drive --repo`): every fix turn, verify command and reviewer works
/// in it, so round N+1 (and the reviewer, artifact set, peripheral
/// attachments and spill files) see round N's edits, while the operator's
/// working tree and index are never touched. Detached at the repo's HEAD
/// (no branch to leak). Removed by [`DriveWorktree::close`] — or by drop,
/// with the blocking git CLI, so error, panic and cancelled-future paths
/// clean up too; either way `git worktree prune` follows.
#[derive(Debug)]
pub struct DriveWorktree {
    path: PathBuf,
    repo: PathBuf,
    start: String,
    closed: bool,
}

impl DriveWorktree {
    /// Create `parent/<id>` detached at `repo`'s HEAD (refuses an existing
    /// target, like [`create`]) and record the start commit.
    pub async fn create(repo: &Path, parent: &Path, id: &str) -> anyhow::Result<Self> {
        let wt = create(repo, parent, id, &WorktreeMode::Detached).await?;
        let mut tree = Self {
            path: wt.path,
            repo: wt.repo,
            start: String::new(),
            closed: false,
        };
        // From here on the guard owns the tree: a failed rev-parse drops
        // (and removes) it.
        tree.start = git(&tree.path, &["rev-parse", "--short", "HEAD"])
            .await?
            .trim()
            .to_string();
        Ok(tree)
    }

    /// The worktree root (the drive's workdir).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The repo the worktree hangs off.
    pub fn repo(&self) -> &Path {
        &self.repo
    }

    /// Abbreviated commit the drive started from (the worktree's HEAD at
    /// creation; a fixer `git commit` later moves HEAD, never this).
    pub fn start_commit(&self) -> &str {
        &self.start
    }

    /// Remove the worktree now, reporting failure (drop only warns).
    pub fn close(mut self) -> anyhow::Result<()> {
        self.closed = true;
        remove_blocking(&self.repo, &self.path)
    }
}

impl Drop for DriveWorktree {
    fn drop(&mut self) {
        if !self.closed {
            if let Err(e) = remove_blocking(&self.repo, &self.path) {
                tracing::warn!(path = %self.path.display(), "drive worktree removal failed: {e:#}");
            }
        }
    }
}

/// Synchronous teardown for [`DriveWorktree`] (drop cannot await):
/// `worktree remove --force`; when git refuses (a file still held open on
/// Windows), delete the directory and let `worktree prune` drop the admin
/// entry. Prune always runs (it only forgets trees whose dir is gone).
fn remove_blocking(repo: &Path, path: &Path) -> anyhow::Result<()> {
    let git_sync = |args: &[&str]| -> anyhow::Result<()> {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| anyhow::anyhow!("cannot spawn git: {e}"))?;
        if !out.status.success() {
            anyhow::bail!(
                "git {} failed ({}): {}",
                args.join(" "),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    };
    let removed = git_sync(&["worktree", "remove", "--force", &path.to_string_lossy()]);
    let fallback = match removed {
        Ok(()) => Ok(()),
        Err(e) if path.exists() => std::fs::remove_dir_all(path)
            .map_err(|io| anyhow::anyhow!("{e:#}; removing {} failed: {io}", path.display())),
        Err(_) => Ok(()),
    };
    let pruned = git_sync(&["worktree", "prune"]);
    fallback.and(pruned)
}

/// A kept child worktree handed back to the caller for merge-back.
/// `Worktree::remove` is teardown;
/// this is handoff: the caller owns `merge_back` or `discard`, and the
/// outcome records whether the bus committed the tree.
#[derive(Debug)]
pub struct KeptWorktree {
    path: PathBuf,
    branch: Option<String>,
    repo: PathBuf,
    committed: bool,
}

impl KeptWorktree {
    pub fn new(path: PathBuf, branch: Option<String>, repo: PathBuf, committed: bool) -> Self {
        Self {
            path,
            branch,
            repo,
            committed,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn branch(&self) -> Option<&str> {
        self.branch.as_deref()
    }

    pub fn repo(&self) -> &Path {
        &self.repo
    }

    /// True when the bus committed the tree at child end (Branch +
    /// Completed + dirty; clean trees need no commit).
    pub fn committed(&self) -> bool {
        self.committed
    }

    /// Give up: remove the worktree and delete our branch. The merge-back
    /// dual of `Worktree::remove` (conflict paths already cleaned up by
    /// `merge_back` itself; this is for callers that decline to merge).
    pub async fn discard(self) -> anyhow::Result<()> {
        git(
            &self.repo,
            &[
                "worktree",
                "remove",
                "--force",
                &self.path.to_string_lossy(),
            ],
        )
        .await?;
        if let Some(branch) = &self.branch {
            git(&self.repo, &["branch", "-D", branch]).await?;
        }
        Ok(())
    }
}

/// Merge-back failure modes. Conflicts carry the file list (never prose);
/// every variant names the repo or branch at fault.
#[derive(Debug, PartialEq, thiserror::Error)]
pub enum MergeError {
    #[error("merge_back needs a branch (detached worktree at {path})")]
    NoBranch { path: String },
    #[error("merge_back refused: worktree {path} has uncommitted changes")]
    DirtyWorktree { path: String },
    #[error("merge_back refused: repo {repo} has uncommitted changes: {files:?}")]
    DirtyRepo { repo: String, files: Vec<String> },
    #[error("merge conflict merging {branch}: {files:?} (aborted, cleaned up)")]
    Conflict { branch: String, files: Vec<String> },
    #[error("git failed: {0}")]
    Git(String),
}

/// What a successful merge-back landed.
#[derive(Debug, Clone, PartialEq)]
pub struct MergeReport {
    pub branch: String,
    pub commit: String,
    pub files: Vec<String>,
}

/// Fallback commit identity for repos with no configured user (temp/bench
/// repos). Real repos keep their own identity: [`commit_identity_args`]
/// prefers the repo's configured `user.email`, falling back only when unset.
pub const FALLBACK_EMAIL: &str = "polymorph@local";
/// Fallback commit identity name (see [`FALLBACK_EMAIL`]).
pub const FALLBACK_NAME: &str = "polymorph-merge";

/// Commit-args for `dir`: empty when the repo configures its own identity
/// (operator identity respected), `-c` fallback otherwise (temp repos).
async fn commit_identity_args(dir: &Path) -> Vec<String> {
    match git(dir, &["config", "user.email"]).await {
        Ok(v) if !v.trim().is_empty() => Vec::new(),
        _ => vec![
            "-c".to_string(),
            format!("user.email={FALLBACK_EMAIL}"),
            "-c".to_string(),
            format!("user.name={FALLBACK_NAME}"),
        ],
    }
}

/// True when `status --short` output holds nothing but our marker (the
/// marker is untracked by design and must never force a commit or block
/// a merge — it rides with the tree, never into history).
fn is_effectively_clean(status: &str) -> bool {
    status.lines().all(|l| {
        let t = l.trim();
        t.is_empty() || t == format!("?? {MARKER}")
    })
}

/// Commit a worktree's current state. Clean tree → `Ok(false)` (no empty
/// commits, ever). Otherwise `git add -A` minus our marker (the marker
/// must never ride a squash into the caller's tree) + commit → `Ok(true)`.
/// Identity follows [`commit_identity_args`]. Untracked-but-ignored files
/// stay ignored (`add -A` honors `.gitignore`).
pub async fn commit_all(workdir: &Path, message: &str) -> anyhow::Result<bool> {
    if !is_effectively_clean(&git(workdir, &["status", "--short"]).await?) {
        let id = commit_identity_args(workdir).await;
        let mut cmd: Vec<String> = vec!["add".into(), "-A".into(), "--".into(), ".".into()];
        cmd.push(format!(":!{MARKER}"));
        git_ref(workdir, &cmd).await?;
        let mut commit: Vec<String> = vec!["commit".into(), "-m".into(), message.into()];
        let mut full = id;
        full.append(&mut commit);
        git_ref(workdir, &full).await?;
        return Ok(true);
    }
    Ok(false)
}

/// Merge a kept Branch worktree back into its repo and clean up.
/// Consuming: success reports, conflict aborts + cleans up
/// and names the files — the caller escalates, never auto-resolves.
/// Pre-checks (fail-fast, nothing touched): branch present, worktree
/// clean, repo clean.
pub async fn merge_back(kept: KeptWorktree, message: &str) -> Result<MergeReport, MergeError> {
    let branch = kept.branch.clone().ok_or_else(|| MergeError::NoBranch {
        path: kept.path.to_string_lossy().into_owned(),
    })?;
    if !is_effectively_clean(
        &status_short(&kept.path)
            .await
            .map_err(|e| MergeError::Git(e.to_string()))?,
    ) {
        return Err(MergeError::DirtyWorktree {
            path: kept.path.to_string_lossy().into_owned(),
        });
    }
    let repo_status = status_short(&kept.repo)
        .await
        .map_err(|e| MergeError::Git(e.to_string()))?;
    if !repo_status.trim().is_empty() {
        let files = status_files(&repo_status);
        return Err(MergeError::DirtyRepo {
            repo: kept.repo.to_string_lossy().into_owned(),
            files,
        });
    }
    // Identity first: git refuses to prepare a squash merge at all when no
    // user.email is resolvable (`Committer identity unknown`, exit 128), so
    // the merge must run under the same fallback identity as the commit.
    let id = commit_identity_args(&kept.repo).await;
    let mut merge_cmd = id.clone();
    merge_cmd.extend(["merge".to_string(), "--squash".to_string(), branch.clone()]);
    if let Err(merge_err) = git_ref(&kept.repo, &merge_cmd).await {
        // Squash leaves no MERGE_HEAD, so `--abort` is a no-op here:
        // `reset --hard` clears the conflict debris (the repo was
        // pre-checked clean, so nothing of value is discarded).
        let files = unmerged_files(&kept.repo).await;
        let _ = git(&kept.repo, &["reset", "--hard", "HEAD"]).await;
        let _ = cleanup_kept(&kept).await;
        // No unmerged paths ⇒ the merge died for a non-conflict reason
        // (locked index, unreadable tree, missing branch). Report the Git
        // fault verbatim: an empty `Conflict` would blame the child's
        // content for a plumbing failure (never mislabel).
        if files.is_empty() {
            return Err(MergeError::Git(merge_err.to_string()));
        }
        return Err(MergeError::Conflict { branch, files });
    }
    // Empty diff (child changed nothing durable): a squash that stages
    // nothing is a clean no-op, not an error — report HEAD with no files
    // (live catch: a failed fix otherwise mislabels as a Git fault).
    if status_short(&kept.repo)
        .await
        .map_err(|e| MergeError::Git(e.to_string()))?
        .trim()
        .is_empty()
    {
        let sha = git(&kept.repo, &["rev-parse", "HEAD"])
            .await
            .map_err(|e| MergeError::Git(e.to_string()))?;
        cleanup_kept(&kept)
            .await
            .map_err(|e| MergeError::Git(e.to_string()))?;
        return Ok(MergeReport {
            branch,
            commit: sha.trim().to_string(),
            files: Vec::new(),
        });
    }
    let mut commit: Vec<String> = vec!["commit".into(), "-m".into(), message.into()];
    let mut full = id;
    full.append(&mut commit);
    git_ref(&kept.repo, &full)
        .await
        .map_err(|e| MergeError::Git(e.to_string()))?;
    let sha = git(&kept.repo, &["rev-parse", "HEAD"])
        .await
        .map_err(|e| MergeError::Git(e.to_string()))?;
    let files = git(
        &kept.repo,
        &["show", "--name-only", "--pretty=format:", "HEAD"],
    )
    .await
    .map_err(|e| MergeError::Git(e.to_string()))?;
    cleanup_kept(&kept)
        .await
        .map_err(|e| MergeError::Git(e.to_string()))?;
    Ok(MergeReport {
        branch,
        commit: sha.trim().to_string(),
        files: files
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect(),
    })
}

/// `git status --short` output for `dir`.
async fn status_short(dir: &Path) -> anyhow::Result<String> {
    git(dir, &["status", "--short"]).await
}

/// File paths from `status --short` output (display only, never parsed
/// for control flow beyond listing).
fn status_files(out: &str) -> Vec<String> {
    out.lines()
        .filter_map(|l| l.split_whitespace().last().map(str::to_string))
        .collect()
}

/// Unmerged paths (`U` in either status column, or `AA`/`DD`) after a
/// failed merge. Marker-matched, never message-sniffed.
async fn unmerged_files(repo: &Path) -> Vec<String> {
    let Ok(out) = git(repo, &["status", "--short"]).await else {
        return Vec::new();
    };
    out.lines()
        .filter(|l| {
            let b = l.as_bytes();
            b.len() > 2
                && (b[0] == b'U' || b[1] == b'U' || l.starts_with("AA") || l.starts_with("DD"))
        })
        .filter_map(|l| l.split_whitespace().last().map(str::to_string))
        .collect()
}

/// Post-merge teardown shared by success and conflict paths.
async fn cleanup_kept(kept: &KeptWorktree) -> anyhow::Result<()> {
    git(
        &kept.repo,
        &[
            "worktree",
            "remove",
            "--force",
            &kept.path.to_string_lossy(),
        ],
    )
    .await?;
    if let Some(branch) = &kept.branch {
        git(&kept.repo, &["branch", "-D", branch]).await?;
    }
    Ok(())
}
pub fn branch_name(child_id: &str) -> String {
    format!("{BRANCH_NS}/{child_id}")
}

/// Create a worktree for `child_id` under `parent` (which must exist).
/// Refuses when the target dir already exists (never adopt stray state).
pub async fn create(
    repo: &Path,
    parent: &Path,
    child_id: &str,
    mode: &WorktreeMode,
) -> anyhow::Result<Worktree> {
    let path = parent.join(child_id);
    if path.exists() {
        anyhow::bail!(
            "worktree target {} already exists — refusing to adopt it",
            path.display()
        );
    }
    let branch: Option<String> = match mode {
        WorktreeMode::Detached => {
            git(
                repo,
                &[
                    "worktree",
                    "add",
                    "--detach",
                    &path.to_string_lossy(),
                    "HEAD",
                ],
            )
            .await?;
            None
        }
        WorktreeMode::Branch(name) => {
            let branch = name.clone().unwrap_or_else(|| branch_name(child_id));
            git(
                repo,
                &[
                    "worktree",
                    "add",
                    &path.to_string_lossy(),
                    "-b",
                    &branch,
                    "HEAD",
                ],
            )
            .await?;
            Some(branch)
        }
    };
    std::fs::write(path.join(MARKER), format!("{child_id}\n")).map_err(|e| {
        anyhow::anyhow!(
            "worktree created but marker unwritable at {}: {e}",
            path.display()
        )
    })?;
    Ok(Worktree {
        path,
        repo: repo.to_path_buf(),
        branch,
    })
}

/// Sweep leaked worktrees: any `git worktree list` entry (other than the
/// main tree) containing OUR marker is force-removed (+ its `polymorph/`
/// branch deleted). Returns the number removed. Foreign trees untouched.
pub async fn prune_orphans(repo: &Path) -> anyhow::Result<usize> {
    let out = git(repo, &["worktree", "list", "--porcelain"]).await?;
    let mut removed = 0;
    for block in out.split("\n\n") {
        let mut path: Option<&str> = None;
        let mut branch: Option<&str> = None;
        for line in block.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                path = Some(p.trim());
            }
            if let Some(b) = line.strip_prefix("branch refs/heads/") {
                branch = Some(b.trim());
            }
        }
        let Some(path) = path else { continue };
        if !Path::new(path).join(MARKER).exists() {
            continue; // main tree or foreign worktree — hands off.
        }
        git(repo, &["worktree", "remove", "--force", path]).await?;
        if let Some(b) = branch {
            if b.starts_with(BRANCH_NS) {
                git(repo, &["branch", "-D", b]).await?;
            }
        }
        removed += 1;
    }
    Ok(removed)
}

/// True when `path` is inside a git worktree/repo (used to refuse `--repo`
/// gracefully before spawning worktree children).
pub async fn is_git_repo(path: &Path) -> bool {
    git(path, &["rev-parse", "--git-dir"]).await.is_ok()
}

/// One `git -C <repo> <args>` call: piped stdio (never inherit),
/// stderr folded into the error (paths only, no secrets exist here).
async fn git(repo: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("cannot spawn git: {e}"))?;
    if !out.status.success() {
        // Both streams: git reports "nothing to commit" on stdout, so
        // stderr-only errors mislead (merge_back diagnosis).
        anyhow::bail!(
            "git {} failed ({}): {} {}",
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stdout).trim(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Owned-args twin of [`git`] for programmatically built argv (`-c`
/// identity flags, commit messages).
async fn git_ref(repo: &Path, args: &[String]) -> anyhow::Result<String> {
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    git(repo, &refs).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fresh repo with one commit: `init -b main` + local identity + file.
    /// Identity via `-c` flags (never touches the operator's gitconfig).
    async fn init_repo(dir: &Path) -> PathBuf {
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let run = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap();
            assert!(status.success(), "{args:?}");
        };
        run(&["init", "-b", "main"]);
        // Byte-deterministic fixtures: the operator's global `autocrlf`
        // otherwise round-trips working-tree bytes (CRLF catch).
        run(&["config", "core.autocrlf", "false"]);
        std::fs::write(repo.join("f.txt"), "v1\n").unwrap();
        run(&["add", "."]);
        run(&[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-m",
            "seed",
            "--quiet",
        ]);
        repo
    }

    fn wt_parent(dir: &Path) -> PathBuf {
        let p = dir.join("wt");
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// Comparison key for a path git may spell differently than we do: 8.3
    /// short names (`RUNNER~1`) versus the long form, backslashes versus
    /// slashes, and Windows case. Resolves through the filesystem and drops
    /// the `\\?\` verbatim prefix `canonicalize` adds. `None` when the path
    /// no longer resolves (a removed worktree).
    fn canonical_cmp(path: &Path) -> Option<String> {
        let canon = std::fs::canonicalize(path).ok()?;
        let text = canon.to_string_lossy().replace('\\', "/");
        let text = text
            .strip_prefix("//?/")
            .map_or(text.clone(), str::to_string);
        Some(text.to_ascii_lowercase())
    }

    #[tokio::test]
    async fn detached_create_remove_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path()).await;
        let wt = create(&repo, &wt_parent(dir.path()), "c1", &WorktreeMode::Detached)
            .await
            .unwrap();
        assert!(wt.path().join("f.txt").exists());
        assert!(wt.branch().is_none());
        assert_eq!(
            std::fs::read_to_string(wt.path().join(MARKER)).unwrap(),
            "c1\n"
        );
        let list = git(&repo, &["worktree", "list"]).await.unwrap();
        // Match on the worktree PATH, never substring "c1": the temp parent
        // itself may randomly contain "c1". Compare through the filesystem —
        // git prints the long form while %TEMP% may be the 8.3 short form
        // (`C:/Users/RUNNER~1/...` on windows CI), which broke string equality.
        let here = canonical_cmp(wt.path()).expect("live worktree path resolves");
        let listed = |list: &str| {
            list.lines().any(|l| {
                l.split_whitespace()
                    .next()
                    .and_then(|p| canonical_cmp(Path::new(p)))
                    .is_some_and(|c| c == here)
            })
        };
        assert!(listed(&list), "want {here:?} listed in:\n{list}");
        wt.remove().await.unwrap();
        assert!(!dir.path().join("wt").join("c1").exists());
        // Registration gone: only the main tree stays listed (count is
        // spelling-independent, so the 8.3/long-form split cannot hide it).
        let list = git(&repo, &["worktree", "list"]).await.unwrap();
        assert_eq!(list.lines().count(), 1, "still registered:\n{list}");
    }

    #[tokio::test]
    async fn branch_create_names_and_cleans_branch() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path()).await;
        let wt = create(
            &repo,
            &wt_parent(dir.path()),
            "c2",
            &WorktreeMode::Branch(None),
        )
        .await
        .unwrap();
        assert_eq!(wt.branch(), Some("polymorph/c2"));
        let branches = git(&repo, &["branch", "--list", "polymorph/c2"])
            .await
            .unwrap();
        assert!(branches.contains("polymorph/c2"));
        // Explicit name honored.
        let wt2 = create(
            &repo,
            &wt_parent(dir.path()),
            "c3",
            &WorktreeMode::Branch(Some("feat/x".into())),
        )
        .await
        .unwrap();
        assert_eq!(wt2.branch(), Some("feat/x"));
        wt.remove().await.unwrap();
        wt2.remove().await.unwrap();
        let branches = git(&repo, &["branch", "--list", "polymorph/*"])
            .await
            .unwrap();
        assert!(!branches.contains("polymorph/c2"));
        let branches = git(&repo, &["branch", "--list", "feat/x"]).await.unwrap();
        assert!(!branches.contains("feat/x"));
    }

    #[tokio::test]
    async fn create_refuses_existing_target() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path()).await;
        let parent = wt_parent(dir.path());
        std::fs::create_dir_all(parent.join("squat")).unwrap();
        let err = create(&repo, &parent, "squat", &WorktreeMode::Detached)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("already exists"));
    }

    #[tokio::test]
    async fn prune_orphans_sweeps_marked_spares_foreign() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path()).await;
        let parent = wt_parent(dir.path());
        // Leaked tree: raw git worktree + OUR marker (as if the guard died).
        let leaked = parent.join("leaked");
        git(
            &repo,
            &[
                "worktree",
                "add",
                "--detach",
                &leaked.to_string_lossy(),
                "HEAD",
            ],
        )
        .await
        .unwrap();
        std::fs::write(leaked.join(MARKER), "leaked\n").unwrap();
        // Foreign tree: no marker — hands off even though it matches nothing.
        let foreign = parent.join("foreign");
        git(
            &repo,
            &[
                "worktree",
                "add",
                "--detach",
                &foreign.to_string_lossy(),
                "HEAD",
            ],
        )
        .await
        .unwrap();
        assert_eq!(prune_orphans(&repo).await.unwrap(), 1);
        assert!(!leaked.exists());
        assert!(foreign.exists());
        assert_eq!(prune_orphans(&repo).await.unwrap(), 0);
        git(
            &repo,
            &["worktree", "remove", "--force", &foreign.to_string_lossy()],
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn is_git_repo_reports_honestly() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path()).await;
        assert!(is_git_repo(&repo).await);
        assert!(!is_git_repo(dir.path()).await);
    }

    async fn commit_file(tree: &Path, name: &str, content: &str) {
        std::fs::write(tree.join(name), content).unwrap();
        commit_all(tree, "test commit").await.unwrap();
    }

    #[tokio::test]
    async fn commit_all_skips_clean_trees() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path()).await;
        assert!(!commit_all(&repo, "nothing to commit").await.unwrap());
    }

    #[tokio::test]
    async fn commit_all_commits_dirty_trees_with_fallback_identity() {
        // init_repo sets NO user identity: the fallback must carry these
        // temp-only commits (operator gitconfig untouched by construction).
        // init_repo sets NO identity: blank the LOCAL user so any ambient
        // global identity cannot leak in (env is process-global
        // forbids env tricks; a local empty value wins scoping honestly).
        // `-c` fallback flags outrank it at commit time.
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path()).await;
        git(&repo, &["config", "user.email", ""]).await.unwrap();
        git(&repo, &["config", "user.name", ""]).await.unwrap();
        std::fs::write(repo.join("g.txt"), "v2\n").unwrap();
        assert!(commit_all(&repo, "dirty commit").await.unwrap());
        let author = git(&repo, &["log", "-1", "--format=%ae"]).await.unwrap();
        assert!(author.trim() == FALLBACK_EMAIL, "{author}");
    }

    #[tokio::test]
    async fn commit_all_respects_configured_identity() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path()).await;
        git(&repo, &["config", "user.email", "op@example.com"])
            .await
            .unwrap();
        git(&repo, &["config", "user.name", "op"]).await.unwrap();
        std::fs::write(repo.join("h.txt"), "v\n").unwrap();
        assert!(commit_all(&repo, "op commit").await.unwrap());
        let author = git(&repo, &["log", "-1", "--format=%ae"]).await.unwrap();
        assert!(author.trim() == "op@example.com", "{author}");
    }

    #[tokio::test]
    async fn commit_all_never_carries_the_marker() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path()).await;
        let wt = create(
            &repo,
            &wt_parent(dir.path()),
            "m1",
            &WorktreeMode::Branch(None),
        )
        .await
        .unwrap();
        std::fs::write(wt.path().join("work.txt"), "w\n").unwrap();
        assert!(commit_all(wt.path(), "marked work").await.unwrap());
        let files = git(
            &repo,
            &["show", "--name-only", "--pretty=format:", "polymorph/m1"],
        )
        .await
        .unwrap();
        assert!(files.contains("work.txt"), "{files}");
        assert!(!files.contains(MARKER), "{files}");
        wt.remove().await.unwrap();
    }

    #[tokio::test]
    async fn merge_back_lands_content_and_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path()).await;
        let wt = create(
            &repo,
            &wt_parent(dir.path()),
            "k1",
            &WorktreeMode::Branch(None),
        )
        .await
        .unwrap();
        commit_file(wt.path(), "fix.txt", "fixed\n").await;
        let kept = KeptWorktree::new(
            wt.path().to_path_buf(),
            wt.branch().map(str::to_string),
            repo.clone(),
            true,
        );
        std::mem::forget(wt); // caller-owned now, like keep_worktree.
        let report = merge_back(kept, "merge k1").await.unwrap();
        assert_eq!(report.branch, "polymorph/k1");
        assert_eq!(report.commit.len(), 40);
        assert_eq!(report.files, vec!["fix.txt".to_string()]);
        assert_eq!(
            std::fs::read_to_string(repo.join("fix.txt")).unwrap(),
            "fixed\n"
        );
        assert!(!dir.path().join("wt").join("k1").exists());
        let branches = git(&repo, &["branch", "--list", "polymorph/k1"])
            .await
            .unwrap();
        assert!(!branches.contains("polymorph/k1"), "{branches}");
    }

    #[tokio::test]
    async fn merge_back_conflict_is_typed_and_leaves_clean() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path()).await;
        // Both branches fork from the SAME base BEFORE either merges —
        // sequential merge-then-fork goes linear and never conflicts
        // (test-shape catch).
        let mut kept_a = None;
        let mut kept_b = None;
        for (id, content, slot) in [("a1", "A\n", &mut kept_a), ("b1", "B\n", &mut kept_b)] {
            let wt = create(
                &repo,
                &wt_parent(dir.path()),
                id,
                &WorktreeMode::Branch(None),
            )
            .await
            .unwrap();
            commit_file(wt.path(), "f.txt", content).await;
            *slot = Some(KeptWorktree::new(
                wt.path().to_path_buf(),
                wt.branch().map(str::to_string),
                repo.clone(),
                true,
            ));
            std::mem::forget(wt);
        }
        merge_back(kept_a.unwrap(), "merge a1").await.unwrap();
        let err = merge_back(kept_b.unwrap(), "merge b1").await.unwrap_err();
        match err {
            MergeError::Conflict { branch, files } => {
                assert_eq!(branch, "polymorph/b1");
                assert!(files.iter().any(|f| f.contains("f.txt")), "{files:?}");
            }
            _ => panic!("want Conflict, got {err:?}"),
        }
        // Aborted + cleaned: main tree clean, no worktrees, no branches.
        let status = git(&repo, &["status", "--short"]).await.unwrap();
        assert!(status.trim().is_empty(), "{status}");
        let list = git(&repo, &["worktree", "list"]).await.unwrap();
        assert_eq!(list.lines().count(), 1, "{list}");
        let branches = git(&repo, &["branch", "--list", "polymorph/*"])
            .await
            .unwrap();
        assert!(!branches.contains("polymorph/"), "{branches}");
    }

    #[tokio::test]
    async fn merge_back_empty_diff_is_a_clean_noop() {
        // A child that changed nothing durable
        // (failed fix, clean tree) must not mislabel as a Git fault.
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path()).await;
        let before = git(&repo, &["rev-parse", "HEAD"]).await.unwrap();
        let wt = create(
            &repo,
            &wt_parent(dir.path()),
            "n1",
            &WorktreeMode::Branch(None),
        )
        .await
        .unwrap();
        let kept = KeptWorktree::new(
            wt.path().to_path_buf(),
            wt.branch().map(str::to_string),
            repo.clone(),
            false,
        );
        std::mem::forget(wt);
        let report = merge_back(kept, "merge n1").await.unwrap();
        assert_eq!(report.branch, "polymorph/n1");
        assert!(report.files.is_empty());
        assert_eq!(report.commit.trim(), before.trim());
        let list = git(&repo, &["worktree", "list"]).await.unwrap();
        assert_eq!(list.lines().count(), 1, "{list}");
    }

    #[tokio::test]
    async fn merge_back_refuses_dirty_repo_and_detached() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path()).await;
        // Detached kept has no branch: refused before touching anything.
        let wt = create(&repo, &wt_parent(dir.path()), "d1", &WorktreeMode::Detached)
            .await
            .unwrap();
        let kept = KeptWorktree::new(
            wt.path().to_path_buf(),
            wt.branch().map(str::to_string),
            repo.clone(),
            false,
        );
        std::mem::forget(wt);
        assert!(matches!(
            merge_back(kept, "merge d1").await.unwrap_err(),
            MergeError::NoBranch { .. }
        ));
        // Refusal precedes cleanup: sweep the declined tree directly.
        git(
            &repo,
            &[
                "worktree",
                "remove",
                "--force",
                &dir.path().join("wt").join("d1").to_string_lossy(),
            ],
        )
        .await
        .unwrap();
        // Dirty repo: the branch survives for the caller's retry.
        let wt = create(
            &repo,
            &wt_parent(dir.path()),
            "e1",
            &WorktreeMode::Branch(None),
        )
        .await
        .unwrap();
        commit_file(wt.path(), "ok.txt", "ok\n").await;
        std::fs::write(repo.join("dirty.txt"), "dirty\n").unwrap();
        let kept = KeptWorktree::new(
            wt.path().to_path_buf(),
            wt.branch().map(str::to_string),
            repo.clone(),
            true,
        );
        std::mem::forget(wt);
        let err = merge_back(kept, "merge e1").await.unwrap_err();
        assert!(matches!(err, MergeError::DirtyRepo { .. }), "{err:?}");
        let branches = git(&repo, &["branch", "--list", "polymorph/e1"])
            .await
            .unwrap();
        assert!(branches.contains("polymorph/e1"), "{branches}");
        let kept = KeptWorktree::new(
            dir.path().join("wt").join("e1"),
            Some("polymorph/e1".to_string()),
            repo.clone(),
            true,
        );
        std::fs::remove_file(repo.join("dirty.txt")).unwrap();
        let report = merge_back(kept, "merge e1 retry").await.unwrap();
        assert_eq!(report.files, vec!["ok.txt".to_string()]);
    }

    /// No worktree but the main one, and no branch but `main`.
    async fn assert_no_litter(repo: &Path) {
        let list = git(repo, &["worktree", "list"]).await.unwrap();
        assert_eq!(list.lines().count(), 1, "{list}");
        let branches = git(repo, &["branch", "--list"]).await.unwrap();
        assert_eq!(branches.trim(), "* main", "{branches}");
    }

    /// `--repo` drive tree: detached at HEAD (start commit recorded), edits
    /// stay inside it, `close` removes it with no worktree, branch or
    /// operator-tree change left behind.
    #[tokio::test]
    async fn drive_worktree_isolates_and_closes_clean() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path()).await;
        let head = git(&repo, &["rev-parse", "--short", "HEAD"]).await.unwrap();
        let tree = DriveWorktree::create(&repo, &wt_parent(dir.path()), "drive-0")
            .await
            .unwrap();
        assert_eq!(tree.start_commit(), head.trim());
        assert_eq!(tree.repo(), repo.as_path());
        std::fs::write(tree.path().join("f.txt"), "edited\n").unwrap();
        std::fs::write(tree.path().join("new.txt"), "n\n").unwrap();
        assert_eq!(std::fs::read_to_string(repo.join("f.txt")).unwrap(), "v1\n");
        assert!(!repo.join("new.txt").exists());
        let path = tree.path().to_path_buf();
        tree.close().unwrap();
        assert!(!path.exists());
        assert_no_litter(&repo).await;
        let status = git(&repo, &["status", "--short"]).await.unwrap();
        assert!(status.trim().is_empty(), "{status}");
    }

    /// Error, panic and cancel paths: dropping the guard (here during a
    /// panic unwind) removes the tree just like `close`.
    #[tokio::test]
    async fn drive_worktree_drop_removes_on_panic() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path()).await;
        let tree = DriveWorktree::create(&repo, &wt_parent(dir.path()), "drive-p")
            .await
            .unwrap();
        let path = tree.path().to_path_buf();
        std::fs::write(path.join("dirty.txt"), "d\n").unwrap();
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _held = tree;
            panic!("mid-drive failure");
        }));
        assert!(unwound.is_err());
        assert!(!path.exists());
        assert_no_litter(&repo).await;
    }
}
