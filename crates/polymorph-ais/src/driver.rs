//! The loop: normalize → propose → elaborate → check → classify → refine →
//! certificate. Deterministic control flow; the untrusted half can only change
//! *how many* rounds are spent, never the verdict.
//!
//! Budgets (rounds, tokens, wall clock) are enforced here, not by a proposer.
//! Nothing in this file may convert a `sat`, an `unknown`, a timeout, a
//! contradiction or a malformed proposal into `proved`.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use crate::cert::{self, Certificate, SourcePin};
use crate::classify::{self, Assessment, BmcResult, Outcome};
use crate::ir::{parse_expr, Expr, Program};
use crate::propose::{
    self, Feedback, HeuristicProposer, Proposal, ProposalContext, Proposer, ProposerError,
};
use crate::solver::{BatchItem, Checker, Sat};
use crate::types::{
    Annotation, CertificateRef, Cost, Counterexample, Failure, FailureClass, ObligationStatus,
    Provenance, Request, Response, Round, Verdict,
};
use crate::vc::{self, Goal, VcSet};

/// Everything the driver needs that is not in the request.
#[derive(Clone)]
pub struct DriverConfig {
    /// The resolved, pinned checker.
    pub checker: Checker,
    /// The untrusted proposer/refiner. `None` = deterministic pool only.
    pub proposer: Option<Arc<dyn Proposer>>,
    /// Bounded model-check depth used to separate "too weak" from "false".
    pub bmc_depth: usize,
    /// Run Houdini pruning over every proposal (recommended, and the default).
    pub houdini: bool,
    /// Seed round 0 with the deterministic candidate pool.
    pub use_pool: bool,
    /// The recheck command recorded in the certificate.
    pub recheck_command: String,
}

impl DriverConfig {
    /// A config that uses only the deterministic pool.
    pub fn deterministic(checker: Checker) -> Self {
        Self {
            checker,
            proposer: None,
            bmc_depth: 6,
            houdini: true,
            use_pool: true,
            recheck_command: "polymorph proof recheck --cert <certificate.json> --source <file>..."
                .into(),
        }
    }
}

/// The driver's whole result: the wire response plus the certificate when one
/// was produced (the response carries a reference to it).
#[derive(Debug)]
pub struct DriverOutcome {
    /// The response returned to the caller.
    pub response: Response,
    /// The certificate object, when the verdict is `proved`.
    pub certificate: Option<Certificate>,
}

/// Run one request to a verdict.
pub async fn run(request: Request, cfg: DriverConfig) -> DriverOutcome {
    let mut d = Driver::new(request, cfg);
    d.drive().await
}

struct Driver {
    req: Request,
    cfg: DriverConfig,
    program: Program,
    source_text: String,
    source_digest: String,
    source_pins: Vec<SourcePin>,
    entry: String,
    goal: Goal,
    rounds: Vec<Round>,
    cexs: Vec<Counterexample>,
    tokens: u64,
    /// Any proposal so far charged estimated (not provider-reported) tokens.
    tokens_estimated: bool,
    solver_ms: u64,
    started: Instant,
    proposer_label: String,
    refiner_label: String,
    bmc: Option<BmcResult>,
    annotations: Vec<Annotation>,
    last: Option<Assessment>,
}

impl Driver {
    fn new(req: Request, cfg: DriverConfig) -> Self {
        Self {
            req,
            cfg,
            program: Program::default(),
            source_text: String::new(),
            source_digest: String::new(),
            source_pins: Vec::new(),
            entry: String::new(),
            goal: Goal::default(),
            rounds: Vec::new(),
            cexs: Vec::new(),
            tokens: 0,
            tokens_estimated: false,
            solver_ms: 0,
            started: Instant::now(),
            proposer_label: "none".into(),
            refiner_label: "none".into(),
            bmc: None,
            annotations: Vec::new(),
            last: None,
        }
    }

