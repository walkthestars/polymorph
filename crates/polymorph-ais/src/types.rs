//! Wire types of the AIS primitive: request, response, verdicts, failure
//! taxonomy, per-round ledger, cost and provenance.
//!
//! The JSON shapes here are the contract an agent or harness codes against
//! (the request and response JSON; see `docs/PROOF.md`). Field names are
//! snake_case on the wire and stable; additive fields are the only permitted
//! change.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The primitive's own version, recorded in every certificate.
pub const AIS_VERSION: &str = "ais-1.0";

/// The final word of the primitive. Only [`Verdict::Proved`] is a pass, and it
/// can only be produced by a checker run whose obligations all discharged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Every obligation discharged by the backend.
    Proved,
    /// The backend produced a counterexample to the property.
    Disproved,
    /// The backend exceeded its solver budget; no verdict was reached.
    Timeout,
    /// The request is outside the backend's declared capabilities.
    Unsupported,
    /// The obligations are contradictory or the precondition is unsatisfiable:
    /// the property as given cannot be proved of anything.
    SpecInconsistent,
    /// A governance budget (rounds / tokens / wall clock) ran out first.
    BudgetExhausted,
}

impl Verdict {
    /// True only for a verdict that a gate may treat as a pass.
    pub fn is_pass(self) -> bool {
        matches!(self, Verdict::Proved)
    }

    /// Lowercase wire name, matching the serde representation.
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Proved => "proved",
            Verdict::Disproved => "disproved",
            Verdict::Timeout => "timeout",
            Verdict::Unsupported => "unsupported",
            Verdict::SpecInconsistent => "spec_inconsistent",
            Verdict::BudgetExhausted => "budget_exhausted",
        }
    }
}

/// Why synthesis stopped short — the failure taxonomy. Every
/// non-`proved` response carries exactly one class, so a caller never has to
/// guess whether "not proved" means "wrong code" or "wrong question".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    /// A counterexample shows the candidate invariant is too weak.
    InsufficientInvariant,
    /// The candidate invariant is itself false (refuted at entry or by the body).
    UnsoundInvariant,
    /// The property is inconsistent with the code, or the precondition is
    /// unsatisfiable. Never auto-repaired: this escalates to a human.
    SpecBug,
    /// The property needs logic the backend does not have (quantifiers, ghost
    /// state, uninterpreted helpers) — a downgrade, never a silent proof.
    ExpressivenessCeiling,
    /// The solver returned `unknown`/timed out on a well-formed obligation.
    SolverIncompleteness,
    /// A language construct outside the fan-out's declared fragment.
    UnsupportedFeature,
    /// The proposer emitted text the elaborator could not parse.
    ProposerMalformed,
    /// Rounds, tokens or wall clock ran out.
    BudgetExhausted,
}

impl FailureClass {
    /// True when only a human can resolve the failure, so the caller escalates.
    pub fn needs_human(self) -> bool {
        matches!(
            self,
            FailureClass::SpecBug | FailureClass::ExpressivenessCeiling
        )
    }

    /// Lowercase wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            FailureClass::InsufficientInvariant => "insufficient_invariant",
            FailureClass::UnsoundInvariant => "unsound_invariant",
            FailureClass::SpecBug => "spec_bug",
            FailureClass::ExpressivenessCeiling => "expressiveness_ceiling",
            FailureClass::SolverIncompleteness => "solver_incompleteness",
            FailureClass::UnsupportedFeature => "unsupported_feature",
            FailureClass::ProposerMalformed => "proposer_malformed",
            FailureClass::BudgetExhausted => "budget_exhausted",
        }
    }
}

/// What the caller wants proved.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PropertyKind {
    /// A condition on the function's result / final state.
    #[default]
    Postcondition,
    /// A safety condition that must hold at every `assert` in the unit.
    Safety,
    /// The named loop terminates.
    Termination,
    /// A refinement relation between an abstract and a concrete state.
    Refinement,
    /// Not implemented: no postcondition is synthesized. A request with this
    /// kind proves the unit's own `ensures` exactly like `postcondition`, and
    /// the response reports `inferred: false`.
    Infer,
}

