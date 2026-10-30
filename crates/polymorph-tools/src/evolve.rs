//! Evolution-lite data shapes.
//!
//! Export + candidate layer for prompt-pack evolution: scored eval
//! pairs fold into failure-first JSONL traces ([`export_traces`], parsed
//! back by [`parse_traces`]), and [`PackCandidate`] carries full-file
//! pack contents + parent manifest hashes with [`render_diff`] deriving
//! the PR-facing unified diff via the `similar` dep.
//!
//! Deliberately IO-free (mirrors the 127 planner): scoring lives in
//! [`super::eval`], the gate runner rides U1b, proposers ride U1c. The
//! rendered diff is a RENDER (hunk headers, no apply-contract) —
//! anything applying a candidate uses full-file `files`, never parses
//! the diff text.

use std::collections::BTreeMap;

use super::eval::{CaseOutcome, CaseReport, EvalCase};

/// Fault detail cap (R124 200-char precedent — consistency, not novelty).
pub const MAX_FAULT_CHARS: usize = 200;

/// One exported trace line: a scored case plus its pack provenance.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TraceRecord {
    pub case_id: String,
    pub pack_id: String,
    /// Outcome variant name (`Pass`/`ExtractFail`/`ApplyFail`/`Mismatch`).
    pub outcome: String,
    /// Fault detail (variant payload, capped at [`MAX_FAULT_CHARS`]).
    pub fault: String,
    pub model_output: String,
}

/// Scored input pair for [`export_traces`] (borrows — the export is pure;
/// loading + scoring is U1b's pipeline, not a gap here).
pub struct TraceSource<'a> {
    pub case: &'a EvalCase,
    pub pack_id: &'a str,
    pub report: &'a CaseReport,
}

fn outcome_name(outcome: &CaseOutcome) -> &'static str {
    match outcome {
        CaseOutcome::Pass => "Pass",
        CaseOutcome::ExtractFail(_) => "ExtractFail",
        CaseOutcome::ApplyFail(_) => "ApplyFail",
        CaseOutcome::Mismatch(_) => "Mismatch",
    }
}

fn fault_of(outcome: &CaseOutcome) -> String {
    let full = match outcome {
        CaseOutcome::Pass => String::new(),
        CaseOutcome::ExtractFail(d) | CaseOutcome::ApplyFail(d) | CaseOutcome::Mismatch(d) => {
            d.clone()
        }
    };
    if full.chars().count() > MAX_FAULT_CHARS {
        full.chars().take(MAX_FAULT_CHARS).collect()
    } else {
        full
    }
}

/// Fold scored pairs into failure-first JSONL (one record per line).
/// Failures lead (mutation signal, F5e precedent); passes trail sampled
/// to `max_passes` (regression guard, F5h precedent — `0` admits none).
pub fn export_traces(sources: &[TraceSource], max_passes: usize) -> String {
    let mut failed = Vec::new();
    let mut passed = Vec::new();
    for s in sources {
        let rec = TraceRecord {
            case_id: s.case.id.clone(),
            pack_id: s.pack_id.to_string(),
            outcome: outcome_name(&s.report.outcome).to_string(),
            fault: fault_of(&s.report.outcome),
            model_output: s.case.model_output.clone(),
        };
        if s.report.passed() {
            passed.push(rec);
        } else {
            failed.push(rec);
        }
    }
    passed.truncate(max_passes);
    let mut out = String::new();
    for rec in failed.into_iter().chain(passed) {
        // Serialization of plain strings cannot fail; a panic here is a
        // programming error, loud by construction (registry precedent).
        out.push_str(&serde_json::to_string(&rec).expect("trace record serializes"));
        out.push('\n');
    }
    out
}

/// Strict JSONL parse (manifest-gate precedent): corrupt lines are hard
/// errors naming the line number, never silent skips.
pub fn parse_traces(jsonl: &str) -> Result<Vec<TraceRecord>, String> {
    let mut out = Vec::new();
    for (i, line) in jsonl.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let rec: TraceRecord =
            serde_json::from_str(line).map_err(|e| format!("traces: bad line {}: {e}", i + 1))?;
        out.push(rec);
    }
    Ok(out)
}

/// A pack-mutation proposal: FULL file contents per prompt path plus the parent manifest rows it
/// was dashed against. Full files, never diffs: diffs derive at PR-render ([`render_diff`]).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PackCandidate {
    pub id: String,
    /// Parent `prompts.toml` sha rows (path → sha256) the candidate was dashed against.
    pub parent_hashes: BTreeMap<String, String>,
    /// Proposed full contents per prompt path.
    pub files: BTreeMap<String, String>,
}

