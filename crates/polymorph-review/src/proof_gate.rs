//! The proof gate: AIS as the highest deterministic gate in the drive.
//!
//! Hierarchy: `proof > tests > LSP > LLM review`. A fix turn that
//! leaves the verified unit unproved cannot pass, even if the reviewer liked the
//! diff — the reviewer never sees this finding until the harness injects it, so
//! "the model said it looks right" cannot outvote a refuted obligation.
//!
//! Scope, stated plainly: the gate applies to a unit written in the fragment
//! the AIS front end verifies (`ais-core`). A Rust artifact needs a companion
//! `.ais` model or a foreign-language backend, neither of which v1 ships; the
//! gate says so instead of pretending to verify Rust.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;

use polymorph_core::{Finding, Severity};

use crate::binding::HarnessGate;

/// The gate's last word, for the event ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProofSummary {
    /// `proved` / `disproved` / `budget_exhausted` / …
    pub verdict: String,
    /// Taxonomy class, when the verdict was not a pass.
    pub class: Option<String>,
    /// `sha256:` of the certificate, when one was produced.
    pub certificate_digest: Option<String>,
    /// Solver wall clock this invocation spent, in milliseconds.
    pub solver_ms: u64,
    /// One line a human can read.
    pub summary: String,
}

/// Shared state of the proof gate: the request, the checker, and the last result.
pub struct ProofGate {
    request: polymorph_ais::Request,
    checker: polymorph_ais::solver::Checker,
    bmc_depth: usize,
    /// One summary per invocation, in order; the last is the gate's verdict.
    history: std::sync::Mutex<Vec<ProofSummary>>,
    /// The on-disk files the last certificate pins, as `(path, digest)`.
    /// `None` when the last invocation produced no certificate.
    proved_sources: std::sync::Mutex<Option<Vec<(PathBuf, String)>>>,
}

impl std::fmt::Debug for ProofGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProofGate")
            .field("function", &self.request.source.entry)
            .field("checker", &self.checker.binary)
            .finish()
    }
}

impl ProofGate {
    /// Build a gate over one request and one pinned checker.
    pub fn new(
        request: polymorph_ais::Request,
        checker: polymorph_ais::solver::Checker,
        bmc_depth: usize,
    ) -> Self {
        Self {
            request,
            checker,
            bmc_depth: bmc_depth.clamp(1, 32),
            history: std::sync::Mutex::new(Vec::new()),
            proved_sources: std::sync::Mutex::new(None),
        }
    }

    /// Run AIS once in `workdir`, record the summary and return the veto
    /// findings (empty = the unit is proved).
    pub async fn check(&self, workdir: &Path) -> Vec<Finding> {
        let mut request = self.request.clone();
        for file in &mut request.source.files {
            if file.text.is_none() {
                file.path = resolve(workdir, &file.path).to_string_lossy().to_string();
            }
        }
        let mut cfg = polymorph_ais::driver::DriverConfig::deterministic(self.checker.clone());
        cfg.bmc_depth = self.bmc_depth;
        let out = polymorph_ais::driver::run(request.clone(), cfg).await;
        let findings = proof_findings(&out.response);
        // Inline-text files travel with the request; only files read from the
        // workdir can later go missing, so only those are pinned here.
        let proved_sources = out.certificate.as_ref().map(|c| {
            request
                .source
                .files
                .iter()
                .zip(&c.body.source_files)
                .filter(|(file, _)| file.text.is_none())
                .map(|(_, pin)| (PathBuf::from(&pin.path), pin.digest.clone()))
                .collect()
        });
        let summary = ProofSummary {
            verdict: out.response.verdict.as_str().to_string(),
            class: out
                .response
                .failure
                .as_ref()
                .map(|f| f.class.as_str().to_string()),
            certificate_digest: out.certificate.as_ref().map(|c| c.digest.clone()),
            solver_ms: out.response.cost.solver_ms,
            summary: match &out.response.failure {
                Some(f) => format!("{}: {}", f.class.as_str(), f.message),
                None => "all obligations discharged".to_string(),
            },
        };
        if let Ok(mut history) = self.history.lock() {
            history.push(summary);
        }
        if let Ok(mut sources) = self.proved_sources.lock() {
            *sources = proved_sources;
        }
        findings
    }

