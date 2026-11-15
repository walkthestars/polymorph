//! `ais-core` — the front end: a small imperative language with integer
//! scalars, integer arrays, `while` loops carrying candidate invariants and an
//! optional termination variant, `if`, `assert`/`assume`, and function
//! contracts.
//!
//! The grammar is deliberately tiny. Everything the elaboration step can accept
//! is written here, and everything it cannot accept is refused at parse time
//! with a named fault rather than approximated (the primitive's honesty rule).
//! One exception keeps that rule honest: `old(...)`, calls and `lemma`
//! blocks parse into the IR and are refused at *elaboration* as an
//! expressiveness ceiling — named, never approximated, never misreported as a
//! parse error.
//!
//! ```text
//! fn sum(n: int) -> int
//!   requires n >= 0
//!   ensures result == n * (n + 1) / 2
//! {
//!   var r: int = 0;
//!   var i: int = 0;
//!   while (i < n)
//!     invariant i >= 0 && i <= n && r == i * (i + 1) / 2
//!     variant n - i
//!   {
//!     i = i + 1;
//!     r = r + i;
//!   }
//!   return r;
//! }
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// Scalar/aggregate types of the core language. No floats, no references, no
/// heap: those are outside the fragment and are rejected by the parser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ty {
    /// Unbounded mathematical integer (SMT `Int`).
    Int,
    /// Boolean.
    Bool,
    /// Function from `Int` to `Int` (SMT `(Array Int Int)`).
    Array,
}

impl Ty {
    /// SMT-LIB2 sort name.
    pub fn sort(self) -> &'static str {
        match self {
            Ty::Int => "Int",
            Ty::Bool => "Bool",
            Ty::Array => "(Array Int Int)",
        }
    }

    fn parse(text: &str) -> Option<Ty> {
        match text {
            "int" => Some(Ty::Int),
            "bool" => Some(Ty::Bool),
            "array" => Some(Ty::Array),
            _ => None,
        }
    }
}

/// Binary operators, in the surface syntax.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    /// `+`
    Add,
    /// `-`
    Sub,
    /// `*`
    Mul,
    /// `/` — SMT `div` (Euclidean, matches `div`/`mod` in SMT-LIB2, not Rust's
    /// truncating `%`; the IR documents the semantics it proves).
    Div,
    /// `%` — SMT `mod`.
    Mod,
    /// `==`
    Eq,
    /// `!=`
    Ne,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
    /// `&&`
    And,
    /// `||`
    Or,
    /// `=>`
    Imp,
}

impl BinOp {
    /// SMT-LIB2 operator for the theory of integer arithmetic, arrays and
    /// booleans.
    pub fn smt(self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "div",
            BinOp::Mod => "mod",
            BinOp::Eq => "=",
            BinOp::Ne => "distinct",
            BinOp::Lt => "<",
            BinOp::Le => "<=",
            BinOp::Gt => ">",
            BinOp::Ge => ">=",
            BinOp::And => "and",
            BinOp::Or => "or",
            BinOp::Imp => "=>",
        }
    }

    fn precedence(self) -> u8 {
        match self {
            BinOp::Imp => 1,
            BinOp::Or => 2,
            BinOp::And => 3,
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => 4,
            BinOp::Add | BinOp::Sub => 5,
            BinOp::Mul | BinOp::Div | BinOp::Mod => 6,
        }
    }
}

/// Unary operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    /// `-`
    Neg,
    /// `!`
    Not,
}

impl UnOp {
    /// SMT-LIB2 operator.
    pub fn smt(self) -> &'static str {
        match self {
            UnOp::Neg => "-",
            UnOp::Not => "not",
        }
    }
}

/// Quantifier kind. Quantified formulas are emitted verbatim; a solver may
/// answer `unknown`, which the analyzer reports as `solver_incompleteness` — it
/// is never turned into a proof.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuantKind {
    /// `forall`
    Forall,
    /// `exists`
    Exists,
}

/// One expression of the core language.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    /// Integer literal.
    Int(i64),
    /// Boolean literal.
    Bool(bool),
    /// Variable reference (a parameter, a declared local, `result`, or a
    /// quantifier-bound name).
    Var(String),
    /// Array read `a[i]`.
    Select(Box<Expr>, Box<Expr>),
    /// Array write `a[i := v]`. Surface syntax only writes through
    /// `a[i] = v;`; the form exists so symbolic execution can thread state.
    Store(Box<Expr>, Box<Expr>, Box<Expr>),
    /// Unary application.
    Unary(UnOp, Box<Expr>),
    /// Binary application.
    Binary(BinOp, Box<Expr>, Box<Expr>),
    /// Quantified formula over one integer variable.
    Quant {
        /// `forall` or `exists`.
        kind: QuantKind,
        /// Bound variable name.
        var: String,
        /// Body, in which `var` is bound.
        body: Box<Expr>,
    },
    /// `old(e)` — the pre-state value of `e`. Parsed and then refused at
    /// elaboration as an expressiveness ceiling: the fragment is single-state,
    /// so a two-state postcondition cannot be expressed, only named.
    Old(Box<Expr>),
    /// Call expression `f(a, b)`. Parsed and then refused at elaboration:
    /// the fragment proves one function at a time and has no uninterpreted
    /// helper functions to give a call a semantics.
    Call {
        /// Callee name.
        name: String,
        /// Arguments, in source order.
        args: Vec<Expr>,
    },
}

