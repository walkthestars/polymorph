//! `polymorph proof` — the AIS surface on the substrate CLI.
//!
//! Four verbs, one contract: a request in, a verdict out, and a certificate a
//! third party can re-check without this binary's cooperation.
//!
//! * `run` — execute one request; exit 0 only on `proved`.
//! * `recheck` — re-derive and replay a certificate against its source files.
//! * `bench` — the curated deterministic corpus.
//! * `backends` — what the fragment admits, so a caller fails fast.
//!
//! The model proposer lives here, not in `polymorph-ais`: the primitive's core
//! stays free of LLM dependencies and can be audited without one.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use clap::Subcommand;

use polymorph_ais::cert::{self, Certificate};
use polymorph_ais::driver::{self, DriverConfig};
use polymorph_ais::propose::{Feedback, Proposal, ProposalContext, Proposer, ProposerError};
use polymorph_ais::solver::{Checker, SolverSpec};
use polymorph_ais::types::{Annotation, Request, Response};

/// `polymorph proof <verb>`.
#[derive(Debug, Subcommand)]
pub enum ProofCmd {
    /// Run one proof request (JSON; format in docs/PROOF.md) to a verdict.
    Run {
        /// Request JSON file (`-` reads stdin).
        #[arg(long)]
        request: PathBuf,
        /// Write the certificate body here (JSON).
        #[arg(long)]
        cert_out: Option<PathBuf>,
        /// Write the round ledger as `ais_round` events here.
        #[arg(long)]
        events_json: Option<PathBuf>,
        /// Pin the proposer/refiner model (overrides the role chains).
        #[arg(long)]
        model: Option<String>,
        /// Do not call a model at all: deterministic pool only.
        #[arg(long)]
        deterministic: bool,
        /// Bounded model-check depth used to separate "too weak" from "false".
        #[arg(long, default_value_t = 6)]
        bmc_depth: usize,
        /// Solver command line containing `{input}` (default: `POLYMORPH_AIS_SOLVER`,
        /// else `z3 -smt2 {input}`).
        #[arg(long)]
        solver: Option<String>,
        /// Print the full response JSON instead of a human summary.
        #[arg(long)]
        json: bool,
    },
    /// Re-check a certificate: digests, backend pin, re-derived payload, and
    /// every obligation.
    Recheck {
        /// Certificate JSON file.
        #[arg(long)]
        cert: PathBuf,
        /// Source file the certificate should be bound to. Repeat once per
        /// pinned file, in the certificate's pin order: files are matched to
        /// the pins by path when every pin path is present, otherwise the
        /// supplied order is used, and a single blob is taken as the joined
        /// source text.
        #[arg(long)]
        source: Vec<PathBuf>,
        /// Skip payload re-derivation: verify digests and replay the payload
        /// only. A rewritten property (or rewritten queries) with recomputed
        /// digests is not detected in this mode.
        #[arg(long)]
        digest_only: bool,
        /// Also replay every discharged obligation under a second solver
        /// (command line containing `{input}`, e.g. `cvc5 {input}`) and
        /// require agreement: a `differential` check row. Extra evidence,
        /// not a trust upgrade.
        #[arg(long)]
        differential: Option<String>,
        /// Print the check list as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Run the curated deterministic benchmark.
    Bench {
        /// Write the full report as JSON here.
        #[arg(long)]
        json: Option<PathBuf>,
    },
    /// Print the fragment's declared capabilities and limits.
    Backends,
}

/// Dispatch one `proof` subcommand.
pub async fn run(
    cmd: ProofCmd,
    overrides: &polymorph_core::config::CliOverrides,
) -> anyhow::Result<()> {
    match cmd {
        ProofCmd::Run {
            request,
            cert_out,
            events_json,
            model,
            deterministic,
            bmc_depth,
            solver,
            json,
        } => {
            run_one(
                &request,
                cert_out.as_deref(),
                events_json.as_deref(),
                model,
                deterministic,
                bmc_depth,
                solver,
                json,
                overrides,
            )
            .await
        }
        ProofCmd::Recheck {
            cert,
            source,
            digest_only,
            differential,
            json,
        } => recheck_one(&cert, &source, digest_only, differential.as_deref(), json),
        ProofCmd::Bench { json } => run_bench(json.as_deref()).await,
        ProofCmd::Backends => {
            let caps = polymorph_ais::backend::Capabilities;
            println!("backend      {}", caps.name());
            println!("languages    {}", caps.languages().join(", "));
            println!("limits");
            for l in caps.limits() {
                println!("  - {l}");
            }
            println!("solver       {}", solver_description());
            Ok(())
        }
    }
}

fn solver_description() -> String {
    match Checker::resolve(default_solver()) {
        Ok(c) => format!(
            "{} ({}) digest {}",
            c.version,
            c.binary.display(),
            c.binary_digest
        ),
        Err(e) => format!("unresolved: {e}"),
    }
}

fn default_solver() -> SolverSpec {
    let timeout = std::time::Duration::from_secs(30);
    match std::env::var("POLYMORPH_AIS_SOLVER") {
        Ok(line) if !line.trim().is_empty() => SolverSpec::from_command_line(&line, timeout)
            .unwrap_or_else(|_| SolverSpec::z3(timeout)),
        _ => SolverSpec::z3(timeout),
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_one(
    request_path: &Path,
    cert_out: Option<&Path>,
    events_json: Option<&Path>,
    model: Option<String>,
    deterministic: bool,
    bmc_depth: usize,
    solver: Option<String>,
    json: bool,
    overrides: &polymorph_core::config::CliOverrides,
) -> anyhow::Result<()> {
    let text = read_request(request_path)?;
    let mut request: Request = serde_json::from_str(&text)
        .map_err(|e| anyhow::anyhow!("request is not valid AIS JSON: {e}"))?;
    if request.budget.max_rounds == 0 {
        request.budget.max_rounds = 1;
    }
    request.validate().map_err(|e| anyhow::anyhow!("{e}"))?;

    // `--solver` wins; otherwise the same variable `recheck` reads, so a
    // certificate is re-checked under the solver that produced it.
    let solver = solver.or_else(|| {
        std::env::var("POLYMORPH_AIS_SOLVER")
            .ok()
            .filter(|line| !line.trim().is_empty())
    });
    let spec = match solver {
        Some(line) => SolverSpec::from_command_line(
            &line,
            std::time::Duration::from_secs(request.budget.solver_timeout_s.max(1)),
        )
        .map_err(|e| anyhow::anyhow!("{e}"))?,
        None => SolverSpec::z3(std::time::Duration::from_secs(
            request.budget.solver_timeout_s.max(1),
        )),
    };
    let checker = Checker::resolve(spec).map_err(|e| anyhow::anyhow!("{e}"))?;

    let mut cfg = DriverConfig::deterministic(checker);
    cfg.bmc_depth = bmc_depth.clamp(1, 32);
    if !deterministic {
        match build_llm_proposer(model, overrides).await {
            Ok(p) => cfg.proposer = Some(p),
            Err(e) => eprintln!("note: running deterministically ({e})"),
        }
    }

    let out = driver::run(request.clone(), cfg).await;
    let response = &out.response;
    if let Some(path) = events_json {
        write_events(path, &request, response, out.certificate.as_ref())?;
    }
    if let Some(path) = cert_out {
        if let Some(c) = &out.certificate {
            std::fs::write(path, serde_json::to_vec_pretty(c)?)?;
            eprintln!("certificate written to {}", path.display());
        } else {
            eprintln!(
                "no certificate: the verdict was {}",
                response.verdict.as_str()
            );
        }
    }
    if json {
        println!("{}", serde_json::to_string_pretty(response)?);
    } else {
        print_summary(response);
    }
    if response.verdict.is_pass() {
        Ok(())
    } else {
        anyhow::bail!("verdict: {}", response.verdict.as_str())
    }
}

fn read_request(path: &Path) -> anyhow::Result<String> {
    if path == Path::new("-") {
        use std::io::Read as _;
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s)?;
        return Ok(s);
    }
    std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", path.display()))
}

fn print_summary(r: &Response) {
    println!("verdict      {}", r.verdict.as_str());
    if let Some(f) = &r.failure {
        println!(
            "class        {} ({})",
            f.class.as_str(),
            if f.needs_human {
                "needs human"
            } else {
                "automatic"
            }
        );
        println!("why          {}", f.message);
        if !f.obligations.is_empty() {
            println!("obligations  {}", f.obligations.join(", "));
        }
    }
    if let Some(c) = &r.certificate {
        println!(
            "certificate  {} {} blob={}B",
            c.format,
            c.digest,
            c.blob.len()
        );
    }
    println!(
        "backend      {} {} ({})",
        r.provenance.backend, r.provenance.backend_version, r.provenance.backend_digest
    );
    println!("source       {}", r.provenance.source_digest);
    println!(
        "proposer     {} / refiner {}",
        r.provenance.proposer, r.provenance.refiner
    );
    println!(
        "cost         {} tokens ({}), {} ms solver, {} ms wall",
        r.cost.tokens,
        if r.cost.tokens_estimated {
            "estimated"
        } else {
            "measured"
        },
        r.cost.solver_ms,
        r.cost.wall_ms
    );
    if !r.rounds.is_empty() {
        println!("rounds");
        for rnd in &r.rounds {
            println!(
                "  {} {} discharged {}/{} failed={:?} class={:?}",
                rnd.round,
                rnd.proposer,
                rnd.obligations_discharged,
                rnd.obligations_checked,
                rnd.failed,
                rnd.class.map(|c| c.as_str())
            );
        }
    }
    if !r.annotations.is_empty() {
        println!("annotations");
        for a in &r.annotations {
            println!("  {} {} {}", a.target, a.kind, a.text);
        }
    }
    if !r.counterexamples.is_empty() {
        println!("counterexamples");
        for c in &r.counterexamples {
            println!(
                "  {} [{}{}] {}",
                c.obligation,
                c.class.as_str(),
                if c.witnessed {
                    ", witnessed execution"
                } else {
                    ", model only"
                },
                c.values
                    .iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
        }
    }
}

fn write_events(
    path: &Path,
    request: &Request,
    response: &Response,
    certificate: Option<&Certificate>,
) -> anyhow::Result<()> {
    let mut events = Vec::new();
    for r in &response.rounds {
        events.push(serde_json::json!({
            "type": "ais_round",
            "round": r.round,
            "proposer": r.proposer,
            "candidates": r.candidates,
            "obligations_checked": r.obligations_checked,
            "obligations_discharged": r.obligations_discharged,
            "failed": r.failed,
            "class": r.class.map(|c| c.as_str()),
            "solver_ms": r.solver_ms,
            "tokens": r.tokens,
            "tokens_estimated": r.tokens_estimated,
        }));
    }
    events.push(serde_json::json!({
        "type": "ais_finished",
        "verdict": response.verdict.as_str(),
        "inferred": response.inferred,
        "class": response.failure.as_ref().map(|f| f.class.as_str()),
        "needs_human": response.failure.as_ref().map(|f| f.needs_human).unwrap_or(false),
        "certificate_digest": certificate.map(|c| c.digest.clone()),
        "certificate_format": certificate.map(|c| c.format.clone()),
        "counterexamples": response.counterexamples.iter().map(|c| serde_json::json!({
            "obligation": c.obligation,
            "class": c.class.as_str(),
            "witnessed": c.witnessed,
            "values": c.values,
        })).collect::<Vec<_>>(),
        "cost": {
            "tokens": response.cost.tokens,
            "solver_ms": response.cost.solver_ms,
            "wall_ms": response.cost.wall_ms,
            "tokens_estimated": response.cost.tokens_estimated,
        },
        "provenance": response.provenance,
        "property": request.property,
        "source_digest": response.provenance.source_digest,
    }));
    std::fs::write(path, serde_json::to_vec_pretty(&events)?)
        .map_err(|e| anyhow::anyhow!("cannot write {}: {e}", path.display()))
}

fn recheck_one(
    cert_path: &Path,
    sources: &[PathBuf],
    digest_only: bool,
    differential: Option<&str>,
    json: bool,
) -> anyhow::Result<()> {
    let text = std::fs::read_to_string(cert_path)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", cert_path.display()))?;
    let certificate: Certificate =
        serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("not a certificate: {e}"))?;
    // One `(path, bytes)` pair per `--source`, in the supplied order; the
    // re-check matches them to the certificate's per-file pins.
    let pairs: Option<Vec<(String, Vec<u8>)>> = if sources.is_empty() {
        None
    } else {
        let mut files = Vec::new();
        for p in sources {
            let bytes = std::fs::read(p)
                .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", p.display()))?;
            files.push((p.display().to_string(), bytes));
        }
        Some(files)
    };
    let checker = Checker::resolve(default_solver()).map_err(|e| anyhow::anyhow!("{e}"))?;
    let mode = if digest_only {
        cert::RecheckMode::DigestOnly
    } else {
        cert::RecheckMode::Full
    };
    let mut outcome = cert::recheck_sources(&certificate, pairs.as_deref(), &checker, mode);
    if let Some(line) = differential {
        let timeout = std::time::Duration::from_secs(certificate.body.solver_timeout_s.max(1));
        let spec =
            SolverSpec::from_command_line(line, timeout).map_err(|e| anyhow::anyhow!("{e}"))?;
        let second = Checker::resolve(spec).map_err(|e| anyhow::anyhow!("{e}"))?;
        outcome.push_check(cert::differential(&certificate, &second));
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&outcome)?);
    } else {
        println!("recheck      {}", outcome.verdict);
        println!("certificate  {}", certificate.digest);
        println!(
            "backend      {} ({})",
            checker.version, checker.binary_digest
        );
        for c in &outcome.checks {
            println!(
                "  {} {} — {}",
                if c.ok { "ok  " } else { "FAIL" },
                c.name,
                c.detail
            );
        }
    }
    if outcome.ok {
        Ok(())
    } else {
        anyhow::bail!("certificate did not re-check")
    }
}