    async fn drive(&mut self) -> DriverOutcome {
        // 1. Validate before spending anything.
        if let Err(e) = self.req.validate() {
            return self.fail(
                Verdict::Unsupported,
                FailureClass::UnsupportedFeature,
                format!("request rejected: {e}"),
            );
        }
        // 2. Source and admission.
        if let Err(e) = self.load_source() {
            return self.fail(Verdict::Unsupported, FailureClass::UnsupportedFeature, e);
        }
        let language = self.req.source.language.to_ascii_lowercase();
        if language != "ais-core" && language != "ais" {
            return self.fail(
                Verdict::Unsupported,
                FailureClass::UnsupportedFeature,
                format!(
                    "language {:?} is outside the fragment; this backend admits `ais-core` only \
                     (concurrency, heaps, floats, pointers and other languages are declared \
                     unsupported rather than approximated)",
                    self.req.source.language
                ),
            );
        }
        match Program::parse(&self.source_text) {
            Ok(p) => self.program = p,
            Err(e) => {
                return self.fail(
                    Verdict::Unsupported,
                    FailureClass::UnsupportedFeature,
                    format!("the unit did not parse: {e}"),
                )
            }
        }
        self.entry = match &self.req.source.entry {
            Some(f) => f.clone(),
            None => match self.program.functions.last() {
                Some(f) => f.name.clone(),
                None => {
                    return self.fail(
                        Verdict::Unsupported,
                        FailureClass::UnsupportedFeature,
                        "the unit defines no function".into(),
                    )
                }
            },
        };
        if !self.program.functions.iter().any(|f| f.name == self.entry) {
            return self.fail(
                Verdict::Unsupported,
                FailureClass::UnsupportedFeature,
                format!("entry function {:?} is not defined", self.entry),
            );
        }
        // 3. Property normalization.
        match self.build_goal() {
            Ok(()) => {}
            Err(e) => {
                return self.fail(Verdict::Unsupported, FailureClass::ExpressivenessCeiling, e)
            }
        }
        // 4. Precondition satisfiability: a vacuous spec proves nothing.
        match self.precheck() {
            Precheck::Consistent => {}
            Precheck::Inconsistent(msg) => {
                return self.fail(Verdict::SpecInconsistent, FailureClass::SpecBug, msg)
            }
            Precheck::Unknown(msg) => {
                return self.fail(Verdict::Timeout, FailureClass::SolverIncompleteness, msg)
            }
            Precheck::Ceiling(msg) => {
                // `old(...)`, calls and lemma blocks parse and are refused
                // here as an expressiveness ceiling — reachable at precheck,
                // never misreported as an unsupported feature.
                return self.fail(
                    Verdict::Unsupported,
                    FailureClass::ExpressivenessCeiling,
                    msg,
                );
            }
            Precheck::Error(msg) => {
                return self.fail(Verdict::Unsupported, FailureClass::UnsupportedFeature, msg)
            }
        }

        // 5. The loop.
        let max_rounds = self.req.budget.max_rounds;
        let mut feedback: Option<Feedback> = None;
        let mut previous: Vec<Annotation> = Vec::new();
        for round in 0..max_rounds {
            if self.out_of_wall_clock() {
                let a = self.take_assessment().unwrap_or_else(|| {
                    self.classify("the wall-clock budget ran out before a verdict".into())
                });
                return self.finish(a);
            }
            let ctx = ProposalContext {
                source: self.source_text.clone(),
                loops: propose::loop_briefs(&self.program),
                property: self.req.property.clone(),
                round,
                previous: previous.clone(),
            };
            // ---- proposal
            let proposal = match self.next_proposal(&ctx, round, feedback.as_ref()).await {
                Ok(p) => p,
                Err(ProposerError::Declined(why)) => {
                    let a = self
                        .take_assessment()
                        .unwrap_or_else(|| self.classify(format!("no further proposals: {why}")));
                    return self.finish(a);
                }
                Err(e) => {
                    // A proposer fault is retried, never converted into a verdict
                    // about the code.
                    let a = Assessment {
                        class: FailureClass::ProposerMalformed,
                        verdict: Verdict::BudgetExhausted,
                        failed: Vec::new(),
                        message: format!("the proposer produced no usable annotation: {e}"),
                    };
                    self.last = Some(a.clone());
                    self.rounds.push(Round {
                        round,
                        proposer: self.proposer_label.clone(),
                        candidates: 0,
                        obligations_checked: 0,
                        obligations_discharged: 0,
                        failed: Vec::new(),
                        class: Some(FailureClass::ProposerMalformed),
                        solver_ms: 0,
                        tokens: 0,
                        tokens_estimated: false,
                        annotations: Vec::new(),
                    });
                    feedback = Some(Feedback {
                        class: FailureClass::ProposerMalformed,
                        message: a.message.clone(),
                        failed: Vec::new(),
                        counterexamples: Vec::new(),
                        notes: Vec::new(),
                    });
                    if round + 1 >= max_rounds || self.cfg.proposer.is_none() {
                        return self.finish(a);
                    }
                    continue;
                }
            };
            self.tokens = self.tokens.saturating_add(proposal.tokens);
            self.tokens_estimated |= proposal.tokens_estimated;
            previous = proposal.annotations.clone();

            // ---- elaboration + Houdini pruning
            let mut program = self.program.clone();
            // The unit's own annotations are the caller's claims: they are always
            // applied, and Houdini never prunes them (a false one is reported as
            // an unsound invariant instead of being quietly deleted).
            let baseline = baseline_annotations(&self.program);
            let fixed = fixed_counts(&baseline);
            let mut applied = baseline;
            applied.extend(proposal.annotations.clone());
            let mut prune_ms = 0u64;
            if self.cfg.houdini {
                let (kept, kept_anns, ms) = self.houdini(&program, &applied, &fixed);
                prune_ms = ms;
                self.solver_ms = self.solver_ms.saturating_add(ms);
                applied = kept_anns;
                program = kept;
            }
            if let Err(e) = apply_annotations(&mut program, &applied) {
                let a = Assessment {
                    class: FailureClass::ProposerMalformed,
                    verdict: Verdict::BudgetExhausted,
                    failed: Vec::new(),
                    message: format!("annotation did not elaborate: {e}"),
                };
                self.last = Some(a.clone());
                self.rounds.push(Round {
                    round,
                    proposer: self.proposer_label.clone(),
                    candidates: u32::try_from(applied.len()).unwrap_or(u32::MAX),
                    obligations_checked: 0,
                    obligations_discharged: 0,
                    failed: Vec::new(),
                    class: Some(FailureClass::ProposerMalformed),
                    solver_ms: prune_ms,
                    tokens: proposal.tokens,
                    tokens_estimated: proposal.tokens_estimated,
                    annotations: applied.clone(),
                });
                feedback = Some(Feedback {
                    class: FailureClass::ProposerMalformed,
                    message: a.message.clone(),
                    failed: Vec::new(),
                    counterexamples: Vec::new(),
                    notes: Vec::new(),
                });
                if round + 1 >= max_rounds || self.cfg.proposer.is_none() {
                    return self.finish(a);
                }
                continue;
            }
            self.annotations = applied.clone();

            // ---- check (every function in the unit is elaborated, not
            // only the entry; the per-function sets are merged below so a
            // single failing function fails the whole unit)
            let per_function = match vc::elaborate_all(
                &program,
                &self.entry,
                &self.goal,
                self.req.budget.solver_timeout_s,
                self.req.determinism.seed,
            ) {
                Ok(v) => v,
                Err(e) => {
                    // An unbound name or a bad loop target is the *proposal's*
                    // fault (retryable); a missing entry or an out-of-fragment
                    // construct is the caller's (not retryable).
                    let (class, verdict) = match e {
                        vc::ElabError::Unbound(_, _) | vc::ElabError::NoSuchLoop(_, _) => {
                            (FailureClass::ProposerMalformed, Verdict::BudgetExhausted)
                        }
                        // A construct the fragment parses but cannot express
                        // (`old(...)`, a call, a `lemma` block) is an
                        // expressiveness ceiling — understood and named — not
                        // an unsupported feature; only a human can lift it.
                        vc::ElabError::Ceiling(_) => {
                            (FailureClass::ExpressivenessCeiling, Verdict::Unsupported)
                        }
                        _ => (FailureClass::UnsupportedFeature, Verdict::Unsupported),
                    };
                    let a = Assessment {
                        class,
                        verdict,
                        failed: Vec::new(),
                        message: e.to_string(),
                    };
                    return self.finish(a);
                }
            };
            let vc = merge_vcs(&per_function);
            // A unit that elaborates to zero obligations must not be proved.
            // An empty obligation set "discharges" vacuously and a certificate
            // over nothing re-checks as valid, so the unit is refused here on the
            // normal proof path — for every property kind, before the round is
            // checked.
            if vc.obligations.is_empty() {
                return self.fail(
                    Verdict::Unsupported,
                    FailureClass::UnsupportedFeature,
                    "the unit elaborates to an empty obligation set: there is nothing to \
                     prove, so the unit is refused rather than proved"
                        .into(),
                );
            }
            let report = self.check_round(&vc);
            let mut assessment = classify::assess(&vc.obligations, &report.outcomes);
            // An inconsistent invariant set would discharge everything: it is an
            // unsound invariant, never a proof. An *undecided* or *errored*
            // consistency probe is not a consistency pass either — only `sat`
            // is — so it blocks `proved` too, reported as solver incompleteness
            // or a checker fault. A contradiction is the strongest finding and
            // takes precedence over both.
            if !report.contradictory.is_empty() {
                assessment = Assessment {
                    class: FailureClass::UnsoundInvariant,
                    verdict: Verdict::BudgetExhausted,
                    failed: report.contradictory.clone(),
                    message: format!(
                        "the proposed invariant set is contradictory ({}); assuming it would \
                         discharge every obligation for free, so it is rejected",
                        report.contradictory.join(", ")
                    ),
                };
            } else if !report.undecided_consistency.is_empty() {
                assessment = Assessment {
                    class: FailureClass::SolverIncompleteness,
                    verdict: Verdict::Timeout,
                    failed: report.undecided_consistency.clone(),
                    message: format!(
                        "the solver could not decide whether the invariant set of {} is \
                         satisfiable; an undecided consistency probe is not a consistency pass, \
                         so the round is not proved",
                        report.undecided_consistency.join(", ")
                    ),
                };
            } else if !report.errored_consistency.is_empty() {
                assessment = Assessment {
                    class: FailureClass::UnsupportedFeature,
                    verdict: Verdict::Unsupported,
                    failed: report.errored_consistency.clone(),
                    message: format!(
                        "the consistency probe for {} errored, so the invariant set was not \
                         shown consistent and the round is not proved",
                        report.errored_consistency.join(", ")
                    ),
                };
            }
            self.cexs.extend(report.counterexamples.clone());
            self.solver_ms = self.solver_ms.saturating_add(report.solver_ms);
            let discharged = vc
                .obligations
                .iter()
                .filter(|o| report.outcomes.get(&o.id) == Some(&Outcome::Discharged))
                .count();
            self.rounds.push(Round {
                round,
                proposer: proposal.label.clone(),
                candidates: u32::try_from(applied.len()).unwrap_or(u32::MAX),
                obligations_checked: u32::try_from(vc.obligations.len()).unwrap_or(u32::MAX),
                obligations_discharged: u32::try_from(discharged).unwrap_or(u32::MAX),
                failed: assessment.failed.clone(),
                // A proved round carries no failure class: the class field is
                // for "why it stopped short", and nothing stopped short.
                class: if assessment.verdict == Verdict::Proved {
                    None
                } else {
                    Some(assessment.class)
                },
                solver_ms: report.solver_ms.saturating_add(prune_ms),
                tokens: proposal.tokens,
                tokens_estimated: proposal.tokens_estimated,
                annotations: applied.clone(),
            });

            // Termination is only proved if the named loop carries a variant:
            // the postcondition obligations alone do not speak about termination.
            if self.req.property.kind == crate::types::PropertyKind::Termination
                && assessment.verdict == Verdict::Proved
            {
                let target = self
                    .goal
                    .termination_target
                    .clone()
                    .unwrap_or_else(|| "loop1".into());
                let has_variant = vc
                    .obligations
                    .iter()
                    .any(|o| o.kind == "variant" && o.id.starts_with(&format!("{target}.")));
                if !has_variant {
                    assessment = Assessment {
                        class: FailureClass::InsufficientInvariant,
                        verdict: Verdict::BudgetExhausted,
                        failed: Vec::new(),
                        message: format!(
                            "{target}: termination was requested but no `variant` annotation \
                             exists or was proposed, so termination is not proved"
                        ),
                    };
                }
            }

            if assessment.verdict == Verdict::Proved {
                let certificate = self.build_certificate(&vc, &report);
                self.last = Some(assessment.clone());
                return self.finish_proved(assessment, certificate);
            }

            // ---- a real counterexample beats "too weak": bounded model check
            if matches!(
                assessment.class,
                FailureClass::InsufficientInvariant | FailureClass::SpecBug
            ) {
                let bmc = self.bmc_result();
                if let Some(ce) = &bmc.violation {
                    self.cexs.push(ce.clone());
                }
                assessment = classify::upgrade_with_bmc(assessment, &bmc);
            }
            self.last = Some(assessment.clone());

            // ---- refine
            feedback = Some(Feedback {
                class: assessment.class,
                message: assessment.message.clone(),
                failed: assessment.failed.clone(),
                counterexamples: report.counterexamples.clone(),
                notes: vc.notes.clone(),
            });
            if round + 1 >= max_rounds {
                break;
            }
            if self.cfg.proposer.is_none() {
                break;
            }
            if self.req.budget.token_budget > 0 && self.tokens >= self.req.budget.token_budget {
                break;
            }
        }

        let a = self
            .take_assessment()
            .unwrap_or_else(|| self.classify("the round budget ran out before a verdict".into()));
        self.finish(a)
    }