impl Expr {
    /// Substitute every free occurrence of the keys in `env` with the mapped
    /// expression. Quantifier-bound names are never substituted.
    pub fn subst(&self, env: &BTreeMap<String, Expr>) -> Expr {
        match self {
            Expr::Int(_) | Expr::Bool(_) => self.clone(),
            Expr::Var(name) => env.get(name).cloned().unwrap_or_else(|| self.clone()),
            Expr::Select(a, i) => Expr::Select(Box::new(a.subst(env)), Box::new(i.subst(env))),
            Expr::Store(a, i, v) => Expr::Store(
                Box::new(a.subst(env)),
                Box::new(i.subst(env)),
                Box::new(v.subst(env)),
            ),
            Expr::Unary(op, e) => Expr::Unary(*op, Box::new(e.subst(env))),
            Expr::Binary(op, l, r) => {
                Expr::Binary(*op, Box::new(l.subst(env)), Box::new(r.subst(env)))
            }
            Expr::Quant { kind, var, body } => {
                let mut inner = env.clone();
                inner.remove(var);
                Expr::Quant {
                    kind: *kind,
                    var: var.clone(),
                    body: Box::new(body.subst(&inner)),
                }
            }
            Expr::Old(e) => Expr::Old(Box::new(e.subst(env))),
            Expr::Call { name, args } => Expr::Call {
                name: name.clone(),
                args: args.iter().map(|a| a.subst(env)).collect(),
            },
        }
    }

    /// Every free variable name in the expression, sorted and deduplicated.
    pub fn free_vars(&self) -> Vec<String> {
        let mut out = BTreeSet::new();
        self.collect_free(&mut out, &BTreeSet::new());
        out.into_iter().collect()
    }

    fn collect_free(&self, out: &mut BTreeSet<String>, bound: &BTreeSet<String>) {
        match self {
            Expr::Int(_) | Expr::Bool(_) => {}
            Expr::Var(name) => {
                if !bound.contains(name) {
                    out.insert(name.clone());
                }
            }
            Expr::Select(a, i) => {
                a.collect_free(out, bound);
                i.collect_free(out, bound);
            }
            Expr::Store(a, i, v) => {
                a.collect_free(out, bound);
                i.collect_free(out, bound);
                v.collect_free(out, bound);
            }
            Expr::Unary(_, e) => e.collect_free(out, bound),
            Expr::Binary(_, l, r) => {
                l.collect_free(out, bound);
                r.collect_free(out, bound);
            }
            Expr::Quant { var, body, .. } => {
                let mut inner = bound.clone();
                inner.insert(var.clone());
                body.collect_free(out, &inner);
            }
            Expr::Old(e) => e.collect_free(out, bound),
            Expr::Call { args, .. } => {
                for a in args {
                    a.collect_free(out, bound);
                }
            }
        }
    }

    /// True when the expression contains no quantifiers. Quantifier-free
    /// fragments are the ones a solver can always decide (within its timeout).
    pub fn is_quantifier_free(&self) -> bool {
        match self {
            Expr::Int(_) | Expr::Bool(_) | Expr::Var(_) => true,
            Expr::Select(a, i) => a.is_quantifier_free() && i.is_quantifier_free(),
            Expr::Store(a, i, v) => {
                a.is_quantifier_free() && i.is_quantifier_free() && v.is_quantifier_free()
            }
            Expr::Unary(_, e) => e.is_quantifier_free(),
            Expr::Binary(_, l, r) => l.is_quantifier_free() && r.is_quantifier_free(),
            Expr::Quant { .. } => false,
            Expr::Old(e) => e.is_quantifier_free(),
            Expr::Call { args, .. } => args.iter().all(|a| a.is_quantifier_free()),
        }
    }

    /// The conjunction of a slice, or `true` when the slice is empty.
    pub fn and_all(mut parts: Vec<Expr>) -> Expr {
        if parts.is_empty() {
            return Expr::Bool(true);
        }
        let mut acc = parts.remove(0);
        for p in parts {
            acc = Expr::Binary(BinOp::And, Box::new(acc), Box::new(p));
        }
        acc
    }

    /// The disjunction of a slice, or `false` when the slice is empty.
    pub fn or_all(mut parts: Vec<Expr>) -> Expr {
        if parts.is_empty() {
            return Expr::Bool(false);
        }
        let mut acc = parts.remove(0);
        for p in parts {
            acc = Expr::Binary(BinOp::Or, Box::new(acc), Box::new(p));
        }
        acc
    }

    /// Negation, with double-negation and literal folding so obligations stay
    /// readable in the transcript.
    pub fn negate(self) -> Expr {
        match self {
            Expr::Bool(b) => Expr::Bool(!b),
            Expr::Unary(UnOp::Not, e) => *e,
            other => Expr::Unary(UnOp::Not, Box::new(other)),
        }
    }
}

impl fmt::Display for Expr {
    /// Renders the surface syntax back out. Deterministic: always fully
    /// parenthesized, so two structurally equal expressions render identically
    /// (the certificate digests this rendering).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Expr::Int(n) => write!(f, "{n}"),
            Expr::Bool(b) => write!(f, "{b}"),
            Expr::Var(v) => write!(f, "{v}"),
            Expr::Select(a, i) => write!(f, "{a}[{i}]"),
            Expr::Store(a, i, v) => write!(f, "{a}[{i} := {v}]"),
            Expr::Unary(op, e) => match op {
                UnOp::Neg => write!(f, "(-{e})"),
                UnOp::Not => write!(f, "(!{e})"),
            },
            Expr::Binary(op, l, r) => {
                let sym = match op {
                    BinOp::Add => "+",
                    BinOp::Sub => "-",
                    BinOp::Mul => "*",
                    BinOp::Div => "/",
                    BinOp::Mod => "%",
                    BinOp::Eq => "==",
                    BinOp::Ne => "!=",
                    BinOp::Lt => "<",
                    BinOp::Le => "<=",
                    BinOp::Gt => ">",
                    BinOp::Ge => ">=",
                    BinOp::And => "&&",
                    BinOp::Or => "||",
                    BinOp::Imp => "=>",
                };
                write!(f, "({l} {sym} {r})")
            }
            Expr::Quant { kind, var, body } => {
                let kw = match kind {
                    QuantKind::Forall => "forall",
                    QuantKind::Exists => "exists",
                };
                write!(f, "({kw} {var}: int :: {body})")
            }
            // `old(...)` and calls render in their surface shape so the
            // rendering round-trips through the parser (annotation text is
            // re-parsed by Houdini and by the certificate re-check).
            Expr::Old(e) => write!(f, "old({e})"),
            Expr::Call { name, args } => {
                let rendered: Vec<String> = args.iter().map(|a| a.to_string()).collect();
                write!(f, "{name}({})", rendered.join(", "))
            }
        }
    }
}