async fn run_bench(json: Option<&Path>) -> anyhow::Result<()> {
    let checker = polymorph_ais::bench::default_checker().map_err(|e| anyhow::anyhow!("{e}"))?;
    let report = polymorph_ais::bench::run(checker).await;
    print!("{}", report.table());
    if let Some(path) = json {
        let rows: Vec<serde_json::Value> = report
            .results
            .iter()
            .map(|r| {
                serde_json::json!({
                    "case": r.case.name,
                    "shape": r.case.shape,
                    "expect": r.case.expect.label(),
                    "verdict": r.verdict.as_str(),
                    "class": r.class.map(|c| c.as_str()),
                    "obligations": r.obligations,
                    "rounds": r.rounds,
                    "solver_ms": r.solver_ms,
                    "matched": r.matched,
                })
            })
            .collect();
        let doc = serde_json::json!({
            "matched": report.matched(),
            "cases": report.results.len(),
            "proved": report.proved(),
            "expected_proved": report.provable(),
            "solver_ms": report.results.iter().map(|r| r.solver_ms).sum::<u64>(),
            "results": rows,
        });
        std::fs::write(path, serde_json::to_vec_pretty(&doc)?)
            .map_err(|e| anyhow::anyhow!("cannot write {}: {e}", path.display()))?;
    }
    if report.matched() == report.results.len() {
        Ok(())
    } else {
        anyhow::bail!(
            "{} case(s) did not match their declared expectation",
            report.results.len() - report.matched()
        )
    }
}