    async fn next_proposal(
        &mut self,
        ctx: &ProposalContext,
        round: u32,
        feedback: Option<&Feedback>,
    ) -> Result<Proposal, ProposerError> {
        let bound = self.cfg.proposer.clone();
        match (bound, round, feedback) {
            (Some(p), 0, _) => {
                let label = p.label();
                let out = p.propose(ctx).await;
                self.proposer_label = label;
                out
            }
            (Some(p), _, Some(fb)) => {
                let label = p.label();
                let out = p.refine(ctx, fb).await;
                self.refiner_label = label;
                out
            }
            // Deterministic pool: round 0, or a proposer asked to refine without
            // feedback (which cannot happen in the loop, but must not panic).
            // Declining afterwards is honest — the pool has already been pruned.
            (_, 0, _) | (None, _, _) | (Some(_), _, None) => {
                if !self.cfg.use_pool {
                    self.proposer_label = "none".into();
                    return Ok(Proposal {
                        annotations: Vec::new(),
                        tokens: 0,
                        tokens_estimated: false,
                        label: "none".into(),
                    });
                }
                self.proposer_label = "heuristic".into();
                let program = Program::parse(&ctx.source)
                    .map_err(|e| ProposerError::Malformed(e.to_string()))?;
                let function = program
                    .functions
                    .iter()
                    .find(|f| f.name == self.entry)
                    .ok_or_else(|| ProposerError::Declined("entry function is gone".into()))?;
                Ok(HeuristicProposer.initial(&program, function))
            }
        }
    }