/// Derive the PR-facing unified diff of `candidate` against `current`
/// (path → contents). Identical files are SKIPPED (a diff that shows
/// nothing proves nothing — gate precedent). Empty candidate (or no
/// changes) renders empty.
pub fn render_diff(candidate: &PackCandidate, current: &BTreeMap<String, String>) -> String {
    let mut out = String::new();
    for (path, new) in &candidate.files {
        let old = current.get(path).map(String::as_str).unwrap_or("");
        if old == new {
            continue;
        }
        let diff = similar::TextDiff::from_lines(old, new)
            .unified_diff()
            .context_radius(3)
            .header(&format!("a/{path}"), &format!("b/{path}"))
            .to_string();
        out.push_str(&diff);
    }
    out
}

/// U1b judging layer + U1c proposer seam: trace→board rebuild, parent-hash (dashed-against) verify,
/// offline baseline replay, before/after judge reusing [`super::eval::gate`], and the external-
/// proposer boundary ([`ProposerBrief`], [`Proposer`], [`SimpleMutator`]). Claimed after-boards are
/// shape- + math-checked here; trust arrives via the key-gated live re-run (129 D3 — stated, not
/// hidden).
///
/// sha256 hex over bytes (mirrors the CLI manifest hasher —
/// `cli/src/main.rs:verify_prompt_manifest`; same bytes ⇒ same row).
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// Rebuild a [`super::eval::Scoreboard`] from exported traces (fault
/// reattached as the variant payload). Unknown outcome names refuse
/// (strict-parse precedent — a board built on typos proves nothing).
pub fn to_scoreboard(records: &[TraceRecord]) -> Result<super::eval::Scoreboard, String> {
    use super::eval::{CaseReport, Scoreboard};
    let mut reports = Vec::with_capacity(records.len());
    for r in records {
        let outcome = match r.outcome.as_str() {
            "Pass" => CaseOutcome::Pass,
            "ExtractFail" => CaseOutcome::ExtractFail(r.fault.clone()),
            "ApplyFail" => CaseOutcome::ApplyFail(r.fault.clone()),
            "Mismatch" => CaseOutcome::Mismatch(r.fault.clone()),
            other => {
                return Err(format!(
                    "board: unknown outcome {other:?} (case {})",
                    r.case_id
                ))
            }
        };
        reports.push(CaseReport {
            id: r.case_id.clone(),
            outcome,
        });
    }
    Ok(Scoreboard { reports })
}

/// Dashed-against check: every parent row must be present in `current`
/// (path → sha256) and EQUAL. Drift or absence names the path (manifest
/// precedent — the candidate states what it was measured against; the
/// harness verifies, never assumes).
pub fn verify_parents(
    candidate: &PackCandidate,
    current: &BTreeMap<String, String>,
) -> Result<(), String> {
    for (path, want) in &candidate.parent_hashes {
        match current.get(path) {
            None => return Err(format!("parents: {path} not in current manifest")),
            Some(got) if got != want => {
                return Err(format!("parents: {path} drifted since candidate dashed"))
            }
            Some(_) => {}
        }
    }
    Ok(())
}

/// Offline baseline replay: `score_case` per case under a pid-stamped
/// std tempdir (F5c precedent — removed on EVERY exit; `tempfile` stays
/// dev-only, 131-plan ruling). Infallible by construction (`score_case`
/// folds setup faults into `ApplyFail`, never Err).
pub fn replay_baseline(cases: &[EvalCase]) -> super::eval::Scoreboard {
    use super::eval::{score_case, Scoreboard};
    let mut reports = Vec::with_capacity(cases.len());
    for (i, case) in cases.iter().enumerate() {
        let root =
            std::env::temp_dir().join(format!("polymorph-evolve-{}-{i}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        if std::fs::create_dir_all(&root).is_err() {
            reports.push(super::eval::CaseReport {
                id: case.id.clone(),
                outcome: CaseOutcome::ApplyFail("evolve: cannot create replay root".into()),
            });
            continue;
        }
        reports.push(score_case(case, &root));
        let _ = std::fs::remove_dir_all(&root);
    }
    Scoreboard { reports }
}

/// The judged verdict, PR-body shaped (serializable for the candidate
/// record, U1c's proposer seam).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct GateReport {
    pub verdict: String,
    pub baseline_rate: f64,
    pub candidate_rate: f64,
    pub min_rate: f64,
    pub passed: usize,
    pub total: usize,
    pub failed_ids: Vec<String>,
}

/// Before/after judge: the after-board must clear the baseline rate
/// AND `eval::gate(after, min_rate)` (one-gate precedent — the delta
/// rule is the only new logic; empty/red boards fail through `gate`).
pub fn judge(
    before: &super::eval::Scoreboard,
    after: &super::eval::Scoreboard,
    min_rate: f64,
) -> Result<GateReport, String> {
    super::eval::gate(after, min_rate).map_err(|e| format!("judge: {e}"))?;
    if after.rate() < before.rate() {
        return Err(format!(
            "judge: regression {:.0}% -> {:.0}% (baseline holds)",
            before.rate() * 100.0,
            after.rate() * 100.0
        ));
    }
    Ok(GateReport {
        verdict: "green".to_string(),
        baseline_rate: before.rate(),
        candidate_rate: after.rate(),
        min_rate,
        passed: after.passed(),
        total: after.total(),
        failed_ids: after.failed_ids(),
    })
}

