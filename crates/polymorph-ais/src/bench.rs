//! The curated AIS benchmark — the primitive's honest scoreboard.
//!
//! Every case is a small unit in the core language with an *intended* outcome,
//! written down before running, so a mismatch is a finding rather than a
//! surprise. The bench runs with the deterministic pool only: no model, no
//! tokens, reproducible byte for byte. That is deliberate — the scoreboard
//! measures the primitive, not the model behind it.
//!
//! Scope honesty: this is a small curated corpus (14 cases) built for this
//! chunk. It is *not* Code2Inv, Lemur or VERINA, and the numbers below must not
//! be compared with published results on those corpora. What it does show is
//! which shapes the v1 fragment handles, which it refuses, and that nothing is
//! ever reported `proved` without a discharged obligation set.

use std::time::Duration;

use crate::driver::{self, DriverConfig};
use crate::solver::{Checker, SolverSpec};
use crate::types::{
    BackendSpec, Budget, Determinism, FileSpec, Property, PropertyKind, Request, Source, Verdict,
};

/// What a case is expected to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expect {
    /// The property is true and the primitive must prove it.
    Proved,
    /// The property is false of the code and the primitive must say so with a
    /// witnessed counterexample.
    Disproved,
    /// The specification is vacuous or contradictory.
    SpecInconsistent,
    /// The language or the property is outside the fragment.
    Unsupported,
    /// The property is true but outside what the deterministic pool can find.
    /// A model proposer may close these; the bench records them as open.
    NotProved,
}

impl Expect {
    /// The verdict this expectation maps to (NotProved accepts any non-proof).
    pub fn accepts(self, verdict: Verdict) -> bool {
        match self {
            Expect::Proved => verdict == Verdict::Proved,
            Expect::Disproved => verdict == Verdict::Disproved,
            Expect::SpecInconsistent => verdict == Verdict::SpecInconsistent,
            Expect::Unsupported => verdict == Verdict::Unsupported,
            Expect::NotProved => !verdict.is_pass(),
        }
    }

    /// One-word label.
    pub fn label(self) -> &'static str {
        match self {
            Expect::Proved => "proved",
            Expect::Disproved => "disproved",
            Expect::SpecInconsistent => "spec_inconsistent",
            Expect::Unsupported => "unsupported",
            Expect::NotProved => "not_proved",
        }
    }
}

/// One benchmark case.
#[derive(Debug, Clone)]
pub struct Case {
    /// Short name.
    pub name: &'static str,
    /// The unit, in `ais-core`.
    pub source: &'static str,
    /// Source language (admission tests use something else).
    pub language: &'static str,
    /// Property kind.
    pub kind: PropertyKind,
    /// Property expression, if any.
    pub expr: Option<&'static str>,
    /// What the primitive should do.
    pub expect: Expect,
    /// What shape the case probes, for the report.
    pub shape: &'static str,
}

/// One case's measured outcome.
#[derive(Debug, Clone)]
pub struct CaseResult {
    /// The case.
    pub case: Case,
    /// Verdict reached.
    pub verdict: Verdict,
    /// Failure class, when not proved.
    pub class: Option<crate::types::FailureClass>,
    /// Elaborated obligations.
    pub obligations: usize,
    /// Solver milliseconds.
    pub solver_ms: u64,
    /// Rounds used.
    pub rounds: usize,
    /// Whether the verdict matched the expectation.
    pub matched: bool,
}

/// The whole run.
#[derive(Debug, Clone, Default)]
pub struct BenchReport {
    /// Per-case outcomes, in corpus order.
    pub results: Vec<CaseResult>,
}

impl BenchReport {
    /// Cases whose observed verdict matched the expectation.
    pub fn matched(&self) -> usize {
        self.results.iter().filter(|r| r.matched).count()
    }

    /// Cases the primitive proved.
    pub fn proved(&self) -> usize {
        self.results
            .iter()
            .filter(|r| r.verdict == Verdict::Proved)
            .count()
    }

    /// Cases the primitive should have proved (the proof-rate denominator).
    pub fn provable(&self) -> usize {
        self.results
            .iter()
            .filter(|r| r.case.expect == Expect::Proved)
            .count()
    }

