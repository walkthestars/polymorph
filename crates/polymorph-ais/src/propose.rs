//! The untrusted half: proposers, refiners, and the deterministic candidate
//! pool that lets the primitive work with no model at all.
//!
//! Nothing here can decide a verdict. A [`Proposer`] returns annotation *text*
//! that the elaborator must parse and the checker must discharge; a proposer
//! that lies, hallucinates, or returns nothing changes only how many rounds the
//! loop spends, never the verdict.
//!
//! Two proposers ship:
//!
//! * [`HeuristicProposer`] — a deterministic candidate pool (bounds, affine
//!   relations, small-polynomial equalities, modular facts, and quantified array
//!   facts) pruned by Houdini-style guess-and-check. Zero tokens, reproducible.
//! * [`ScriptedProposer`] — a test seam: fixed proposals per round, so the loop,
//!   the taxonomy and the certificate can be exercised with no model at all.
//!
//! An LLM proposer lives outside this crate (the CLI wires it), which is why the
//! spec's "role-routed proposer/refiner" costs this crate nothing: the trait is
//! the seam.

use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;

use crate::ir::{BinOp, Expr, Function, LoopSpec, Program, QuantKind, Stmt};
use crate::types::{Annotation, Counterexample, FailureClass};

/// What a proposer is told. Deliberately structured: source text, loop shapes,
/// and (for refinement) the taxonomy class and counterexamples — never a raw
/// solver log.
#[derive(Debug, Clone)]
pub struct ProposalContext {
    /// The unit's source text, exactly as elaborated.
    pub source: String,
    /// One entry per loop, in source order.
    pub loops: Vec<LoopBrief>,
    /// The property being proved.
    pub property: crate::types::Property,
    /// 0-based round number.
    pub round: u32,
    /// Prior annotations (so a refiner can see what it must repair).
    pub previous: Vec<Annotation>,
}

/// The shape of one loop, as a proposer sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopBrief {
    /// `loop1`, `loop2`, …
    pub id: String,
    /// Enclosing function.
    pub function: String,
    /// Guard, rendered.
    pub guard: String,
    /// Variables the body writes.
    pub writes: Vec<String>,
    /// Current invariant text, if the source already had one.
    pub invariants: Vec<String>,
    /// Current variant text, if any.
    pub variant: Option<String>,
}

/// Structured feedback for a refinement round.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Feedback {
    /// Taxonomy class assigned by the analyzer.
    pub class: FailureClass,
    /// One actionable line from the analyzer.
    pub message: String,
    /// Obligations that did not discharge.
    pub failed: Vec<String>,
    /// Validated counterexamples (already re-evaluated independently).
    pub counterexamples: Vec<Counterexample>,
    /// Notes from elaboration (fragment limits noticed this round).
    pub notes: Vec<String>,
}

/// What a proposer returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proposal {
    /// Annotations in front-end syntax, one expression per annotation.
    pub annotations: Vec<Annotation>,
    /// Tokens charged (0 for deterministic proposers).
    pub tokens: u64,
    /// True when `tokens` is an estimate because the provider reported no
    /// usage; false when it is a measurement (or exactly 0).
    pub tokens_estimated: bool,
    /// Label recorded in provenance.
    pub label: String,
}

/// A proposer failure. Malformed output is a *proposer* fault and is retried;
/// it never becomes a proof failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProposerError {
    /// The proposal could not be understood at all.
    #[error("proposal is unusable: {0}")]
    Malformed(String),
    /// The proposer declined to make a proposal.
    #[error("proposer declined: {0}")]
    Declined(String),
    /// Transport/model failure (not the proposer's opinion, an outage).
    #[error("proposer transport failed: {0}")]
    Transport(String),
}

impl Proposal {
    /// Annotations for one target, in order.
    pub fn for_target(&self, target: &str) -> Vec<String> {
        self.annotations
            .iter()
            .filter(|a| a.target == target && a.kind == "invariant")
            .map(|a| a.text.clone())
            .collect()
    }

    /// The variant proposed for a target, if any.
    pub fn variant_for(&self, target: &str) -> Option<String> {
        self.annotations
            .iter()
            .find(|a| a.target == target && a.kind == "variant")
            .map(|a| a.text.clone())
    }
}

/// The untrusted proposer seam.
#[async_trait]
pub trait Proposer: Send + Sync {
    /// Stable label for provenance (`heuristic`, `llm:glm-5.3`, …).
    fn label(&self) -> String;

    /// Round-0 proposal.
    async fn propose(&self, ctx: &ProposalContext) -> Result<Proposal, ProposerError>;

