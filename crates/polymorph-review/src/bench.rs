//! Agent-bench corpus + scoring primitives.
//!
//! The seeded corpus (TOML manifest, B6-dialect schema) and the LLM-free
//! machinery around it: loading, tempdir materialization, deadline-bound
//! `cargo test` runs, hidden-test scoring. Live loop-driving lives in the
//! CLI (`review bench`); this module stays drivable from unit tests.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Default `cargo test` deadline per run: a hung compiler fails the task,
/// never the CI shift.
pub const CARGO_DEADLINE: Duration = Duration::from_secs(180);

#[derive(Debug, Clone, serde::Deserialize)]
pub struct Corpus {
    pub task: Vec<Task>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct Task {
    pub id: String,
    pub description: String,
    pub instruction: String,
    pub fail_to_pass: Vec<String>,
    pub pass_to_pass: Vec<String>,
    pub files: BTreeMap<String, String>,
    pub fix: Fix,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct Fix {
    pub files: BTreeMap<String, String>,
}

/// Parse a corpus manifest (TOML text). Corrupt manifests are hard errors
/// naming the failure — never empty corpora (F5c precedent: empties fail).
pub fn load_corpus(text: &str) -> anyhow::Result<Corpus> {
    let corpus: Corpus = toml::from_str(text).map_err(|e| anyhow::anyhow!("corpus TOML: {e}"))?;
    if corpus.task.is_empty() {
        anyhow::bail!("corpus TOML: no tasks");
    }
    Ok(corpus)
}

/// Write task files under `dir` (creating parents). Traversal outside the
/// root is a hard error — corpus data never escapes its tempdir.
pub fn write_files(dir: &Path, files: &BTreeMap<String, String>) -> anyhow::Result<()> {
    for (rel, content) in files {
        if rel.contains("..") {
            anyhow::bail!("task file escapes the tempdir: {rel}");
        }
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, content)?;
    }
    Ok(())
}

/// Materialize task files as a committable repo: `write_files` + `git init -b main` + seed commit
/// under a FIXED fallback identity (temp repos only — never the operator's gitconfig; worktree.rs
/// identity rule still governs later fixer commits). Worktree children need a repo; plain
/// `write_files` copies stay non-repos for InPlace arms.
///
/// Build debris discipline: the fixer runs `cargo test` inside the repo, which writes `target/` +
/// `Cargo.lock` — without an ignore, that dirt trips merge_back's repo-clean pre-check AND bloats
/// branch commits via `add -A`. A seeded `.gitignore` keeps both clean. A manifest-shipped
/// `.gitignore` wins when present (never overwritten).
pub fn materialize_repo(dir: &Path, files: &BTreeMap<String, String>) -> anyhow::Result<()> {
    use std::process::{Command, Stdio};
    write_files(dir, files)?;
    if !files.contains_key(".gitignore") {
        std::fs::write(dir.join(".gitignore"), "/target\nCargo.lock\n")?;
    }
    let run = |args: &[&str]| -> anyhow::Result<()> {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|e| anyhow::anyhow!("cannot spawn git: {e}"))?;
        if status.success() {
            Ok(())
        } else {
            anyhow::bail!("git {} failed in {}", args.join(" "), dir.display())
        }
    };
    run(&["init", "-b", "main"])?;
    run(&["config", "core.autocrlf", "false"])?;
    run(&["add", "-A"])?;
    run(&[
        "-c",
        "user.email=polymorph@local",
        "-c",
        "user.name=polymorph-merge",
        "commit",
        "-m",
        "bench seed",
        "--quiet",
    ])?;
    Ok(())
}

