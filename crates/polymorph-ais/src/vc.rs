//! Elaboration: symbolic execution of the core IR into SMT-LIB2 obligations.
//!
//! The encoding is the standard havoc/assume rule for loops, spelled out so a
//! reader can audit it:
//!
//! * `loop.init` — the invariant holds when control first reaches the loop.
//! * `loop.preserve` — the invariant is closed under the body while the guard
//!   holds. The body starts from *fresh* constants (the havoc) constrained only
//!   by the invariant, because an invariant that mentions the pre-loop values is
//!   not inductive.
//! * `loop.exit` — the invariant plus a false guard implies the postcondition.
//! * `loop.variant` — a non-negative variant that strictly decreases.
//!
//! Every obligation is a [`Query`] asserted as `assumptions ∧ ¬goal`; `unsat`
//! means the obligation holds. Path conditions are accumulated structurally, so
//! `if` and nested loops compose without a special theory.

use std::collections::{BTreeMap, BTreeSet};

use crate::ir::{parse_expr, written_names, BinOp, Expr, Function, Program, Stmt, Ty};
use crate::smt::{Decls, Query};
use crate::types::{Obligation, Probe};

/// A failed elaboration. Every variant maps to a taxonomy class in
/// [`crate::classify`]; none of them may be turned into a proof.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ElabError {
    /// The entry function is not in the unit.
    #[error("entry function {0:?} is not defined in the unit")]
    NoEntry(String),
    /// An expression names a variable that is not in scope.
    #[error("{0}: unknown variable {1}")]
    Unbound(String, String),
    /// A loop target named by the request does not exist.
    #[error("loop target {0:?} does not exist (the unit has: {1})")]
    NoSuchLoop(String, String),
    /// The unit uses a construct outside the fragment.
    #[error("unsupported construct: {0}")]
    Unsupported(String),
    /// The unit uses a construct the fragment *parses* but cannot express:
    /// `old(...)`, a call, or a `lemma` block. An expressiveness ceiling —
    /// reported with its own class, never as a parse error, never approximated.
    #[error("expressiveness ceiling: {0}")]
    Ceiling(String),
}

/// The elaboration product: obligations plus the satisfiability probe used to
/// detect a vacuous specification.
#[derive(Debug, Clone, Default)]
pub struct VcSet {
    /// Obligations in program order.
    pub obligations: Vec<Obligation>,
    /// Precondition conjunction. Tested for satisfiability before the
    /// obligations mean anything: a vacuous `requires` proves nothing.
    pub precondition: Option<Expr>,
    /// Declarations in scope for the precondition.
    pub precondition_decls: Decls,
    /// Notes about what the fragment could not cover.
    pub notes: Vec<String>,
    /// The obligations' IR form, used to validate a counterexample with the
    /// independent evaluator instead of trusting the solver's model.
    pub checks: Vec<IrCheck>,
    /// One satisfiability probe per loop: an *inconsistent* invariant set would
    /// discharge every obligation for free, so it is checked explicitly and a
    /// contradiction is reported as an unsound invariant, never as a proof.
    pub consistency: Vec<ConsistencyCheck>,
}

/// A loop-head satisfiability probe over the candidate invariant conjunction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsistencyCheck {
    /// Loop id.
    pub id: String,
    /// The conjunction as sent.
    pub conjunction: Expr,
    /// The query: `(assert conjunction) (check-sat)`; `sat` = consistent.
    pub query: Query,
}

/// One obligation in IR form: exactly what a model must refute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IrCheck {
    /// Matches the [`Obligation`] id.
    pub id: String,
    /// Facts available at this program point.
    pub assumptions: Vec<Expr>,
    /// The obligation itself.
    pub goal: Expr,
}

impl VcSet {
    /// The pre-check query: `(assert pre) (check-sat)`. `sat` means the
    /// preconditions are consistent; `unsat` means the specification is vacuous
    /// (`spec_inconsistent`), which is never a proof.
    pub fn precondition_query(&self, timeout_s: u64, seed: u64) -> Option<Query> {
        let pre = self.precondition.clone()?;
        Some(Query {
            decls: self.precondition_decls.clone(),
            assumptions: vec![pre],
            // `goal = false` is asserted as `(not false)` = true, so this query
            // asks exactly "is the assumption set satisfiable?".
            goal: Expr::Bool(false),
            probes: Vec::new(),
            timeout_s,
            seed,
            options: Vec::new(),
        })
    }
}

/// What the caller wants proved, after normalization.
#[derive(Debug, Clone, Default)]
pub struct Goal {
    /// Extra preconditions (from `property.assumptions`).
    pub assumptions: Vec<Expr>,
    /// Extra postconditions (from `property.expr` for postcondition/refinement).
    pub ensures: Vec<Expr>,
    /// Extra assertion injected at every completed path (for `safety`).
    pub assertion: Option<Expr>,
    /// Loop that must be shown terminating (`termination`).
    pub termination_target: Option<String>,
}