    /// One line per case, for a terminal.
    pub fn table(&self) -> String {
        let mut s = String::new();
        s.push_str("case                        shape                         expect          observed        ok\n");
        for r in &self.results {
            s.push_str(&format!(
                "{:<27} {:<29} {:<15} {:<15} {}\n",
                r.case.name,
                r.case.shape,
                r.case.expect.label(),
                r.verdict.as_str(),
                if r.matched { "yes" } else { "NO" },
            ));
        }
        s.push_str(&format!(
            "\nmatched {}/{} · proved {}/{} expected-proved · solver {} ms\n",
            self.matched(),
            self.results.len(),
            self.proved(),
            self.provable(),
            self.results.iter().map(|r| r.solver_ms).sum::<u64>()
        ));
        s
    }
}

/// Resolve the checker the bench uses: `z3` on `PATH`.
pub fn default_checker() -> Result<Checker, String> {
    Checker::resolve(SolverSpec::z3(Duration::from_secs(20)))
}

/// Run the whole corpus with the deterministic pool.
pub async fn run(checker: Checker) -> BenchReport {
    let mut results = Vec::new();
    for case in corpus() {
        let request = Request {
            source: Source {
                language: case.language.into(),
                files: vec![FileSpec {
                    path: format!("{}.ais", case.name),
                    text: Some(case.source.to_string()),
                }],
                entry: None,
            },
            property: Property {
                kind: case.kind,
                expr: case.expr.map(str::to_string),
                assumptions: Vec::new(),
                at: None,
            },
            backend: BackendSpec {
                name: "smtlib2".into(),
                binary: None,
                version: None,
            },
            budget: Budget {
                max_rounds: 2,
                solver_timeout_s: 20,
                token_budget: 0,
                total_wall_s: 300,
            },
            determinism: Determinism::default(),
            emit: Vec::new(),
        };
        let out = driver::run(request, DriverConfig::deterministic(checker.clone())).await;
        let elapsed_obligations = out
            .response
            .rounds
            .last()
            .map(|r| r.obligations_checked as usize)
            .unwrap_or(0);
        results.push(CaseResult {
            verdict: out.response.verdict,
            class: out.response.failure.as_ref().map(|f| f.class),
            obligations: elapsed_obligations,
            solver_ms: out.response.cost.solver_ms,
            rounds: out.response.rounds.len(),
            matched: case.expect.accepts(out.response.verdict),
            case,
        });
    }
    BenchReport { results }
}