/// Run `cargo test` in `dir` with a hard deadline. Returns combined stdout+stderr output and
/// whether the suite passed. A deadline breach kills the direct child and reports as a failed run,
/// never a hang.
pub fn cargo_test(dir: &Path, deadline: Duration) -> (String, bool) {
    let mut child = Command::new("cargo")
        .arg("test")
        .arg("--color")
        .arg("never")
        .current_dir(dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("cargo must spawn");
    let start = Instant::now();
    loop {
        match child.try_wait().expect("try_wait must not fail") {
            Some(status) => {
                let mut out = String::new();
                child
                    .stdout
                    .take()
                    .unwrap()
                    .read_to_string(&mut out)
                    .unwrap();
                let mut err = String::new();
                child
                    .stderr
                    .take()
                    .unwrap()
                    .read_to_string(&mut err)
                    .unwrap();
                out.push_str(&err);
                let _ = child.wait();
                return (out, status.success());
            }
            None => {
                if start.elapsed() > deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return (format!("TIMEOUT after {}s", deadline.as_secs()), false);
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

/// Which harness arm produced a result. Tools are held constant across arms (the Δ prices REVIEW,
/// never tooling — plan-§4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BenchArm {
    Loop,
    Single,
}

impl BenchArm {
    pub fn parse(name: &str) -> Option<BenchArm> {
        match name {
            "loop" => Some(BenchArm::Loop),
            "single" => Some(BenchArm::Single),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            BenchArm::Loop => "loop",
            BenchArm::Single => "single",
        }
    }
}

/// Parse the `--arm` flag: `loop` | `single` | `both` (CLI default).
/// Unknown values bail naming the constraint (fail-fast, creates nothing).
pub fn parse_bench_arms(name: &str) -> anyhow::Result<Vec<BenchArm>> {
    match name {
        "loop" => Ok(vec![BenchArm::Loop]),
        "single" => Ok(vec![BenchArm::Single]),
        "both" => Ok(vec![BenchArm::Loop, BenchArm::Single]),
        _ => anyhow::bail!("unknown arm {name:?} (want `loop`, `single` or `both`)"),
    }
}

/// Fixture-scoped tool profile name, stamped on records for attribution
/// (boards attribute to a profile, never to "the bench" unqualified).
pub const SCOPED_PROFILE: &str = "scoped-v1";

/// Discriminator variant: bare-`bash` allow to isolate whether 100% bash-error is phrasing-shaped
/// (gate) or systematic (exec env). Destructive denies still apply; everything else identical.
pub const SCOPED_BASH_PROFILE: &str = "scoped-v1-bash";

/// Extra destructive bash prefixes refused under the bench profile (on top
/// of the operator denies, which are always kept). Temp copies hold no repo
/// and need no remover; `cargo` is the only shell shape the tasks need.
pub const BENCH_DENY_EXTRA: &[&str] = &["bash:rm", "bash:rmdir", "bash:del", "bash:git"];

/// Resolve a `--tool-profile` name to its shell rule. Unknown names bail
/// (fail-fast, creates nothing — F5d precedent).
pub fn profile_shell_rule(profile: &str) -> anyhow::Result<&'static str> {
    match profile {
        "scoped-v1" => Ok("bash:cargo"),
        "scoped-v1-bash" => Ok("bash"),
        _ => {
            anyhow::bail!("unknown tool profile {profile:?} (want `scoped-v1` or `scoped-v1-bash`)")
        }
    }
}

/// Tools-off axis: the single arm's tool list. `true` empties it (text-only control — without
/// merge-back the workdir evaporates, so 0/10 is the predicted floor, never a failure). Mirrors the
/// `review run` `--tools` precedent (`tools: if tools { tool_names } else { Vec::new }`).
pub fn single_tool_list(no_tools: bool, all: &[String]) -> Vec<String> {
    if no_tools {
        Vec::new()
    } else {
        all.to_vec()
    }
}

/// Record-profile stamp for the axis. The profile string is opaque to replay
/// (`bench-check` never parses it), so the suffix attributes the board
/// without a schema change (old records replay unchanged).
pub fn single_record_profile(base: &str, no_tools: bool) -> String {
    if no_tools {
        format!("{base}+single-notools")
    } else {
        base.to_string()
    }
}

/// Record-profile stamp for worktree-merged single runs: the runner changed (Branch worktree +
/// merge-back) while tools and profile stayed constant, so the board attributes the runner. Same
/// replay-opaque convention as [`single_record_profile`].
pub fn worktree_record_profile(base: &str) -> String {
    format!("{base}+worktree")
}

/// Record-profile stamp for loop-worktree runs: the loop fixer merged per-turn Branch worktrees
/// while tools and profile stayed constant. Same replay-opaque convention (bench-check never parses
/// the profile string) so old records replay unchanged.
pub fn loop_worktree_record_profile(base: &str) -> String {
    format!("{base}+loop-worktree")
}

/// Parse the `--preamble` flag: `full` | `minimal` (fence-ban-only ablation). Unknown names bail
/// (fail-fast, creates nothing — F5d precedent). Returns true for minimal.
pub fn parse_preamble(name: &str) -> anyhow::Result<bool> {
    match name {
        "full" => Ok(false),
        "minimal" => Ok(true),
        _ => anyhow::bail!("unknown preamble {name:?} (want `full` or `minimal`)"),
    }
}

/// Record-profile suffix for the preamble axis: minimal runs stamp `{base}+preamble-min`, composing
/// AFTER any runner stamp (e.g. `scoped-v1-bash+loop-worktree+preamble-min`). Replay-opaque like
/// all profile stamps — old records replay unchanged.
pub fn preamble_record_suffix(base: &str, minimal: bool) -> String {
    if minimal {
        format!("{base}+preamble-min")
    } else {
        base.to_string()
    }
}

/// Default-profile constructor: `scoped-v1` shell scoping (thin wrapper;
/// the discriminator variant passes its own rule).
pub fn bench_gate(task: &Task, operator_denies: &[String]) -> (Vec<String>, Vec<String>) {
    bench_gate_shell(task, operator_denies, "bash:cargo")
}

/// Build the fixture-scoped (allow, deny) rule lists for one task.
/// Reads stay bare (as today); writes/edits scope to manifest files;
/// shell scopes to `shell_rule` (`bash:cargo` phrasings, or bare `bash`
/// for the discriminator variant). Operator denies are preserved verbatim
/// (deny beats allow on shared prefixes — gate.rs contract).
/// NO operator config is touched: the bench constructs its own Gate from
/// these lists (the mechanism is code, not files).
pub fn bench_gate_shell(
    task: &Task,
    operator_denies: &[String],
    shell_rule: &str,
) -> (Vec<String>, Vec<String>) {
    let mut allow = vec![
        "read".to_string(),
        "grep".to_string(),
        "glob".to_string(),
        shell_rule.to_string(),
    ];
    let mut names: Vec<&String> = task.files.keys().collect();
    names.sort();
    for name in names {
        allow.push(format!("edit:{name}"));
        allow.push(format!("write:{name}"));
    }
    let mut deny: Vec<String> = operator_denies.to_vec();
    for extra in BENCH_DENY_EXTRA {
        if !deny.iter().any(|d| d == extra) {
            deny.push(extra.to_string());
        }
    }
    (allow, deny)
}

/// Score a materialized workdir with the hidden suite: green exit = resolved. Returns (resolved,
/// one-line summary). A suite that reports nothing (compile break, timeout, harness fault) scores
/// unresolved with a `NO-REPORT:` summary — distinct from a reported FAIL. The REVIEWER's verdict
/// never enters here — it feeds the agreement column at record time.
pub fn score_dir(dir: &Path) -> (bool, String) {
    let (out, ok) = cargo_test(dir, CARGO_DEADLINE);
    let summary = match out.lines().find(|l| l.contains("test result:")) {
        Some(line) => line.trim().to_string(),
        None => "NO-REPORT: suite produced no test-result line (compile break, timeout, or harness fault)"
            .to_string(),
    };
    (ok, summary)
}

/// One scored file for record snapshots (recording-v2 files-arrays
/// precedent, F5e: replay without re-spend).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct NamedFile {
    pub name: String,
    pub content: String,
}

/// A single bench case record: task × arm outcome with everything needed
/// to replay offline. `verdict` is the loop outcome in words (Passed /
/// Escalated-reason); `agreement` says the verdict matched the hidden
/// suite (false-passes are reviewer findings, not scorer noise).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BenchRecord {
    pub task: String,
    pub arm: String,
    /// Tool profile the case ran under ([`SCOPED_PROFILE`] etc.): boards
    /// attribute to a profile, never to "the bench" unqualified.
    pub profile: String,
    pub resolved: bool,
    pub turns: u32,
    pub tool_calls: u32,
    pub verdict: String,
    pub agreement: bool,
    pub issues: Vec<String>,
    pub files: Vec<NamedFile>,
}

/// Snapshot a workdir's files for a record (sorted by name; the corpus
/// manifest is the file list — no directory walk, no surprises).
pub fn snapshot_files(dir: &Path, names: &[String]) -> anyhow::Result<Vec<NamedFile>> {
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        if name.contains("..") {
            anyhow::bail!("record file escapes the root: {name}");
        }
        let content = std::fs::read_to_string(dir.join(name))
            .map_err(|e| anyhow::anyhow!("record snapshot {name}: {e}"))?;
        out.push(NamedFile {
            name: name.clone(),
            content,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Write one record as `<dir>/<task>.<arm>.bench.json`. Created dirs are
/// the caller's (CLI owns pid-scoped parents per established pattern).
pub fn write_record(dir: &Path, record: &BenchRecord) -> anyhow::Result<PathBuf> {
    // Arm validated at write time: a record that cannot replay is never
    // written (F5c precedent — corrupt files are hard errors, never scores).
    if BenchArm::parse(&record.arm).is_none() {
        anyhow::bail!("record task {}: unknown arm {:?}", record.task, record.arm);
    }
    let path = dir.join(format!("{}.{}.bench.json", record.task, record.arm));
    std::fs::write(&path, serde_json::to_string_pretty(record)?)?;
    Ok(path)
}

/// Load one record (unknown arms and malformed files are hard errors
/// naming the file — never scores).
pub fn load_record(path: &Path) -> anyhow::Result<BenchRecord> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("record {}: {e}", path.display()))?;
    let record: BenchRecord = serde_json::from_str(&text)
        .map_err(|e| anyhow::anyhow!("record {}: {e}", path.display()))?;
    if BenchArm::parse(&record.arm).is_none() {
        anyhow::bail!("record {}: unknown arm {:?}", path.display(), record.arm);
    }
    Ok(record)
}

/// Replay one record into caller-owned `dir`: materialize the recorded
/// files, run the hidden suite, and require the SAME resolved bit.
/// Returns the replayed bit on match; any mismatch or I/O fault is a hard
/// error naming the task (CI must fail, not print red).
pub fn check_record(record: &BenchRecord, dir: &Path) -> anyhow::Result<bool> {
    let mut map = BTreeMap::new();
    for f in &record.files {
        map.insert(f.name.clone(), f.content.clone());
    }
    write_files(dir, &map)?;
    let (replayed, _) = score_dir(dir);
    if replayed != record.resolved {
        anyhow::bail!(
            "record {} ({}): replay resolved={replayed} != recorded={}",
            record.task,
            record.arm,
            record.resolved
        );
    }
    Ok(replayed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(task: &str, arm: &str, resolved: bool) -> BenchRecord {
        BenchRecord {
            task: task.to_string(),
            arm: arm.to_string(),
            profile: "test".to_string(),
            resolved,
            turns: 2,
            tool_calls: 1,
            verdict: "Passed".to_string(),
            agreement: true,
            issues: vec![],
            files: vec![],
        }
    }

    #[test]
    fn scoped_profile_gates_manifest_paths() {
        use std::collections::BTreeMap;
        let mut files = BTreeMap::new();
        files.insert("src/main.rs".to_string(), String::new());
        let task = Task {
            id: "t".to_string(),
            description: String::new(),
            instruction: String::new(),
            fail_to_pass: vec![],
            pass_to_pass: vec![],
            files,
            fix: Fix {
                files: BTreeMap::new(),
            },
        };
        let operator = vec!["bash:git push".to_string()];
        let (allow, deny) = bench_gate(&task, &operator);
        // Reads bare + manifest-scoped writes + cargo shell.
        for rule in [
            "read",
            "grep",
            "glob",
            "bash:cargo",
            "edit:src/main.rs",
            "write:src/main.rs",
        ] {
            assert!(allow.iter().any(|a| a == rule), "{allow:?}");
        }
        // Operator denies preserved + destructive extras, no duplicates.
        for rule in [
            "bash:git push",
            "bash:rm",
            "bash:rmdir",
            "bash:del",
            "bash:git",
        ] {
            assert!(deny.iter().any(|d| d == rule), "{deny:?}");
        }
        assert_eq!(deny.len(), 5);
        // End-to-end through the real Gate (rule shapes, not just lists).
        let gate = polymorph_tools::Gate::new(allow, deny);
        assert!(gate.check("read:src/main.rs").is_ok());
        assert!(gate.check("edit:src/main.rs").is_ok());
        assert!(gate.check("write:src/main.rs").is_ok());
        assert!(gate.check("bash:cargo test").is_ok());
        assert!(gate.check("bash:cargo test 2>&1").is_ok());
        assert!(gate.check("edit:C:\\evil.rs").is_err());
        assert!(gate.check("write:/tmp/x").is_err());
        assert!(gate.check("bash:rm -rf x").is_err());
        assert!(gate.check("bash:git push").is_err());
        // Discriminator variant: bare-bash admits any shell phrasing while
        // destructive denies still bite (deny beats allow).
        let (allow_b, _) = bench_gate_shell(&task, &operator, "bash");
        let gate_b = polymorph_tools::Gate::new(allow_b, vec!["bash:rm".to_string()]);
        assert!(gate_b.check("bash:cd x && cargo test").is_ok());
        assert!(gate_b.check("bash:rm -rf x").is_err());
        assert!(profile_shell_rule("scoped-v1").unwrap() == "bash:cargo");
        assert!(profile_shell_rule("scoped-v1-bash").unwrap() == "bash");
        assert!(profile_shell_rule("bogus").is_err());
    }

    #[test]
    fn single_notools_axis_empties_tools_and_suffixes_profile() {
        // Tools-off axis: empty list (text-only control) + opaque suffixed stamp (replay-compat:
        // the profile string is never parsed).
        let all = vec!["read".to_string(), "edit".to_string()];
        assert_eq!(single_tool_list(false, &all), all);
        assert!(single_tool_list(true, &all).is_empty());
        assert_eq!(
            single_record_profile("scoped-v1-bash", false),
            "scoped-v1-bash"
        );
        assert_eq!(
            single_record_profile("scoped-v1-bash", true),
            "scoped-v1-bash+single-notools"
        );
        assert_eq!(
            worktree_record_profile("scoped-v1-bash"),
            "scoped-v1-bash+worktree"
        );
        assert_eq!(
            loop_worktree_record_profile("scoped-v1-bash"),
            "scoped-v1-bash+loop-worktree"
        );
        assert_eq!(
            preamble_record_suffix("scoped-v1-bash", false),
            "scoped-v1-bash"
        );
        assert_eq!(
            preamble_record_suffix("scoped-v1-bash+loop-worktree", true),
            "scoped-v1-bash+loop-worktree+preamble-min"
        );
        assert!(!parse_preamble("full").unwrap());
        assert!(parse_preamble("minimal").unwrap());
        assert!(parse_preamble("bogus").is_err());
    }

    #[test]
    fn materialize_repo_seeds_a_repo() {
        // Files + main branch + one seed commit; worktree children attach to this. Traversal guard
        // inherited from write_files.
        let dir = tempfile::tempdir().unwrap();
        let mut files = BTreeMap::new();
        files.insert("src/main.rs".to_string(), "fn main() {}\n".to_string());
        materialize_repo(dir.path(), &files).unwrap();
        assert!(dir.path().join(".git").exists());
        // Seeded ignore keeps cargo debris out of commits and merges.
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".gitignore")).unwrap(),
            "/target\nCargo.lock\n"
        );
        // Manifest-shipped.gitignore wins (never overwritten).
        let mut custom = BTreeMap::new();
        custom.insert(".gitignore".to_string(), "mine\n".to_string());
        let dir2 = tempfile::tempdir().unwrap();
        materialize_repo(dir2.path(), &custom).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir2.path().join(".gitignore")).unwrap(),
            "mine\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("src/main.rs")).unwrap(),
            "fn main() {}\n"
        );
        let mut bad = BTreeMap::new();
        bad.insert("../evil".to_string(), String::new());
        assert!(materialize_repo(dir.path(), &bad).is_err());
    }

    #[test]
    fn bench_arm_flag_parses() {
        assert_eq!(parse_bench_arms("loop").unwrap(), vec![BenchArm::Loop]);
        assert_eq!(parse_bench_arms("single").unwrap(), vec![BenchArm::Single]);
        assert_eq!(
            parse_bench_arms("both").unwrap(),
            vec![BenchArm::Loop, BenchArm::Single]
        );
        assert!(parse_bench_arms("bogus").is_err());
        assert!(BenchArm::parse("both").is_none());
    }

    #[test]
    fn score_dir_marks_no_report_distinct_from_fail() {
        // Empty dir: cargo errors before any test binary exists.
        let dir = tempfile::tempdir().unwrap();
        let (ok, summary) = score_dir(dir.path());
        assert!(!ok);
        assert!(summary.starts_with("NO-REPORT:"), "{summary}");
    }

    #[test]
    fn loader_rejects_empty_and_garbage() {
        assert!(load_corpus("task = []").is_err());
        assert!(load_corpus("not toml [[[").is_err());
    }

    #[test]
    fn record_round_trip_and_arm_gates() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_record(dir.path(), &record("t", "loop", true)).unwrap();
        let back = load_record(&path).unwrap();
        assert!(back.resolved);
        assert_eq!(back.arm, "loop");
        // Unknown arms never write and never load.
        assert!(write_record(dir.path(), &record("t", "bogus", true)).is_err());
        std::fs::write(dir.path().join("x.loop.bench.json"), "{bad json").unwrap();
        assert!(load_record(&dir.path().join("x.loop.bench.json")).is_err());
        let mut tampered = record("t", "loop", true);
        tampered.arm = "bogus".to_string();
        let tp = dir.path().join("t.bench.json");
        std::fs::write(&tp, serde_json::to_string(&tampered).unwrap()).unwrap();
        assert!(load_record(&tp).is_err());
    }

    const GREEN_RS: &str = "#[test]\nfn ok() {}\n";
    const RED_RS: &str = "#[test]\nfn ok() { panic!(\"seeded\") }\n";
    const CARGO_TOML: &str = "[package]\nname = \"t\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";

    fn crate_dir(dir: &Path, rs: &str) {
        let mut files = BTreeMap::new();
        files.insert("Cargo.toml".to_string(), CARGO_TOML.to_string());
        files.insert("src/main.rs".to_string(), format!("fn main() {{}}\n{rs}"));
        write_files(dir, &files).unwrap();
    }

    #[test]
    fn check_record_replays_and_refuses_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let green = dir.path().join("green");
        std::fs::create_dir_all(&green).unwrap();
        crate_dir(&green, GREEN_RS);
        let names = vec!["Cargo.toml".to_string(), "src/main.rs".to_string()];
        let files = snapshot_files(&green, &names).unwrap();
        // Sorted snapshot order regardless of manifest order.
        assert_eq!(files[0].name, "Cargo.toml");
        let mut rec = record("t", "single", true);
        rec.files = files;
        let replay = dir.path().join("replay");
        std::fs::create_dir_all(&replay).unwrap();
        assert!(check_record(&rec, &replay).unwrap());
        // Flipped bit must hard-error naming the task.
        let mut bad = rec.clone();
        bad.resolved = false;
        let err = check_record(&bad, &replay).unwrap_err().to_string();
        assert!(err.contains('t'), "{err}");
        // Red files replay red.
        let red = dir.path().join("red");
        std::fs::create_dir_all(&red).unwrap();
        crate_dir(&red, RED_RS);
        let rfiles = snapshot_files(&red, &names).unwrap();
        let mut rrec = record("t", "single", false);
        rrec.files = rfiles;
        let replay2 = dir.path().join("replay2");
        std::fs::create_dir_all(&replay2).unwrap();
        assert!(!check_record(&rrec, &replay2).unwrap());
    }
}