/// Elaborate one function of `program` into obligations.
///
/// Loop ids inside the function are numbered from `loop1` (offset 0); to prove
/// a whole unit with globally numbered loop ids, use [`elaborate_all`].
///
/// The returned obligations mention only declared symbols; an invariant naming
/// an unknown variable is refused here ([`ElabError::Unbound`]), which is a
/// proposer fault, not a checker failure.
pub fn elaborate(
    program: &Program,
    function_name: &str,
    goal: &Goal,
    timeout_s: u64,
    seed: u64,
) -> Result<VcSet, ElabError> {
    elaborate_offset(program, function_name, goal, timeout_s, seed, 0)
}

/// Elaborate **every** function of the unit, in source order, so no function
/// beside the entry goes unchecked.
///
/// The entry function keeps the caller's property `goal`; every other function
/// is checked against its own `requires`/`ensures`/`assert`s with
/// [`Goal::default()`], because a proof of the unit is a proof of all of it: a
/// false function beside a true entry must not report `proved`, and a single
/// failing function fails the whole unit. Loop ids stay globally numbered
/// across functions (`loop1`, `loop2`, … in source order), so the second
/// function's first loop is not also named `loop1`.
///
/// Returns one [`VcSet`] per function, in source order, paired with the
/// function's name. A unit that names a function twice is refused: its
/// obligations could not be addressed unambiguously.
pub fn elaborate_all(
    program: &Program,
    entry: &str,
    goal: &Goal,
    timeout_s: u64,
    seed: u64,
) -> Result<Vec<(String, VcSet)>, ElabError> {
    let default_goal = Goal::default();
    let mut out = Vec::new();
    for (function, is_entry, loop_offset) in unit_scan(program, entry)? {
        let g = if is_entry { goal } else { &default_goal };
        out.push((
            function.name.clone(),
            elaborate_offset(program, &function.name, g, timeout_s, seed, loop_offset)?,
        ));
    }
    Ok(out)
}

/// The per-function scan shared by [`elaborate_all`] and [`bounded_check_all`]:
/// every function in source order, whether it is the entry, and its global
/// loop-id offset ([`Program::loop_offset`]).
fn unit_scan<'a>(
    program: &'a Program,
    entry: &str,
) -> Result<Vec<(&'a Function, bool, usize)>, ElabError> {
    if !program.functions.iter().any(|f| f.name == entry) {
        return Err(ElabError::NoEntry(entry.to_string()));
    }
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for (i, f) in program.functions.iter().enumerate() {
        if !seen.insert(f.name.as_str()) {
            return Err(ElabError::Unsupported(format!(
                "the unit defines function {:?} more than once, so its obligations could not be \
                 addressed unambiguously",
                f.name
            )));
        }
        out.push((f, f.name == entry, program.loop_offset(i)));
    }
    Ok(out)
}

/// The expressiveness-ceiling scan. `old(...)`, call expressions, call
/// statements and `lemma` blocks all *parse*; this walk refuses them at
/// elaboration, naming the function and the construct, so the driver can report
/// `expressiveness_ceiling` instead of misreporting a parse fault as
/// `unsupported_feature`. `elaborate_offset` and `bounded_check_offset` both
/// call it, so every entry point (`elaborate`, `elaborate_all`,
/// `bounded_check`, `bounded_check_all`) refuses the same constructs.
fn function_ceiling(function: &Function, goal: &Goal) -> Option<String> {
    if function.is_lemma {
        return Some(format!(
            "the unit declares `{}` as a `lemma` block: the fragment has no helper lemmas, so \
             the unit is refused rather than approximated",
            function.name
        ));
    }
    let name = &function.name;
    for e in &function.requires {
        if let Some(c) = expr_ceiling(e) {
            return Some(format!(
                "function `{name}` uses {c} in its `requires`: the fragment cannot express it, \
                 so the unit is refused rather than approximated"
            ));
        }
    }
    for e in &function.ensures {
        if let Some(c) = expr_ceiling(e) {
            return Some(format!(
                "function `{name}` uses {c} in its `ensures`: the fragment cannot express it, \
                 so the unit is refused rather than approximated"
            ));
        }
    }
    if let Some(found) = block_ceiling(&function.body) {
        return Some(format!(
            "function `{name}` uses {found}: the fragment cannot express it, so the unit is \
             refused rather than approximated"
        ));
    }
    // The caller's property is elaborated into this function's obligations, so
    // it is held to the same ceiling.
    for e in goal.assumptions.iter().chain(goal.ensures.iter()) {
        if let Some(c) = expr_ceiling(e) {
            return Some(format!(
                "function `{name}` is given a property that uses {c}: the fragment cannot \
                 express it, so the unit is refused rather than approximated"
            ));
        }
    }
    if let Some(e) = &goal.assertion {
        if let Some(c) = expr_ceiling(e) {
            return Some(format!(
                "function `{name}` is given a property that uses {c}: the fragment cannot \
                 express it, so the unit is refused rather than approximated"
            ));
        }
    }
    None
}

