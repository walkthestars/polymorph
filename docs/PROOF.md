# Proof gate (experimental)

The proof gate checks properties of programs written in `ais-core`, a small
imperative language. It turns "the tests pass" into "every proof obligation was
discharged by an SMT solver" for one unit, and writes a certificate that can be
re-checked later. It does not verify Rust or any other language: code in another
language has to be modelled in `ais-core` first, and the proof covers the model.

**It is experimental.** The request format, event fields and certificate format
may change between releases.

## 1. What it is and is not

What it is:

- A checker for units written in `ais-core` (section 4): integer and boolean
  scalars, integer arrays, `while` loops with invariants and variants, `if`,
  `assert`/`assume`, and function contracts (`requires`/`ensures`).
- An invariant synthesizer. Candidate loop invariants come from a built-in
  deterministic pool or, optionally, from a configured model. Every candidate is
  checked by the solver; a proposal is never trusted.
- A verdict with a reason. Anything other than `proved` carries a failure class
  that says whether to strengthen an invariant, fix the code or the property, or
  give up on the claim (section 5).
- A gate. `review drive --proof-request` re-runs the check after every fix turn
  and vetoes a pass until the unit is proved (section 7).

What it is not:

- **Not a Rust verifier.** It does not read Rust, C, or any other language. A
  request whose `source.language` is not `ais-core` is refused as `unsupported`.
  To use it on real code, write an `ais-core` model of the function you care
  about and prove the model. The proof says nothing about how faithfully the
  model matches the original code.
- **Not machine arithmetic.** Integers are unbounded mathematical integers.
  Overflow, wrapping and Rust's truncating `/` and `%` are not modelled.
- **Not a proof-term checker.** See section 6.
- **Not a postcondition inferencer.** `property.kind: "infer"` is accepted but
  proves the unit's own `ensures` exactly like `postcondition`, and the response
  reports `"inferred": false`.

## 2. Requirements

The `z3` SMT solver must be on `PATH`. It is **not** bundled with the release
binaries. Install it from the Z3 project's releases or a package manager (for
example `scoop install z3`, `brew install z3`, or `apt install z3`), then check:

```bash
z3 --version
polymorph proof backends     # the last line names the resolved solver and its sha256
```

To use a different binary, pass a command line that contains the `{input}`
placeholder (the query file path). Which override a command honours differs:

| Command | Solver used |
|---|---|
| `proof run` | `--solver "<cmd> {input}"`, otherwise `POLYMORPH_AIS_SOLVER`, otherwise `z3 -smt2 {input}` from `PATH` |
| `proof recheck`, `proof backends` | `POLYMORPH_AIS_SOLVER="<cmd> {input}"`, otherwise `z3 -smt2 {input}` |
| `review drive --proof-request` | `POLYMORPH_AIS_SOLVER`, otherwise `z3 -smt2 {input}` |
| `proof bench` | always `z3 -smt2 {input}` from `PATH` |
| MCP `synthesize_invariants` | always `z3 -smt2 {input}` from `PATH` |

A certificate produced with `--solver X` re-checks only when
`POLYMORPH_AIS_SOLVER` names the same binary, because the re-check pins the
solver binary by digest (section 6). Setting the variable for both commands
keeps them on the same solver. The primary
prover is exercised with z3; other SMT-LIB2 solvers are accepted but not
covered by the test suite in that role.

When the solver cannot be resolved:

- `proof run`, `proof recheck` and `proof bench` stop before doing any work and
  exit `1` with `solver executable "z3" was not found.` on stderr, followed by
  the install commands above. No response JSON, certificate or events file is
  written. The message also names `POLYMORPH_AIS_SOLVER`; `proof bench` always
  uses `z3` and does not read it.
- `proof backends` prints `solver       unresolved: ...` and exits `0`.
- `review drive --proof-request` refuses to start, before any model call, with
  `proof gate cannot arm: ...`. A gate that cannot run is not armed silently.
- The MCP tool returns `isError` with a body of the form
  `{"verdict": "unsupported", "error": "...", "certificate": null}`.

A solver that resolves but fails while a query is running yields the verdict
`unsupported` with class `unsupported_feature`.

