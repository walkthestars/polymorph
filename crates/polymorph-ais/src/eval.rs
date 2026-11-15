//! A tiny independent evaluator.
//!
//! Its job is to make counterexamples checkable **without** the solver: given
//! the model values the checker read out, evaluate `assumptions ∧ ¬goal` with
//! this evaluator and require `true`. A counterexample that fails this test is
//! discarded instead of being fed to the refiner, so a solver bug cannot turn
//! into a fabricated witness. The same evaluator backs `recheck` for
//! `disproved` certificates.
//!
//! Consequences, stated: quantified formulas are not evaluated (this is not a
//! theorem prover), so a quantified counterexample is reported unverified
//! rather than assumed good.

use std::collections::BTreeMap;

use crate::ir::{BinOp, Expr, QuantKind, UnOp};

/// A concrete value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// Mathematical integer.
    Int(i128),
    /// Boolean.
    Bool(bool),
    /// Array as a default value plus explicit overrides.
    Array {
        /// Value at indices without an override.
        default: i128,
        /// Explicit entries, in insertion order.
        entries: Vec<(i128, i128)>,
    },
}

impl Value {
    fn as_int(&self) -> Option<i128> {
        match self {
            Value::Int(n) => Some(*n),
            _ => None,
        }
    }

    fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// Render as a source-level literal for the counterexample record.
    pub fn render(&self) -> String {
        match self {
            Value::Int(n) => n.to_string(),
            Value::Bool(b) => b.to_string(),
            Value::Array { default, entries } => {
                let mut s = format!("const {default}");
                for (i, v) in entries {
                    s.push_str(&format!("; [{i}] = {v}"));
                }
                s
            }
        }
    }
}

/// A model: symbol → value.
pub type Model = BTreeMap<String, Value>;

/// Why evaluation could not produce a value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EvalError {
    /// A symbol is missing from the model.
    #[error("model has no value for {0:?}")]
    Missing(String),
    /// The expression used a quantifier, which this evaluator does not decide.
    #[error("quantified formulas are not evaluated ({0})")]
    Quantifier(String),
    /// A sort mismatch (an int where a bool was needed, or vice versa).
    #[error("sort mismatch in {0}")]
    Sort(String),
    /// Division or modulo by zero.
    #[error("division by zero")]
    DivZero,
    /// An integer overflowed `i128` (the evaluator is not the prover; the
    /// solver uses unbounded integers).
    #[error("integer overflow while evaluating")]
    Overflow,
    /// A literal could not be parsed back from the solver's output.
    #[error("unparsable value literal {0:?}")]
    BadLiteral(String),
    /// The expression used `old(...)` or a call: the elaborator refuses
    /// both before anything reaches a solver, and this evaluator never guesses
    /// a semantics for them.
    #[error("{0} is outside the evaluator's fragment")]
    Outside(String),
}