    /// Houdini: drop every candidate whose `init` or `preserve` obligation is
    /// refuted, repeat until stable. Sound by construction — dropping a
    /// conjunct only weakens the assumed invariant.
    fn houdini(
        &mut self,
        program: &Program,
        annotations: &[Annotation],
        fixed: &BTreeMap<String, usize>,
    ) -> (Program, Vec<Annotation>, u64) {
        let mut current: Vec<Annotation> = annotations.to_vec();
        let mut solver_ms = 0u64;
        for _ in 0..64 {
            let mut candidate = program.clone();
            if apply_annotations(&mut candidate, &current).is_err() {
                return (candidate, current, solver_ms);
            }
            // Pruning covers every function's obligations, so a false
            // candidate on a non-entry loop is pruned here too.
            let per_function = match vc::elaborate_all(
                &candidate,
                &self.entry,
                &self.goal,
                self.req.budget.solver_timeout_s,
                self.req.determinism.seed,
            ) {
                Ok(v) => v,
                Err(_) => return (candidate, current, solver_ms),
            };
            let vc = merge_vcs(&per_function);
            let report = self.check_round(&vc);
            solver_ms = solver_ms.saturating_add(report.solver_ms);
            let mut drop: Vec<(String, usize, String)> = Vec::new();
            for o in &vc.obligations {
                if report.outcomes.get(&o.id) != Some(&Outcome::Refuted) {
                    continue;
                }
                if o.kind != "init" && o.kind != "preserve" {
                    continue;
                }
                if let Some((target, idx, kind)) = obligation_candidate(&o.id) {
                    // Only proposer-supplied candidates are prunable.
                    if idx >= fixed.get(&target).copied().unwrap_or(0) {
                        drop.push((target, idx, kind));
                    }
                }
            }
            if drop.is_empty() {
                return (candidate, current, solver_ms);
            }
            // Remove the named candidates. Indices refer to the pre-drop
            // ordering, so the marking pass computes each candidate's index once
            // and never renumbers mid-flight.
            let mut drop_set: BTreeMap<String, std::collections::BTreeSet<usize>> = BTreeMap::new();
            for (target, idx, _) in &drop {
                drop_set.entry(target.clone()).or_default().insert(*idx);
            }
            let mut counters: BTreeMap<String, usize> = BTreeMap::new();
            for a in current.iter_mut() {
                if a.kind != "invariant" {
                    continue;
                }
                let cursor = counters.entry(a.target.clone()).or_insert(0);
                if drop_set
                    .get(&a.target)
                    .map(|s| s.contains(cursor))
                    .unwrap_or(false)
                {
                    a.kind = "dropped".into();
                }
                *cursor += 1;
            }
            current.retain(|a| a.kind != "dropped");
        }
        let mut candidate = program.clone();
        let _ = apply_annotations(&mut candidate, &current);
        (candidate, current, solver_ms)
    }