/// What crosses the file boundary to an external proposer (129 D2):
/// failure-first traces, current pack contents, dashed-against hashes,
/// focus case ids. Serialize → file → proposer → [`PackCandidate`]
/// JSON back. The harness never imports proposer code (no `Cargo.lock`
/// row for evolution ever — 129, stated).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProposerBrief {
    pub traces_jsonl: String,
    pub current_files: BTreeMap<String, String>,
    pub parent_hashes: BTreeMap<String, String>,
    pub focus: Vec<String>,
}

/// The file-boundary render (what is literally written for the proposer).
/// [`parse_traces`] already validates the traces leg on return; this side
/// is serialization-infallible for the same reason as [`export_traces`].
pub fn brief_json(brief: &ProposerBrief) -> String {
    serde_json::to_string_pretty(brief).expect("proposer brief serializes")
}

/// Shape-check a returned candidate (129 D3: id + files + parent rows
/// must all be present — an underspecified proposal never reaches the
/// gate). JSON faults name the fault; the gate owns deeper verdicts.
pub fn parse_candidate(json_text: &str) -> Result<PackCandidate, String> {
    let cand: PackCandidate =
        serde_json::from_str(json_text).map_err(|e| format!("candidate: invalid JSON: {e}"))?;
    if cand.id.trim().is_empty() {
        return Err("candidate: empty `id`".to_string());
    }
    if cand.files.is_empty() {
        return Err("candidate: empty `files`".to_string());
    }
    if cand.parent_hashes.is_empty() {
        return Err("candidate: empty `parent_hashes`".to_string());
    }
    if cand.files.keys().any(|p| p.trim().is_empty()) {
        return Err("candidate: empty file path".to_string());
    }
    Ok(cand)
}

/// The external-proposer boundary (129 D2/D7): implementors read a
/// [`ProposerBrief`] and return a [`PackCandidate`]. Synchronous by
/// design — proposers are local text transforms; spawning, timeouts,
/// and scheduling belong to the CI driver, not this trait (stated seam).
pub trait Proposer {
    fn propose(&self, brief: &ProposerBrief) -> anyhow::Result<PackCandidate>;
}

/// The v1 proposer (129 D7 fallback): deterministic byte-hygiene —
/// single trailing newline per file, otherwise verbatim copy, parents
/// carried from the brief. Changes NOTHING model-visible (pipeline
/// proof only — quality claims explicitly disclaimed, D7).
pub struct SimpleMutator {
    pub candidate_id: String,
}

impl Proposer for SimpleMutator {
    fn propose(&self, brief: &ProposerBrief) -> anyhow::Result<PackCandidate> {
        let mut files = BTreeMap::new();
        for (path, content) in &brief.current_files {
            let trimmed = content.trim_end_matches('\n');
            files.insert(path.clone(), format!("{trimmed}\n"));
        }
        Ok(PackCandidate {
            id: self.candidate_id.clone(),
            parent_hashes: brief.parent_hashes.clone(),
            files,
        })
    }
}

/// The composed verification U2 and CI reuse (naming it prevents three
/// copies — 133-plan ruling): parents first (dashed-against), then the
/// shared-gate judge.
pub fn chain_verify(
    candidate: &PackCandidate,
    current_hashes: &BTreeMap<String, String>,
    before: &super::eval::Scoreboard,
    after: &super::eval::Scoreboard,
    min_rate: f64,
) -> Result<GateReport, String> {
    verify_parents(candidate, current_hashes).map_err(|e| format!("chain: {e}"))?;
    judge(before, after, min_rate).map_err(|e| format!("chain: {e}"))
}

/// U2 skill layer: `SKILL.md` candidates with the same PR shape as packs (full file + parent hash +
/// rules) plus a hand-rolled hygiene diagnoser. Strict validation itself lives in `polymorph-
/// skills::validate` (dev-dep proof only — runtime stays decoupled, 134-plan ruling).
///
/// A skill-mutation proposal: the FULL `SKILL.md` (single-file v1 —
/// resources/ rides a triggered follow-up) + parent sha + note.
/// Empty `parent_sha` means a NEW skill (nothing to dash against).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SkillCandidate {
    pub id: String,
    pub skill_name: String,
    pub skill_md: String,
    pub parent_sha: String,
    pub note: String,
}

