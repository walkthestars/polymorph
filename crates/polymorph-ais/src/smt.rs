//! SMT-LIB2 emission: the elaboration target.
//!
//! One [`Query`] is one self-contained SMT-LIB2 script. The script is the whole
//! trust chain: declarations, assumptions, the negated goal, a `check-sat`, and
//! — when the query may be satisfiable — `get-value` probes so a counterexample
//! comes back as structured data instead of solver prose.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::ir::{BinOp, Expr, QuantKind, Ty};
use crate::types::Probe;

/// Declarations of one query: symbol → sort. A symbol declared twice with
/// different sorts is an elaboration fault, not something to paper over.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Decls {
    map: BTreeMap<String, Ty>,
}

impl Decls {
    /// Empty declaration set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Declare `name` with `ty`. Returns `false` when the name is already
    /// declared with a different sort (the caller refuses the query).
    pub fn declare(&mut self, name: &str, ty: Ty) -> bool {
        match self.map.get(name) {
            Some(existing) => *existing == ty,
            None => {
                self.map.insert(name.to_string(), ty);
                true
            }
        }
    }

    /// Sort of a declared symbol.
    pub fn get(&self, name: &str) -> Option<Ty> {
        self.map.get(name).copied()
    }

    /// All declarations, sorted by symbol.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &Ty)> {
        self.map.iter()
    }

    /// Number of declared symbols.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// True when nothing is declared.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

/// Render one expression as SMT-LIB2. Deterministic and fully parenthesized;
/// the certificate digests exactly these bytes.
pub fn render(e: &Expr) -> String {
    let mut out = String::new();
    write_expr(e, &mut out);
    out
}

fn write_expr(e: &Expr, out: &mut String) {
    match e {
        Expr::Int(n) => {
            if *n < 0 {
                let _ = write!(out, "(- {})", n.unsigned_abs());
            } else {
                let _ = write!(out, "{n}");
            }
        }
        Expr::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Expr::Var(v) => out.push_str(v),
        Expr::Select(a, i) => {
            let _ = write!(out, "(select {} {})", render(a), render(i));
        }
        Expr::Store(a, i, v) => {
            let _ = write!(out, "(store {} {} {})", render(a), render(i), render(v));
        }
        Expr::Unary(op, inner) => {
            let _ = write!(out, "({} {})", op.smt(), render(inner));
        }
        Expr::Binary(BinOp::Ne, l, r) => {
            // SMT-LIB2 has no `!=`; `distinct` is the n-ary disequality.
            let _ = write!(out, "(distinct {} {})", render(l), render(r));
        }
        Expr::Binary(op, l, r) => {
            let _ = write!(out, "({} {} {})", op.smt(), render(l), render(r));
        }
        Expr::Quant { kind, var, body } => {
            let kw = match kind {
                QuantKind::Forall => "forall",
                QuantKind::Exists => "exists",
            };
            let _ = write!(out, "({kw} (({var} Int)) {})", render(body));
        }
        // `old(...)` and calls are refused at elaboration and so never
        // reach a query in practice. If a bug ever lets one through, it renders
        // as an application of an undeclared symbol: the solver errors loudly
        // instead of answering, so nothing can silently become a proof.
        Expr::Old(inner) => {
            let _ = write!(out, "(@old {})", render(inner));
        }
        Expr::Call { name, args } => {
            let rendered: Vec<String> = args.iter().map(render).collect();
            let _ = write!(out, "(@call {} {})", name, rendered.join(" "));
        }
    }
}

/// A self-contained SMT-LIB2 verification query.
///
/// The contract of the encoding (the havoc/assume loop rule): the goal is
/// negated and asserted together with the assumptions, so **`unsat` means the
/// obligation holds** and **`sat` means a counterexample exists**, whose values
/// the probes then read out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    /// Declarations in scope for this query.
    pub decls: Decls,
    /// Facts available at this program point.
    pub assumptions: Vec<Expr>,
    /// The obligation. Negated before being asserted.
    pub goal: Expr,
    /// Symbols to read from a model if the query is satisfiable.
    pub probes: Vec<Probe>,
    /// Solver wall-clock cap in seconds.
    pub timeout_s: u64,
    /// Determinism seed.
    pub seed: u64,
    /// Extra solver options (name, value), applied in order.
    pub options: Vec<(String, String)>,
}