    fn check_round(&self, vc: &VcSet) -> CheckReport {
        let mut items: Vec<BatchItem> = vc
            .obligations
            .iter()
            .map(|o| BatchItem {
                script: o.smt.clone(),
                probes: o.probes.clone(),
            })
            .collect();
        for c in &vc.consistency {
            items.push(BatchItem {
                script: c.query.text(),
                probes: Vec::new(),
            });
        }
        let runs = self.cfg.checker.run_batch(&items);
        let mut outcomes = BTreeMap::new();
        let mut counterexamples = Vec::new();
        let mut contradictory = Vec::new();
        let mut undecided_consistency = Vec::new();
        let mut errored_consistency = Vec::new();
        let mut solver_ms = 0u64;
        let mut transcript = String::new();
        for (i, o) in vc.obligations.iter().enumerate() {
            let run = runs.get(i);
            let (outcome, values, digest) = match run {
                Some(r) => {
                    solver_ms = solver_ms.saturating_add(r.elapsed_ms);
                    let outcome = match &r.sat {
                        Sat::Sat => Outcome::Refuted,
                        Sat::Unsat => Outcome::Discharged,
                        Sat::Unknown => Outcome::Unknown,
                        Sat::Timeout => Outcome::TimedOut,
                        Sat::Error(e) => Outcome::Error(e.clone()),
                    };
                    (outcome, r.values.clone(), r.stdout_digest.clone())
                }
                None => (
                    Outcome::Error("the checker returned no run for this obligation".into()),
                    Vec::new(),
                    String::new(),
                ),
            };
            transcript.push_str(&format!(
                "{}\t{}\t{}\n",
                o.id,
                outcome_name(&outcome),
                digest
            ));
            if outcome == Outcome::Refuted {
                if let Some(check) = vc.checks.iter().find(|c| c.id == o.id) {
                    let class = match o.kind.as_str() {
                        "init" | "preserve" => FailureClass::UnsoundInvariant,
                        // A bounded-check refutation is a real execution: the
                        // property is false of the code, not merely unproved.
                        "assert" | "bmc" => FailureClass::SpecBug,
                        _ => FailureClass::InsufficientInvariant,
                    };
                    if let Some(ce) =
                        classify::counterexample(o, check, class, &values, o.kind == "bmc")
                    {
                        counterexamples.push(ce);
                    }
                }
            }
            outcomes.insert(o.id.clone(), outcome);
        }
        for (j, c) in vc.consistency.iter().enumerate() {
            let run = runs.get(vc.obligations.len() + j);
            if let Some(r) = run {
                solver_ms = solver_ms.saturating_add(r.elapsed_ms);
                transcript.push_str(&format!(
                    "{}\tconsistency:{}\t{}\n",
                    c.id,
                    match r.sat {
                        Sat::Sat => "sat",
                        Sat::Unsat => "unsat",
                        Sat::Unknown => "unknown",
                        Sat::Timeout => "timeout",
                        Sat::Error(_) => "error",
                    },
                    r.stdout_digest
                ));
            }
            // Only `sat` is a consistency pass. `unsat` is a contradictory
            // (unsound) invariant set, and anything else — `unknown`, a
            // timeout, an error, or no run at all — means the invariant set
            // was *not* shown consistent, so it blocks `proved` all the same.
            // The transcript row above is unchanged.
            match run.map(|r| &r.sat) {
                Some(Sat::Sat) => {}
                Some(Sat::Unsat) => contradictory.push(c.id.clone()),
                Some(Sat::Unknown) | Some(Sat::Timeout) => undecided_consistency.push(c.id.clone()),
                Some(Sat::Error(_)) | None => errored_consistency.push(c.id.clone()),
            }
        }
        CheckReport {
            outcomes,
            counterexamples,
            contradictory,
            undecided_consistency,
            errored_consistency,
            solver_ms,
            transcript,
        }
    }

    fn bmc_result(&mut self) -> BmcResult {
        if let Some(b) = &self.bmc {
            return b.clone();
        }
        // Loop-free units need no unrolling, and their obligations are exact:
        // the same bounded check still decides whether a refutation is a real
        // execution rather than an artefact of a weak invariant.
        // The bounded check covers every function in the unit, so a
        // refuted obligation of a non-entry function is upgraded with a real
        // witnessed execution too, not only the entry's.
        let per_function = match vc::bounded_check_all(
            &self.program,
            &self.entry,
            &self.goal,
            self.cfg.bmc_depth.max(1),
            self.req.budget.solver_timeout_s,
            self.req.determinism.seed,
        ) {
            Ok(v) => v,
            Err(_) => {
                self.bmc = Some(BmcResult::not_run());
                return BmcResult::not_run();
            }
        };
        let vc = merge_vcs(&per_function);
        let report = self.check_round(&vc);
        self.solver_ms = self.solver_ms.saturating_add(report.solver_ms);
        let mut violation = None;
        let mut undecided = false;
        for o in &vc.obligations {
            match report.outcomes.get(&o.id) {
                Some(Outcome::Refuted) => {
                    if let Some(ce) = report
                        .counterexamples
                        .iter()
                        .find(|c| c.obligation == o.id)
                        .cloned()
                    {
                        violation = Some(ce);
                        break;
                    }
                }
                Some(Outcome::Unknown) | Some(Outcome::TimedOut) => undecided = true,
                _ => {}
            }
        }
        let result = BmcResult {
            depth: self.cfg.bmc_depth.max(1),
            violation,
            undecided,
        };
        self.bmc = Some(result.clone());
        result
    }