/// The property as the caller states it, plus the assumptions it rests on.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Property {
    /// What kind of claim this is.
    pub kind: PropertyKind,
    /// The claim itself, in the backend's expression syntax. `None` means
    /// "prove the unit's own `ensures`" and is allowed for `postcondition` and
    /// `infer`; the primitive does not synthesize a postcondition — it proves
    /// the unit's own `ensures` and reports `inferred: false`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expr: Option<String>,
    /// Hypotheses the caller grants (e.g. `n > 0`). They are assumptions, not
    /// claims: if they are unsatisfiable the request is `spec_inconsistent`.
    #[serde(default)]
    pub assumptions: Vec<String>,
    /// Loop selector for `termination` (`loop1`, `loop2`, … in source order).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
}

/// One input file: either a path the driver reads, or inline text. Inline text
/// exists so a harness can synthesize over a buffer it has not written yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileSpec {
    /// Display path, recorded in the certificate.
    pub path: String,
    /// Inline content. `None` = read `path` from disk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// The code under analysis.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    /// `ais-core` is the implemented front end. Anything else is refused at
    /// admission with `unsupported_feature` (no silent approximation).
    pub language: String,
    /// Files making up the unit.
    pub files: Vec<FileSpec>,
    /// Entry function name; defaults to the last function in the unit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<String>,
}

/// Which verifier to use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendSpec {
    /// Backend name (`smtlib2-z3` is the one that ships).
    pub name: String,
    /// Optional executable path; defaults to the backend's own discovery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<PathBuf>,
    /// Optional version string the caller requires; a mismatch is refused
    /// rather than silently using a different solver.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// Hard governance bounds. Enforced by the driver, never by the proposer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Budget {
    /// Proposer/refiner rounds, >= 1.
    pub max_rounds: u32,
    /// Wall-clock cap for one solver invocation, in seconds.
    pub solver_timeout_s: u64,
    /// Token cap over all proposer/refiner calls; 0 = unlimited.
    pub token_budget: u64,
    /// Wall-clock cap for the whole request, in seconds.
    pub total_wall_s: u64,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            max_rounds: 8,
            solver_timeout_s: 30,
            token_budget: 60_000,
            total_wall_s: 600,
        }
    }
}

/// Reproducibility controls recorded in provenance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Determinism {
    /// Seed handed to the solver (`:random-seed`) and to candidate ordering.
    pub seed: u64,
    /// Sampling temperature for any LLM the caller wires in. Recorded, not
    /// enforced here — the crate does not call models.
    pub temperature: String,
}

impl Default for Determinism {
    fn default() -> Self {
        Self {
            seed: 42,
            temperature: "0".into(),
        }
    }
}

/// Optional artifacts a caller asks for. The certificate is emitted whenever a
/// proof exists, regardless of this list; `emit` only suppresses the optional
/// extras.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Emit {
    /// The certificate (always produced on `proved`).
    Certificate,
    /// The final annotation text.
    Annotations,
    /// Counterexamples from failed rounds.
    Counterexamples,
    /// The per-round ledger.
    Ledger,
}

/// The primitive's input: the request JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    /// Code under analysis.
    pub source: Source,
    /// What to prove.
    pub property: Property,
    /// Which verifier to use.
    pub backend: BackendSpec,
    /// Governance bounds.
    #[serde(default)]
    pub budget: Budget,
    /// Reproducibility controls.
    #[serde(default)]
    pub determinism: Determinism,
    /// Optional artifact selection.
    #[serde(default)]
    pub emit: Vec<Emit>,
}

impl Request {
    /// Validate bounds before any work happens (fail fast, spend nothing).
    pub fn validate(&self) -> Result<(), String> {
        if self.source.language.trim().is_empty() {
            return Err("source.language must be non-empty".into());
        }
        if self.source.files.is_empty() {
            return Err("source.files must contain at least one file".into());
        }
        if self.source.files.len() > 64 {
            return Err("source.files is capped at 64 entries".into());
        }
        for f in &self.source.files {
            if f.path.trim().is_empty() {
                return Err("every source.files entry needs a non-empty path".into());
            }
            if f.text.is_none() && f.path.trim().is_empty() {
                return Err("a file with no inline text needs a readable path".into());
            }
        }
        if self.backend.name.trim().is_empty() {
            return Err("backend.name must be non-empty".into());
        }
        if self.budget.max_rounds == 0 || self.budget.max_rounds > 64 {
            return Err("budget.max_rounds must be in 1..=64".into());
        }
        if self.budget.solver_timeout_s == 0 || self.budget.solver_timeout_s > 3600 {
            return Err("budget.solver_timeout_s must be in 1..=3600".into());
        }
        if self.budget.total_wall_s == 0 || self.budget.total_wall_s > 86_400 {
            return Err("budget.total_wall_s must be in 1..=86400".into());
        }
        // `postcondition` without an expression means "prove the unit's own
        // `ensures`"; so does `infer` — it is not an inference, and the response
        // reports `inferred: false`. `termination` needs no expression at all.
        // `safety` and `refinement` state a claim and therefore require one.
        if self.property.expr.is_none()
            && matches!(
                self.property.kind,
                PropertyKind::Safety | PropertyKind::Refinement | PropertyKind::Postcondition
            )
            && self.property.kind != PropertyKind::Postcondition
        {
            return Err(format!(
                "property.expr is required for kind {:?}",
                self.property.kind
            ));
        }
        Ok(())
    }
}