/// One ceiling construct found in an expression, named.
fn expr_ceiling(e: &Expr) -> Option<String> {
    match e {
        Expr::Old(_) => Some("`old(...)` (a two-state value)".into()),
        Expr::Call { name, .. } => Some(format!("a call expression `{name}(...)`")),
        Expr::Int(_) | Expr::Bool(_) | Expr::Var(_) => None,
        Expr::Select(a, i) => expr_ceiling(a).or_else(|| expr_ceiling(i)),
        Expr::Store(a, i, v) => expr_ceiling(a)
            .or_else(|| expr_ceiling(i))
            .or_else(|| expr_ceiling(v)),
        Expr::Unary(_, inner) => expr_ceiling(inner),
        Expr::Binary(_, l, r) => expr_ceiling(l).or_else(|| expr_ceiling(r)),
        Expr::Quant { body, .. } => expr_ceiling(body),
    }
}

/// The same walk over statements; the returned text also says where the
/// construct was found (body, guard, loop `invariant`, loop `variant`).
fn stmt_ceiling(s: &Stmt) -> Option<String> {
    match s {
        Stmt::Call { name, .. } => Some(format!("a call statement `{name}(...)` in its body")),
        Stmt::Decl { init, .. } => init
            .as_ref()
            .and_then(expr_ceiling)
            .map(|c| format!("{c} in its body")),
        Stmt::Assign { value, .. } => expr_ceiling(value).map(|c| format!("{c} in its body")),
        Stmt::Store { index, value, .. } => expr_ceiling(index)
            .or_else(|| expr_ceiling(value))
            .map(|c| format!("{c} in its body")),
        Stmt::If { cond, then, els } => expr_ceiling(cond)
            .map(|c| format!("{c} in an `if` guard"))
            .or_else(|| block_ceiling(then))
            .or_else(|| block_ceiling(els)),
        Stmt::While { cond, spec, body } => expr_ceiling(cond)
            .map(|c| format!("{c} in a loop guard"))
            .or_else(|| {
                spec.invariants
                    .iter()
                    .find_map(|i| expr_ceiling(i).map(|c| format!("{c} in a loop `invariant`")))
            })
            .or_else(|| {
                spec.variant
                    .as_ref()
                    .and_then(expr_ceiling)
                    .map(|c| format!("{c} in a loop `variant`"))
            })
            .or_else(|| block_ceiling(body)),
        Stmt::Assert(e) | Stmt::Assume(e) => expr_ceiling(e).map(|c| format!("{c} in its body")),
        Stmt::Return(e) => e
            .as_ref()
            .and_then(expr_ceiling)
            .map(|c| format!("{c} in its body")),
    }
}

fn block_ceiling(body: &[Stmt]) -> Option<String> {
    body.iter().find_map(stmt_ceiling)
}

/// The offset-aware core of [`elaborate`]: `loop_offset` is the number of loops
/// that precede `function_name` in source order, so the function's first loop
/// is `loop{loop_offset + 1}` in the unit's global numbering.
fn elaborate_offset(
    program: &Program,
    function_name: &str,
    goal: &Goal,
    timeout_s: u64,
    seed: u64,
    loop_offset: usize,
) -> Result<VcSet, ElabError> {
    let function = program
        .functions
        .iter()
        .find(|f| f.name == function_name)
        .ok_or_else(|| ElabError::NoEntry(function_name.to_string()))?;
    // `old(...)`, calls and `lemma` blocks parse, and are refused here —
    // named, at elaboration, as an expressiveness ceiling rather than
    // misreported as a parse error (unsupported_feature).
    if let Some(refusal) = function_ceiling(function, goal) {
        return Err(ElabError::Ceiling(refusal));
    }
    if let Some(target) = &goal.termination_target {
        let ids: Vec<String> = program.loop_ids().into_iter().map(|(id, _)| id).collect();
        if !ids.iter().any(|id| id == target) {
            return Err(ElabError::NoSuchLoop(target.clone(), ids.join(", ")));
        }
    }

    let mut cx = Cx {
        sorts: BTreeMap::new(),
        obligations: Vec::new(),
        notes: Vec::new(),
        loop_seq: loop_offset,
        fresh_seq: 0usize,
        assert_seq: 0usize,
        prefix: "",
        unroll: None,
        consistency: Vec::new(),
        checks: Vec::new(),
        termination_target: goal.termination_target.clone(),
        timeout_s,
        seed,
    };

    // Preconditions: source `requires` then the caller's assumptions, in a
    // scope of parameters only.
    let mut start = State::default();
    for (name, ty) in &function.params {
        start.env.insert(name.clone(), Expr::Var(name.clone()));
        start.scope.insert(name.clone(), *ty);
        cx.sorts.insert(name.clone(), *ty);
    }
    if let Some(ret) = function.ret {
        cx.sorts.insert("result".into(), ret);
        start.scope.insert("result".into(), ret);
    }
    for r in function.requires.iter().chain(goal.assumptions.iter()) {
        let sym = r.subst(&start.env);
        cx.check_sorts(&sym, &start.scope, function_name)?;
        start.path.push(sym);
    }
    let requires = start.path.clone();

    let mut returned: Vec<State> = Vec::new();
    let mut finals = exec_block(
        &mut cx,
        &function.body,
        vec![start],
        &mut returned,
        function,
    )?;
    // A `return` ends a path exactly like falling off the end of the body.
    finals.extend(returned);

    // Safety property: injected as an assertion on every completed path.
    if let Some(a) = &goal.assertion {
        for f in &finals {
            let sym = a.subst(&f.env);
            cx.check_sorts(&sym, &f.scope, function_name)?;
            cx.assert_obligation(function_name, sym, f, "assert");
        }
    }

    // Postconditions: source `ensures` then the caller's property expression.
    let ensures: Vec<Expr> = function
        .ensures
        .iter()
        .chain(goal.ensures.iter())
        .cloned()
        .collect();
    if !goal.ensures.is_empty() && function.ret.is_none() {
        return Err(ElabError::Unsupported(format!(
            "function `{function_name}` returns no value, so a property cannot be stated with `result`"
        )));
    }
    for f in &finals {
        for (k, e) in ensures.iter().enumerate() {
            let sym = e.subst(&f.env);
            cx.check_sorts(&sym, &f.scope, function_name)?;
            cx.obligation(
                format!("{}{function_name}.post{k}", cx.prefix),
                function_name,
                "exit",
                sym,
                f,
            )?;
        }
    }

    // An unreturned value is a fragment limit, not a vacuous truth: refuse it.
    if function.ret.is_some() && !ensures.is_empty() {
        for f in &finals {
            if !f.env.contains_key("result") {
                return Err(ElabError::Unsupported(format!(
                    "a path through `{function_name}` returns no value, so its postcondition is unsound"
                )));
            }
        }
    }

    let precondition = if requires.is_empty() {
        None
    } else {
        Some(Expr::and_all(requires))
    };
    let mut precondition_decls = Decls::new();
    for (name, ty) in &function.params {
        precondition_decls.declare(name, *ty);
    }
    Ok(VcSet {
        obligations: cx.obligations,
        precondition,
        precondition_decls,
        notes: cx.notes,
        checks: cx.checks,
        consistency: cx.consistency,
    })
}