    fn build_certificate(&self, vc: &VcSet, report: &CheckReport) -> Option<Certificate> {
        let mut statuses = BTreeMap::new();
        for o in &vc.obligations {
            let status = match report.outcomes.get(&o.id) {
                Some(Outcome::Discharged) => ObligationStatus::Discharged,
                Some(Outcome::Refuted) => ObligationStatus::Refuted,
                Some(Outcome::Unknown) => ObligationStatus::Unknown,
                Some(Outcome::TimedOut) => ObligationStatus::TimedOut,
                _ => ObligationStatus::Error,
            };
            statuses.insert(o.id.clone(), status);
        }
        let backend = cert::BackendPin {
            name: "smtlib2".into(),
            version: self.cfg.checker.version.clone(),
            binary_digest: self.cfg.checker.binary_digest.clone(),
            command: self.cfg.checker.spec.command.clone(),
        };
        Certificate::build(
            self.req.property.clone(),
            self.entry.clone(),
            self.req.determinism.seed,
            self.req.budget.solver_timeout_s,
            self.source_digest.clone(),
            self.source_pins.clone(),
            self.annotations.clone(),
            backend,
            &vc.obligations,
            &statuses,
            &report.transcript,
            &self.cfg.recheck_command,
        )
        .ok()
    }

    fn load_source(&mut self) -> Result<(), String> {
        let mut all = Vec::new();
        let mut pins = Vec::new();
        for f in &self.req.source.files {
            let text = match &f.text {
                Some(t) => t.clone(),
                None => std::fs::read_to_string(&f.path)
                    .map_err(|e| format!("cannot read source {:?}: {e}", f.path))?,
            };
            pins.push(SourcePin {
                path: f.path.clone(),
                digest: cert::sha256_tag(&text),
                bytes: u64::try_from(text.len()).unwrap_or(u64::MAX),
            });
            all.push(text);
        }
        self.source_text = all.join("\n");
        self.source_digest = cert::sha256_tag(&self.source_text);
        self.source_pins = pins;
        Ok(())
    }

    fn build_goal(&mut self) -> Result<(), String> {
        self.goal = goal_from_property(&self.req.property)?;
        Ok(())
    }

    fn precheck(&self) -> Precheck {
        // Every function's precondition conjunction is tested, not only
        // the entry's — a vacuous `requires` on any function in the unit makes
        // that function's obligations meaningless, so it is refused for the
        // whole unit.
        let per_function = match vc::elaborate_all(
            &self.program,
            &self.entry,
            &self.goal,
            self.req.budget.solver_timeout_s,
            self.req.determinism.seed,
        ) {
            Ok(v) => v,
            Err(e) => {
                // A ceiling refusal keeps its own class here instead of
                // collapsing into the generic unsupported-feature one.
                if matches!(e, vc::ElabError::Ceiling(_)) {
                    return Precheck::Ceiling(e.to_string());
                }
                return Precheck::Error(format!("the unit could not be elaborated: {e}"));
            }
        };
        for (name, set) in &per_function {
            let Some(q) =
                set.precondition_query(self.req.budget.solver_timeout_s, self.req.determinism.seed)
            else {
                continue;
            };
            let run = self.cfg.checker.run(&q);
            match run.sat {
                Sat::Sat => {}
                Sat::Unsat => {
                    return Precheck::Inconsistent(format!(
                        "function {name}: the preconditions/assumptions are unsatisfiable, so \
                         every property of {name} holds vacuously; refusing to report a proof \
                         of a vacuous specification"
                    ))
                }
                Sat::Unknown | Sat::Timeout => {
                    return Precheck::Unknown(format!(
                        "function {name}: the solver could not decide whether the preconditions \
                         are satisfiable"
                    ))
                }
                Sat::Error(e) => return Precheck::Error(format!("the checker could not run: {e}")),
            }
        }
        Precheck::Consistent
    }

    fn out_of_wall_clock(&self) -> bool {
        let limit = std::time::Duration::from_secs(self.req.budget.total_wall_s);
        self.started.elapsed() >= limit
    }

    fn take_assessment(&mut self) -> Option<Assessment> {
        self.last.clone()
    }

    fn classify(&self, message: String) -> Assessment {
        Assessment {
            class: FailureClass::BudgetExhausted,
            verdict: Verdict::BudgetExhausted,
            failed: Vec::new(),
            message,
        }
    }

    fn provenance(&self) -> Provenance {
        Provenance {
            proposer: self.proposer_label.clone(),
            refiner: self.refiner_label.clone(),
            backend: "smtlib2".into(),
            backend_version: self.cfg.checker.version.clone(),
            backend_digest: self.cfg.checker.binary_digest.clone(),
            source_digest: self.source_digest.clone(),
            seed: self.req.determinism.seed,
            temperature: self.req.determinism.temperature.clone(),
            ais_version: crate::types::AIS_VERSION.into(),
        }
    }