The proof-gate tests run against z3 on Windows and Linux. macOS is not covered
by the test suite.

When building from source, tests that need the real solver print a `SKIPPED`
line and pass if `z3` is missing; set `POLYMORPH_REQUIRE_Z3=1` to make a missing
solver a test failure.

## 3. Quick start

Save this unit as `sum.ais`:

```text
# Sum of 1..n.
fn sum(n: int) -> int
  requires n >= 0
  ensures result == n * (n + 1) / 2
{
  var r: int = 0;
  var i: int = 0;
  while (i < n)
    invariant i >= 0 && i <= n && r == i * (i + 1) / 2
    variant n - i
  {
    i = i + 1;
    r = r + i;
  }
  return r;
}
```

And this request as `request.json`, in the same directory:

```json
{
  "source": {
    "language": "ais-core",
    "files": [{ "path": "sum.ais" }]
  },
  "property": { "kind": "postcondition" },
  "backend": { "name": "smtlib2" },
  "budget": {
    "max_rounds": 3,
    "solver_timeout_s": 20,
    "token_budget": 0,
    "total_wall_s": 120
  }
}
```

Run it from that directory, then re-check the certificate:

```bash
polymorph proof run --request request.json --deterministic \
  --cert-out cert.json --events-json events.json
polymorph proof recheck --cert cert.json --source sum.ais
```

`proof run` prints a summary and exits `0`:

```text
verdict      proved
certificate  ais-cert-v1 sha256:2d3acadf... blob=26520B
...
proposer     heuristic / refiner none
rounds
  0 heuristic discharged 31/31 failed=[] class=None
```

`proof recheck` prints one row per check and exits `0` only if all of them hold:

```text
recheck      proved
  ok   format — certificate advertises "ais-cert-v1"
  ok   self_digest — body digest matches sha256:2d3acadf...
  ok   backend — checker binary matches the pinned digest sha256:...
  ok   source_files — 1 source pins verified (digest and length), joined in pin order
  ok   source_digest — joined source hashes to sha256:32f5c1db...; certificate binds sha256:32f5c1db...
  ok   payload — 31 queries decode and match their digests
  ok   payload_rederived — 31 obligations re-derived from the source; the payload is byte-identical
  ok   precondition — 1 precondition probe(s) re-run under the pinned checker; ...
  ok   consistency — 1 invariant-consistency probe(s) re-run under the pinned checker; ...
  ok   replay — 31 discharged obligations replayed `unsat`
  ok   obligations — all 31 obligations are discharged
```

Digests depend on the exact source bytes and the solver binary, so yours will
differ. Next, change the contract to `ensures result == n * n` and run again: the
verdict is `disproved` with class `spec_bug`, the command exits `1`, and the
summary lists a witnessed counterexample (`n=6`, a real execution that violates
the property).

### Request format

| Field | Required | Meaning |
|---|---|---|
| `source.language` | yes | `ais-core` (also accepted: `ais`). Anything else is refused. |
| `source.files` | yes | 1 to 64 entries of `{ "path": ..., "text": ... }`. Without `text`, `path` is read from disk: relative to the current directory for `proof run`, relative to the drive's working directory for the gate. With `text`, `path` is only a label. Files are joined with a newline, in order. |
| `source.entry` | no | Entry function. Default: the last function in the unit. |
| `property.kind` | yes | `postcondition`, `safety`, `termination`, `infer`, or `refinement` (section 4). |
| `property.expr` | for `safety`, `refinement` | The claim, in `ais-core` expression syntax. |
| `property.assumptions` | no | Extra hypotheses, each an expression, added to the entry function's preconditions. |
| `property.at` | no | Loop id for `termination` (`loop1`, `loop2`, ... in source order). Default `loop1`. |
| `backend.name` | yes | Any non-empty string; `smtlib2` by convention. `backend.binary` and `backend.version` are accepted but not used; choose the solver as in section 2. |
| `budget.max_rounds` | no | Proposal rounds, 1 to 64. Default `8`. |
| `budget.solver_timeout_s` | no | Wall-clock cap per solver invocation, 1 to 3600. Default `30`. |
| `budget.token_budget` | no | Token cap over model calls; `0` means unlimited. Default `60000`. |
| `budget.total_wall_s` | no | Wall-clock cap for the whole request, 1 to 86400. Default `600`. |
| `determinism.seed` | no | Passed to the solver as `:random-seed`. Default `42`. |
| `determinism.temperature` | no | Recorded in provenance, not enforced. Default `"0"`. |
| `emit` | no | Any of `certificate`, `annotations`, `counterexamples`, `ledger`. Empty (the default) returns everything. A non-empty list without `ledger` drops the per-round records from the response; without `counterexamples` it drops the counterexamples. A certificate is always produced on `proved`. |