    /// Refinement from structured feedback. The default replays [`Self::propose`]
    /// so a stateless proposer needs only one method.
    async fn refine(
        &self,
        ctx: &ProposalContext,
        feedback: &Feedback,
    ) -> Result<Proposal, ProposerError> {
        let _ = feedback;
        self.propose(ctx).await
    }
}

/// The deterministic candidate pool + Houdini pruning policy.
///
/// This is the classical half of the primitive: generate candidates, refute the
/// false ones with the checker (the driver runs the pruning loop), keep what
/// survives. Its output is only as good as the grammar; its value is that it
/// costs no tokens and is reproducible byte for byte.
#[derive(Debug, Clone, Default)]
pub struct HeuristicProposer;

impl HeuristicProposer {
    /// The candidate pool for one function: `(loop id, expression)` pairs, in a
    /// deterministic order.
    pub fn pool(program: &Program, function: &Function) -> Vec<(String, Expr)> {
        let mut out = Vec::new();
        let ids = loop_ids_with_specs(program);
        let scopes = scope_at_loops(program);
        for (id, func, guard, spec, body) in ids {
            if func != function.name {
                continue;
            }
            // A candidate may only mention names that exist at the loop head:
            // a variable first declared inside the body is not in scope there,
            // and proposing it would be a (correctly rejected) fault.
            let in_scope = scopes.get(&id).cloned().unwrap_or_default();
            let mut int_vars: BTreeSet<String> = BTreeSet::new();
            let mut arrays: BTreeSet<String> = BTreeSet::new();
            for v in guard.free_vars() {
                if in_scope.contains(&v) {
                    int_vars.insert(v);
                }
            }
            let writes = crate::ir::written_names(&body);
            for w in &writes {
                if in_scope.contains(w) {
                    int_vars.insert(w.clone());
                }
            }
            for (p, ty) in &function.params {
                match ty {
                    crate::ir::Ty::Int => {
                        int_vars.insert(p.clone());
                    }
                    crate::ir::Ty::Array => {
                        arrays.insert(p.clone());
                    }
                    crate::ir::Ty::Bool => {}
                }
            }
            // Arrays are not ordered values: keep them out of scalar atoms.
            for a in &arrays {
                int_vars.remove(a);
            }
            let mut consts: BTreeSet<i64> = [0i64, 1].into_iter().collect();
            collect_consts(&guard, &mut consts);
            for st in &body {
                collect_stmt_consts(st, &mut consts);
            }
            for e in &spec.invariants {
                collect_consts(e, &mut consts);
            }
            let consts: Vec<i64> = consts
                .into_iter()
                .filter(|c| c.abs() <= 64)
                .take(8)
                .collect();
            let vars: Vec<String> = int_vars.into_iter().collect();

            let mut push = |e: Expr| {
                out.push((id.clone(), e));
            };
            // Bounds against zero and against the pool's constants.
            for v in &vars {
                push(cmp(v, BinOp::Ge, 0));
                push(cmp(v, BinOp::Le, 0));
                for c in &consts {
                    push(cmp(v, BinOp::Ge, *c));
                    push(cmp(v, BinOp::Le, *c));
                }
            }
            // Relational atoms between every pair, plus small affine offsets.
            for v in &vars {
                for w in &vars {
                    if v == w {
                        continue;
                    }
                    push(cmp2(v, BinOp::Le, w));
                    push(cmp2(v, BinOp::Ge, w));
                    for c in consts.iter().take(4) {
                        push(affine(v, w, 1, *c)); // v == w + c
                    }
                    for c in [-1i64, 1, 2] {
                        push(cmp2_off(v, BinOp::Le, w, c)); // v <= w + c
                        push(cmp2_off(v, BinOp::Ge, w, c)); // v >= w + c
                    }
                    push(mul_eq(v, w, 2));
                    push(mul_eq(v, w, 3));
                    // Triangular numbers: the shape of sum loops.
                    push(tri_eq(v, w, 1));
                    push(tri_eq(v, w, -1));
                }
            }
            // Modular facts (parity and small moduli).
            for v in &vars {
                for k in [2i64, 3] {
                    push(Expr::Binary(
                        BinOp::Eq,
                        Box::new(Expr::Binary(
                            BinOp::Mod,
                            Box::new(Expr::Var(v.clone())),
                            Box::new(Expr::Int(k)),
                        )),
                        Box::new(Expr::Int(0)),
                    ));
                }
            }
            // Quantified array facts: the shape of "everything scanned so far".
            for a in &arrays {
                for i in &vars {
                    for m in &vars {
                        push(forall_idx(a, i, BinOp::Le, &Expr::Var(m.clone())));
                        push(forall_idx(a, i, BinOp::Ge, &Expr::Var(m.clone())));
                        for k in consts.iter().take(3) {
                            push(forall_idx(a, i, BinOp::Ne, &Expr::Int(*k)));
                        }
                    }
                }
            }
            let _ = (&writes,);
            let written = crate::ir::written_names(&body);
            prune_pool(&mut out, &written);
        }
        out
    }
}