/// Symbolic state of one execution path.
#[derive(Debug, Clone, Default)]
struct State {
    /// Variable → symbolic value (substituted: only declared symbols survive).
    env: BTreeMap<String, Expr>,
    /// Sort of every symbol in scope on this path.
    scope: BTreeMap<String, Ty>,
    /// Accumulated path conditions.
    path: Vec<Expr>,
}

struct Cx {
    sorts: BTreeMap<String, Ty>,
    obligations: Vec<Obligation>,
    checks: Vec<IrCheck>,
    notes: Vec<String>,
    loop_seq: usize,
    fresh_seq: usize,
    assert_seq: usize,
    prefix: &'static str,
    unroll: Option<usize>,
    consistency: Vec<ConsistencyCheck>,
    termination_target: Option<String>,
    timeout_s: u64,
    seed: u64,
}

impl Cx {
    fn check_sorts(
        &self,
        e: &Expr,
        scope: &BTreeMap<String, Ty>,
        function: &str,
    ) -> Result<(), ElabError> {
        for v in e.free_vars() {
            if !scope.contains_key(&v) && !self.sorts.contains_key(&v) {
                return Err(ElabError::Unbound(function.to_string(), v));
            }
        }
        Ok(())
    }

    fn assert_obligation(&mut self, function: &str, goal: Expr, st: &State, kind: &str) {
        self.assert_seq += 1;
        let id = format!("{}{function}.{kind}{}", self.prefix, self.assert_seq);
        if let Err(e) = self.obligation(id, function, kind, goal, st) {
            self.notes.push(format!("assert dropped: {e}"));
        }
    }

    fn obligation(
        &mut self,
        mut id: String,
        function: &str,
        kind: &str,
        goal: Expr,
        st: &State,
    ) -> Result<(), ElabError> {
        // Several paths through one loop body produce the same logical id; the
        // transcript must still be addressable, one row per executed query.
        if self.obligations.iter().any(|o| o.id == id) {
            let base = id.clone();
            let mut n = 2usize;
            while self.obligations.iter().any(|o| o.id == id) {
                id = format!("{base}@{n}");
                n += 1;
            }
        }
        let assumptions = st.path.clone();
        let mut decls = Decls::new();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        for e in assumptions.iter().chain(std::iter::once(&goal)) {
            for v in e.free_vars() {
                if !seen.insert(v.clone()) {
                    continue;
                }
                let ty = st
                    .scope
                    .get(&v)
                    .or_else(|| self.sorts.get(&v))
                    .copied()
                    .ok_or_else(|| ElabError::Unbound(function.to_string(), v.clone()))?;
                if !decls.declare(&v, ty) {
                    return Err(ElabError::Unbound(
                        function.to_string(),
                        format!("{v} is asserted with two different sorts"),
                    ));
                }
            }
        }
        let probes: Vec<Probe> = decls
            .iter()
            .map(|(sym, _)| Probe {
                symbol: sym.clone(),
                label: sym.clone(),
            })
            .collect();
        let query = Query {
            decls,
            assumptions,
            goal,
            probes,
            timeout_s: self.timeout_s,
            seed: self.seed,
            options: Vec::new(),
        };
        self.checks.push(IrCheck {
            id: id.clone(),
            assumptions: query.assumptions.clone(),
            goal: query.goal.clone(),
        });
        self.obligations.push(Obligation {
            id,
            function: function.to_string(),
            kind: kind.to_string(),
            smt: query.text(),
            probes: query.probes,
        });
        Ok(())
    }