    fn cost(&self) -> Cost {
        Cost {
            tokens: self.tokens,
            tokens_estimated: self.tokens_estimated,
            solver_ms: self.solver_ms,
            wall_ms: u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX),
        }
    }

    fn emit(&self, mut response: Response) -> Response {
        use crate::types::Emit;
        if self.req.emit.is_empty() {
            return response;
        }
        if !self.req.emit.contains(&Emit::Ledger) {
            response.rounds.clear();
        }
        if !self.req.emit.contains(&Emit::Counterexamples) {
            response.counterexamples.clear();
        }
        response
    }

    fn finish_proved(
        &self,
        assessment: Assessment,
        certificate: Option<Certificate>,
    ) -> DriverOutcome {
        let cert_ref = certificate.as_ref().map(|c| CertificateRef {
            format: c.format.clone(),
            checker: c.body.backend.name.clone(),
            blob: c.blob.clone(),
            digest: c.digest.clone(),
        });
        let response = Response {
            verdict: if certificate.is_some() {
                Verdict::Proved
            } else {
                Verdict::Unsupported
            },
            // The primitive never infers a postcondition. A `kind: infer`
            // request proves the unit's own `ensures` exactly like
            // `postcondition`, and the response says so here.
            inferred: false,
            annotations: self.annotations.clone(),
            certificate: cert_ref,
            counterexamples: self.cexs.clone(),
            rounds: self.rounds.clone(),
            cost: self.cost(),
            provenance: self.provenance(),
            failure: if certificate.is_some() {
                None
            } else {
                Some(Failure {
                    class: FailureClass::UnsupportedFeature,
                    message: format!(
                        "every obligation discharged but the certificate could not be built: {}",
                        assessment.message
                    ),
                    obligations: assessment.failed,
                    needs_human: true,
                })
            },
        };
        DriverOutcome {
            response: self.emit(response),
            certificate,
        }
    }

    fn finish(&self, assessment: Assessment) -> DriverOutcome {
        let mut verdict = assessment.verdict;
        // A failure to prove is never a pass, and an escalation always says so.
        if verdict == Verdict::Proved {
            verdict = Verdict::BudgetExhausted;
        }
        let response = Response {
            verdict,
            inferred: false,
            annotations: self.annotations.clone(),
            certificate: None,
            counterexamples: self.cexs.clone(),
            rounds: self.rounds.clone(),
            cost: self.cost(),
            provenance: self.provenance(),
            failure: Some(Failure {
                class: assessment.class,
                message: assessment.message.clone(),
                obligations: assessment.failed.clone(),
                needs_human: assessment.class.needs_human(),
            }),
        };
        DriverOutcome {
            response: self.emit(response),
            certificate: None,
        }
    }

    fn fail(&self, verdict: Verdict, class: FailureClass, message: String) -> DriverOutcome {
        let response = Response {
            verdict,
            inferred: false,
            annotations: Vec::new(),
            certificate: None,
            counterexamples: Vec::new(),
            rounds: Vec::new(),
            cost: self.cost(),
            provenance: self.provenance(),
            failure: Some(Failure {
                class,
                message,
                obligations: Vec::new(),
                needs_human: class.needs_human(),
            }),
        };
        DriverOutcome {
            response: self.emit(response),
            certificate: None,
        }
    }
}

enum Precheck {
    Consistent,
    Inconsistent(String),
    Unknown(String),
    /// The unit parses but uses `old(...)`, a call or a `lemma` block — an
    /// expressiveness ceiling, kept in its own class rather than misreported
    /// as an unsupported feature.
    Ceiling(String),
    Error(String),
}

struct CheckReport {
    outcomes: BTreeMap<String, Outcome>,
    counterexamples: Vec<Counterexample>,
    contradictory: Vec<String>,
    /// Loops whose consistency probe the solver could not decide
    /// (`unknown` or a timeout). An undecided probe is not a consistency pass,
    /// so the round may not report `proved` on it.
    undecided_consistency: Vec<String>,
    /// Loops whose consistency probe errored, or produced no run at all.
    errored_consistency: Vec<String>,
    solver_ms: u64,
    transcript: String,
}

fn outcome_name(o: &Outcome) -> &'static str {
    match o {
        Outcome::Discharged => "unsat/discharged",
        Outcome::Refuted => "sat/refuted",
        Outcome::Unknown => "unknown",
        Outcome::TimedOut => "timeout",
        Outcome::Error(_) => "error",
    }
}

/// Normalize a property into an elaboration [`Goal`]: the caller's assumptions
/// become extra preconditions, `property.expr` an extra postcondition (or the
/// safety assertion), `at` the termination target. Shared by the driver and by
/// the certificate re-check so both elaborate exactly the same way.
pub(crate) fn goal_from_property(property: &crate::types::Property) -> Result<Goal, String> {
    use crate::types::PropertyKind;
    let mut goal = Goal::default();
    for a in &property.assumptions {
        goal.assumptions
            .push(parse_expr(a).map_err(|e| format!("property.assumptions entry {a:?}: {e}"))?);
    }
    match property.kind {
        // `infer` is not an inference. It elaborates exactly like
        // `postcondition` — the unit's own `ensures`, plus `property.expr`
        // when one is given — and the response reports `inferred: false`.
        PropertyKind::Postcondition | PropertyKind::Infer => {
            if let Some(e) = &property.expr {
                goal.ensures
                    .push(parse_expr(e).map_err(|e| format!("property.expr {e}"))?);
            }
        }
        PropertyKind::Safety => {
            let e = property
                .expr
                .as_ref()
                .ok_or_else(|| "safety needs property.expr".to_string())?;
            goal.assertion = Some(parse_expr(e).map_err(|err| format!("property.expr: {err}"))?);
        }
        PropertyKind::Termination => {
            let target = property.at.clone().unwrap_or_else(|| "loop1".to_string());
            goal.termination_target = Some(target);
        }
        PropertyKind::Refinement => {
            return Err(
                "refinement needs a two-state logic with `old(...)`, which this fragment does \
                 not have; it is reported as an expressiveness ceiling instead of approximated"
                    .into(),
            )
        }
    }
    Ok(goal)
}