/// Evaluate one expression under a model.
pub fn eval(e: &Expr, m: &Model) -> Result<Value, EvalError> {
    match e {
        Expr::Int(n) => Ok(Value::Int(i128::from(*n))),
        Expr::Bool(b) => Ok(Value::Bool(*b)),
        Expr::Var(v) => m
            .get(v)
            .cloned()
            .ok_or_else(|| EvalError::Missing(v.clone())),
        Expr::Select(a, i) => {
            let av = eval(a, m)?;
            let iv = eval(i, m)?
                .as_int()
                .ok_or_else(|| EvalError::Sort("select index".into()))?;
            match av {
                Value::Array { default, entries } => Ok(Value::Int(
                    entries
                        .iter()
                        .find(|(k, _)| *k == iv)
                        .map(|(_, v)| *v)
                        .unwrap_or(default),
                )),
                _ => Err(EvalError::Sort("select on a non-array".into())),
            }
        }
        Expr::Store(a, i, v) => {
            let av = eval(a, m)?;
            let iv = eval(i, m)?
                .as_int()
                .ok_or_else(|| EvalError::Sort("store index".into()))?;
            let vv = eval(v, m)?
                .as_int()
                .ok_or_else(|| EvalError::Sort("store value".into()))?;
            match av {
                Value::Array {
                    default,
                    mut entries,
                } => {
                    entries.retain(|(k, _)| *k != iv);
                    entries.push((iv, vv));
                    Ok(Value::Array { default, entries })
                }
                _ => Err(EvalError::Sort("store on a non-array".into())),
            }
        }
        Expr::Unary(UnOp::Neg, inner) => {
            let n = eval(inner, m)?
                .as_int()
                .ok_or_else(|| EvalError::Sort("negation".into()))?;
            n.checked_neg().map(Value::Int).ok_or(EvalError::Overflow)
        }
        Expr::Unary(UnOp::Not, inner) => {
            let b = eval(inner, m)?
                .as_bool()
                .ok_or_else(|| EvalError::Sort("negation".into()))?;
            Ok(Value::Bool(!b))
        }
        Expr::Binary(op, l, r) => bin(*op, l, r, m),
        Expr::Quant { kind, .. } => Err(EvalError::Quantifier(match kind {
            QuantKind::Forall => "forall".into(),
            QuantKind::Exists => "exists".into(),
        })),
        Expr::Old(_) => Err(EvalError::Outside("`old(...)`".into())),
        Expr::Call { name, .. } => Err(EvalError::Outside(format!("a call of `{name}`"))),
    }
}

fn bin(op: BinOp, l: &Expr, r: &Expr, m: &Model) -> Result<Value, EvalError> {
    // Short-circuiting connectives would be wrong for a total evaluator only if
    // evaluation could fail on the skipped side; it can (`missing symbol`), so
    // both sides are evaluated and AND/OR are then combined. A partial model
    // therefore fails loudly instead of guessing.
    let lv = eval(l, m)?;
    let rv = eval(r, m)?;
    match op {
        BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod => {
            let a = lv
                .as_int()
                .ok_or_else(|| EvalError::Sort("arithmetic".into()))?;
            let b = rv
                .as_int()
                .ok_or_else(|| EvalError::Sort("arithmetic".into()))?;
            let out = match op {
                BinOp::Add => a.checked_add(b),
                BinOp::Sub => a.checked_sub(b),
                BinOp::Mul => a.checked_mul(b),
                // SMT `div`/`mod` are Euclidean: the remainder is non-negative
                // when the divisor is positive, and divides by zero are
                // uninterpreted in SMT-LIB. Use `div_euclid`/`rem_euclid` for
                // positive divisors and refuse zero (which the solver also
                // leaves unconstrained for most logics).
                BinOp::Div => {
                    if b == 0 {
                        return Err(EvalError::DivZero);
                    }
                    Some(a.div_euclid(b))
                }
                BinOp::Mod => {
                    if b == 0 {
                        return Err(EvalError::DivZero);
                    }
                    Some(a.rem_euclid(b))
                }
                _ => unreachable!(),
            };
            out.map(Value::Int).ok_or(EvalError::Overflow)
        }
        BinOp::Eq | BinOp::Ne => {
            let eq = match (&lv, &rv) {
                (Value::Int(a), Value::Int(b)) => a == b,
                (Value::Bool(a), Value::Bool(b)) => a == b,
                (
                    Value::Array {
                        default: d1,
                        entries: e1,
                    },
                    Value::Array {
                        default: d2,
                        entries: e2,
                    },
                ) => {
                    let mut m1 = e1.clone();
                    let mut m2 = e2.clone();
                    m1.sort();
                    m2.sort();
                    d1 == d2 && m1 == m2
                }
                _ => return Err(EvalError::Sort("equality".into())),
            };
            Ok(Value::Bool(if op == BinOp::Eq { eq } else { !eq }))
        }
        BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
            let a = lv
                .as_int()
                .ok_or_else(|| EvalError::Sort("comparison".into()))?;
            let b = rv
                .as_int()
                .ok_or_else(|| EvalError::Sort("comparison".into()))?;
            Ok(Value::Bool(match op {
                BinOp::Lt => a < b,
                BinOp::Le => a <= b,
                BinOp::Gt => a > b,
                BinOp::Ge => a >= b,
                _ => unreachable!(),
            }))
        }
        BinOp::And => Ok(Value::Bool(
            lv.as_bool().ok_or_else(|| EvalError::Sort("and".into()))?
                && rv.as_bool().ok_or_else(|| EvalError::Sort("and".into()))?,
        )),
        BinOp::Or => Ok(Value::Bool(
            lv.as_bool().ok_or_else(|| EvalError::Sort("or".into()))?
                || rv.as_bool().ok_or_else(|| EvalError::Sort("or".into()))?,
        )),
        BinOp::Imp => Ok(Value::Bool(
            !lv.as_bool().ok_or_else(|| EvalError::Sort("imp".into()))?
                || rv.as_bool().ok_or_else(|| EvalError::Sort("imp".into()))?,
        )),
    }
}