/// One synthesized annotation, in the front end's syntax, with the proof
/// obligation target it belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Annotation {
    /// The proof target: a loop id (`loop1`) or a function name.
    pub target: String,
    /// `invariant`, `variant`, or `postcondition`.
    pub kind: String,
    /// The annotation's expression text, exactly as the checker consumed it.
    pub text: String,
}

/// Display name of the proof target. Identical for annotations and obligations.
pub type Target = String;

/// One obligation the checker must discharge, with the exact SMT-LIB2 query
/// that was sent. The query plus the solver binary is the whole trust chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Obligation {
    /// Stable id: `{target}.{kind}[@{path}]`.
    pub id: String,
    /// Function the obligation belongs to.
    pub function: String,
    /// Obligation kind (`init`, `preserve`, `exit`, `assert`, `variant`, `pre`).
    pub kind: String,
    /// The self-contained SMT-LIB2 text (ends with `check-sat`).
    pub smt: String,
    /// Variable names to read from a model if the query is satisfiable.
    #[serde(default)]
    pub probes: Vec<Probe>,
}

/// A named SMT constant plus the source-level name a refiner should see.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Probe {
    /// SMT symbol as emitted.
    pub symbol: String,
    /// Source-level name (`i`, `r`, `a@loop2`, …).
    pub label: String,
}

/// What the checker said about one obligation in one round.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObligationStatus {
    /// The negation of the obligation is unsatisfiable: obligation holds.
    Discharged,
    /// The negation is satisfiable: a counterexample exists.
    Refuted,
    /// The solver said `unknown`.
    Unknown,
    /// The solver exceeded its wall clock.
    TimedOut,
    /// The query could not be run (solver missing, malformed, sandbox fault).
    Error,
}

/// A structured counterexample: the failing obligation, its class, the model
/// values, and the raw SMT-LIB2 assumption set that produced it. A refiner sees
/// this instead of solver noise.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counterexample {
    /// Obligation the model refutes.
    pub obligation: String,
    /// Failure class assigned by the analyzer.
    pub class: FailureClass,
    /// Source-level variable assignments, sorted.
    pub values: BTreeMap<String, String>,
    /// Function the obligation belongs to.
    pub function: String,
    /// How the model was obtained (`get_value` on the negation query).
    pub method: String,
    /// True when the model is a *real execution* (bounded symbolic execution),
    /// false when it is only a model of the negation of an obligation under the
    /// candidate invariant. An unwitnessed counterexample may be spurious — a
    /// refiner may use it, a bug report must not.
    pub witnessed: bool,
}

/// One round of the loop, exactly as it happened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Round {
    /// 0-based round index.
    pub round: u32,
    /// Who proposed: `heuristic`, `scripted`, or a caller-supplied label.
    pub proposer: String,
    /// How many candidate invariants this round submitted.
    pub candidates: u32,
    /// Obligations checked this round.
    pub obligations_checked: u32,
    /// Obligations discharged this round.
    pub obligations_discharged: u32,
    /// Ids of the obligations that did not discharge.
    #[serde(default)]
    pub failed: Vec<String>,
    /// Class assigned to the first failure, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class: Option<FailureClass>,
    /// Solver wall-clock for this round.
    pub solver_ms: u64,
    /// Corpus tokens charged to proposer/refiner in this round.
    pub tokens: u64,
    /// True when this round's `tokens` include an estimate (the provider
    /// reported no usage); false when measured or exactly 0.
    #[serde(default)]
    pub tokens_estimated: bool,
    /// The exact annotation text submitted this round (one line per target).
    #[serde(default)]
    pub annotations: Vec<Annotation>,
}