/// Shape-check a skill proposal (mirrors [`parse_candidate`]: present +
/// fenced; deeper verdicts belong to `validate`).
pub fn parse_skill_candidate(json_text: &str) -> Result<SkillCandidate, String> {
    let cand: SkillCandidate = serde_json::from_str(json_text)
        .map_err(|e| format!("skill-candidate: invalid JSON: {e}"))?;
    if cand.id.trim().is_empty() {
        return Err("skill-candidate: empty `id`".to_string());
    }
    if cand.skill_name.trim().is_empty() {
        return Err("skill-candidate: empty `skill_name`".to_string());
    }
    if cand.skill_md.trim().is_empty() {
        return Err("skill-candidate: empty `skill_md`".to_string());
    }
    if !cand.skill_md.starts_with("---\n") && !cand.skill_md.starts_with("---\r\n") {
        return Err("skill-candidate: missing frontmatter fence".to_string());
    }
    Ok(cand)
}

/// Dashed-against for skills: `parent_sha` must equal the current md's
/// hash (drift names the skill); empty parent means NEW (nothing to
/// check — documented, not a hole).
pub fn verify_skill_parents(candidate: &SkillCandidate, current_md: &str) -> Result<(), String> {
    if candidate.parent_sha.is_empty() {
        return Ok(());
    }
    if sha256_hex(current_md.as_bytes()) != candidate.parent_sha {
        return Err(format!(
            "skill-parents: {} drifted since candidate dashed",
            candidate.skill_name
        ));
    }
    Ok(())
}

/// The hygiene diagnosis: what is wrong + the repaired text. `context`
/// echoes the caller's failure note (failure provenance, never parsed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillDiagnosis {
    pub skill_name: String,
    pub context: String,
    pub issues: Vec<String>,
    pub fixed_md: String,
    /// True iff repairs were applied AND no structural blocker remains
    /// (a newline fix on a fenceless file is still red — stated).
    pub fixed: bool,
}

/// Flat frontmatter fields between the fences (`key: value` per line;
/// one indented level and unknown shapes are NOT parsed — anything
/// fancier than flat scalars is a structural issue for `validate`,
/// never guessed here).
fn flat_fields(md: &str) -> Option<BTreeMap<String, String>> {
    let mut lines = md.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }
    let mut fields = BTreeMap::new();
    for line in lines {
        if line.trim() == "---" {
            return Some(fields);
        }
        if line.starts_with(' ') || line.starts_with('\t') || line.trim().is_empty() {
            continue;
        }
        let (k, v) = line.split_once(':')?;
        fields.insert(k.trim().to_string(), v.trim().to_string());
    }
    None
}