/// What a loop carries for the checker: candidate invariants and an optional
/// termination variant.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoopSpec {
    /// Invariants, in the order the proposer supplied them.
    pub invariants: Vec<Expr>,
    /// Termination variant, if one was supplied.
    pub variant: Option<Expr>,
}

/// One statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stmt {
    /// `var x: ty = e;`
    Decl {
        /// Declared name.
        name: String,
        /// Declared type.
        ty: Ty,
        /// Optional initializer; a declaration without one is `havoc`ed (which
        /// the checker models as an unconstrained fresh constant).
        init: Option<Expr>,
    },
    /// `x = e;`
    Assign {
        /// Target local.
        name: String,
        /// Right-hand side.
        value: Expr,
    },
    /// `a[i] = e;`
    Store {
        /// Target array.
        array: String,
        /// Index expression.
        index: Expr,
        /// Stored value.
        value: Expr,
    },
    /// `if (c) { … } else { … }`
    If {
        /// Guard.
        cond: Expr,
        /// Then-branch.
        then: Vec<Stmt>,
        /// Else-branch (empty when absent).
        els: Vec<Stmt>,
    },
    /// `while (c) invariant … { … }`
    While {
        /// Guard.
        cond: Expr,
        /// Candidate invariants and variant.
        spec: LoopSpec,
        /// Body.
        body: Vec<Stmt>,
    },
    /// `assert e;`
    Assert(Expr),
    /// `assume e;`
    Assume(Expr),
    /// `return e;` — `Some` for a value-returning function.
    Return(Option<Expr>),
    /// `f(a, b);` — a call statement. Parsed and then refused at
    /// elaboration as an expressiveness ceiling: the fragment proves one
    /// function at a time, so a call has no semantics here to execute.
    Call {
        /// Callee name.
        name: String,
        /// Arguments, in source order.
        args: Vec<Expr>,
    },
}

/// One function of the unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Function {
    /// Function name.
    pub name: String,
    /// Parameters with their types.
    pub params: Vec<(String, Ty)>,
    /// Return type (`Ty::Int`/`Ty::Bool` when a value is returned).
    pub ret: Option<Ty>,
    /// Preconditions.
    pub requires: Vec<Expr>,
    /// Postconditions, in which `result` names the return value.
    pub ensures: Vec<Expr>,
    /// Body.
    pub body: Vec<Stmt>,
    /// True when the unit declares this as a `lemma` block instead of `fn`:
    /// a helper proof, which the fragment has no logic for, so
    /// elaboration refuses it as an expressiveness ceiling.
    pub is_lemma: bool,
}

/// A parsed unit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Program {
    /// Functions in source order.
    pub functions: Vec<Function>,
}

impl Program {
    /// Parse a unit. Any failure names the line and what was expected; the
    /// driver turns a failure here into `unsupported_feature` (the source is
    /// the caller's, not the proposer's).
    pub fn parse(text: &str) -> Result<Program, ParseError> {
        let tokens = lex(text)?;
        let mut p = Parser {
            tokens,
            pos: 0,
            depth: 0,
        };
        p.program()
    }

    /// The entry function: the named one, or the last in source order.
    pub fn entry(&self, name: Option<&str>) -> Option<&Function> {
        match name {
            Some(n) => self.functions.iter().find(|f| f.name == n),
            None => self.functions.last(),
        }
    }

    /// Every loop id in source order (`loop1`, `loop2`, …), paired with its
    /// enclosing function name. Ids are globally numbered across the whole
    /// unit, matching [`Self::set_invariants`] and the elaborator, so the
    /// second function's first loop is not also named `loop1`.
    pub fn loop_ids(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut n = 0usize;
        for f in &self.functions {
            collect_loops(&f.body, &f.name, &mut n, &mut out);
        }
        out
    }

    /// Total number of `while` loops in the unit, nested loops included. Loop
    /// ids are globally numbered in source order, so this is also the highest
    /// loop id an annotation can address.
    pub fn count_loops(&self) -> usize {
        self.functions.iter().map(|f| count_loops(&f.body)).sum()
    }

    /// The global loop-id offset for the function at index `i`: how many
    /// `while` loops the functions before it contain. Elaboration numbers loop
    /// ids across the whole unit (`loop1`, `loop2`, … in source order), so
    /// function `i`'s first loop is `loop{loop_offset + 1}`, never a second
    /// `loop1`.
    pub fn loop_offset(&self, i: usize) -> usize {
        self.functions
            .iter()
            .take(i)
            .map(|f| count_loops(&f.body))
            .sum()
    }

    /// Replace the invariants of the loop with `target` id.
    pub fn set_invariants(&mut self, target: &str, invariants: Vec<Expr>) -> bool {
        let mut n = 0usize;
        for f in &mut self.functions {
            if set_loop_invariants(&mut f.body, target, &mut n, invariants.clone()) {
                return true;
            }
        }
        false
    }

    /// Replace the variant of the loop with `target` id.
    pub fn set_variant(&mut self, target: &str, variant: Option<Expr>) -> bool {
        let mut n = 0usize;
        for f in &mut self.functions {
            if set_loop_variant(&mut f.body, target, &mut n, variant.clone()) {
                return true;
            }
        }
        false
    }
}

fn collect_loops(body: &[Stmt], func: &str, n: &mut usize, out: &mut Vec<(String, String)>) {
    for s in body {
        match s {
            Stmt::While { body, .. } => {
                *n += 1;
                out.push((format!("loop{n}"), func.to_string()));
                collect_loops(body, func, n, out);
            }
            Stmt::If { then, els, .. } => {
                collect_loops(then, func, n, out);
                collect_loops(els, func, n, out);
            }
            _ => {}
        }
    }
}