If `budget` is given, all four of its fields must be present; if `determinism`
is given, both `seed` and `temperature` must be present. `request` may be
`-` to read the JSON from stdin.

## 4. The `ais-core` language

A unit is one or more functions. Every function in the unit is checked; the
request's property applies to the entry function, and the other functions are
checked against their own annotations.

```text
fn name(p: int, flag: bool, a: array) -> int
  requires <expr>          # zero or more
  ensures <expr>           # zero or more; `result` is the return value
{
  <statements>
}
```

**Types.** `int` (unbounded integer), `bool`, and `array` (a total map from
`int` to `int`). There are no floats, strings, pointers, references, heap
objects, structs, or concurrency.

**Statements.**

| Statement | Form |
|---|---|
| Declaration | `var x: int = e;` (the initializer is optional) |
| Assignment | `x = e;` |
| Array store | `a[i] = e;` |
| Conditional | `if (c) { ... } else { ... }` (braces required; `else` optional) |
| Loop | `while (c) invariant e1 invariant e2 variant v { ... }` |
| Assertion | `assert e;` |
| Assumption | `assume e;` |
| Return | `return e;` or `return;` |

A loop may carry any number of `invariant` clauses and at most one `variant`.
Loops are named `loop1`, `loop2`, ... in source order across the whole unit.

**Expressions.** Integer literals (64-bit range in source), `true`, `false`,
variables, `a[i]`, unary `-` and `!`, and the binary operators below, from
loosest to tightest binding:

| Precedence | Operators |
|---|---|
| 1 | `=>` |
| 2 | `\|\|` |
| 3 | `&&` |
| 4 | `==` `!=` `<` `<=` `>` `>=` |
| 5 | `+` `-` |
| 6 | `*` `/` `%` |

Quantifiers bind integers only: `(forall i: int :: e)` and
`(exists i: int :: e)`. `result` names the return value in `ensures`.

`/` and `%` are SMT-LIB2 `div` and `mod`, which are **Euclidean**: `-7 / 2` is
`-4` and `-7 % 2` is `1`. They are not Rust's truncating operators. Division by
zero is unspecified in SMT-LIB2; a property that depends on it is not proved.

**Comments** start with `#` and run to the end of the line.

**Parsed but refused.** `old(e)`, function calls (`f(x)` as an expression or a
statement) and `lemma` blocks parse, and are then refused with class
`expressiveness_ceiling`. The language is single-state and proves one function
at a time, so these constructs have no meaning to encode, and they are never
approximated. `property.kind: "refinement"` is refused the same way.

**Limits.** Nesting is capped at 96 levels (a nested statement counts as 4);
deeper input is refused as `nesting limit exceeded`. A unit that elaborates to
zero proof obligations is refused as `unsupported` rather than proved.

**Property kinds.**

| Kind | What is proved |
|---|---|
| `postcondition` | The entry function's own `ensures`, plus `property.expr` if given. |
| `infer` | Same as `postcondition`; nothing is inferred. |
| `safety` | `property.expr` holds on every path that completes the entry function. |
| `termination` | The loop named by `property.at` has a `variant` that is non-negative while the guard holds and strictly decreases on each iteration. Without a `variant` (written or proposed), termination is not proved. |
| `refinement` | Refused (`expressiveness_ceiling`). |

**What is checked.** For each loop with invariant `I`, guard `G` and body `B`,
the solver must find each of these queries unsatisfiable (`unsat`):