/// Merge every function's [`VcSet`] into one, in source order: the round
/// check, the zero-obligation refusal, the certificate and the feedback all
/// run over the merged set, so a single failing function fails the whole unit.
/// The merged set carries no single precondition — preconditions are per
/// function and are prechecked per function by the driver.
pub(crate) fn merge_vcs(sets: &[(String, VcSet)]) -> VcSet {
    let mut merged = VcSet::default();
    for (_function, set) in sets {
        merged.obligations.extend(set.obligations.iter().cloned());
        merged.checks.extend(set.checks.iter().cloned());
        merged.consistency.extend(set.consistency.iter().cloned());
        merged.notes.extend(set.notes.iter().cloned());
    }
    merged
}

/// The unit's own invariants and variants, in source order.
pub fn baseline_annotations(program: &Program) -> Vec<Annotation> {
    let mut out = Vec::new();
    for brief in propose::loop_briefs(program) {
        for inv in &brief.invariants {
            out.push(Annotation {
                target: brief.id.clone(),
                kind: "invariant".into(),
                text: inv.clone(),
            });
        }
        if let Some(v) = &brief.variant {
            out.push(Annotation {
                target: brief.id.clone(),
                kind: "variant".into(),
                text: v.clone(),
            });
        }
    }
    out
}

/// How many leading annotations per target are fixed (non-prunable).
fn fixed_counts(baseline: &[Annotation]) -> BTreeMap<String, usize> {
    let mut out: BTreeMap<String, usize> = BTreeMap::new();
    for a in baseline {
        if a.kind == "invariant" {
            *out.entry(a.target.clone()).or_insert(0) += 1;
        }
    }
    out
}

/// `loop3.init2@4` → `("loop3", 2, "init")`. Only invariant obligations map to a
/// candidate; everything else returns `None`.
pub fn obligation_candidate(id: &str) -> Option<(String, usize, String)> {
    // Strip a @n disambiguator.
    let base = id.split('@').next()?;
    let (target, rest) = base.split_once('.')?;
    if !target.starts_with("loop") {
        return None;
    }
    let digits: String = rest.chars().skip_while(|c| !c.is_ascii_digit()).collect();
    let kind: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect();
    if kind != "init" && kind != "preserve" {
        return None;
    }
    let idx = digits.parse::<usize>().ok()?;
    Some((target.to_string(), idx, kind))
}

/// Apply annotations to a program copy. Unknown targets and unparsable text are
/// the proposer's fault, reported as such.
pub(crate) fn apply_annotations(
    program: &mut Program,
    annotations: &[Annotation],
) -> Result<(), String> {
    let mut by_target: BTreeMap<String, (Vec<Expr>, Option<Expr>)> = BTreeMap::new();
    for a in annotations {
        let e = vc::parse_annotation(&a.text)
            .map_err(|err| format!("annotation {:?} did not parse: {err}", a.text))?;
        let slot = by_target
            .entry(a.target.clone())
            .or_insert_with(|| (Vec::new(), None));
        match a.kind.as_str() {
            "invariant" => slot.0.push(e),
            "variant" => slot.1 = Some(e),
            other => return Err(format!("unknown annotation kind {other:?}")),
        }
    }
    for (target, (invariants, variant)) in by_target {
        if !program.set_invariants(&target, invariants) {
            return Err(format!(
                "annotation targets {:?}, which is not a loop in the unit",
                target
            ));
        }
        if variant.is_some() {
            program.set_variant(&target, variant);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn obligation_ids_map_back_to_their_candidate() {
        assert_eq!(
            obligation_candidate("loop3.init2"),
            Some(("loop3".into(), 2, "init".into()))
        );
        assert_eq!(
            obligation_candidate("loop1.preserve11@4"),
            Some(("loop1".into(), 11, "preserve".into()))
        );
        assert_eq!(obligation_candidate("loop1.variant_nonneg"), None);
        assert_eq!(obligation_candidate("f.post0"), None);
        assert_eq!(obligation_candidate("bmc.f.post0"), None);
    }

    #[test]
    fn apply_annotations_refuses_unknown_targets_and_bad_syntax() {
        let src = "fn f(n: int) -> int\n{\n  return n;\n}\n";
        let mut program = Program::parse(src).unwrap();
        let err = apply_annotations(
            &mut program,
            &[Annotation {
                target: "loop1".into(),
                kind: "invariant".into(),
                text: "n >= 0".into(),
            }],
        )
        .unwrap_err();
        assert!(err.contains("not a loop"), "{err}");

        let err = apply_annotations(
            &mut program,
            &[Annotation {
                target: "f".into(),
                kind: "invariant".into(),
                text: "n >< 0".into(),
            }],
        )
        .unwrap_err();
        assert!(err.contains("did not parse"), "{err}");
    }

    #[test]
    fn baseline_annotations_mirror_the_unit_and_fixed_counts_are_per_target() {
        let src = "fn f(n: int) -> int\n{\n  var i: int = 0;\n  while (i < n)\n    invariant i >= 0\n    variant n - i\n  {\n    i = i + 1;\n  }\n  return i;\n}\n";
        let program = Program::parse(src).unwrap();
        let base = baseline_annotations(&program);
        assert_eq!(base.len(), 2);
        assert_eq!(base[0].kind, "invariant");
        assert_eq!(base[1].kind, "variant");
        let fixed = fixed_counts(&base);
        assert_eq!(fixed.get("loop1"), Some(&1));
    }
}