/// Number of `while` statements in a body, nested loops included. The backing
/// walk for [`Program::count_loops`] and [`Program::loop_offset`].
fn count_loops(body: &[Stmt]) -> usize {
    body.iter()
        .map(|s| match s {
            Stmt::While { body, .. } => 1 + count_loops(body),
            Stmt::If { then, els, .. } => count_loops(then) + count_loops(els),
            _ => 0,
        })
        .sum()
}

fn set_loop_invariants(body: &mut [Stmt], target: &str, n: &mut usize, inv: Vec<Expr>) -> bool {
    for s in body {
        match s {
            Stmt::While { spec, body, .. } => {
                *n += 1;
                if format!("loop{n}") == target {
                    spec.invariants = inv.clone();
                    return true;
                }
                if set_loop_invariants(body, target, n, inv.clone()) {
                    return true;
                }
            }
            Stmt::If { then, els, .. } => {
                if set_loop_invariants(then, target, n, inv.clone()) {
                    return true;
                }
                if set_loop_invariants(els, target, n, inv.clone()) {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

fn set_loop_variant(body: &mut [Stmt], target: &str, n: &mut usize, var: Option<Expr>) -> bool {
    for s in body {
        match s {
            Stmt::While { spec, body, .. } => {
                *n += 1;
                if format!("loop{n}") == target {
                    spec.variant = var.clone();
                    return true;
                }
                if set_loop_variant(body, target, n, var.clone()) {
                    return true;
                }
            }
            Stmt::If { then, els, .. } => {
                if set_loop_variant(then, target, n, var.clone()) {
                    return true;
                }
                if set_loop_variant(els, target, n, var.clone()) {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

/// Names the loop body writes (locals and arrays), used to `havoc` exactly the
/// state the invariant must be closed under.
pub fn written_names(body: &[Stmt]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    fn walk(body: &[Stmt], out: &mut BTreeSet<String>) {
        for s in body {
            match s {
                Stmt::Decl { name, .. } | Stmt::Assign { name, .. } => {
                    out.insert(name.clone());
                }
                Stmt::Store { array, .. } => {
                    out.insert(array.clone());
                }
                Stmt::If { then, els, .. } => {
                    walk(then, out);
                    walk(els, out);
                }
                Stmt::While { body, .. } => walk(body, out),
                _ => {}
            }
        }
    }
    walk(body, &mut out);
    out
}

/// The deepest nesting the parser accepts, in levels: each parenthesis, `!`/`-`,
/// index or quantifier body costs one, and each nested statement block costs
/// [`BLOCK_NESTING_COST`]. The parser is recursive descent, so unbounded input
/// would exhaust the stack and abort the process; past this depth it refuses
/// with [`NESTING_FAULT`] instead. Sized so the whole driver (parse,
/// elaboration, rendering, certificate) on a unit at the limit stays inside a
/// 1 MiB stack in a debug build: the Windows main thread, where the CLI drives.
pub const MAX_NESTING: usize = 96;

/// What one nested statement costs against [`MAX_NESTING`]: a statement level
/// uses several times the stack of an expression level, so at most 22 blocks
/// nest around a statement.
pub const BLOCK_NESTING_COST: usize = 4;

/// The message prefix of the nesting-limit fault, so callers can name it.
pub const NESTING_FAULT: &str = "nesting limit exceeded";

/// A parse fault with the position that produced it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("line {line}: {message}")]
pub struct ParseError {
    /// 1-based line.
    pub line: usize,
    /// What was wrong.
    pub message: String,
}

// ---------------------------------------------------------------- lexer

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    Ident(String),
    Int(i64),
    /// Multi-character operator/punctuation token.
    Punct(&'static str),
    Newline,
}

fn lex(text: &str) -> Result<Vec<(Tok, usize)>, ParseError> {
    let puncts: &[&'static str] = &[
        "->", "=>", "==", "!=", "<=", ">=", "&&", "||", "::", ":=", "(", ")", "{", "}", "[", "]",
        ",", ";", ":", "+", "-", "*", "/", "%", "<", ">", "=", "!",
    ];
    let bytes: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut line = 1usize;
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i];
        if c == '\n' {
            out.push((Tok::Newline, line));
            line += 1;
            i += 1;
            continue;
        }
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c == '#' {
            while i < bytes.len() && bytes[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c.is_ascii_digit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            let s: String = bytes[start..i].iter().collect();
            let n: i64 = s.parse().map_err(|_| ParseError {
                line,
                message: format!("integer literal {s} does not fit in 64 bits"),
            })?;
            out.push((Tok::Int(n), line));
            continue;
        }
        if c.is_ascii_alphabetic() || c == '_' {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == '_') {
                i += 1;
            }
            let s: String = bytes[start..i].iter().collect();
            out.push((Tok::Ident(s), line));
            continue;
        }
        let rest: String = bytes[i..bytes.len().min(i + 2)].iter().collect();
        let mut matched = None;
        for p in puncts {
            if rest.starts_with(p) {
                matched = Some(*p);
                break;
            }
        }
        match matched {
            Some(p) => {
                out.push((Tok::Punct(p), line));
                i += p.chars().count();
            }
            None => {
                return Err(ParseError {
                    line,
                    message: format!("unexpected character {c:?}"),
                })
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------- parser

struct Parser {
    tokens: Vec<(Tok, usize)>,
    pos: usize,
    /// Current recursion depth, bounded by [`MAX_NESTING`].
    depth: usize,
}

impl Parser {
    /// Run `f` `cost` nesting levels deeper, refusing past [`MAX_NESTING`].
    fn nested<T>(
        &mut self,
        cost: usize,
        f: impl FnOnce(&mut Self) -> Result<T, ParseError>,
    ) -> Result<T, ParseError> {
        if self.depth + cost > MAX_NESTING {
            return Err(ParseError {
                line: self.line(),
                message: format!(
                    "{NESTING_FAULT}: the unit nests deeper than {MAX_NESTING} levels \
                     (parentheses, `!`/`-` chains or blocks); it is refused rather than \
                     risk exhausting the parser's stack"
                ),
            });
        }
        self.depth += cost;
        let out = f(self);
        self.depth -= cost;
        out
    }

    fn peek(&self) -> Option<&Tok> {
        self.tokens.get(self.pos).map(|(t, _)| t)
    }

    fn peek_punct(&self, p: &str) -> bool {
        matches!(self.peek(), Some(Tok::Punct(q)) if *q == p)
    }

    fn line(&self) -> usize {
        self.tokens
            .get(self.pos)
            .map(|(_, l)| *l)
            .or_else(|| self.tokens.last().map(|(_, l)| *l))
            .unwrap_or(1)
    }

    fn bump(&mut self) -> Option<Tok> {
        let t = self.tokens.get(self.pos).map(|(t, _)| t.clone());
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn skip_newlines(&mut self) {
        while matches!(self.peek(), Some(Tok::Newline)) {
            self.pos += 1;
        }
    }

    fn expect_punct(&mut self, p: &str) -> Result<(), ParseError> {
        if self.peek_punct(p) {
            self.pos += 1;
            Ok(())
        } else {
            Err(ParseError {
                line: self.line(),
                message: format!("expected {p:?}, found {:?}", self.peek()),
            })
        }
    }

    fn expect_ident(&mut self) -> Result<String, ParseError> {
        match self.bump() {
            Some(Tok::Ident(s)) => Ok(s),
            other => Err(ParseError {
                line: self.line(),
                message: format!("expected an identifier, found {other:?}"),
            }),
        }
    }

    fn eat_ident(&mut self, word: &str) -> bool {
        if matches!(self.peek(), Some(Tok::Ident(s)) if s == word) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn program(&mut self) -> Result<Program, ParseError> {
        let mut functions = Vec::new();
        loop {
            self.skip_newlines();
            if self.peek().is_none() {
                break;
            }
            // Tolerate a `crate`/`module` line so a unit can be self-describing.
            if self.eat_ident("crate") || self.eat_ident("module") {
                while !matches!(self.peek(), None | Some(Tok::Newline)) {
                    self.pos += 1;
                }
                continue;
            }
            functions.push(self.function()?);
        }
        if functions.is_empty() {
            return Err(ParseError {
                line: self.line().max(1),
                message: "no function found; the unit must define at least one `fn`".into(),
            });
        }
        Ok(Program { functions })
    }

    fn function(&mut self) -> Result<Function, ParseError> {
        // `lemma` shares `fn`'s whole shape: parameters, return type,
        // contracts, body. The marker is what elaboration refuses.
        let is_lemma = if self.eat_ident("lemma") {
            true
        } else if self.eat_ident("fn") {
            false
        } else {
            return Err(ParseError {
                line: self.line(),
                message: format!("expected `fn` or `lemma`, found {:?}", self.peek()),
            });
        };
        let name = self.expect_ident()?;
        self.expect_punct("(")?;
        let mut params = Vec::new();
        loop {
            self.skip_newlines();
            if self.peek_punct(")") {
                self.pos += 1;
                break;
            }
            let pname = self.expect_ident()?;
            self.expect_punct(":")?;
            let tyname = self.expect_ident()?;
            let ty = Ty::parse(&tyname).ok_or_else(|| ParseError {
                line: self.line(),
                message: format!("type {tyname:?} is outside the fragment (int, bool, array only)"),
            })?;
            params.push((pname, ty));
            self.skip_newlines();
            if self.peek_punct(",") {
                self.pos += 1;
                continue;
            }
            self.expect_punct(")")?;
            break;
        }
        self.skip_newlines();
        let ret = if self.peek_punct("->") {
            self.pos += 1;
            let tyname = self.expect_ident()?;
            Some(Ty::parse(&tyname).ok_or_else(|| ParseError {
                line: self.line(),
                message: format!("return type {tyname:?} is outside the fragment"),
            })?)
        } else {
            None
        };
        let mut requires = Vec::new();
        let mut ensures = Vec::new();
        loop {
            self.skip_newlines();
            if self.eat_ident("requires") {
                requires.push(self.expr()?);
            } else if self.eat_ident("ensures") {
                ensures.push(self.expr()?);
            } else {
                break;
            }
        }
        self.skip_newlines();
        self.expect_punct("{")?;
        let body = self.block_rest()?;
        Ok(Function {
            name,
            params,
            ret,
            requires,
            ensures,
            body,
            is_lemma,
        })
    }

    /// Parses statements until the closing `}` (already past the opening one).
    fn block_rest(&mut self) -> Result<Vec<Stmt>, ParseError> {
        let mut out = Vec::new();
        loop {
            self.skip_newlines();
            if self.peek_punct("}") {
                self.pos += 1;
                return Ok(out);
            }
            if self.peek().is_none() {
                return Err(ParseError {
                    line: self.line(),
                    message: "unterminated block: reached end of input".into(),
                });
            }
            out.push(self.stmt()?);
        }
    }

    /// Parse a call's argument list `(...)`, shared by call expressions
    /// and call statements. An empty list parses; a missing separator or `)` is
    /// a parse error, exactly like a parameter list.
    fn arg_list(&mut self) -> Result<Vec<Expr>, ParseError> {
        self.expect_punct("(")?;
        let mut args = Vec::new();
        loop {
            self.skip_newlines();
            if self.peek_punct(")") {
                self.pos += 1;
                break;
            }
            args.push(self.expr()?);
            self.skip_newlines();
            if self.peek_punct(",") {
                self.pos += 1;
                continue;
            }
            self.expect_punct(")")?;
            break;
        }
        Ok(args)
    }

    fn stmt(&mut self) -> Result<Stmt, ParseError> {
        self.nested(BLOCK_NESTING_COST, Self::stmt_inner)
    }

    fn stmt_inner(&mut self) -> Result<Stmt, ParseError> {
        if self.eat_ident("var") {
            let name = self.expect_ident()?;
            self.expect_punct(":")?;
            let tyname = self.expect_ident()?;
            let ty = Ty::parse(&tyname).ok_or_else(|| ParseError {
                line: self.line(),
                message: format!("type {tyname:?} is outside the fragment"),
            })?;
            let init = if self.peek_punct("=") {
                self.pos += 1;
                Some(self.expr()?)
            } else {
                None
            };
            self.expect_punct(";")?;
            return Ok(Stmt::Decl { name, ty, init });
        }
        if self.eat_ident("assert") {
            let e = self.expr()?;
            self.expect_punct(";")?;
            return Ok(Stmt::Assert(e));
        }
        if self.eat_ident("assume") {
            let e = self.expr()?;
            self.expect_punct(";")?;
            return Ok(Stmt::Assume(e));
        }
        if self.eat_ident("return") {
            if self.peek_punct(";") {
                self.pos += 1;
                return Ok(Stmt::Return(None));
            }
            let e = self.expr()?;
            self.expect_punct(";")?;
            return Ok(Stmt::Return(Some(e)));
        }
        if self.eat_ident("if") {
            self.expect_punct("(")?;
            let cond = self.expr()?;
            self.expect_punct(")")?;
            self.skip_newlines();
            self.expect_punct("{")?;
            let then = self.block_rest()?;
            self.skip_newlines();
            let els = if self.eat_ident("else") {
                self.skip_newlines();
                self.expect_punct("{")?;
                self.block_rest()?
            } else {
                Vec::new()
            };
            return Ok(Stmt::If { cond, then, els });
        }
        if self.eat_ident("while") {
            self.expect_punct("(")?;
            let cond = self.expr()?;
            self.expect_punct(")")?;
            let mut spec = LoopSpec::default();
            loop {
                self.skip_newlines();
                if self.eat_ident("invariant") {
                    spec.invariants.push(self.expr()?);
                } else if self.eat_ident("variant") {
                    spec.variant = Some(self.expr()?);
                } else {
                    break;
                }
            }
            self.skip_newlines();
            self.expect_punct("{")?;
            let body = self.block_rest()?;
            return Ok(Stmt::While { cond, spec, body });
        }
        // Assignment or store: `name = e;` or `name[i] = e;`
        let name = self.expect_ident()?;
        // Call statement: `f(a, b);`. Parsed here, refused at elaboration.
        if self.peek_punct("(") {
            let args = self.arg_list()?;
            self.expect_punct(";")?;
            return Ok(Stmt::Call { name, args });
        }
        if self.peek_punct("[") {
            self.pos += 1;
            let index = self.expr()?;
            self.expect_punct("]")?;
            self.expect_punct("=")?;
            let value = self.expr()?;
            self.expect_punct(";")?;
            return Ok(Stmt::Store {
                array: name,
                index,
                value,
            });
        }
        self.expect_punct("=")?;
        let value = self.expr()?;
        self.expect_punct(";")?;
        Ok(Stmt::Assign { name, value })
    }

    // ------------------------------------------------------------ expressions

    /// Parse one expression from the token stream (used for contracts and
    /// annotations). Trailing newlines are not consumed.
    fn expr(&mut self) -> Result<Expr, ParseError> {
        self.expr_bp(1)
    }

    fn expr_bp(&mut self, min_bp: u8) -> Result<Expr, ParseError> {
        let mut lhs = self.unary()?;
        while let Some(op) = self.peek_binop() {
            let bp = op.precedence();
            if bp < min_bp {
                break;
            }
            self.pos += 1;
            // Left associative: parse the right side one level tighter.
            let rhs = self.expr_bp(bp + 1)?;
            lhs = Expr::Binary(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn peek_binop(&self) -> Option<BinOp> {
        let Some(Tok::Punct(p)) = self.peek() else {
            return None;
        };
        Some(match *p {
            "+" => BinOp::Add,
            "-" => BinOp::Sub,
            "*" => BinOp::Mul,
            "/" => BinOp::Div,
            "%" => BinOp::Mod,
            "==" => BinOp::Eq,
            "!=" => BinOp::Ne,
            "<" => BinOp::Lt,
            "<=" => BinOp::Le,
            ">" => BinOp::Gt,
            ">=" => BinOp::Ge,
            "&&" => BinOp::And,
            "||" => BinOp::Or,
            "=>" => BinOp::Imp,
            _ => return None,
        })
    }

    fn unary(&mut self) -> Result<Expr, ParseError> {
        self.nested(1, Self::unary_inner)
    }

    fn unary_inner(&mut self) -> Result<Expr, ParseError> {
        if self.peek_punct("-") {
            self.pos += 1;
            let e = self.unary()?;
            return Ok(match e {
                Expr::Int(n) => Expr::Int(-n),
                other => Expr::Unary(UnOp::Neg, Box::new(other)),
            });
        }
        if self.peek_punct("!") {
            self.pos += 1;
            let e = self.unary()?;
            return Ok(e.negate());
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<Expr, ParseError> {
        match self.peek().cloned() {
            Some(Tok::Int(n)) => {
                self.pos += 1;
                Ok(Expr::Int(n))
            }
            Some(Tok::Ident(word)) if word == "true" || word == "false" => {
                self.pos += 1;
                Ok(Expr::Bool(word == "true"))
            }
            Some(Tok::Ident(word)) if word == "forall" || word == "exists" => {
                self.pos += 1;
                let kind = if word == "forall" {
                    QuantKind::Forall
                } else {
                    QuantKind::Exists
                };
                let var = self.expect_ident()?;
                self.expect_punct(":")?;
                let tyname = self.expect_ident()?;
                if tyname != "int" {
                    return Err(ParseError {
                        line: self.line(),
                        message: format!(
                            "quantifiers bind integers only; {tyname:?} is outside the fragment"
                        ),
                    });
                }
                self.expect_punct("::")?;
                let body = self.expr_bp(1)?;
                Ok(Expr::Quant {
                    kind,
                    var,
                    body: Box::new(body),
                })
            }
            Some(Tok::Ident(name)) => {
                self.pos += 1;
                // `old(e)`: parsed here, refused at elaboration — the
                // fragment is single-state, so there is no pre-state to read.
                // A call `f(a, b)` likewise: the fragment proves one
                // function at a time, so a call has no semantics to encode.
                let mut e = if name == "old" && self.peek_punct("(") {
                    self.pos += 1;
                    let inner = self.expr()?;
                    self.expect_punct(")")?;
                    Expr::Old(Box::new(inner))
                } else if self.peek_punct("(") {
                    let args = self.arg_list()?;
                    Expr::Call { name, args }
                } else {
                    Expr::Var(name)
                };
                // A name can be indexed and re-indexed: `a[i]`, `a[i := v][j]`.
                while self.peek_punct("[") {
                    self.pos += 1;
                    let idx = self.expr()?;
                    if self.peek_punct(":=") {
                        self.pos += 1;
                        let val = self.expr()?;
                        self.expect_punct("]")?;
                        e = Expr::Store(Box::new(e), Box::new(idx), Box::new(val));
                    } else {
                        self.expect_punct("]")?;
                        e = Expr::Select(Box::new(e), Box::new(idx));
                    }
                }
                Ok(e)
            }
            Some(Tok::Punct("(")) => {
                self.pos += 1;
                let e = self.expr()?;
                self.expect_punct(")")?;
                Ok(e)
            }
            other => Err(ParseError {
                line: self.line(),
                message: format!("expected an expression, found {other:?}"),
            }),
        }
    }
}

/// Parse a single expression (annotation text, property text, assumptions).
/// This is the elaborator's entry point for untrusted input: a failure here is
/// `proposer_malformed`, never a proof failure.
pub fn parse_expr(text: &str) -> Result<Expr, ParseError> {
    let toks = lex(text)?;
    let mut p = Parser {
        tokens: toks,
        pos: 0,
        depth: 0,
    };
    p.skip_newlines();
    let e = p.expr()?;
    p.skip_newlines();
    if p.peek().is_some() {
        return Err(ParseError {
            line: p.line(),
            message: format!("trailing tokens after the expression: {:?}", p.peek()),
        });
    }
    Ok(e)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_unit_with_contracts_loops_and_arrays() {
        let src = r#"
fn find(a: array, n: int) -> int
  requires n >= 0
  ensures result == n || a[result] > 0
{
  var i: int = 0;
  while (i < n && a[i] <= 0)
    invariant i >= 0 && i <= n
    variant n - i
  {
    i = i + 1;
    assert i <= n;
  }
  return i;
}
"#;
        let p = Program::parse(src).expect("parses");
        assert_eq!(p.functions.len(), 1);
        let f = &p.functions[0];
        assert_eq!(f.name, "find");
        assert_eq!(f.params.len(), 2);
        assert_eq!(f.requires.len(), 1);
        assert_eq!(f.ensures.len(), 1);
        assert_eq!(
            p.loop_ids(),
            vec![("loop1".to_string(), "find".to_string())]
        );
    }

    #[test]
    fn precedence_is_arithmetic_then_comparison_then_and_then_or() {
        let e = parse_expr("i + 1 < n && r >= 0 || x == 1").expect("parses");
        match e {
            Expr::Binary(BinOp::Or, l, _) => match *l {
                Expr::Binary(BinOp::And, _, _) => {}
                other => panic!("expected && under ||, got {other:?}"),
            },
            other => panic!("expected ||, got {other:?}"),
        }
    }

    #[test]
    fn unparsable_text_names_the_fault() {
        let err = Program::parse("fn f(x: int) -> int { var y: float = 1; }").unwrap_err();
        assert!(err.message.contains("float"), "{err}");
        let err = parse_expr("i >> 2").unwrap_err();
        assert!(err.message.contains("expected an expression"), "{err}");
    }

    #[test]
    fn old_calls_and_lemmas_parse_and_render_back() {
        // The constructs parse and render deterministically in their
        // surface shape, so annotation text round-trips through the parser.
        let e = parse_expr("old(x) == g(x, 1)").unwrap();
        assert_eq!(e.to_string(), "(old(x) == g(x, 1))");
        assert_eq!(parse_expr(&e.to_string()).unwrap(), e);

        let p = Program::parse("lemma helper() { }\nfn f() { helper(); }\n").unwrap();
        assert!(p.functions[0].is_lemma);
        assert!(!p.functions[1].is_lemma);
        match &p.functions[1].body[0] {
            Stmt::Call { name, .. } => assert_eq!(name, "helper"),
            other => panic!("expected a call statement, got {other:?}"),
        }

        // Genuinely malformed syntax is still a parse error, not a ceiling.
        assert!(Program::parse("fn f() { helper() }\n").is_err());
        assert!(parse_expr("g(x").is_err());
        assert!(parse_expr("old(x, y)").is_err());
    }

    #[test]
    fn substitution_respects_quantifier_scoping() {
        let e = parse_expr("(forall j: int :: j < i) && j > 0").unwrap();
        let mut env = BTreeMap::new();
        env.insert("j".to_string(), Expr::Int(7));
        env.insert("i".to_string(), Expr::Int(3));
        let out = e.subst(&env);
        assert_eq!(out.to_string(), "((forall j: int :: (j < 3)) && (7 > 0))");
    }

    #[test]
    fn free_variable_collection_skips_bound_names() {
        let e = parse_expr("(forall j: int :: j < i) && k > 0").unwrap();
        assert_eq!(e.free_vars(), vec!["i".to_string(), "k".to_string()]);
    }

    #[test]
    fn written_names_covers_assignments_stores_and_nesting() {
        let p = Program::parse(
            r#"
fn f(a: array, n: int) -> int
{
  var i: int = 0;
  while (i < n)
  {
    a[i] = i;
    i = i + 1;
  }
  return i;
}
"#,
        )
        .unwrap();
        let names = written_names(&p.functions[0].body);
        assert!(names.contains("i"));
        assert!(names.contains("a"));
    }

    #[test]
    fn set_invariants_replaces_only_the_named_loop() {
        let mut p = Program::parse(
            r#"
fn f(n: int) -> int
{
  var i: int = 0;
  while (i < n)
    invariant i >= 0
  {
    i = i + 1;
  }
  return i;
}
"#,
        )
        .unwrap();
        assert!(p.set_invariants("loop1", vec![Expr::Int(1)]));
        assert!(!p.set_invariants("loop9", vec![Expr::Int(1)]));
        assert_eq!(p.loop_ids().len(), 1);
    }

    #[test]
    fn loop_ids_are_globally_numbered_across_functions() {
        let mut p = Program::parse(
            r#"
fn first(n: int) -> int
{
  var i: int = 0;
  while (i < n)
  {
    i = i + 1;
  }
  return i;
}

fn second(n: int) -> int
{
  var j: int = 0;
  while (j < n)
  {
    j = j + 1;
  }
  while (j > 0)
  {
    j = j - 1;
  }
  return j;
}
"#,
        )
        .unwrap();
        // Loop ids are globally numbered in source order across functions,
        // so the second function's first loop is `loop2`, never a second
        // `loop1` — and `set_invariants`, which always addressed loops
        // unit-wide, agrees with the numbering.
        assert_eq!(
            p.loop_ids(),
            vec![
                ("loop1".to_string(), "first".to_string()),
                ("loop2".to_string(), "second".to_string()),
                ("loop3".to_string(), "second".to_string()),
            ]
        );
        assert_eq!(p.count_loops(), 3);
        assert_eq!(p.loop_offset(0), 0);
        assert_eq!(p.loop_offset(1), 1);
        assert_eq!(p.loop_offset(2), 3);
        assert!(p.set_invariants("loop2", vec![Expr::Int(1)]));
    }

    fn nested_parens(depth: usize) -> String {
        format!("{}x{}", "(".repeat(depth), ")".repeat(depth))
    }

    /// The deepest legal `if` nest around one statement: every block level
    /// and the innermost statement cost [`BLOCK_NESTING_COST`] each, and the
    /// statement's expression one more.
    const DEEPEST_BLOCKS: usize = (MAX_NESTING - 1) / BLOCK_NESTING_COST - 1;

    fn assert_nesting_fault(e: ParseError) {
        assert!(e.message.starts_with(NESTING_FAULT), "{e}");
    }

    #[test]
    fn nesting_at_the_limit_parses() {
        // `(` costs one level, and the outermost expression one more.
        let e = parse_expr(&nested_parens(MAX_NESTING - 1)).unwrap();
        assert_eq!(e, Expr::Var("x".into()));
        // The deepest legal block nest, with a statement inside it.
        let depth = DEEPEST_BLOCKS;
        let src = format!(
            "fn f(x: int) -> int\n{{\n{}x = 1;\n{}return x;\n}}\n",
            "if (true) {\n".repeat(depth),
            "}\n".repeat(depth)
        );
        Program::parse(&src).unwrap();
    }

    #[test]
    fn nesting_past_the_limit_is_a_named_fault_not_a_crash() {
        // Far past any stack: without the cap this aborts the process.
        assert_nesting_fault(parse_expr(&nested_parens(100_000)).unwrap_err());
        assert_nesting_fault(parse_expr(&nested_parens(MAX_NESTING)).unwrap_err());
        assert_nesting_fault(parse_expr(&format!("{}x", "-".repeat(100_000))).unwrap_err());
        assert_nesting_fault(parse_expr(&format!("{}x", "!".repeat(100_000))).unwrap_err());
        assert_nesting_fault(
            parse_expr(&format!("{}0{}", "a[".repeat(100_000), "]".repeat(100_000))).unwrap_err(),
        );
    }

    #[test]
    fn nested_blocks_past_the_limit_are_a_named_fault() {
        let depth = 100_000;
        let src = format!(
            "fn f(x: int) -> int\n{{\n{}return x;\n{}}}\n",
            "if (true) {\n".repeat(depth),
            "}\n".repeat(depth)
        );
        assert_nesting_fault(Program::parse(&src).unwrap_err());
    }

    #[test]
    fn the_depth_counter_unwinds_after_each_expression() {
        // Many shallow expressions in one unit must not accumulate depth.
        let body: String = (0..2 * MAX_NESTING)
            .map(|_| format!("  x = {};\n", nested_parens(8)))
            .collect();
        let src = format!("fn f(x: int) -> int\n{{\n{body}  return x;\n}}\n");
        Program::parse(&src).unwrap();
    }

    #[test]
    fn the_limit_holds_on_a_small_stack() {
        // The CLI drives on the main thread (1 MiB on Windows), so the limit
        // must hold there, in a debug build.
        let handle = std::thread::Builder::new()
            .stack_size(1 << 20)
            .spawn(|| {
                parse_expr(&nested_parens(MAX_NESTING - 1)).unwrap();
                parse_expr(&nested_parens(100_000)).unwrap_err();
                // Blocks cost more stack per level than parentheses: the
                // deepest legal block nest, with a statement inside it.
                let depth = DEEPEST_BLOCKS;
                let src = format!(
                    "fn f(x: int) -> int\n{{\n{}return x;\n{}}}\n",
                    "if (true) {\n".repeat(depth),
                    "}\n".repeat(depth)
                );
                Program::parse(&src).unwrap();
            })
            .unwrap();
        handle
            .join()
            .expect("parsing at the limit fits in 1 MiB of stack");
    }
}