| Obligation id | Query | Meaning |
|---|---|---|
| `loopN.initK` | precondition and path to the loop, and not `I_k` | `I_k` holds on entry |
| `loopN.preserveK` | `I` and `G` and one pass of `B`, and not `I_k` afterwards | `I_k` is preserved by the body |
| `<fn>.postK` | `I` and not `G` (and the rest of the path), and not the postcondition | the invariant on exit implies the property |
| `loopN.variant_nonneg` | `G` and not `V >= 0` | the variant stays non-negative |
| `loopN.variant_decrease` | `G` and one pass of `B`, and not `V_after < V_before` | the variant strictly decreases |
| `<fn>.assertK` | path to the assertion, and not the assertion | the assertion holds |

Two guards run before anything counts as proved, because either failure would
make every obligation `unsat` for free:

- **Precondition probe.** Each function's preconditions (with the assumptions)
  must be satisfiable. If not, the verdict is `spec_inconsistent`.
- **Invariant consistency.** Each loop's invariant conjunction must be
  satisfiable. A contradictory set is reported as `unsound_invariant`, never as
  a proof. An undecided probe is not a pass either.

**Bounded check.** When the property is not implied, the loops are unrolled up
to `--bmc-depth` times (default `6`, range 1 to 32). A violation found there is a
real execution, so the verdict is `disproved` rather than "invariant too weak".
Counterexamples carry `"witnessed": true` when they come from this check, and
`false` when they are only a model of a failed obligation (which may be
spurious). Every reported counterexample is re-evaluated without the solver
before it is shown.

## 5. Verdicts and failure classes

Only `proved` is a pass. Every other verdict carries exactly one failure class.

| Verdict | Meaning |
|---|---|
| `proved` | Every obligation discharged and a certificate was built. |
| `disproved` | A real execution violates the property. |
| `spec_inconsistent` | The preconditions and assumptions cannot all hold, so the claim is vacuous. |
| `timeout` | The solver answered `unknown` or hit its wall clock. |
| `unsupported` | The request is outside the language or the checker could not run. |
| `budget_exhausted` | Rounds, tokens or wall clock ran out before a proof. |

| Class | Typical cause | Verdict | Needs a human | What an orchestrator should do |
|---|---|---|---|---|
| `insufficient_invariant` | The property, or an `assert`, is not implied by the invariants, and the bounded check found no violation | `budget_exhausted` | no | Add or strengthen invariants (more rounds, or a model proposer), or raise `--bmc-depth` to look harder for a real bug. |
| `unsound_invariant` | An invariant fails on entry or is not preserved, or the invariant set is contradictory | `budget_exhausted` | no | Fix or drop the named invariant. Invariants written in the unit are never pruned automatically. |
| `spec_bug` | A witnessed execution violates the property | `disproved` | **yes** | Stop. The code or the property is wrong. Do not weaken the claim to make it pass. |
| `spec_bug` | The preconditions are unsatisfiable | `spec_inconsistent` | **yes** | Stop. Fix the `requires` clauses or the assumptions. |
| `expressiveness_ceiling` | `old(...)`, calls, `lemma`, or a `refinement` request | `unsupported` | **yes** | Restate the claim within the language, or do not use the gate for this property. |
| `solver_incompleteness` | Solver `unknown` or timeout | `timeout` | no | Raise `solver_timeout_s`, simplify the unit, or split the property. Not a proof and not a refutation. |
| `unsupported_feature` | Wrong language, a parse error, an undefined entry, zero obligations, or a solver failure | `unsupported` | no | Fix the request or the environment. |
| `proposer_malformed` | A proposal did not parse or named an out-of-scope variable or unknown loop | `budget_exhausted` | no | With a model proposer, retried while rounds remain. Otherwise check the unit's own annotations. |
| `budget_exhausted` | Rounds, tokens or wall clock ran out | `budget_exhausted` | no | Raise the budget or stop. |

`needs_human` is `true` for `spec_bug` and `expressiveness_ceiling`. These two
classes are never repaired automatically and never turn into `proved`. It is
also `true` in the rare case where every obligation discharged but the
certificate could not be built; that response is `unsupported` with class
`unsupported_feature`.

## 6. Trust boundary and certificates