    fn fresh(&mut self, base: &str, tag: &str) -> String {
        self.fresh_seq += 1;
        format!("{base}__{tag}{}", self.fresh_seq)
    }
}

/// Execute a block over a set of incoming states. Returns the states that reach
/// the end of the block (they continue in the enclosing block); `Return`
/// statements are the only paths that land in `returned`.
fn exec_block(
    cx: &mut Cx,
    stmts: &[Stmt],
    mut states: Vec<State>,
    returned: &mut Vec<State>,
    function: &Function,
) -> Result<Vec<State>, ElabError> {
    for s in stmts {
        let mut next = Vec::new();
        for st in states {
            exec_stmt(cx, s, st, &mut next, returned, function)?;
        }
        states = next;
        if states.is_empty() {
            // Every path returned; the rest of the block is unreachable.
            return Ok(Vec::new());
        }
    }
    Ok(states)
}

fn exec_stmt(
    cx: &mut Cx,
    s: &Stmt,
    mut st: State,
    next: &mut Vec<State>,
    finals: &mut Vec<State>,
    function: &Function,
) -> Result<(), ElabError> {
    match s {
        Stmt::Decl { name, ty, init } => {
            st.scope.insert(name.clone(), *ty);
            cx.sorts.insert(name.clone(), *ty);
            let value = match init {
                Some(e) => {
                    let sym = e.subst(&st.env);
                    cx.check_sorts(&sym, &st.scope, &function.name)?;
                    sym
                }
                None => {
                    let fresh = cx.fresh(name, "d");
                    st.scope.insert(fresh.clone(), *ty);
                    Expr::Var(fresh)
                }
            };
            st.env.insert(name.clone(), value);
            next.push(st);
        }
        Stmt::Assign { name, value } => {
            let sym = value.subst(&st.env);
            cx.check_sorts(&sym, &st.scope, &function.name)?;
            if !st.scope.contains_key(name) && !cx.sorts.contains_key(name) {
                return Err(ElabError::Unbound(function.name.clone(), name.clone()));
            }
            st.env.insert(name.clone(), sym);
            next.push(st);
        }
        Stmt::Store {
            array,
            index,
            value,
        } => {
            let idx = index.subst(&st.env);
            let val = value.subst(&st.env);
            cx.check_sorts(&idx, &st.scope, &function.name)?;
            cx.check_sorts(&val, &st.scope, &function.name)?;
            if !st.scope.contains_key(array) && !cx.sorts.contains_key(array) {
                return Err(ElabError::Unbound(function.name.clone(), array.clone()));
            }
            let cur = st
                .env
                .get(array)
                .cloned()
                .unwrap_or_else(|| Expr::Var(array.clone()));
            st.env.insert(
                array.clone(),
                Expr::Store(Box::new(cur), Box::new(idx), Box::new(val)),
            );
            next.push(st);
        }
        Stmt::If { cond, then, els } => {
            let c = cond.subst(&st.env);
            cx.check_sorts(&c, &st.scope, &function.name)?;
            let mut t = st.clone();
            t.path.push(c.clone());
            next.extend(exec_block(cx, then, vec![t], finals, function)?);
            let mut e = st;
            e.path.push(c.negate());
            next.extend(exec_block(cx, els, vec![e], finals, function)?);
        }
        Stmt::While { cond, spec, body } => {
            cx.loop_seq += 1;
            let id = format!("loop{}", cx.loop_seq);
            match cx.unroll {
                Some(depth) => {
                    exec_while_unroll(cx, cond, body, st, depth, finals, function, next)?
                }
                None => exec_while(cx, &id, cond, spec, body, st, finals, function, next)?,
            }
        }
        Stmt::Assert(e) => {
            let sym = e.subst(&st.env);
            cx.check_sorts(&sym, &st.scope, &function.name)?;
            cx.assert_obligation(&function.name, sym.clone(), &st, "assert");
            // An assert also licenses the fact afterwards.
            st.path.push(sym);
            next.push(st);
        }
        Stmt::Assume(e) => {
            let sym = e.subst(&st.env);
            cx.check_sorts(&sym, &st.scope, &function.name)?;
            st.path.push(sym);
            next.push(st);
        }
        Stmt::Return(e) => {
            if let Some(ty) = function.ret {
                cx.sorts.entry("result".into()).or_insert(ty);
                st.scope.entry("result".into()).or_insert(ty);
            }
            if let Some(e) = e {
                let sym = e.subst(&st.env);
                cx.check_sorts(&sym, &st.scope, &function.name)?;
                st.env.insert("result".into(), sym);
            }
            finals.push(st);
        }
        Stmt::Call { name, .. } => {
            // Unreachable in practice — `function_ceiling` refuses the function
            // before execution — but the executor must stay total and must
            // never guess a semantics for a call.
            return Err(ElabError::Ceiling(format!(
                "function `{}` uses a call statement `{name}(...)` in its body: the fragment \
                 proves one function at a time, so the unit is refused rather than approximated",
                function.name
            )));
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn exec_while(
    cx: &mut Cx,
    id: &str,
    cond: &Expr,
    spec: &crate::ir::LoopSpec,
    body: &[Stmt],
    st: State,
    finals: &mut Vec<State>,
    function: &Function,
    next: &mut Vec<State>,
) -> Result<(), ElabError> {
    // 1. The invariant must hold on arrival (per incoming path: the caller's
    //    path conditions are already in `st.path`).
    for (k, inv) in spec.invariants.iter().enumerate() {
        let sym = inv.subst(&st.env);
        cx.check_sorts(&sym, &st.scope, &function.name)?;
        cx.obligation(format!("{id}.init{k}"), &function.name, "init", sym, &st)?;
    }

    // 2. Havoc exactly what the body writes; everything else is loop-invariant.
    let written = written_names(body);
    let mut havoced = st.clone();
    for name in &written {
        let ty = st.scope.get(name).or_else(|| cx.sorts.get(name)).copied();
        let Some(ty) = ty else {
            cx.notes.push(format!(
                "{id}: the body writes {name:?}, which is not declared in scope"
            ));
            continue;
        };
        let fresh = cx.fresh(name, id);
        havoced.env.insert(name.clone(), Expr::Var(fresh.clone()));
        havoced.scope.insert(fresh.clone(), ty);
    }

    // 3. Assume the invariant over the havoced state.
    for inv in &spec.invariants {
        let sym = inv.subst(&havoced.env);
        cx.check_sorts(&sym, &havoced.scope, &function.name)?;
        havoced.path.push(sym);
    }

    // 3b. The assumed invariant set must be satisfiable at the loop head. If it
    //     is not, every obligation below would discharge for free; the driver
    //     treats a contradiction as an unsound invariant.
    if !spec.invariants.is_empty() {
        let conj = Expr::and_all(
            spec.invariants
                .iter()
                .map(|i| i.subst(&havoced.env))
                .collect(),
        );
        // The conjunction is in havoced symbols; declare them as well as any
        // pre-loop symbol it still mentions.
        let mut decls = crate::smt::Decls::new();
        for v in conj.free_vars() {
            if let Some(ty) = havoced
                .scope
                .get(&v)
                .or_else(|| st.scope.get(&v))
                .or_else(|| cx.sorts.get(&v))
                .copied()
            {
                decls.declare(&v, ty);
            }
        }
        cx.consistency.push(ConsistencyCheck {
            id: format!("{id}.consistent"),
            conjunction: conj.clone(),
            query: Query {
                decls,
                assumptions: vec![conj],
                goal: Expr::Bool(false),
                probes: Vec::new(),
                timeout_s: cx.timeout_s,
                seed: cx.seed,
                options: Vec::new(),
            },
        });
    }

    // 4. Preservation: assume the guard, run the body, require the invariant on
    //    every path out of it.
    let guard = cond.subst(&havoced.env);
    cx.check_sorts(&guard, &havoced.scope, &function.name)?;
    let mut body_state = havoced.clone();
    body_state.path.push(guard.clone());
    let mut body_returned: Vec<State> = Vec::new();
    let body_finals = exec_block(cx, body, vec![body_state], &mut body_returned, function)?;
    // A `return` inside a loop body leaves the function; those paths are already
    // in `finals`, and the loop's preservation obligation does not apply to them.
    finals.extend(body_returned);
    for f in &body_finals {
        for (k, inv) in spec.invariants.iter().enumerate() {
            let sym = inv.subst(&f.env);
            cx.check_sorts(&sym, &f.scope, &function.name)?;
            cx.obligation(
                format!("{id}.preserve{k}"),
                &function.name,
                "preserve",
                sym,
                f,
            )?;
        }
    }

    // 5. Termination: a non-negative variant that strictly decreases on every
    //    path through the guarded body.
    if let Some(variant) = &spec.variant {
        let v_pre = variant.subst(&havoced.env);
        cx.check_sorts(&v_pre, &havoced.scope, &function.name)?;
        let mut head = havoced.clone();
        head.path.push(guard.clone());
        cx.obligation(
            format!("{id}.variant_nonneg"),
            &function.name,
            "variant",
            Expr::Binary(BinOp::Ge, Box::new(v_pre.clone()), Box::new(Expr::Int(0))),
            &head,
        )?;
        for f in &body_finals {
            let v_post = variant.subst(&f.env);
            cx.check_sorts(&v_post, &f.scope, &function.name)?;
            cx.obligation(
                format!("{id}.variant_decrease"),
                &function.name,
                "variant",
                Expr::Binary(BinOp::Lt, Box::new(v_post), Box::new(v_pre.clone())),
                f,
            )?;
        }
    } else if cx.termination_target.as_deref() == Some(id) {
        cx.notes.push(format!(
            "{id}: termination was requested but the loop carries no `variant` annotation"
        ));
    }

    // 6. Exit path: the invariant holds and the guard is false.
    let mut exit_state = havoced;
    exit_state.path.push(guard.negate());
    next.push(exit_state);
    Ok(())
}

/// Bounded symbolic execution (a sound bounded model check).
///
/// Loops are inlined `depth` times: a path that exits after exactly `d <= depth`
/// iterations is a *real* execution, so a satisfiable obligation here is a
/// genuine counterexample — not an over-approximation artifact. A path that
/// would iterate more than `depth` times is dropped rather than approximated.
///
/// This is what separates "the invariant is too weak" from "the property is
/// false of the code" (failure classes `insufficient_invariant` vs `spec_bug`).
pub fn bounded_check(
    program: &Program,
    function_name: &str,
    goal: &Goal,
    depth: usize,
    timeout_s: u64,
    seed: u64,
) -> Result<VcSet, ElabError> {
    bounded_check_offset(program, function_name, goal, depth, timeout_s, seed, 0)
}

/// The bounded model check over **every** function in the unit: the entry
/// with the caller's goal, every other function against its own contract, loop
/// ids globally numbered. A refuted obligation of a non-entry function is
/// upgraded with a real witnessed execution exactly like the entry's.
pub fn bounded_check_all(
    program: &Program,
    entry: &str,
    goal: &Goal,
    depth: usize,
    timeout_s: u64,
    seed: u64,
) -> Result<Vec<(String, VcSet)>, ElabError> {
    let default_goal = Goal::default();
    let mut out = Vec::new();
    for (function, is_entry, loop_offset) in unit_scan(program, entry)? {
        let g = if is_entry { goal } else { &default_goal };
        out.push((
            function.name.clone(),
            bounded_check_offset(
                program,
                &function.name,
                g,
                depth,
                timeout_s,
                seed,
                loop_offset,
            )?,
        ));
    }
    Ok(out)
}

/// The offset-aware core of [`bounded_check`], mirroring [`elaborate_offset`]:
/// loop ids continue the unit's global numbering instead of restarting at
/// `loop1` for each function.
#[allow(clippy::too_many_arguments)]
fn bounded_check_offset(
    program: &Program,
    function_name: &str,
    goal: &Goal,
    depth: usize,
    timeout_s: u64,
    seed: u64,
    loop_offset: usize,
) -> Result<VcSet, ElabError> {
    let function = program
        .functions
        .iter()
        .find(|f| f.name == function_name)
        .ok_or_else(|| ElabError::NoEntry(function_name.to_string()))?;
    // The bounded check refuses the same constructs as `elaborate_offset`,
    // with the same message, so the ceiling class is reachable on this path too.
    if let Some(refusal) = function_ceiling(function, goal) {
        return Err(ElabError::Ceiling(refusal));
    }
    let mut cx = Cx {
        sorts: BTreeMap::new(),
        obligations: Vec::new(),
        notes: Vec::new(),
        loop_seq: loop_offset,
        fresh_seq: 0usize,
        assert_seq: 0usize,
        prefix: "bmc.",
        unroll: Some(depth),
        consistency: Vec::new(),
        checks: Vec::new(),
        termination_target: None,
        timeout_s,
        seed,
    };
    let mut start = State::default();
    for (name, ty) in &function.params {
        start.env.insert(name.clone(), Expr::Var(name.clone()));
        start.scope.insert(name.clone(), *ty);
        cx.sorts.insert(name.clone(), *ty);
    }
    if let Some(ret) = function.ret {
        cx.sorts.insert("result".into(), ret);
        start.scope.insert("result".into(), ret);
    }
    for r in function.requires.iter().chain(goal.assumptions.iter()) {
        let sym = r.subst(&start.env);
        cx.check_sorts(&sym, &start.scope, function_name)?;
        start.path.push(sym);
    }
    let requires = start.path.clone();
    let mut returned: Vec<State> = Vec::new();
    let finals = exec_block(
        &mut cx,
        &function.body,
        vec![start],
        &mut returned,
        function,
    )?;
    let mut finals = finals;
    finals.extend(returned);

    if let Some(a) = &goal.assertion {
        for f in &finals {
            let sym = a.subst(&f.env);
            cx.check_sorts(&sym, &f.scope, function_name)?;
            cx.assert_obligation(function_name, sym, f, "bmc");
        }
    }
    for f in &finals {
        for (k, e) in function
            .ensures
            .iter()
            .chain(goal.ensures.iter())
            .enumerate()
        {
            let sym = e.subst(&f.env);
            cx.check_sorts(&sym, &f.scope, function_name)?;
            cx.obligation(
                format!("bmc.{function_name}.post{k}"),
                function_name,
                "bmc",
                sym,
                f,
            )?;
        }
    }
    Ok(VcSet {
        obligations: cx.obligations,
        precondition: if requires.is_empty() {
            None
        } else {
            Some(Expr::and_all(requires))
        },
        precondition_decls: {
            let mut d = Decls::new();
            for (name, ty) in &function.params {
                d.declare(name, *ty);
            }
            d
        },
        notes: cx.notes,
        checks: cx.checks,
        consistency: cx.consistency,
    })
}

#[allow(clippy::too_many_arguments)]
fn exec_while_unroll(
    cx: &mut Cx,
    cond: &Expr,
    body: &[Stmt],
    st: State,
    depth: usize,
    finals: &mut Vec<State>,
    function: &Function,
    next: &mut Vec<State>,
) -> Result<(), ElabError> {
    let guard = cond.subst(&st.env);
    cx.check_sorts(&guard, &st.scope, &function.name)?;
    if depth == 0 {
        let mut exit = st;
        exit.path.push(guard.negate());
        next.push(exit);
        return Ok(());
    }
    let mut entering = st;
    entering.path.push(guard);
    let mut body_returned: Vec<State> = Vec::new();
    let body_finals = exec_block(cx, body, vec![entering], &mut body_returned, function)?;
    finals.extend(body_returned);
    for f in body_finals {
        exec_while_unroll(cx, cond, body, f, depth - 1, finals, function, next)?;
    }
    Ok(())
}

/// Parse an annotation expression for a loop, mapping a syntax error to the
/// elaborator's fault vocabulary.
pub fn parse_annotation(text: &str) -> Result<Expr, String> {
    parse_expr(text).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elaborate_all_proves_every_function_with_globally_numbered_loops() {
        let src = r#"
fn first(n: int) -> int
  requires n >= 0
  ensures result == n
{
  var i: int = 0;
  while (i < n)
    invariant i >= 0 && i <= n
  {
    i = i + 1;
  }
  return i;
}

fn second(n: int) -> int
  requires n >= 0
  ensures result == n
{
  var j: int = 0;
  while (j < n)
    invariant j >= 0 && j <= n
  {
    j = j + 1;
  }
  return j;
}
"#;
        let program = Program::parse(src).unwrap();
        // The entry is `second`; `first` is elaborated against its own contract.
        let goal = Goal {
            ensures: vec![parse_expr("result >= 0").unwrap()],
            ..Goal::default()
        };
        let sets = elaborate_all(&program, "second", &goal, 5, 0).unwrap();
        assert_eq!(sets.len(), 2);
        assert_eq!(sets[0].0, "first");
        assert_eq!(sets[1].0, "second");
        // The caller's extra postcondition lands on the entry only.
        let first_posts = sets[0]
            .1
            .obligations
            .iter()
            .filter(|o| o.kind == "exit")
            .count();
        let second_posts = sets[1]
            .1
            .obligations
            .iter()
            .filter(|o| o.kind == "exit")
            .count();
        assert_eq!(first_posts, 1, "own `ensures` only");
        assert_eq!(second_posts, 2, "own `ensures` plus the caller's goal");
        // Loop ids are globally numbered: the entry's loop is `loop2`, never a
        // second `loop1`.
        let second_ids: Vec<&str> = sets[1]
            .1
            .obligations
            .iter()
            .map(|o| o.id.as_str())
            .collect();
        assert!(second_ids.contains(&"loop2.init0"), "{second_ids:?}");
        assert!(
            second_ids.iter().all(|id| !id.starts_with("loop1.")),
            "{second_ids:?}"
        );
    }

    #[test]
    fn elaborate_all_refuses_a_duplicate_function_name_and_a_missing_entry() {
        let src = r#"fn f() -> int
  ensures result == 1
{
  return 1;
}

fn f() -> int
  ensures result == 1
{
  return 1;
}
"#;
        let program = Program::parse(src).unwrap();
        assert!(matches!(
            elaborate_all(&program, "f", &Goal::default(), 5, 0),
            Err(ElabError::Unsupported(_))
        ));
        let single =
            Program::parse("fn g() -> int\n  ensures result == 1\n{\n  return 1;\n}\n").unwrap();
        let err = elaborate_all(&single, "missing", &Goal::default(), 5, 0).unwrap_err();
        assert_eq!(err, ElabError::NoEntry("missing".to_string()));
    }

    #[test]
    fn elaboration_and_the_bounded_check_refuse_ceiling_constructs() {
        // `old(...)`, calls and lemma blocks parse and are refused at
        // elaboration as an expressiveness ceiling, naming the function and the
        // construct — never as a parse error.
        let old_post =
            Program::parse("fn f(x: int) -> int\n  ensures result == old(x)\n{\n  return x;\n}\n")
                .unwrap();
        let err = elaborate(&old_post, "f", &Goal::default(), 5, 0).unwrap_err();
        assert!(matches!(err, ElabError::Ceiling(_)), "{err}");
        assert!(err.to_string().contains("`f`"), "{err}");
        assert!(err.to_string().contains("old"), "{err}");

        let calls = Program::parse("fn f() { helper(); }\nfn helper() { }\n").unwrap();
        let err = elaborate_all(&calls, "helper", &Goal::default(), 5, 0).unwrap_err();
        assert!(matches!(err, ElabError::Ceiling(_)), "{err}");
        assert!(err.to_string().contains("`f`"), "{err}");
        assert!(err.to_string().contains("helper"), "{err}");

        let lemma_unit = Program::parse(
            "lemma helper() { }\nfn f() -> int\n  ensures result == 1\n{\n  return 1;\n}\n",
        )
        .unwrap();
        let err = bounded_check_all(&lemma_unit, "f", &Goal::default(), 2, 5, 0).unwrap_err();
        assert!(matches!(err, ElabError::Ceiling(_)), "{err}");
        assert!(err.to_string().contains("lemma"), "{err}");
        assert!(err.to_string().contains("helper"), "{err}");
    }
}