impl Query {
    /// The exact SMT-LIB2 text sent to the backend.
    pub fn text(&self) -> String {
        let mut s = String::new();
        s.push_str("(set-logic ALL)\n");
        let _ = writeln!(
            s,
            "(set-option :timeout {})",
            self.timeout_s.saturating_mul(1000)
        );
        let _ = writeln!(s, "(set-option :random-seed {})", self.seed);
        let _ = writeln!(s, "(set-option :print-success false)");
        for (k, v) in &self.options {
            let _ = writeln!(s, "(set-option :{k} {v})");
        }
        for (name, ty) in self.decls.iter() {
            let _ = writeln!(s, "(declare-const {name} {})", ty.sort());
        }
        for a in &self.assumptions {
            let _ = writeln!(s, "(assert {})", render(a));
        }
        let _ = writeln!(s, "(assert {})", render(&self.goal.clone().negate()));
        s.push_str("(check-sat)\n");
        if !self.probes.is_empty() {
            let names: Vec<&str> = self.probes.iter().map(|p| p.symbol.as_str()).collect();
            let _ = writeln!(s, "(get-value ({}))", names.join(" "));
        }
        s
    }

    /// `sha256:` of the text, used in transcripts and certificate digests.
    pub fn digest(&self) -> String {
        crate::cert::sha256_tag(&self.text())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::parse_expr;
    use crate::types::Probe;

    #[test]
    fn renders_negative_literals_and_disequality_the_smt_way() {
        assert_eq!(render(&parse_expr("-5").unwrap()), "(- 5)");
        assert_eq!(render(&parse_expr("i != j").unwrap()), "(distinct i j)");
        assert_eq!(render(&parse_expr("i / 2").unwrap()), "(div i 2)");
        assert_eq!(render(&parse_expr("i % 2").unwrap()), "(mod i 2)");
    }

    #[test]
    fn a_query_declares_its_symbols_asserts_its_facts_and_negates_the_goal() {
        let mut decls = Decls::new();
        assert!(decls.declare("n", Ty::Int));
        assert!(decls.declare("a", Ty::Array));
        assert!(!decls.declare("n", Ty::Bool), "a symbol cannot change sort");
        let q = Query {
            decls,
            assumptions: vec![parse_expr("n >= 0").unwrap()],
            goal: parse_expr("n <= 10").unwrap(),
            probes: vec![Probe {
                symbol: "n".into(),
                label: "n".into(),
            }],
            timeout_s: 5,
            seed: 7,
            options: Vec::new(),
        };
        let text = q.text();
        assert!(text.contains("(declare-const n Int)"));
        assert!(text.contains("(declare-const a (Array Int Int))"));
        assert!(text.contains("(assert (>= n 0))"));
        assert!(text.contains("(assert (not (<= n 10)))"), "{text}");
        assert!(text.contains("(check-sat)"));
        assert!(text.contains("(get-value (n))"));
        assert!(text.contains(":timeout 5000"));
        assert!(text.contains(":random-seed 7"));
        assert_eq!(q.digest(), crate::cert::sha256_tag(&text));
    }

    #[test]
    fn quantifier_rendering_binds_its_variable() {
        let e = parse_expr("(forall j: int :: j < n)").unwrap();
        assert_eq!(render(&e), "(forall ((j Int)) (< j n))");
    }

    #[test]
    fn old_and_calls_render_as_loud_undeclared_symbols() {
        // Never sent in practice (elaboration refuses them first); if one
        // ever leaks into a query it must fail loudly, not approximate.
        assert_eq!(render(&parse_expr("old(x)").unwrap()), "(@old x)");
        assert_eq!(render(&parse_expr("g(x, 1)").unwrap()), "(@call g x 1)");
    }
}
