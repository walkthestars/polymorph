//! B6 seeded edit-corpus driver.
//!
//! Loads `eval_b6/edit_corpus.toml`, runs every task in an isolated tempdir:
//! write initial files → mint tags → substitute `{{TAG:path}}` (or corrupt
//! when the task says so) → `apply_text_to_disk` → assert the declared
//! outcome. Prints a per-task board plus `B6 SCORE: n/20`; the gate is 100%
//! (the plan's ≥90% is a MINIMUM bar we exceed — a single regression must go
//! red, never hide inside a percentage).

use polymorph_tools::hashline::{
    apply::normalize, apply_text_to_disk, HashlineError, SnapshotStore,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[derive(Debug, serde::Deserialize)]
struct Corpus {
    task: Vec<Task>,
}

#[derive(Debug, serde::Deserialize)]
struct Task {
    id: String,
    description: String,
    #[serde(default)]
    files: BTreeMap<String, String>,
    patch: String,
    expect: Expect,
    corrupt_tag_for: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct Expect {
    files: Option<BTreeMap<String, String>>,
    error: Option<String>,
}

fn err_kind(e: &HashlineError) -> &'static str {
    match e {
        HashlineError::Parse { .. } => "Parse",
        HashlineError::Unsupported(_) => "Unsupported",
        HashlineError::StaleTag { .. } => "StaleTag",
        HashlineError::OutOfBounds { .. } => "OutOfBounds",
        HashlineError::Overlap { .. } => "Overlap",
        HashlineError::EmptyBody { .. } => "EmptyBody",
        HashlineError::MissingRegister(_) => "MissingRegister",
        HashlineError::Traversal(_) => "Traversal",
        HashlineError::RangeTooLarge(_) => "RangeTooLarge",
        HashlineError::Io { .. } => "Io",
    }
}

/// Rotate the first hex digit (+1 mod 16): deterministic, guaranteed stale.
fn corrupt_tag(tag: &str) -> String {
    const HEX: &[u8] = b"0123456789abcdef";
    let mut out = tag.to_string();
    if let Some(first) = out.get(..1) {
        let pos = HEX
            .iter()
            .position(|h| *h == first.as_bytes()[0])
            .unwrap_or(0);
        out.replace_range(..1, &String::from(HEX[(pos + 1) % 16] as char));
    }
    out
}

fn workspace_files(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_file() {
            let name = entry.file_name().to_string_lossy().into_owned();
            out.insert(name, std::fs::read(entry.path()).unwrap());
        }
    }
    out
}

fn run_task(task: &Task) -> Result<(), String> {
    if task.expect.files.is_some() == task.expect.error.is_some() {
        return Err("corpus task must declare exactly one of expect.files / expect.error".into());
    }
    let dir = tempfile::tempdir().map_err(|e| format!("tempdir: {e}"))?;
    let mut initial_bytes: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut store = SnapshotStore::new();
    let mut tags: BTreeMap<String, String> = BTreeMap::new();
    for (name, content) in &task.files {
        let bytes = content.as_bytes().to_vec();
        std::fs::write(dir.path().join(name), &bytes).map_err(|e| format!("setup write: {e}"))?;
        initial_bytes.insert(name.clone(), bytes);
        // Mint over the NORMALIZED text — the same convention the applier
        // verifies against (a corpus that minted raw bytes would fork CRLF/BOM).
        let norm = normalize(content.as_bytes());
        tags.insert(name.clone(), store.mint(Path::new(name), &norm.text));
    }

    let mut patch = task.patch.clone();
    // Substitute longest paths first so `a.txt` can't prefix-clobber `a.txt2`.
    let mut names: Vec<&String> = tags.keys().collect();
    names.sort_by_key(|a| std::cmp::Reverse(a.len()));
    for name in names {
        let mut tag = tags[name].clone();
        if task.corrupt_tag_for.as_ref() == Some(name) {
            tag = corrupt_tag(&tag);
        }
        patch = patch.replace(&format!("{{{{TAG:{name}}}}}"), &tag);
    }
    if patch.contains("{{TAG:") {
        return Err("unsubstituted {{TAG:path}} left in patch (unknown file?)".into());
    }

    match apply_text_to_disk(&patch, dir.path(), &store) {
        Ok(_) => {
            if let Some(want_error) = task.expect.error.as_deref() {
                return Err(format!("expected error {want_error} but the patch applied"));
            }
            let want = task.expect.files.as_ref().unwrap();
            let got = workspace_files(dir.path());
            // No temp files may survive any run.
            for name in got.keys() {
                if PathBuf::from(name).extension().and_then(|x| x.to_str()) == Some("polymorph-tmp")
                {
                    return Err(format!("temp file left behind: {name}"));
                }
            }
            let want_names: BTreeSet<&String> = want.keys().collect();
            let got_names: BTreeSet<&String> = got.keys().collect();
            if want_names != got_names {
                return Err(format!(
                    "file set mismatch: want {want_names:?}, got {got_names:?}"
                ));
            }
            for (name, want_content) in want {
                if got[name] != want_content.as_bytes() {
                    return Err(format!(
                        "{name}: byte mismatch\n want: {:?}\n got:  {:?}",
                        want_content.as_bytes(),
                        got[name]
                    ));
                }
            }
            Ok(())
        }
        Err(e) => {
            let want = task
                .expect
                .error
                .as_deref()
                .ok_or_else(|| format!("patch failed unexpectedly: {e}"))?;
            if err_kind(&e) != want {
                return Err(format!(
                    "wrong error kind: want {want}, got {} ({e})",
                    err_kind(&e)
                ));
            }
            // Atomicity: every initial file byte-identical, no leftovers.
            let got = workspace_files(dir.path());
            for (name, bytes) in &initial_bytes {
                match got.get(name) {
                    Some(b) if b == bytes => {}
                    _ => return Err(format!("{name}: modified despite failed patch")),
                }
            }
            if got.len() != initial_bytes.len() {
                return Err(format!(
                    "file set changed despite failed patch: {:?}",
                    got.keys().collect::<Vec<_>>()
                ));
            }
            Ok(())
        }
    }
}

#[test]
fn b6_edit_corpus() {
    let src = include_str!("eval_b6/edit_corpus.toml");
    let corpus: Corpus = toml::from_str(src).expect("corpus TOML must parse");
    assert_eq!(corpus.task.len(), 20, "B6 corpus is exactly 20 tasks");
    let mut ids = BTreeSet::new();
    let mut passed = 0;
    let mut failures: Vec<String> = Vec::new();
    for task in &corpus.task {
        if !ids.insert(task.id.clone()) {
            failures.push(format!("{}: DUPLICATE id", task.id));
            continue;
        }
        match run_task(task) {
            Ok(()) => {
                passed += 1;
                println!("[PASS] {} — {}", task.id, task.description);
            }
            Err(msg) => {
                println!("[FAIL] {} — {msg}", task.id);
                failures.push(format!("{}: {msg}", task.id));
            }
        }
    }
    println!(
        "B6 SCORE: {passed}/{} (gate: 100% — plan minimum 90%)",
        corpus.task.len()
    );
    assert!(
        failures.is_empty(),
        "{} B6 task(s) failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