// ---------------------------------------------------------------- LLM proposer

/// The on-disk model proposer: one managed turn per proposal, role-routed.
///
/// It is *advisory*. Its output is parsed as annotation text, then checked like
/// any other proposal; a confident wrong answer costs a round and nothing else.
pub struct LlmProposer {
    router: polymorph_llm::RoleRouter,
    providers: Vec<polymorph_core::config::ProviderEntry>,
    state: tokio::sync::Mutex<polymorph_llm::FallbackState>,
    propose_role: polymorph_core::Role,
    refine_role: polymorph_core::Role,
    label: String,
}

#[async_trait]
impl Proposer for LlmProposer {
    fn label(&self) -> String {
        self.label.clone()
    }

    async fn propose(&self, ctx: &ProposalContext) -> Result<Proposal, ProposerError> {
        self.turn(self.propose_role, focus_prompt(ctx, None)).await
    }

    async fn refine(
        &self,
        ctx: &ProposalContext,
        feedback: &Feedback,
    ) -> Result<Proposal, ProposerError> {
        self.turn(self.refine_role, focus_prompt(ctx, Some(feedback)))
            .await
    }
}

impl LlmProposer {
    async fn turn(
        &self,
        role: polymorph_core::Role,
        brief: String,
    ) -> Result<Proposal, ProposerError> {
        let client = polymorph_llm::LlmClient::with_fresh_conversation();
        let cancel = tokio_util::sync::CancellationToken::new();
        let mut sink = String::new();
        let mut state = self.state.lock().await;
        let (outcome, model) = client
            .stream_chat_managed(
                &self.router,
                &self.providers,
                &mut state,
                role,
                None,
                SYSTEM,
                &brief,
                &cancel,
                &mut sink,
            )
            .await
            .map_err(|e| ProposerError::Transport(e.to_string()))?;
        // Provider-reported usage when the stream carried it; otherwise an
        // estimate, flagged as one all the way to the ledger.
        let (tokens, tokens_estimated) = match outcome.usage {
            Some(usage) => (usage.total(), false),
            None => (
                estimate_tokens(SYSTEM.len() + brief.len() + outcome.text.len()),
                true,
            ),
        };
        let annotations = parse_annotations(&outcome.text)?;
        Ok(Proposal {
            annotations,
            tokens,
            tokens_estimated,
            label: format!("llm:{model}"),
        })
    }
}