/// The corpus. Each entry states its intent before it runs.
pub fn corpus() -> Vec<Case> {
    vec![
        Case {
            name: "sum_triangular",
            shape: "closed-form sum, quadratic invariant",
            language: "ais-core",
            source: r#"
fn sum(n: int) -> int
  requires n >= 0
  ensures result == n * (n + 1) / 2
{
  var r: int = 0;
  var i: int = 0;
  while (i < n)
  {
    i = i + 1;
    r = r + i;
  }
  return r;
}
"#,
            kind: PropertyKind::Postcondition,
            expr: None,
            expect: Expect::Proved,
        },
        Case {
            name: "count_up",
            shape: "counter reaches the bound",
            language: "ais-core",
            source: r#"
fn count(n: int) -> int
  requires n >= 0
  ensures result == n
{
  var i: int = 0;
  while (i < n)
  {
    i = i + 1;
  }
  return i;
}
"#,
            kind: PropertyKind::Postcondition,
            expr: None,
            expect: Expect::Proved,
        },
        Case {
            name: "count_by_two",
            shape: "stride 2, off-by-one bound",
            language: "ais-core",
            source: r#"
fn twos(n: int) -> int
  requires n >= 0
  ensures result >= n && result < n + 2
{
  var i: int = 0;
  while (i < n)
  {
    i = i + 2;
  }
  return i;
}
"#,
            kind: PropertyKind::Postcondition,
            expr: None,
            expect: Expect::Proved,
        },
        Case {
            name: "accumulate_three",
            shape: "affine invariant (s == 3i)",
            language: "ais-core",
            source: r#"
fn three(n: int) -> int
  requires n >= 0
  ensures result == 3 * n
{
  var s: int = 0;
  var i: int = 0;
  while (i < n)
  {
    i = i + 1;
    s = s + 3;
  }
  return s;
}
"#,
            kind: PropertyKind::Postcondition,
            expr: None,
            expect: Expect::Proved,
        },
        Case {
            name: "max_of_two",
            shape: "branching, no loop",
            language: "ais-core",
            source: r#"
fn max2(x: int, y: int) -> int
  ensures result >= x && result >= y
{
  var m: int = x;
  if (x < y)
  {
    m = y;
  }
  return m;
}
"#,
            kind: PropertyKind::Postcondition,
            expr: None,
            expect: Expect::Proved,
        },
        Case {
            name: "sign_split",
            shape: "two-branch postcondition",
            language: "ais-core",
            source: r#"
fn abs(x: int) -> int
  ensures result >= 0
  ensures result == x || result == 0 - x
{
  var r: int = x;
  if (x < 0)
  {
    r = 0 - x;
  }
  return r;
}
"#,
            kind: PropertyKind::Postcondition,
            expr: None,
            expect: Expect::Proved,
        },
        Case {
            name: "assert_holds",
            shape: "safety assertion inside a loop",
            language: "ais-core",
            source: r#"
fn bounded(n: int) -> int
  requires n >= 0
  ensures result == n
{
  var i: int = 0;
  while (i < n)
    invariant i >= 0 && i <= n
  {
    i = i + 1;
    assert i <= n;
  }
  return i;
}
"#,
            kind: PropertyKind::Postcondition,
            expr: None,
            expect: Expect::Proved,
        },
        Case {
            name: "termination_down",
            shape: "variant obligation",
            language: "ais-core",
            source: r#"
fn down(n: int) -> int
  requires n >= 0
  ensures result == 0
{
  var i: int = n;
  while (i > 0)
    invariant i >= 0
    variant i
  {
    i = i - 1;
  }
  return 0;
}
"#,
            kind: PropertyKind::Termination,
            expr: None,
            expect: Expect::Proved,
        },
        Case {
            name: "array_max_quantified",
            shape: "quantified array invariant",
            language: "ais-core",
            source: r#"
fn amax(a: array, n: int) -> int
  requires n > 0
  ensures forall j: int :: 0 <= j && j < n => a[j] <= result
{
  var m: int = a[0];
  var i: int = 1;
  while (i < n)
  {
    if (m < a[i])
    {
      m = a[i];
    }
    i = i + 1;
  }
  return m;
}
"#,
            kind: PropertyKind::Postcondition,
            expr: None,
            expect: Expect::Proved,
        },
        Case {
            name: "linear_search",
            shape: "quantified scan invariant",
            language: "ais-core",
            source: r#"
fn find(a: array, n: int, x: int) -> int
  requires n >= 0
  ensures result == n || a[result] == x
{
  var i: int = 0;
  while (i < n && a[i] != x)
  {
    i = i + 1;
  }
  return i;
}
"#,
            kind: PropertyKind::Postcondition,
            expr: None,
            expect: Expect::Proved,
        },
        Case {
            name: "wrong_postcondition",
            shape: "false property (real counterexample)",
            language: "ais-core",
            source: r#"
fn sum(n: int) -> int
  requires n >= 0
{
  var r: int = 0;
  var i: int = 0;
  while (i < n)
  {
    i = i + 1;
    r = r + i;
  }
  return r;
}
"#,
            kind: PropertyKind::Postcondition,
            expr: Some("result == 2 * n"),
            expect: Expect::Disproved,
        },
        Case {
            name: "vacuous_requires",
            shape: "unsatisfiable precondition",
            language: "ais-core",
            source: r#"
fn never(n: int) -> int
  requires n < 0 && n > 10
  ensures result == 0
{
  return 0;
}
"#,
            kind: PropertyKind::Postcondition,
            expr: None,
            expect: Expect::SpecInconsistent,
        },
        Case {
            name: "unsupported_language",
            shape: "language admission",
            language: "rust",
            source: "fn f(x: int) -> int { return x; }",
            kind: PropertyKind::Postcondition,
            expr: None,
            expect: Expect::Unsupported,
        },
        Case {
            name: "nested_loops",
            shape: "two loops in one body",
            language: "ais-core",
            source: r#"
fn pair(n: int, m: int) -> int
  requires n >= 0 && m >= 0
  ensures result == n * m
{
  var t: int = 0;
  var i: int = 0;
  while (i < n)
    invariant i >= 0 && i <= n && t == i * m
  {
    var j: int = 0;
    while (j < m)
      invariant j >= 0 && j <= m && t == i * m + j
    {
      j = j + 1;
      t = t + 1;
    }
    i = i + 1;
  }
  return t;
}
"#,
            kind: PropertyKind::Postcondition,
            expr: None,
            expect: Expect::Proved,
        },
    ]
}