/// Keep only candidates that say something about a variable the loop changes
/// (a fact about parameters alone is either already a precondition or false),
/// and drop duplicates. Deterministic: order is preserved.
fn prune_pool(out: &mut Vec<(String, Expr)>, written: &BTreeSet<String>) {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    out.retain(|(_, e)| {
        let relevant = e.free_vars().iter().any(|v| written.contains(v));
        relevant && seen.insert(e.to_string())
    });
}

fn cmp(v: &str, op: BinOp, c: i64) -> Expr {
    Expr::Binary(
        op,
        Box::new(Expr::Var(v.to_string())),
        Box::new(Expr::Int(c)),
    )
}

fn cmp2_off(v: &str, op: BinOp, w: &str, c: i64) -> Expr {
    Expr::Binary(
        op,
        Box::new(Expr::Var(v.to_string())),
        Box::new(Expr::Binary(
            BinOp::Add,
            Box::new(Expr::Var(w.to_string())),
            Box::new(Expr::Int(c)),
        )),
    )
}

fn cmp2(v: &str, op: BinOp, w: &str) -> Expr {
    Expr::Binary(
        op,
        Box::new(Expr::Var(v.to_string())),
        Box::new(Expr::Var(w.to_string())),
    )
}

/// `v == w + c`.
fn affine(v: &str, w: &str, k: i64, c: i64) -> Expr {
    Expr::Binary(
        BinOp::Eq,
        Box::new(Expr::Var(v.to_string())),
        Box::new(Expr::Binary(
            BinOp::Add,
            Box::new(Expr::Binary(
                BinOp::Mul,
                Box::new(Expr::Var(w.to_string())),
                Box::new(Expr::Int(k)),
            )),
            Box::new(Expr::Int(c)),
        )),
    )
}

/// `v == k * w`.
fn mul_eq(v: &str, w: &str, k: i64) -> Expr {
    Expr::Binary(
        BinOp::Eq,
        Box::new(Expr::Var(v.to_string())),
        Box::new(Expr::Binary(
            BinOp::Mul,
            Box::new(Expr::Var(w.to_string())),
            Box::new(Expr::Int(k)),
        )),
    )
}

/// `v == w * (w + d) / 2` — the closed form of a sum.
fn tri_eq(v: &str, w: &str, d: i64) -> Expr {
    Expr::Binary(
        BinOp::Eq,
        Box::new(Expr::Var(v.to_string())),
        Box::new(Expr::Binary(
            BinOp::Div,
            Box::new(Expr::Binary(
                BinOp::Mul,
                Box::new(Expr::Var(w.to_string())),
                Box::new(Expr::Binary(
                    BinOp::Add,
                    Box::new(Expr::Var(w.to_string())),
                    Box::new(Expr::Int(d)),
                )),
            )),
            Box::new(Expr::Int(2)),
        )),
    )
}

/// `forall j: int :: 0 <= j && j < i => a[j] op bound`.
fn forall_idx(a: &str, i: &str, op: BinOp, bound: &Expr) -> Expr {
    let j = "j_q";
    let range = Expr::Binary(
        BinOp::And,
        Box::new(Expr::Binary(
            BinOp::Le,
            Box::new(Expr::Int(0)),
            Box::new(Expr::Var(j.to_string())),
        )),
        Box::new(Expr::Binary(
            BinOp::Lt,
            Box::new(Expr::Var(j.to_string())),
            Box::new(Expr::Var(i.to_string())),
        )),
    );
    let body = Expr::Binary(
        BinOp::Imp,
        Box::new(range),
        Box::new(Expr::Binary(
            op,
            Box::new(Expr::Select(
                Box::new(Expr::Var(a.to_string())),
                Box::new(Expr::Var(j.to_string())),
            )),
            Box::new(bound.clone()),
        )),
    );
    Expr::Quant {
        kind: QuantKind::Forall,
        var: j.to_string(),
        body: Box::new(body),
    }
}