/// Hand-rolled hygiene diagnosis (134-plan ruling — quality disclaimed):
/// SAFE repairs only (single trailing newline; truncate
/// `compatibility` > 500 chars with the issue named). Structural faults
/// (bad fence, missing name/description, name mismatch) are NAMED and
/// left untouched. Line-trailing whitespace is NEVER trimmed (markdown
/// ` ` = `<br>` — stated non-repair).
pub fn diagnose_skill_fix(name: &str, current_md: &str, failure_note: &str) -> SkillDiagnosis {
    let mut issues = Vec::new();
    let mut structural = false;
    let mut md = current_md.to_string();
    match flat_fields(&md) {
        None => {
            issues.push("frontmatter: no parseable `---` fence".to_string());
            structural = true;
        }
        Some(fields) => {
            match fields.get("name") {
                None => {
                    issues.push("frontmatter: missing `name`".to_string());
                    structural = true;
                }
                Some(n) if n != name => {
                    issues.push(format!("frontmatter: name {n:?} != dir {name:?}"));
                    structural = true;
                }
                Some(_) => {}
            }
            if fields.get("description").is_none_or(|d| d.is_empty()) {
                issues.push("frontmatter: missing/empty `description`".to_string());
                structural = true;
            }
            if let Some(compat) = fields.get("compatibility") {
                if compat.chars().count() > 500 {
                    issues.push("compatibility: >500 chars, truncated".to_string());
                    let cut: String = compat.chars().take(500).collect();
                    let mut out = String::new();
                    let mut in_fence = false;
                    let mut done = false;
                    for line in md.lines() {
                        if line.trim() == "---" {
                            in_fence = !in_fence;
                            out.push_str(line);
                            out.push('\n');
                            continue;
                        }
                        if in_fence && !done && line.starts_with("compatibility:") {
                            out.push_str(&format!("compatibility: {cut}\n"));
                            done = true;
                            continue;
                        }
                        out.push_str(line);
                        out.push('\n');
                    }
                    md = out;
                }
            }
        }
    }
    // Single trailing newline (U1c precedent — byte hygiene; reported
    // whenever bytes change, `fixed` still gates on no structural fault).
    let trimmed = md.trim_end_matches('\n');
    let want = format!("{trimmed}\n");
    if want != md {
        issues.push("hygiene: trailing newline normalized".to_string());
        md = want;
    }
    let repaired = md != current_md;
    SkillDiagnosis {
        skill_name: name.to_string(),
        context: failure_note.to_string(),
        issues,
        fixed_md: md,
        fixed: repaired && !structural,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn case(id: &str, output: &str) -> EvalCase {
        EvalCase {
            id: id.to_string(),
            files: vec![("a.txt".to_string(), "x\n".to_string())],
            expected: vec![("a.txt".to_string(), "y\n".to_string())],
            model_output: output.to_string(),
        }
    }

    fn report(id: &str, outcome: CaseOutcome) -> CaseReport {
        CaseReport {
            id: id.to_string(),
            outcome,
        }
    }

    #[test]
    fn export_failures_lead_passes_trail() {
        let c1 = case("c1", "o1");
        let c2 = case("c2", "o2");
        let c3 = case("c3", "o3");
        let r1 = report("c1", CaseOutcome::Pass);
        let r2 = report("c2", CaseOutcome::Mismatch("first div".into()));
        let r3 = report("c3", CaseOutcome::ExtractFail("prose".into()));
        let srcs = [
            TraceSource {
                case: &c1,
                pack_id: "p",
                report: &r1,
            },
            TraceSource {
                case: &c2,
                pack_id: "p",
                report: &r2,
            },
            TraceSource {
                case: &c3,
                pack_id: "p",
                report: &r3,
            },
        ];
        let recs = parse_traces(&export_traces(&srcs, 10)).unwrap();
        assert_eq!(recs.len(), 3);
        assert_eq!(recs[0].outcome, "Mismatch");
        assert_eq!(recs[1].outcome, "ExtractFail");
        assert_eq!(recs[2].outcome, "Pass");
        assert_eq!(recs[1].fault, "prose");
        assert!(recs[2].fault.is_empty());
        assert_eq!(recs[0].model_output, "o2");
    }

    #[test]
    fn export_samples_passes_to_cap() {
        let c1 = case("c1", "o1");
        let c2 = case("c2", "o2");
        let r1 = report("c1", CaseOutcome::Pass);
        let r2 = report("c2", CaseOutcome::Pass);
        let srcs = [
            TraceSource {
                case: &c1,
                pack_id: "p",
                report: &r1,
            },
            TraceSource {
                case: &c2,
                pack_id: "p",
                report: &r2,
            },
        ];
        assert_eq!(parse_traces(&export_traces(&srcs, 1)).unwrap().len(), 1);
        assert!(export_traces(&srcs, 0).is_empty());
    }

    #[test]
    fn export_fault_truncates_at_cap() {
        let long = "x".repeat(MAX_FAULT_CHARS + 50);
        let c = case("c", "o");
        let r = report("c", CaseOutcome::ApplyFail(long));
        let srcs = [TraceSource {
            case: &c,
            pack_id: "p",
            report: &r,
        }];
        let recs = parse_traces(&export_traces(&srcs, 0)).unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].fault.chars().count(), MAX_FAULT_CHARS);
        assert_eq!(recs[0].outcome, "ApplyFail");
    }

    #[test]
    fn parse_corrupt_line_names_number() {
        let good = "{\"case_id\":\"a\",\"pack_id\":\"p\",\"outcome\":\"Pass\",\"fault\":\"\",\"model_output\":\"o\"}";
        let err = parse_traces(&format!("{good}\nnot json\n")).unwrap_err();
        assert!(err.contains("line 2"), "{err}");
    }

    #[test]
    fn diff_carries_changes_skips_identical() {
        let cand = PackCandidate {
            id: "cand-1".into(),
            parent_hashes: BTreeMap::from([("p/edit.md".into(), "abc".into())]),
            files: BTreeMap::from([
                ("p/edit.md".into(), "line1\nline2-changed\n".into()),
                ("p/same.md".into(), "same\n".into()),
            ]),
        };
        let current = BTreeMap::from([
            ("p/edit.md".into(), "line1\nline2\n".into()),
            ("p/same.md".into(), "same\n".into()),
        ]);
        let d = render_diff(&cand, &current);
        assert!(d.contains("-line2"), "{d}");
        assert!(d.contains("+line2-changed"), "{d}");
        assert!(!d.contains("same.md"), "{d}");
    }

    #[test]
    fn diff_empty_when_nothing_changed() {
        let cand = PackCandidate {
            id: "c".into(),
            parent_hashes: BTreeMap::new(),
            files: BTreeMap::from([("p/a.md".into(), "x\n".into())]),
        };
        let current = BTreeMap::from([("p/a.md".into(), "x\n".into())]);
        assert!(render_diff(&cand, &current).is_empty());
        assert!(render_diff(
            &PackCandidate {
                id: "e".into(),
                parent_hashes: BTreeMap::new(),
                files: BTreeMap::new(),
            },
            &current,
        )
        .is_empty());
    }

    #[test]
    fn candidate_json_round_trip() {
        let cand = PackCandidate {
            id: "cand-9".into(),
            parent_hashes: BTreeMap::from([("p/edit.md".into(), "sha".into())]),
            files: BTreeMap::from([("p/edit.md".into(), "body\n".into())]),
        };
        let back: PackCandidate =
            serde_json::from_str(&serde_json::to_string(&cand).unwrap()).unwrap();
        assert_eq!(back, cand);
    }

    #[test]
    fn sha256_known_vector() {
        // `sha256("abc")` — mirrors the CLI manifest hasher byte for byte.
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    fn trace(id: &str, outcome: &str, fault: &str) -> TraceRecord {
        TraceRecord {
            case_id: id.to_string(),
            pack_id: "p".to_string(),
            outcome: outcome.to_string(),
            fault: fault.to_string(),
            model_output: String::new(),
        }
    }

    #[test]
    fn board_rebuilds_with_fault_reattached() {
        let sb =
            to_scoreboard(&[trace("a", "Pass", ""), trace("b", "Mismatch", "first div")]).unwrap();
        assert_eq!(sb.total(), 2);
        assert_eq!(sb.passed(), 1);
        assert_eq!(sb.failed_ids(), vec!["b".to_string()]);
        assert!(matches!(sb.reports[1].outcome, CaseOutcome::Mismatch(_)));
        let err = to_scoreboard(&[trace("c", "Bogus", "")]).unwrap_err();
        assert!(err.contains("Bogus") && err.contains('c'), "{err}");
    }

    #[test]
    fn parents_green_drift_and_missing_name_path() {
        let cand = PackCandidate {
            id: "c".into(),
            parent_hashes: BTreeMap::from([("p/edit.md".into(), sha256_hex(b"body\n"))]),
            files: BTreeMap::new(),
        };
        let current = BTreeMap::from([("p/edit.md".into(), sha256_hex(b"body\n"))]);
        assert!(verify_parents(&cand, &current).is_ok());
        let drifted = BTreeMap::from([("p/edit.md".into(), sha256_hex(b"other\n"))]);
        let err = verify_parents(&cand, &drifted).unwrap_err();
        assert!(err.contains("p/edit.md"), "{err}");
        let err = verify_parents(&cand, &BTreeMap::new()).unwrap_err();
        assert!(err.contains("p/edit.md"), "{err}");
    }

    /// Mint over NORMALIZED content exactly like `score_case` setup
    /// (eval.rs `fixture_tag` precedent — raw bytes fork the tag).
    fn tag_for(content: &str) -> String {
        use crate::hashline::{apply::normalize, SnapshotStore};
        let mut store = SnapshotStore::new();
        let norm = normalize(content.as_bytes());
        store.mint(std::path::Path::new("a.txt"), &norm.text)
    }

    /// One passing + one failing hand-written case (hashline PUT over
    /// `a.txt`: exact-output pass, wrong-output mismatch — labeled).
    fn two_cases() -> Vec<EvalCase> {
        let tag = tag_for("l1\n");
        vec![
            EvalCase {
                id: "pass-1".into(),
                files: vec![("a.txt".into(), "l1\n".into())],
                expected: vec![("a.txt".into(), "l2\n".into())],
                model_output: format!("[a.txt#{tag}]\nPUT 1.=1:\n+l2\n"),
            },
            EvalCase {
                id: "fail-1".into(),
                files: vec![("a.txt".into(), "l1\n".into())],
                expected: vec![("a.txt".into(), "l2\n".into())],
                model_output: format!("[a.txt#{tag}]\nPUT 1.=1:\n+WRONG\n"),
            },
        ]
    }

    #[test]
    fn replay_baseline_scores_one_of_two() {
        let sb = replay_baseline(&two_cases());
        assert_eq!(sb.total(), 2);
        assert_eq!(sb.passed(), 1, "{sb:?}");
        assert_eq!(sb.failed_ids(), vec!["fail-1".to_string()]);
    }

    #[test]
    fn judge_improving_greens_regressing_reds() {
        let before = to_scoreboard(&[trace("a", "Pass", ""), trace("b", "Mismatch", "d")]).unwrap();
        let after = to_scoreboard(&[trace("a", "Pass", ""), trace("b", "Pass", "")]).unwrap();
        let rep = judge(&before, &after, 1.0).unwrap();
        assert_eq!(rep.verdict, "green");
        assert_eq!((rep.passed, rep.total), (2, 2));
        assert!((rep.candidate_rate - 1.0).abs() < f64::EPSILON);
        // Regression: after below before reds naming the delta.
        let err = judge(&after, &before, 0.0).unwrap_err();
        assert!(err.contains("regression"), "{err}");
        // Empty after-board reds through the shared gate.
        let empty = to_scoreboard(&[]).unwrap();
        let err = judge(&empty, &empty, 0.5).unwrap_err();
        assert!(err.contains("empty"), "{err}");
        // Below min-rate reds naming the ids.
        let err = judge(&before, &before, 1.0).unwrap_err();
        assert!(err.contains('b'), "{err}");
    }

    #[test]
    fn gate_report_json_shape() {
        let before = to_scoreboard(&[trace("a", "Pass", "")]).unwrap();
        let rep = judge(&before, &before, 1.0).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&rep).unwrap()).unwrap();
        assert_eq!(v["verdict"], serde_json::json!("green"));
        assert_eq!(v["passed"], serde_json::json!(1));
    }

    fn brief() -> ProposerBrief {
        ProposerBrief {
            traces_jsonl: "line\n".to_string(),
            current_files: BTreeMap::from([("p/edit.md".into(), "body".into())]),
            parent_hashes: BTreeMap::from([("p/edit.md".into(), "sha".into())]),
            focus: vec!["c1".to_string()],
        }
    }

    #[test]
    fn brief_json_round_trip() {
        let back: ProposerBrief = serde_json::from_str(&brief_json(&brief())).unwrap();
        assert_eq!(back, brief());
    }

    #[test]
    fn candidate_shape_refusals_name_fault() {
        let good = serde_json::to_string(&PackCandidate {
            id: "c".into(),
            parent_hashes: BTreeMap::from([("p/a.md".into(), "s".into())]),
            files: BTreeMap::from([("p/a.md".into(), "b\n".into())]),
        })
        .unwrap();
        assert!(parse_candidate(&good).is_ok());
        let err = parse_candidate("not json").unwrap_err();
        assert!(err.contains("invalid JSON"), "{err}");
        for (label, cand) in [
            (
                "id",
                PackCandidate {
                    id: String::new(),
                    parent_hashes: BTreeMap::from([("p".into(), "s".into())]),
                    files: BTreeMap::from([("p".into(), "b".into())]),
                },
            ),
            (
                "files",
                PackCandidate {
                    id: "c".into(),
                    parent_hashes: BTreeMap::from([("p".into(), "s".into())]),
                    files: BTreeMap::new(),
                },
            ),
            (
                "hashes",
                PackCandidate {
                    id: "c".into(),
                    parent_hashes: BTreeMap::new(),
                    files: BTreeMap::from([("p".into(), "b".into())]),
                },
            ),
        ] {
            let err = parse_candidate(&serde_json::to_string(&cand).unwrap()).unwrap_err();
            assert!(err.contains(label), "{err}");
        }
    }

    #[test]
    fn mutator_deterministic_hygienic_parents_carried() {
        let m = SimpleMutator {
            candidate_id: "m1".into(),
        };
        let b = brief();
        let c1 = m.propose(&b).unwrap();
        let c2 = m.propose(&b).unwrap();
        assert_eq!(c1, c2);
        assert_eq!(c1.id, "m1");
        assert_eq!(c1.parent_hashes, b.parent_hashes);
        // "body" (no trailing newline) → "body\n"; already-clean stays.
        assert_eq!(c1.files["p/edit.md"], "body\n");
        let mut clean = b.clone();
        clean.current_files = BTreeMap::from([("p/a.md".into(), "x\n".into())]);
        assert_eq!(m.propose(&clean).unwrap().files["p/a.md"], "x\n");
    }

    #[test]
    fn chain_end_to_end_green_and_drifted_red() {
        // Full pipeline on scripted boards: export → brief → propose →
        // parse → chain_verify. Zero network (D7 wiring proof, stated).
        let board = to_scoreboard(&[trace("a", "Pass", "")]).unwrap();
        let m = SimpleMutator {
            candidate_id: "pipe".into(),
        };
        let b = brief();
        let cand = m.propose(&b).unwrap();
        let back = parse_candidate(&serde_json::to_string(&cand).unwrap()).unwrap();
        let rep = chain_verify(&back, &b.parent_hashes, &board, &board, 1.0).unwrap();
        assert_eq!(rep.verdict, "green");
        // Drifted parents red naming the chain (not the gate).
        let stale = BTreeMap::from([("p/edit.md".into(), "other".into())]);
        let err = chain_verify(&back, &stale, &board, &board, 1.0).unwrap_err();
        assert!(err.contains("chain") && err.contains("p/edit.md"), "{err}");
    }

    #[test]
    fn skill_candidate_round_trip_and_refusals() {
        let cand = SkillCandidate {
            id: "sc-1".into(),
            skill_name: "demo-skill".into(),
            skill_md: "---\nname: demo-skill\ndescription: D.\n---\nBody.\n".into(),
            parent_sha: "abc".into(),
            note: "hygiene".into(),
        };
        let back: SkillCandidate =
            serde_json::from_str(&serde_json::to_string(&cand).unwrap()).unwrap();
        assert_eq!(back, cand);
        let err = parse_skill_candidate("not json").unwrap_err();
        assert!(err.contains("invalid JSON"), "{err}");
        let mut fenceless = cand.clone();
        fenceless.skill_md = "no fence\n".into();
        let err = parse_skill_candidate(&serde_json::to_string(&fenceless).unwrap()).unwrap_err();
        assert!(err.contains("fence"), "{err}");
        let mut noname = cand.clone();
        noname.skill_name = String::new();
        let err = parse_skill_candidate(&serde_json::to_string(&noname).unwrap()).unwrap_err();
        assert!(err.contains("skill_name"), "{err}");
    }

    #[test]
    fn skill_parents_green_drift_named_new_skips() {
        let md = "---\nname: s\ndescription: D.\n---\nB.\n";
        let cand = SkillCandidate {
            id: "c".into(),
            skill_name: "s".into(),
            skill_md: md.into(),
            parent_sha: sha256_hex(md.as_bytes()),
            note: String::new(),
        };
        assert!(verify_skill_parents(&cand, md).is_ok());
        let err = verify_skill_parents(&cand, "other").unwrap_err();
        assert!(err.contains('s'), "{err}");
        let mut fresh = cand.clone();
        fresh.parent_sha = String::new();
        assert!(verify_skill_parents(&fresh, "anything").is_ok());
    }

    #[test]
    fn diagnose_hygiene_fixed_structural_named_unfixed() {
        // Hygiene: missing trailing newline repairs with fixed=true.
        let d = diagnose_skill_fix(
            "demo-skill",
            "---\nname: demo-skill\ndescription: D.\n---\nBody.",
            "ctx",
        );
        assert!(d.fixed, "{d:?}");
        assert!(d.fixed_md.ends_with("Body.\n"));
        assert_eq!(d.context, "ctx");
        // Structural: fenceless names the fault, bytes gain only the
        // newline, fixed stays false.
        let d = diagnose_skill_fix("s", "no fence\n", "ctx");
        assert!(!d.fixed, "{d:?}");
        assert!(d.issues.iter().any(|i| i.contains("fence")));
        // Structural: name mismatch named, unfixed.
        let d = diagnose_skill_fix(
            "real-name",
            "---\nname: other\ndescription: D.\n---\nB.\n",
            "ctx",
        );
        assert!(!d.fixed, "{d:?}");
        assert!(d.issues.iter().any(|i| i.contains("other")));
        // Compatibility >500 truncates (named) and still counts fixed.
        let long = "y".repeat(501);
        let md = format!("---\nname: s\ndescription: D.\ncompatibility: {long}\n---\nB.\n");
        let d = diagnose_skill_fix("s", &md, "ctx");
        assert!(d.fixed, "{d:?}");
        assert!(d.issues.iter().any(|i| i.contains("compatibility")));
        assert!(d.fixed_md.contains(&"y".repeat(500)));
        assert!(!d.fixed_md.contains(&"y".repeat(501)));
    }

    #[test]
    fn dry_run_broken_to_validate_green() {
        // v2 U2 datum: ≥1 failed skill → diagnosed rewrite → eval-green
        // (validate + discover), dry-run, never merged (stated).
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("demo-skill");
        std::fs::create_dir_all(&dir).unwrap();
        // Broken: no trailing newline + 501-char compatibility.
        let long = "z".repeat(501);
        let broken =
            format!("---\nname: demo-skill\ndescription: D.\ncompatibility: {long}\n---\nBody.");
        std::fs::write(dir.join("SKILL.md"), &broken).unwrap();
        let red = polymorph_skills::validate(&dir).unwrap_err().to_string();
        assert!(red.contains("ompatibility"), "{red}");
        // Diagnose → fix → write back → pass.
        let d = diagnose_skill_fix("demo-skill", &broken, "compat overlong");
        assert!(d.fixed, "{d:?}");
        std::fs::write(dir.join("SKILL.md"), &d.fixed_md).unwrap();
        let skill = polymorph_skills::validate(&dir).unwrap();
        assert_eq!(skill.meta.name, "demo-skill");
        let metas = polymorph_skills::discover(root.path()).unwrap();
        assert!(metas.iter().any(|m| m.name == "demo-skill"));
    }

    #[test]
    fn file_boundary_round_trip() {
        // The D2 crossing itself: brief JSON to disk and back byte-equal.
        let d = std::env::temp_dir().join(format!("polymorph-evolve-{}-brief", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let path = d.join("brief.json");
        std::fs::write(&path, brief_json(&brief())).unwrap();
        let back: ProposerBrief =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back, brief());
        let _ = std::fs::remove_dir_all(&d);
    }
}