**What `proved` means.** The SMT solver answered `unsat` for every obligation,
and the precondition and invariant-consistency probes answered `sat`. There are
no proof terms: trust ends at the solver binary. A solver bug that answers
`unsat` wrongly would produce a wrong `proved`.

| Trusted | Not trusted |
|---|---|
| The solver binary, pinned by sha256 | Proposed invariants, from the pool or a model |
| The re-checker | Any prose claim that something "is proved" |
| The property as the caller stated it | Raw solver output |
| The budget enforcement in the harness | |

**What a certificate binds.** `proof run --cert-out` writes an `ais-cert-v1`
certificate. Its `body` records:

- `property`, `entry`, `seed` and `solver_timeout_s`: the inputs the queries
  were built from;
- `source_digest` over the joined source, and `source_files` with each file's
  path, sha256 and byte length;
- `annotations`: the final invariants and variants;
- `backend`: solver name, version line, `binary_digest` (sha256 of the solver
  executable) and the command template;
- `obligations`: id, function, kind, status and the sha256 of each exact
  SMT-LIB2 query;
- `transcript_digest` over the solver's output, and a `recheck` recipe.

Next to the body, `blob` is the base64 payload of every query (`blob_digest`
covers it) and `digest` is the sha256 of the canonical body. On Windows, a
scoop launcher shim on `PATH` is followed to the real solver executable, so
the pin names the solver rather than the launcher.

The `certificate` object inside the run response (and the MCP result) carries
only the format, the `blob` and the `digest`. That is not enough to re-check a
proof; keep the file from `--cert-out`.

A certificate embeds the source paths, the annotations and queries that encode
the unit's behaviour. Treat it like the source it was derived from.

**What `recheck` re-derives.**

```bash
polymorph proof recheck --cert cert.json --source sum.ais [--source more.ais ...]
```

Pass one `--source` per pinned file. The re-check does not trust the
certificate's own claims. It:

1. verifies the format and recomputes the body digest (`format`,
   `self_digest`);
2. requires the local solver binary to hash to the pinned digest (`backend`);
3. verifies each file's digest and length, and the joined `source_digest`
   (`source_files`, `source_digest`);
4. decodes the payload and checks each query against its recorded digest
   (`payload`);
5. re-derives every obligation from the supplied source plus the recorded
   property, annotations, entry, seed and timeout, and requires the result to be
   byte-identical to the payload (`payload_rederived`);
6. re-runs the precondition and invariant-consistency probes, which must be
   `sat` (`precondition`, `consistency`);
7. replays every discharged obligation, which must be `unsat`, in the same
   sandbox as a proof run: a scratch directory, a scrubbed environment and a
   wall-clock kill (`replay`);
8. refuses a certificate whose obligation list is empty or not fully discharged
   (`obligations`).

Edited source bytes, a rewritten property, a tampered payload, a vacuous
specification, a contradictory invariant set, or a different solver binary each
fail a row. The command exits `0` only when every row holds; `--json` prints the
rows as `{ok, verdict, checks: [{name, ok, detail}]}`.

Because of step 2, a third party needs the same solver build (byte-identical
executable) to re-check a certificate.

`--digest-only` skips steps 5 and 6. It still verifies digests and replays the
payload, and adds a `mode` row that says the payload was not re-derived. In
that mode a forger who rewrites the property and recomputes the digests is not
detected. Prefer the full re-check.

**Second-solver replay.**

```bash
polymorph proof recheck --cert cert.json --source sum.ais --differential "cvc5 {input}"
```

`--differential` adds a `differential` row: every discharged obligation is
replayed under a second solver, which must also answer `unsat`. A `sat`,
`unknown`, timeout or error from the second solver fails the row and the
re-check. This is extra evidence, not a stronger trust model: two independent
solvers agreeing are harder to fool than one, but both are still trusted.

## 7. Surfaces

### CLI

```bash
polymorph proof backends                 # language, limits, resolved solver
polymorph proof bench [--json bench.json]
polymorph proof run --request req.json [--cert-out cert.json] [--events-json events.json] \
  [--use-model | --model <MODEL>] [--bmc-depth 6] [--solver "<cmd> {input}"] [--json]
polymorph proof recheck --cert cert.json --source unit.ais [--source ...] \
  [--digest-only] [--differential "<cmd> {input}"] [--json]
```