/// Token and solver accounting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cost {
    /// Tokens charged across proposer and refiner calls.
    pub tokens: u64,
    /// True when any round's tokens were estimated rather than measured.
    #[serde(default)]
    pub tokens_estimated: bool,
    /// Solver wall-clock in milliseconds.
    pub solver_ms: u64,
    /// Total wall clock in milliseconds.
    pub wall_ms: u64,
}

/// Who did what, with hashes, so a reader can decide what to trust.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    /// Proposer label (`heuristic`, `llm:glm-5.3`, …).
    pub proposer: String,
    /// Refiner label (`heuristic`, `llm:glm-5.3`, `none`).
    pub refiner: String,
    /// Backend name as resolved.
    pub backend: String,
    /// Backend version string reported by the binary.
    pub backend_version: String,
    /// `sha256:` of the backend binary actually executed.
    pub backend_digest: String,
    /// `sha256:` of the concatenated source bytes proven.
    pub source_digest: String,
    /// Seed used.
    pub seed: u64,
    /// Temperature recorded (the crate does not enforce it).
    pub temperature: String,
    /// Primitive version.
    pub ais_version: String,
}

impl Default for Provenance {
    fn default() -> Self {
        Self {
            proposer: "none".into(),
            refiner: "none".into(),
            backend: "none".into(),
            backend_version: "none".into(),
            backend_digest: "none".into(),
            source_digest: "none".into(),
            seed: 0,
            temperature: "0".into(),
            ais_version: AIS_VERSION.into(),
        }
    }
}

/// A reference to the emitted certificate, inline in the response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CertificateRef {
    /// `ais-cert-v1`.
    pub format: String,
    /// Backend name.
    pub checker: String,
    /// Base64 of the certificate payload (the exact SMT-LIB2 queries). The
    /// full `ais-cert-v1` artifact — body, annotations and re-check recipe —
    /// is written by `proof run --cert-out`; a reference alone is not enough
    /// to re-check a proof.
    pub blob: String,
    /// `sha256:` of the exact certificate body the blob decodes to.
    pub digest: String,
}

/// The classified failure behind a non-`proved` verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failure {
    /// Taxonomy class.
    pub class: FailureClass,
    /// One line a human can act on.
    pub message: String,
    /// Obligations that did not discharge, with ids.
    #[serde(default)]
    pub obligations: Vec<String>,
    /// True when only a human can resolve this, so the caller escalates.
    pub needs_human: bool,
}

/// The primitive's output: the response JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Response {
    /// Final verdict. Only `proved` is a pass.
    pub verdict: Verdict,
    /// Whether the primitive inferred the property. Always `false`: the
    /// primitive does not synthesize postconditions. A `kind: infer` request
    /// proves the unit's own `ensures` exactly like `postcondition`, and this
    /// field reports that honestly instead of claiming an inference.
    #[serde(default)]
    pub inferred: bool,
    /// Synthesized annotations in front-end syntax.
    pub annotations: Vec<Annotation>,
    /// Certificate reference when a proof exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub certificate: Option<CertificateRef>,
    /// Counterexamples, oldest first.
    #[serde(default)]
    pub counterexamples: Vec<Counterexample>,
    /// Per-round ledger.
    #[serde(default)]
    pub rounds: Vec<Round>,
    /// Accounting.
    pub cost: Cost,
    /// Who did what.
    pub provenance: Provenance,
    /// Classified failure for any non-`proved` verdict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<Failure>,
}

impl Response {
    /// Whether this response may pass a gate.
    pub fn is_pass(&self) -> bool {
        self.verdict.is_pass() && self.certificate.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::Response;

    #[test]
    fn inferred_is_additive_on_the_wire() {
        // A response JSON without the `inferred` key must still parse —
        // defaulting to `false`, never to a claim of inference — and every
        // serialized response must carry the key.
        let old = r#"{
            "verdict": "proved",
            "annotations": [],
            "counterexamples": [],
            "rounds": [],
            "cost": {"tokens": 0, "solver_ms": 0, "wall_ms": 0},
            "provenance": {
                "proposer": "none",
                "refiner": "none",
                "backend": "none",
                "backend_version": "none",
                "backend_digest": "none",
                "source_digest": "none",
                "seed": 0,
                "temperature": "0",
                "ais_version": "ais-1.0"
            }
        }"#;
        let parsed: Response = serde_json::from_str(old).expect("JSON without `inferred` parses");
        assert!(
            !parsed.inferred,
            "a missing key must default to not-inferred"
        );
        let wire = serde_json::to_string(&parsed).expect("serializes");
        assert!(wire.contains("\"inferred\":false"), "{wire}");
    }
}