    /// The last result, if the gate has run.
    pub fn summary(&self) -> Option<ProofSummary> {
        self.history.lock().ok().and_then(|h| h.last().cloned())
    }

    /// Every invocation's result, in order.
    pub fn history(&self) -> Vec<ProofSummary> {
        self.history.lock().map(|h| h.clone()).unwrap_or_default()
    }

    /// Whether the bytes the last certificate was proved over are still on
    /// disk, re-read now. A drive that halts rolls its tree back (or discards
    /// its worktree), so a certificate from a mid-drive round can outlive its
    /// source; the ledger says so instead of implying the tree is proved.
    /// `None` when the last invocation produced no certificate.
    pub fn certificate_source_present(&self) -> Option<bool> {
        let sources = self.proved_sources.lock().ok()?.clone()?;
        Some(sources.iter().all(|(path, digest)| {
            std::fs::read_to_string(path)
                .map(|text| polymorph_ais::cert::sha256_tag(&text) == *digest)
                .unwrap_or(false)
        }))
    }
}

/// Resolve a request path against the drive's workdir (absolute paths are kept).
fn resolve(workdir: &Path, path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        workdir.join(p)
    }
}

/// The veto findings for one response.
///
/// A `proved` verdict without a certificate is treated exactly like a failure:
/// the certificate is the artifact, and an unbacked claim is not evidence.
pub fn proof_findings(response: &polymorph_ais::Response) -> Vec<Finding> {
    if response.verdict.is_pass() && response.certificate.is_some() {
        return Vec::new();
    }
    let (class, message) = match &response.failure {
        Some(f) => (f.class.as_str(), f.message.clone()),
        None => (
            "missing_certificate",
            "the checker reported a pass without a certificate".to_string(),
        ),
    };
    let mut issue = format!(
        "Proof gate (harness-observed): verdict `{}`, class `{}`: {}",
        response.verdict.as_str(),
        class,
        message
    );
    if let Some(ce) = response
        .counterexamples
        .iter()
        .find(|c| c.witnessed)
        .or_else(|| response.counterexamples.first())
    {
        issue.push_str(&format!(
            " Counterexample on {}: {}",
            ce.obligation,
            ce.values
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(" ")
        ));
    }
    vec![Finding {
        severity: Severity::P1,
        file: PROOF_FINDING_FILE.to_string(),
        line: None,
        issue,
        proposed_fix: if response
            .failure
            .as_ref()
            .map(|f| f.needs_human)
            .unwrap_or(false)
        {
            "This needs a human: the property or the code is wrong, and weakening the claim is not \
             an option the harness may take."
                .to_string()
        } else {
            "Repair the code or the annotations so every obligation discharges, then re-run the \
             drive."
                .to_string()
        },
    }]
}

/// `Finding.file` of proof-gate findings: not a path, so the evidence block's
/// file-existence check does not try to open it (mirrors the verify gate).
pub const PROOF_FINDING_FILE: &str = "proof-gate:ais";

/// Fixer-side decorator: run the inner turn, then re-check the verified unit and
/// record the veto findings on the shared harness gate.
pub struct ProofGateRunner {
    /// The turn runner to decorate.
    pub inner: Arc<dyn polymorph_delegate::TurnRunner>,
    /// The shared harness gate the reviewer reads.
    pub gate: Arc<HarnessGate>,
    /// The proof gate itself.
    pub proof: Arc<ProofGate>,
}