/// Evaluate the counterexample test for one obligation: every assumption holds
/// **and** the goal is false under the model. `Ok(true)` means the model is a
/// genuine refutation; `Ok(false)` means it is not (and must be discarded).
pub fn refutes(assumptions: &[Expr], goal: &Expr, m: &Model) -> Result<bool, EvalError> {
    for a in assumptions {
        match eval(a, m)? {
            Value::Bool(true) => {}
            Value::Bool(false) => return Ok(false),
            other => {
                return Err(EvalError::Sort(format!(
                    "assumption evaluated to {other:?}"
                )))
            }
        }
    }
    match eval(goal, m)? {
        Value::Bool(b) => Ok(!b),
        other => Err(EvalError::Sort(format!("goal evaluated to {other:?}"))),
    }
}

/// Parse a solver-emitted value literal into a [`Value`].
///
/// Handles integers (`3`, `(- 6)`), booleans, `(as const (Array Int Int)) c` and
/// `store` chains.
pub fn parse_literal(text: &str) -> Result<Value, EvalError> {
    let t = text.trim();
    if t == "true" {
        return Ok(Value::Bool(true));
    }
    if t == "false" {
        return Ok(Value::Bool(false));
    }
    if let Ok(n) = t.parse::<i128>() {
        return Ok(Value::Int(n));
    }
    let Some(items) = sexpr_items(t) else {
        return Err(EvalError::BadLiteral(text.to_string()));
    };
    if items.len() == 2 && items[0] == "-" {
        if let Ok(n) = items[1].parse::<i128>() {
            return Ok(Value::Int(-n));
        }
    }
    // `((as const (Array Int Int)) 0)` — the head is itself a list.
    if items.len() == 2 && items[0].starts_with("(as ") {
        let default = parse_literal(&items[1])?
            .as_int()
            .ok_or_else(|| EvalError::BadLiteral(text.to_string()))?;
        return Ok(Value::Array {
            default,
            entries: Vec::new(),
        });
    }
    if items.len() == 2 && items[0] == "as" && items[1] == "const" {
        return Err(EvalError::BadLiteral(format!(
            "{text} names a sort but no default value"
        )));
    }
    if !items.is_empty() && items[0] == "store" && items.len() == 4 {
        let base = parse_literal(&items[1])?;
        let idx = parse_literal(&items[2])?
            .as_int()
            .ok_or_else(|| EvalError::BadLiteral(text.to_string()))?;
        let val = parse_literal(&items[3])?
            .as_int()
            .ok_or_else(|| EvalError::BadLiteral(text.to_string()))?;
        return match base {
            Value::Array {
                default,
                mut entries,
            } => {
                entries.retain(|(k, _)| *k != idx);
                entries.push((idx, val));
                Ok(Value::Array { default, entries })
            }
            _ => Err(EvalError::BadLiteral(text.to_string())),
        };
    }
    Err(EvalError::BadLiteral(text.to_string()))
}