| Command | Exit `0` when |
|---|---|
| `proof run` | the verdict is `proved` with a certificate. Every other verdict exits `1`; `--json` still prints the full response. |
| `proof recheck` | every check row holds, including `differential` when requested. |
| `proof bench` | every case matches its declared expectation. |
| `proof backends` | always. |

Invalid requests, unreadable files and an unresolvable solver also exit `1`.
Full flag tables are in [CLI.md](CLI.md).

`proof bench` runs a small built-in benchmark of 14 cases with the deterministic
pool. It checks that 11 provable units (sums, counters, branching, a safety
assertion inside a loop, a termination variant, quantified array scans, nested
loops) are `proved`, and that a false postcondition is `disproved`, a vacuous
precondition is `spec_inconsistent`, and a non-`ais-core` language is
`unsupported`.

`proof run --events-json` writes a JSON array: one `ais_round` event per round,
then one `ais_finished` event.

| `type` | Keys |
|---|---|
| `ais_round` | `round`, `proposer`, `candidates`, `obligations_checked`, `obligations_discharged`, `failed`, `class`, `solver_ms`, `tokens`, `tokens_estimated` |
| `ais_finished` | `verdict`, `inferred`, `class`, `needs_human`, `certificate_digest`, `certificate_format`, `counterexamples` (`obligation`, `class`, `witnessed`, `values`), `cost` (`tokens`, `solver_ms`, `wall_ms`, `tokens_estimated`), `provenance`, `property`, `source_digest` |

`class`, `certificate_digest` and `certificate_format` are `null` when they do
not apply. `provenance` records the proposer and refiner labels, the solver
name, version and binary digest, the source digest, the seed, the temperature
and the primitive version.

### Gate in a drive

```bash
polymorph review drive --goal "keep sum provable" --artifact-file sum.ais \
  --proof-request request.json --tools --events-json events.json
```

With `--proof-request`, the drive:

- validates the request and resolves the solver before any model call, and
  refuses to start if either fails;
- starts with a seeded veto ("no proof evidence this drive"), so a first-round
  pass cannot stand before the unit has been checked;
- after every successful fix turn, runs the request (deterministic pool only,
  bounded-check depth `6`) with relative `source.files` paths resolved against
  the drive's working directory;
- turns any result other than `proved` with a certificate into a P1 finding
  with file `proof-gate:ais` that names the verdict, the class and a
  counterexample. The reviewer cannot pass the round over it. For `spec_bug`
  and `expressiveness_ceiling`, the finding says a human is needed.

The gate checks only the `ais-core` unit named in the request. If the drive's
real artifact is Rust, the request must point at a companion `.ais` model, and
keeping the model in step with the Rust is the operator's job.

The events file gains these rows. They are appended at the end of the drive,
after `finished` or `halted` and before `budget`:

| `type` | Keys | Meaning |
|---|---|---|
| `proof_round` | `round`, `verdict`, `class`, `solver_ms`, `certificate_digest` | One per gate invocation; `round` counts invocations from 1. Progress and cost, not a verdict. |
| `proof` | `verdict`, `class`, `certificate_digest`, `summary`, `certificate_source_present` | The last invocation's result. `verdict != "proved"` means the gate vetoed. |

`certificate_source_present` is checked at drive end, after any rollback:
`true` means the proved bytes are still on disk, `false` means they are not (for
example, a halted drive rolled them back, so the certificate no longer describes
the tree), and `null` means there was no certificate. The gate records only the
certificate digest; to keep a re-checkable certificate, run `proof run
--cert-out` on the final tree. `polymorph-watch` shows the gate as a proof row.
The rest of the drive event contract is in [INTERPOSER.md](INTERPOSER.md).

### MCP tool

`polymorph mcp` exposes `synthesize_invariants`. It takes a flat request and
runs `polymorph proof run --json` in a scratch directory:

| Input | Default | Range or values |
|---|---|---|
| `source_text` (required) | | `ais-core` source, at most 200000 bytes |
| `language` | `ais-core` | |
| `entry` | last function | |
| `property_kind` | `postcondition` | `postcondition`, `safety`, `termination`, `infer` |
| `property_expr` | none | required for `safety` |
| `assumptions` | `[]` | expressions |
| `property_at` | `loop1` | loop id for `termination` |
| `max_rounds` | `3` | 1 to 8 |
| `solver_timeout_s` | `20` | 1 to 300 |
| `token_budget` | `60000` | `0` = unlimited |
| `total_wall_s` | `600` | 1 to 1800 |
| `use_model` | `false` | `true` lets the configured model propose |
| `model` | none | only with `use_model: true` |
| `bmc_depth` | `6` | 1 to 32 |

The result is the full response JSON (verdict, annotations, certificate format,
blob and digest, counterexamples, rounds, cost, provenance, failure). It is a
success only when the verdict is `proved` and the child exited `0`; everything
else is returned with `isError`. The server runs one subprocess at a time. Set
`POLYMORPH_BIN` to point the server at a specific `polymorph` executable.

## 8. Determinism and cost

**Deterministic proposer.** The default candidate pool needs no API key, makes
no network call and spends no tokens. It proposes bounds, affine relations,
small polynomial equalities, triangular-number shapes, modular facts and
quantified array facts for the entry function's loops, and prunes candidates
that fail `init` or `preserve` until the set is stable. Invariants written in
the unit are always kept. With the same unit, request and solver binary, the
queries, the certificate and its digest are byte-for-byte the same: the
certificate contains no timestamps.

**Model proposer.** A configured model can propose and refine invariants. Its
output is parsed as annotation text and checked like any other proposal; a wrong
answer costs a round and nothing else. The refiner sees the failure class, the
failed obligation ids, validated counterexamples and elaboration notes, not raw
solver output. When a model is in use it replaces the pool for that request: if a model call fails, the round is recorded as `proposer_malformed`, and the pool is not used as a fallback.

Whether a model is called depends on the surface:

| Surface | Model called |
|---|---|
| `proof run` | Only with `--use-model` (the `plan` role proposes and the `review` role refines) or `--model <MODEL>` (one model for both). If no providers are configured it prints a note and runs deterministically. |
| MCP `synthesize_invariants` | Only with `use_model: true`. |
| `review drive --proof-request` | Never; the gate uses the deterministic pool. |
| `proof bench` | Never. |

`proof run` makes no model call and spends no tokens unless `--use-model` or
`--model` is passed. `--deterministic` states that default explicitly and is
refused together with either of them.

**Cost accounting.** Each round records `solver_ms` and `tokens`. Token counts
come from the provider's usage report when it sends one; otherwise they are a
characters-divided-by-four estimate, and the round sets `tokens_estimated:
true`. `cost.tokens_estimated` is `true` if any round was estimated. Bill only
against `tokens_estimated: false`. Treat `solver_ms` as approximate.

**Solver runs.** Every solver process runs in a fresh scratch directory, with a
scrubbed environment and a hard wall-clock kill at `solver_timeout_s`. While
proving, the queries of one check are batched into a single process. Each
query script records `:random-seed` (from `determinism.seed`) and `:timeout`
(from `solver_timeout_s`), so both are part of the certificate and apply when a
query is replayed on its own by `recheck`; the batched proving run applies only
a shared preamble and relies on the wall-clock kill for its time limit.

## 9. Limits

- Only `ais-core`. No Rust, C or other front end; real code must be modelled by
  hand, and the proof covers the model only.
- Unbounded integers and Euclidean `/` and `%`. No overflow, floats, heap,
  pointers, references or concurrency.
- Single-state and one function at a time: no `old(...)`, no calls, no lemmas,
  no refinement.
- No postcondition inference.
- The deterministic pool proposes only for the entry function's loops; other
  functions must carry their own invariants (a model proposer can target any
  loop).
- Trust ends at the SMT solver: no proof terms, no independent proof checker.
  `--differential` adds a second solver, not a proof.
- Re-checking needs the byte-identical solver binary that produced the
  certificate.
- Platform coverage is limited to what section 2 states.