#[async_trait]
impl polymorph_delegate::TurnRunner for ProofGateRunner {
    async fn run_turn(
        &self,
        ctx: polymorph_delegate::TurnContext,
    ) -> anyhow::Result<polymorph_delegate::TurnOutput> {
        let workdir = ctx.workdir.as_path().to_path_buf();
        let out = self.inner.run_turn(ctx).await;
        // A failed turn clears the veto: the loop escalates on executor failure,
        // and a stale proof finding would misattribute the cause.
        let veto = match &out {
            Ok(_) => self.proof.check(&workdir).await,
            Err(_) => Vec::new(),
        };
        self.gate.record(veto);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polymorph_ais::types::{
        Budget, Determinism, Failure, FailureClass, FileSpec, Property, PropertyKind, Request,
        Response, Source, Verdict,
    };

    fn response(verdict: Verdict, with_cert: bool) -> Response {
        Response {
            verdict,
            inferred: false,
            annotations: Vec::new(),
            certificate: with_cert.then(|| polymorph_ais::types::CertificateRef {
                format: "ais-cert-v1".into(),
                checker: "smtlib2".into(),
                blob: "AA==".into(),
                digest: "sha256:00".into(),
            }),
            counterexamples: Vec::new(),
            rounds: Vec::new(),
            cost: Default::default(),
            provenance: Default::default(),
            failure: None,
        }
    }

    #[test]
    fn a_proved_verdict_with_a_certificate_is_no_finding() {
        assert!(proof_findings(&response(Verdict::Proved, true)).is_empty());
    }

    #[test]
    fn a_proved_verdict_without_a_certificate_is_a_veto() {
        let f = proof_findings(&response(Verdict::Proved, false));
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].severity, Severity::P1);
        assert!(f[0].issue.contains("missing_certificate"), "{:?}", f[0]);
    }

    #[test]
    fn a_failure_names_its_class_and_escalation() {
        let mut r = response(Verdict::Disproved, false);
        r.failure = Some(Failure {
            class: FailureClass::SpecBug,
            message: "a real execution violates the property".into(),
            obligations: vec!["fn.post0".into()],
            needs_human: true,
        });
        let f = proof_findings(&r);
        assert_eq!(f.len(), 1);
        assert!(f[0].issue.contains("spec_bug"), "{:?}", f[0]);
        assert!(f[0].proposed_fix.contains("needs a human"), "{:?}", f[0]);
    }

    fn request_for(text: &str) -> Request {
        Request {
            source: Source {
                language: "ais-core".into(),
                files: vec![FileSpec {
                    path: "unit.ais".into(),
                    text: Some(text.to_string()),
                }],
                entry: None,
            },
            property: Property {
                kind: PropertyKind::Postcondition,
                expr: None,
                assumptions: Vec::new(),
                at: None,
            },
            backend: polymorph_ais::types::BackendSpec {
                name: "smtlib2".into(),
                binary: None,
                version: None,
            },
            budget: Budget {
                max_rounds: 2,
                solver_timeout_s: 20,
                token_budget: 0,
                total_wall_s: 120,
            },
            determinism: Determinism::default(),
            emit: Vec::new(),
        }
    }

    #[tokio::test]
    async fn the_gate_proves_a_good_unit_and_vetoes_a_false_property() {
        // Skips without a system `z3` (CI sets POLYMORPH_REQUIRE_Z3=1 to fail
        // instead); the fake-solver tests in `tests/` cover the gate offline.
        let checker = match polymorph_ais::solver::Checker::resolve(
            polymorph_ais::solver::SolverSpec::z3(std::time::Duration::from_secs(20)),
        ) {
            Ok(c) => c,
            Err(e) if std::env::var_os("POLYMORPH_REQUIRE_Z3").is_none() => {
                eprintln!("SKIPPED the_gate_proves_a_good_unit_and_vetoes_a_false_property: {e}");
                return;
            }
            Err(e) => panic!("POLYMORPH_REQUIRE_Z3 is set but no `z3` resolved: {e}"),
        };
        let dir = tempfile::tempdir().unwrap();
        let good = "fn inc(x: int) -> int\n  ensures result == x + 1\n{\n  return x + 1;\n}\n";
        let gate = ProofGate::new(request_for(good), checker.clone(), 4);
        let findings = gate.check(dir.path()).await;
        assert!(findings.is_empty(), "good unit vetoed: {findings:?}");
        let summary = gate.summary().expect("summary recorded");
        assert_eq!(summary.verdict, "proved");
        assert!(summary.certificate_digest.is_some());

        let mut bad_request = request_for(good);
        bad_request.property.expr = Some("result == x + 2".into());
        let gate = ProofGate::new(bad_request, checker, 4);
        let findings = gate.check(dir.path()).await;
        assert_eq!(findings.len(), 1, "false property not vetoed");
        assert!(findings[0].issue.contains("disproved"), "{:?}", findings[0]);
        assert_eq!(gate.summary().unwrap().verdict, "disproved");
    }
}