fn estimate_tokens(chars: usize) -> u64 {
    // Only for a provider that reported no usage: a 4-chars-per-token
    // estimate, recorded as an estimate, never as a measurement.
    u64::try_from(chars / 4).unwrap_or(u64::MAX)
}

const SYSTEM: &str = "\
You are the PROPOSER in a verifier-judged invariant synthesis loop. A deterministic SMT checker \
decides everything; your opinion is not part of the trust chain.

Reply with ONE JSON object and nothing else:
{\"invariants\": {\"<loop id>\": [\"<expression>\", ...]}, \"variants\": {\"<loop id>\": \"<expression>\"}}

Rules:
1. Expression language: integer literals, `+ - * / %`, `== != < <= > >=`, `&& || ! =>`, \
`(forall i: int :: <body>)`, `(exists i: int :: <body>)`, array reads `a[i]`, and the special \
name `result` in postconditions. Nothing else exists — no function calls, no ghost code, no \
helper lemmas.
2. `/` and `%` are SMT-LIB2 `div`/`mod` (Euclidean). They are NOT Rust's truncating operators.
3. Every name you use must be in scope at the loop head: parameters, values assigned before the \
loop, and locals declared before it. A variable first declared inside the loop body is NOT in scope.
4. An invariant must satisfy all three: true on entry, preserved by the body, strong enough with \
the negated guard to imply the postcondition.
5. Prefer a small conjunction of facts over one clever formula. A false conjunct is detected \
immediately and costs a round.
6. Numbers are mathematical integers (unbounded). Do not reason about overflow.";

fn focus_prompt(ctx: &ProposalContext, feedback: Option<&Feedback>) -> String {
    let mut s = String::new();
    s.push_str("# UNIT\n");
    s.push_str(&ctx.source);
    s.push_str("\n# LOOPS\n");
    for l in &ctx.loops {
        s.push_str(&format!(
            "-- {} in {}: while ({}) writes [{}]\n",
            l.id,
            l.function,
            l.guard,
            l.writes.join(", ")
        ));
        for inv in &l.invariants {
            s.push_str(&format!("   existing invariant: {inv}\n"));
        }
        if let Some(v) = &l.variant {
            s.push_str(&format!("   existing variant: {v}\n"));
        }
    }
    s.push_str(&format!(
        "\n# PROPERTY ({:?})\n{}\n",
        ctx.property.kind,
        ctx.property
            .expr
            .clone()
            .unwrap_or_else(|| "(the unit's own ensures)".into())
    ));
    if let Some(f) = feedback {
        s.push_str("\n# CHECKER FEEDBACK\n");
        s.push_str(&format!("class: {}\n", f.class.as_str()));
        s.push_str(&format!("{}\n", f.message));
        if !f.failed.is_empty() {
            s.push_str(&format!("failed obligations: {}\n", f.failed.join(", ")));
        }
        if !f.counterexamples.is_empty() {
            s.push_str(
                "validated counterexamples (a run of the code that falsifies the obligation):\n",
            );
            for ce in &f.counterexamples {
                s.push_str(&format!(
                    "  {}: {}{}\n",
                    ce.obligation,
                    ce.values
                        .iter()
                        .map(|(k, v)| format!("{k}={v}"))
                        .collect::<Vec<_>>()
                        .join(" "),
                    if ce.witnessed {
                        " (real execution)"
                    } else {
                        " (model only; may be spurious)"
                    }
                ));
            }
        }
        for n in &f.notes {
            s.push_str(&format!("note: {n}\n"));
        }
        s.push_str(
            "\nRepair the annotations. If the class is `insufficient_invariant`, add a conjunct. \
             If `unsound_invariant`, the named conjunct is false: fix or drop it. Reply with the \
             full JSON object for every loop.\n",
        );
    } else {
        s.push_str("\nPropose invariants for every loop above.\n");
    }
    s
}

/// Parse the model's JSON reply into annotations. Fences and prose around the
/// object are tolerated; anything that is not the documented shape is a
/// proposer fault (retryable), never a verdict about the code.
pub fn parse_annotations(text: &str) -> Result<Vec<Annotation>, ProposerError> {
    let start = text.find('{');
    let end = text.rfind('}');
    let (Some(start), Some(end)) = (start, end) else {
        return Err(ProposerError::Malformed(
            "no JSON object in the reply".into(),
        ));
    };
    if end <= start {
        return Err(ProposerError::Malformed(
            "unbalanced JSON in the reply".into(),
        ));
    }
    let json = &text[start..=end];
    #[derive(serde::Deserialize)]
    struct Reply {
        #[serde(default)]
        invariants: std::collections::BTreeMap<String, Vec<String>>,
        #[serde(default)]
        variants: std::collections::BTreeMap<String, String>,
    }
    let reply: Reply = serde_json::from_str(json)
        .map_err(|e| ProposerError::Malformed(format!("reply is not the documented JSON: {e}")))?;
    let mut out = Vec::new();
    for (target, invs) in reply.invariants {
        for inv in invs {
            out.push(Annotation {
                target: target.clone(),
                kind: "invariant".into(),
                text: inv,
            });
        }
    }
    for (target, v) in reply.variants {
        out.push(Annotation {
            target,
            kind: "variant".into(),
            text: v,
        });
    }
    if out.is_empty() {
        return Err(ProposerError::Declined("the reply proposed nothing".into()));
    }
    Ok(out)
}

async fn build_llm_proposer(
    model: Option<String>,
    overrides: &polymorph_core::config::CliOverrides,
) -> anyhow::Result<Arc<dyn Proposer>> {
    let (user_path, project_path) = polymorph_core::config::default_paths();
    let cfg = polymorph_core::config::load_with_cli(&user_path, &project_path, overrides)?;
    if cfg.providers.is_empty() {
        anyhow::bail!("no providers configured");
    }
    let router = match &model {
        Some(m) => {
            let mut chains = std::collections::BTreeMap::new();
            chains.insert("plan".to_string(), vec![m.clone()]);
            chains.insert("review".to_string(), vec![m.clone()]);
            polymorph_llm::RoleRouter::new(chains)
        }
        None => polymorph_llm::RoleRouter::from_config(&cfg),
    };
    let label = format!(
        "llm:{}",
        router
            .chain(polymorph_core::Role::Plan)
            .first()
            .cloned()
            .unwrap_or_else(|| "unknown".into())
    );
    Ok(Arc::new(LlmProposer {
        router,
        providers: cfg.providers.clone(),
        state: tokio::sync::Mutex::new(polymorph_llm::FallbackState::default()),
        propose_role: polymorph_core::Role::Plan,
        refine_role: polymorph_core::Role::Review,
        label,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_documented_reply_and_tolerates_fences() {
        let text = "```json\n{\"invariants\":{\"loop1\":[\"i >= 0\",\"i <= n\"]},\"variants\":{\"loop1\":\"n - i\"}}\n```";
        let anns = parse_annotations(text).expect("parses");
        assert_eq!(anns.len(), 3);
        assert_eq!(anns[0].target, "loop1");
        assert_eq!(anns[2].kind, "variant");
    }

    #[test]
    fn prose_without_json_is_a_proposer_fault() {
        let err = parse_annotations("I think the invariant is i >= 0").unwrap_err();
        assert!(matches!(err, ProposerError::Malformed(_)));
    }

    #[test]
    fn wrong_shape_is_a_proposer_fault() {
        let err = parse_annotations("{\"invariants\": {\"loop1\": \"i >= 0\"}}").unwrap_err();
        assert!(matches!(err, ProposerError::Malformed(_)));
    }

    #[test]
    fn an_empty_proposal_is_a_decline() {
        let err = parse_annotations("{\"invariants\":{}}").unwrap_err();
        assert!(matches!(err, ProposerError::Declined(_)));
    }

    fn ledger_for(rounds: &[(u64, bool)]) -> Vec<serde_json::Value> {
        use polymorph_ais::types::{Cost, Round, Verdict};
        let rounds: Vec<Round> = rounds
            .iter()
            .enumerate()
            .map(|(i, (tokens, estimated))| Round {
                round: u32::try_from(i).unwrap(),
                proposer: "llm:test".into(),
                candidates: 1,
                obligations_checked: 1,
                obligations_discharged: 0,
                failed: Vec::new(),
                class: None,
                solver_ms: 5,
                tokens: *tokens,
                tokens_estimated: *estimated,
                annotations: Vec::new(),
            })
            .collect();
        let response = Response {
            verdict: Verdict::BudgetExhausted,
            inferred: false,
            annotations: Vec::new(),
            certificate: None,
            counterexamples: Vec::new(),
            cost: Cost {
                tokens: rounds.iter().map(|r| r.tokens).sum(),
                tokens_estimated: rounds.iter().any(|r| r.tokens_estimated),
                solver_ms: 10,
                wall_ms: 20,
            },
            rounds,
            provenance: Default::default(),
            failure: None,
        };
        let request: Request = serde_json::from_value(serde_json::json!({
            "source": {"language": "ais-core", "files": [{"path": "u.ais", "text": "fn f() -> int { return 0; }"}]},
            "property": {"kind": "postcondition"},
            "backend": {"name": "smtlib2"},
            "budget": {"max_rounds": 2, "solver_timeout_s": 5, "token_budget": 0, "total_wall_s": 60}
        }))
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.json");
        write_events(&path, &request, &response, None).unwrap();
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap()
    }

    #[test]
    fn measured_tokens_reach_the_ledger_unflagged() {
        // 150 = the provider's prompt + completion for the round.
        let events = ledger_for(&[(150, false)]);
        assert_eq!(events[0]["type"], "ais_round");
        assert_eq!(events[0]["tokens"], 150);
        assert_eq!(events[0]["tokens_estimated"], false);
        let finished = events.last().unwrap();
        assert_eq!(finished["type"], "ais_finished");
        assert_eq!(finished["cost"]["tokens"], 150);
        assert_eq!(finished["cost"]["tokens_estimated"], false);
    }

    #[test]
    fn one_estimated_round_flags_the_whole_cost() {
        let events = ledger_for(&[(150, false), (40, true)]);
        assert_eq!(events[0]["tokens_estimated"], false);
        assert_eq!(events[1]["tokens_estimated"], true);
        let finished = events.last().unwrap();
        assert_eq!(finished["cost"]["tokens"], 190);
        assert_eq!(finished["cost"]["tokens_estimated"], true);
    }
}