fn collect_consts(e: &Expr, out: &mut BTreeSet<i64>) {
    match e {
        Expr::Int(n) => {
            out.insert(*n);
        }
        Expr::Bool(_) | Expr::Var(_) => {}
        Expr::Select(a, i) => {
            collect_consts(a, out);
            collect_consts(i, out);
        }
        Expr::Store(a, i, v) => {
            collect_consts(a, out);
            collect_consts(i, out);
            collect_consts(v, out);
        }
        Expr::Unary(_, x) => collect_consts(x, out),
        Expr::Binary(_, l, r) => {
            collect_consts(l, out);
            collect_consts(r, out);
        }
        Expr::Quant { body, .. } => collect_consts(body, out),
        Expr::Old(x) => collect_consts(x, out),
        Expr::Call { args, .. } => {
            for a in args {
                collect_consts(a, out);
            }
        }
    }
}

fn collect_stmt_consts(s: &Stmt, out: &mut BTreeSet<i64>) {
    match s {
        Stmt::Decl { init, .. } => {
            if let Some(e) = init {
                collect_consts(e, out);
            }
        }
        Stmt::Assign { value, .. } => collect_consts(value, out),
        Stmt::Store { index, value, .. } => {
            collect_consts(index, out);
            collect_consts(value, out);
        }
        Stmt::If { cond, then, els } => {
            collect_consts(cond, out);
            for s in then.iter().chain(els.iter()) {
                collect_stmt_consts(s, out);
            }
        }
        Stmt::While { cond, spec, body } => {
            collect_consts(cond, out);
            for i in &spec.invariants {
                collect_consts(i, out);
            }
            if let Some(v) = &spec.variant {
                collect_consts(v, out);
            }
            for s in body {
                collect_stmt_consts(s, out);
            }
        }
        Stmt::Assert(e) | Stmt::Assume(e) => collect_consts(e, out),
        Stmt::Return(e) => {
            if let Some(e) = e {
                collect_consts(e, out);
            }
        }
        Stmt::Call { args, .. } => {
            for a in args {
                collect_consts(a, out);
            }
        }
    }
}

/// Every loop in the unit with the pieces a proposer needs, in source order.
pub fn loop_briefs(program: &Program) -> Vec<LoopBrief> {
    loop_ids_with_specs(program)
        .into_iter()
        .map(|(id, function, guard, spec, body)| LoopBrief {
            id,
            function,
            guard: guard.to_string(),
            writes: crate::ir::written_names(&body).into_iter().collect(),
            invariants: spec.invariants.iter().map(|e| e.to_string()).collect(),
            variant: spec.variant.as_ref().map(|e| e.to_string()),
        })
        .collect()
}

/// Names in scope at each loop head (`loop1`, `loop2`, … in source order).
///
/// A name enters scope when a parameter introduces it or when a `var`/assignment
/// precedes the loop on some path; the walk is deliberately conservative (a name
/// assigned anywhere earlier in the enclosing block counts).
pub fn scope_at_loops(program: &Program) -> BTreeMap<String, BTreeSet<String>> {
    fn walk(
        body: &[Stmt],
        scope: &mut BTreeSet<String>,
        n: &mut usize,
        out: &mut BTreeMap<String, BTreeSet<String>>,
    ) {
        for s in body {
            match s {
                Stmt::Decl { name, .. } | Stmt::Assign { name, .. } => {
                    scope.insert(name.clone());
                }
                Stmt::Store { array, .. } => {
                    scope.insert(array.clone());
                }
                Stmt::If { then, els, .. } => {
                    // Both branches may declare names; be conservative and keep
                    // them for the rest of the enclosing block.
                    walk(then, scope, n, out);
                    walk(els, scope, n, out);
                }
                Stmt::While { body, .. } => {
                    *n += 1;
                    let id = format!("loop{n}");
                    out.insert(id, scope.clone());
                    walk(body, scope, n, out);
                }
                _ => {}
            }
        }
    }
    let mut out = BTreeMap::new();
    // Loop ids are globally numbered across the whole unit (matching
    // `Program::loop_ids`, `Program::set_invariants` and the elaborator), so
    // the counter is shared by every function; the scope itself stays per
    // function.
    let mut n = 0usize;
    for f in &program.functions {
        let mut scope: BTreeSet<String> = f.params.iter().map(|(n, _)| n.clone()).collect();
        walk(&f.body, &mut scope, &mut n, &mut out);
    }
    out
}