fn sexpr_items(s: &str) -> Option<Vec<String>> {
    let s = s.trim();
    if !s.starts_with('(') || !s.ends_with(')') {
        return None;
    }
    let inner = &s[1..s.len() - 1];
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut cur = String::new();
    for c in inner.chars() {
        match c {
            '(' => {
                depth += 1;
                cur.push(c);
            }
            ')' => {
                depth -= 1;
                cur.push(c);
            }
            c if c.is_whitespace() && depth == 0 => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::parse_expr;

    fn model(pairs: &[(&str, i128)]) -> Model {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), Value::Int(*v)))
            .collect()
    }

    #[test]
    fn arithmetic_is_euclidean_like_smt_div_and_mod() {
        let m = model(&[("i", -7)]);
        assert_eq!(
            super::eval(&parse_expr("i / 2").unwrap(), &m).unwrap(),
            Value::Int(-4)
        );
        assert_eq!(
            super::eval(&parse_expr("i % 2").unwrap(), &m).unwrap(),
            Value::Int(1)
        );
        assert_eq!(
            super::eval(&parse_expr("i / 0").unwrap(), &m).unwrap_err(),
            EvalError::DivZero
        );
    }

    #[test]
    fn arrays_are_a_default_plus_overrides() {
        let mut m = model(&[("k", 2), ("v", 9)]);
        m.insert(
            "a".into(),
            Value::Array {
                default: 0,
                entries: vec![(1, 5)],
            },
        );
        assert_eq!(
            super::eval(&parse_expr("a[1]").unwrap(), &m).unwrap(),
            Value::Int(5)
        );
        assert_eq!(
            super::eval(&parse_expr("a[2]").unwrap(), &m).unwrap(),
            Value::Int(0)
        );
        assert_eq!(
            super::eval(&parse_expr("a[k := v][k]").unwrap(), &m).unwrap(),
            Value::Int(9)
        );
    }

    #[test]
    fn refutation_test_requires_every_assumption_and_a_false_goal() {
        let m = model(&[("n", 6), ("i", 6)]);
        let ass = vec![parse_expr("n >= 0").unwrap(), parse_expr("i == n").unwrap()];
        assert!(!refutes(&ass, &parse_expr("i >= n").unwrap(), &m).unwrap());
        assert!(refutes(&ass, &parse_expr("i < n").unwrap(), &m).unwrap());
        assert_eq!(
            refutes(&ass, &parse_expr("zzz > 0").unwrap(), &m).unwrap_err(),
            EvalError::Missing("zzz".into())
        );
    }

    #[test]
    fn literals_round_trip_including_arrays_and_negatives() {
        assert_eq!(parse_literal("3").unwrap(), Value::Int(3));
        assert_eq!(parse_literal("(- 6)").unwrap(), Value::Int(-6));
        assert_eq!(parse_literal("true").unwrap(), Value::Bool(true));
        assert_eq!(
            parse_literal("(store ((as const (Array Int Int)) 0) 2 5)").unwrap(),
            Value::Array {
                default: 0,
                entries: vec![(2, 5)]
            }
        );
        assert!(parse_literal("(+ 1 2)").is_err());
    }

    #[test]
    fn quantified_formulas_are_refused_not_guessed() {
        let m = model(&[("i", 1)]);
        let e = parse_expr("(forall j: int :: j >= i)").unwrap();
        assert!(matches!(
            eval(&e, &m).unwrap_err(),
            EvalError::Quantifier(_)
        ));
    }

    #[test]
    fn old_and_calls_are_refused_not_guessed() {
        // The evaluator refuses them loudly — never a panic, never an
        // invented value; elaboration refuses both before a solver runs.
        let m = model(&[("x", 1)]);
        assert!(matches!(
            eval(&parse_expr("old(x)").unwrap(), &m).unwrap_err(),
            EvalError::Outside(_)
        ));
        assert!(matches!(
            eval(&parse_expr("g(x)").unwrap(), &m).unwrap_err(),
            EvalError::Outside(_)
        ));
    }
}