#[allow(clippy::type_complexity)]
fn loop_ids_with_specs(program: &Program) -> Vec<(String, String, Expr, LoopSpec, Vec<Stmt>)> {
    fn walk(
        body: &[Stmt],
        func: &str,
        n: &mut usize,
        out: &mut Vec<(String, String, Expr, LoopSpec, Vec<Stmt>)>,
    ) {
        for s in body {
            match s {
                Stmt::While { cond, spec, body } => {
                    *n += 1;
                    out.push((
                        format!("loop{n}"),
                        func.to_string(),
                        cond.clone(),
                        spec.clone(),
                        body.clone(),
                    ));
                    walk(body, func, n, out);
                }
                Stmt::If { then, els, .. } => {
                    walk(then, func, n, out);
                    walk(els, func, n, out);
                }
                _ => {}
            }
        }
    }
    let mut out = Vec::new();
    // Loop ids are globally numbered across the whole unit (matching
    // `Program::loop_ids`, `Program::set_invariants` and the elaborator), so
    // the counter is shared by every function rather than reset per function.
    let mut n = 0usize;
    for f in &program.functions {
        walk(&f.body, &f.name, &mut n, &mut out);
    }
    out
}

impl HeuristicProposer {
    /// The round-0 proposal: the whole candidate pool as one conjunction per
    /// loop. Houdini pruning happens in the driver, against the checker.
    pub fn initial(&self, program: &Program, function: &Function) -> Proposal {
        let annotations = HeuristicProposer::pool(program, function)
            .into_iter()
            .map(|(target, expr)| Annotation {
                target,
                kind: "invariant".into(),
                text: expr.to_string(),
            })
            .collect();
        Proposal {
            annotations,
            tokens: 0,
            tokens_estimated: false,
            label: "heuristic".into(),
        }
    }
}

#[async_trait]
impl Proposer for HeuristicProposer {
    fn label(&self) -> String {
        "heuristic".into()
    }

    async fn propose(&self, ctx: &ProposalContext) -> Result<Proposal, ProposerError> {
        let program = Program::parse(&ctx.source)
            .map_err(|e| ProposerError::Malformed(format!("source did not parse: {e}")))?;
        let function = program
            .entry(Some(
                ctx.loops
                    .first()
                    .map(|l| l.function.as_str())
                    .unwrap_or_default(),
            ))
            .ok_or_else(|| ProposerError::Declined("no function to propose for".into()))?;
        Ok(self.initial(&program, function))
    }

    async fn refine(
        &self,
        _ctx: &ProposalContext,
        _feedback: &Feedback,
    ) -> Result<Proposal, ProposerError> {
        // The deterministic pool has nothing new to say: it has already been
        // pruned by Houdini, and re-proposing it would loop forever. Declining
        // is the honest answer, and the driver stops instead of burning rounds.
        Err(ProposerError::Declined(
            "the deterministic pool is exhausted; a model proposer is required to go further"
                .into(),
        ))
    }
}

/// A scripted proposer for tests and for the benchmark's "no model" proof: each
/// round pops one prepared proposal. Exhaustion is a decline, not an error, so
/// the loop terminates deterministically.
#[derive(Debug, Clone, Default)]
pub struct ScriptedProposer {
    /// Proposals in round order. Round `n` uses entry `n`; missing entries mean
    /// "declined".
    pub proposals: Vec<Proposal>,
    /// Refinement proposals, applied after the first failed round.
    pub refinements: Vec<Proposal>,
    /// Label for provenance.
    pub name: String,
}

impl ScriptedProposer {
    /// A proposer that returns `proposals` in order.
    pub fn new(name: &str, proposals: Vec<Proposal>) -> Self {
        Self {
            proposals,
            refinements: Vec::new(),
            name: name.to_string(),
        }
    }

    /// Add one refinement step.
    pub fn with_refinement(mut self, p: Proposal) -> Self {
        self.refinements.push(p);
        self
    }
}

#[async_trait]
impl Proposer for ScriptedProposer {
    fn label(&self) -> String {
        if self.name.is_empty() {
            "scripted".into()
        } else {
            self.name.clone()
        }
    }

    async fn propose(&self, ctx: &ProposalContext) -> Result<Proposal, ProposerError> {
        self.proposals
            .get(ctx.round as usize)
            .cloned()
            .ok_or_else(|| ProposerError::Declined("no scripted proposal for this round".into()))
    }

    async fn refine(
        &self,
        ctx: &ProposalContext,
        _feedback: &Feedback,
    ) -> Result<Proposal, ProposerError> {
        // Refinements are indexed by how many refinements have already happened,
        // which the driver encodes as rounds beyond the first.
        let idx = (ctx.round as usize).saturating_sub(1);
        self.refinements
            .get(idx)
            .cloned()
            .ok_or_else(|| ProposerError::Declined("no scripted refinement for this round".into()))
    }
}
