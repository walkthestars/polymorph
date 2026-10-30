//! `polymorph` CLI — clap 4 derive tree (Epic A4).
//!
//! Design §4 (0-master.md):
//!   polymorph → TUI interactive mode (default)
//!   polymorph exec <prompt> → headless single-turn
//!   polymorph acp → ACP stdio server
//!   polymorph serve → REST API server (Epic W)
//!   polymorph sessions / config / providers / models / update-catalog
//!
//! Chunk 1: tree parses + dispatches to stubs. Real loop wiring is A3/Epic B.

use clap::{Args, CommandFactory, Parser, Subcommand};
use clap_complete::{generate, Shell};

mod tracing_setup;

#[derive(Debug, Parser)]
#[command(
    name = "polymorph",
    version,
    about = "Execution substrate for AI coding harnesses"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Generate shell completions for the given shell and exit.
    #[arg(long, value_enum)]
    completions: Option<Shell>,

    /// Session database path (default: ./.polymorph/sessions.db).
    #[arg(long, global = true)]
    db: Option<std::path::PathBuf>,

    /// Force `options.debug` on (highest config precedence: beats env/files).
    /// Absent = leave the merged file+env value untouched (Epic D1f).
    #[arg(long, global = true, action = clap::ArgAction::SetTrue)]
    debug: bool,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Headless single-turn: run one prompt, stream result, exit.
    Exec {
        /// The prompt to run.
        prompt: String,
        /// Override the model (default: head of the `default` role chain).
        #[arg(long)]
        model: Option<String>,
        /// Override the system prompt (default: Polymorph engineering brief).
        #[arg(long)]
        system: Option<String>,
        /// Path scope for D1b chain pins (unpinned paths use the role chain).
        #[arg(long)]
        scope: Option<String>,
    },
    /// MCP stdio server: one bounded drive tool for Claude Code, Codex, Cursor, or any MCP client.
    Mcp {},
    /// Session management (Epic C).
    Sessions {
        #[command(subcommand)]
        cmd: SessionsCmd,
    },
    /// Config management (Epic D).
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
    /// Provider management (Epic D).
    Providers {
        #[command(subcommand)]
        cmd: ProvidersCmd,
    },
    /// Model catalog (Epic D).
    Models {
        #[command(subcommand)]
        cmd: ModelsCmd,
    },
    /// Subagent fan-out demo (Epic E: echo runners, no LLM).
    Delegate {
        #[command(subcommand)]
        cmd: DelegateCmd,
    },
    /// Reviewer-loop demos and verdict checks (Epic F: scripted, no LLM).
    Review {
        #[command(subcommand)]
        cmd: ReviewCmd,
    },
    /// Model-output eval: run one live turn against an expected end state
    /// (Epic F5b: key-gated single attempt, scored by the F5a scorer).
    Eval {
        #[command(subcommand)]
        cmd: EvalCmd,
    },
    /// Refresh the models.dev capability catalog.
    UpdateCatalog {
        /// Override the registry URL (default: models.dev; tests use a mock).
        #[arg(long)]
        url: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum SessionsCmd {
    List,
    Create { title: String },
    Switch { id: String },
    Delete { id: String },
}

#[derive(Debug, Subcommand)]
enum ConfigCmd {
    Show,
    Set { key: String, value: String },
}

#[derive(Debug, Subcommand)]
enum ProvidersCmd {
    List,
    Add { name: String },
}

#[derive(Debug, Subcommand)]
enum ModelsCmd {
    List,
}

#[derive(Debug, Subcommand)]
enum DelegateCmd {
    /// Fan out N echo children on one task, await all, print typed results.
    /// No LLM: proves the bus lifecycle (spawn/steer/stop/fan-out/worktrees).
    /// With `--repo`, children work in real git worktrees (else in-place temp).
    Run {
        /// How many children (1–8).
        #[arg(long, default_value_t = 2)]
        children: u32,
        /// The brief every child receives (clean context: ONLY this).
        #[arg(long)]
        task: String,
        /// Role for budget routing (default: `default`; smol|tiny run smol).
        #[arg(long)]
        role: Option<String>,
        /// Git repo for worktree-isolated children (refused when not a repo).
        #[arg(long)]
        repo: Option<std::path::PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
enum ReviewCmd {
    /// Run the reviewer loop over scripted reviewer/executor doubles.
    /// No LLM: proves verdict + convergence + mode wiring (Epic F1d).
    Demo {
        /// Loop mode: plan (critique a plan) or act (critique an artifact).
        #[arg(long, default_value = "plan")]
        mode: String,
        /// Scripted behavior: pass | fail-once | never | survivor.
        #[arg(long, default_value = "fail-once")]
        script: String,
    },
    /// Parse + validate a verdict JSON file (exit 1 names the fault).
    Check {
        /// Path to the verdict JSON file.
        path: std::path::PathBuf,
    },
    /// Run the live reviewer loop over LLM turns (needs key). Both modes live
    /// (F2b-part2): plan critiques a plan document, act critiques an artifact.
    /// Reviewer + fixer are Bus children; executor rewrites TEXT in memory
    /// (no tool calls yet — F3; the artifact file is read-only input, never
    /// written back).
    Run {
        #[command(flatten)]
        args: Box<ReviewRunArgs>,
    },
    /// Drive a goal to a verdict with machine-readable events (needs key).
    /// `run` + `--events-json` + drive verdict block: goal-as-text in (no
    /// ingestion — softened charter), `DriveEvent` vec JSON out (the TUI
    /// machine seam), human board on stdout. DAG pre-pass explicitly out
    /// (next slice); merge-back explicitly out (G machinery, charter whole).
    Drive {
        #[command(flatten)]
        args: Box<ReviewRunArgs>,
        /// Write the DriveEvent vec JSON here (drive machine seam).
        #[arg(long)]
        events_json: Option<std::path::PathBuf>,
    },
    /// Run the SAME seeded case through two live reviewer arms and compare
    /// (F4b: same-model vs diverse, TBM-15 live half). Arm A reviews on
    /// `--review-model-a`, arm B on `--review-model-b`; both fixers share
    /// the mode executor chain text-only (no `--tools`: the reviewer is the
    /// isolated variable, and spend stays bounded). Needs key, like `run`.
    EvalAb {
        #[command(flatten)]
        args: Box<ReviewEvalAbArgs>,
    },
    /// Run the admitted agent-bench corpus task-by-task through the live
    /// loop and/or single-shot arms and score hidden tests (F6: bench
    /// runner; first TBM-06 live datum). Each task runs in a pid-scoped
    /// temp copy (InPlace-on-copies — no merge-back exists yet, so worktree
    /// fixes would evaporate before scoring). Needs key, like `run`.
    Bench {
        #[command(flatten)]
        args: Box<ReviewBenchArgs>,
    },
    /// Re-score recorded bench outcomes offline and gate the board. File or
    /// dir (dir = sorted `*.bench.json`); red gate bails non-zero (F5c
    /// precedent — CI must fail the build, not print red). No key.
    BenchCheck {
        /// Recording file or dir of `*.bench.json` recordings.
        #[arg(long)]
        path: std::path::PathBuf,
        /// Minimum resolve rate to hold the gate green (default 1.0).
        #[arg(long, default_value_t = 1.0)]
        min_rate: f64,
    },
    /// Show a PLAN.md file as a task board (parse + validate; exit 1 names
    /// the fault with its line). Offline, no key (Epic G surfacing).
    /// With `--session`, the board shows the session's saved plan graph
    /// (chunk 51; title still comes from the file, checkpoint precedent)
    /// falling back to the file when the session holds no plan.
    PlanShow {
        /// Path to the PLAN.md file (read-only; always parsed for the title).
        #[arg(long)]
        plan: std::path::PathBuf,
        /// Session id whose saved plan graph replaces the file graph.
        #[arg(long)]
        session: Option<String>,
    },
    /// Drive a PLAN.md file through per-task reviewer loops with executor
    /// write-back (Epic G drive slice, chunk 48). Text-only plan fixer;
    /// needs key, like `run`. Never overwrites the input (`--out` REQUIRED
    /// for the updated PLAN.md); `--checkpoint` JSON resumes/saves run
    /// state (opt-in). Exit 0 on a completed drive (the report judges, run
    /// precedent — not a CI gate).
    PlanDrive {
        #[command(flatten)]
        args: Box<ReviewPlanArgs>,
    },
}

/// `review run` flags (flattened into the `Run` variant, boxed: the variant
/// outgrew clippy's `large_enum_variant` budget as flags accumulated, and
/// only indirection shrinks a variant — nesting alone never does).
#[derive(Debug, Args)]
struct ReviewRunArgs {
    /// Goal the reviewer holds the artifact to.
    #[arg(long)]
    goal: String,
    /// Path to the artifact file under review (read-only; its path is
    /// shared with the reviewer so findings name real files). Required
    /// unless `--artifact-files` or `--diff` is given; with either, it is
    /// the first member of the artifact set.
    #[arg(long, required_unless_present_any = ["artifact_files", "diff"])]
    artifact_file: Option<std::path::PathBuf>,
    /// Multi-file artifact set (docket #5), repeatable: a path, or a glob
    /// (`*?[{`; matched against workdir-relative paths, gitignore-aware,
    /// re-expanded every round) graded together with `--artifact-file`
    /// and `--diff` files under one ARTIFACT FILES UNDER REVIEW section,
    /// each with a path header and line numbers. A glob matching nothing
    /// refuses the run. With `--repo`, read from the drive's worktree.
    #[arg(long = "artifact-files", value_name = "PATH|GLOB")]
    artifact_files: Vec<String>,
    /// Worktree-diff review (docket #5): grade every file the workdir
    /// changed vs a git base (default HEAD) — modified, added, deleted,
    /// untracked-not-ignored — re-read after each fix turn, so files the
    /// fixer touches join the set. Small files in full, large ones as a
    /// diff vs the base. Needs git and a work tree; with `--repo`, the
    /// drive's worktree, and the default base is the drive's start commit.
    #[arg(
        long,
        value_name = "BASE",
        num_args = 0..=1,
        default_missing_value = "HEAD"
    )]
    diff: Option<String>,
    /// Acceptance criteria text (may be empty).
    #[arg(long, default_value = "")]
    criteria: String,
    /// Loop mode: act (artifact) or plan (plan document, PLAN.md convention).
    #[arg(long, default_value = "act")]
    mode: String,
    /// Path scope for D1b chain pins (unpinned paths use the role chain).
    #[arg(long)]
    scope: Option<String>,
    /// Git repo to drive in isolation (refused when not a repo): the whole
    /// drive runs in ONE detached worktree of its HEAD, removed when the
    /// drive ends (passed or not); the repo's own tree and index are never
    /// written. Artifact paths are repo-relative.
    #[arg(long)]
    repo: Option<std::path::PathBuf>,
    /// Review-pack file (default: muse-spark-1.3 review pack, read-only).
    #[arg(long, default_value = "prompts/muse/muse-spark-1.3/review.md")]
    review_pack: std::path::PathBuf,
    /// Executor-pack file for act mode (default: glm-5.3 act pack).
    /// Ignored in plan mode (see --plan-pack).
    #[arg(long, default_value = "prompts/glm/glm-5.3/act.md")]
    exec_pack: std::path::PathBuf,
    /// Executor-pack file for plan mode (default: glm-5.3 plan pack).
    /// Ignored in act mode (see --exec-pack).
    #[arg(long, default_value = "prompts/glm/glm-5.3/plan.md")]
    plan_pack: std::path::PathBuf,
    /// Max reviewer→executor rounds (spend guard; surviving-strike
    /// threshold stays at the LoopConfig default of 3). A harness-seeded
    /// CREATE round 1 (see `--no-create-seed`) counts as a round.
    #[arg(long, default_value_t = 3)]
    max_rounds: u32,
    /// Let the fixer wield tools (read/grep/glob/write/edit/bash) through
    /// the registry, Gate-enforced. Off by default: text-only fixing stays.
    #[arg(long, action = clap::ArgAction::SetTrue)]
    tools: bool,
    /// Max model→tools→results feed rounds per fix turn (spend guard for
    /// `--tools`; reviewer→executor rounds stay capped by `--max-rounds`).
    /// Up to `[agent] max_bonus_tool_rounds` (default 3) verification
    /// rounds follow while each round still mutates or verifies.
    #[arg(long, default_value_t = 30)]
    max_tool_rounds: u32,
    /// Opt-in LSP error gate (docket #3; needs `--tools` and a configured
    /// `[[lsp_servers]]` entry): a passing verdict becomes a fail while
    /// the last fix turn left error-severity LSP diagnostics on files it
    /// mutated. Also enabled by `[agent] lsp_error_gate = true`. Off by
    /// default: diagnostics stay evidence + warn-only (chunk 99).
    #[arg(long, action = clap::ArgAction::SetTrue)]
    lsp_error_gate: bool,
    /// Read-only reviewer tools (docket #4): the reviewer may `read`/
    /// `grep`/`glob` the drive's workdir (plus `lsp_hover`/`lsp_definition`
    /// /`lsp_references` when `[[lsp_servers]]` are configured) for up to
    /// `[agent] reviewer_tool_rounds` (default 4) tool rounds, then answers
    /// with the usual JSON verdict. Harness allowlist (never write/edit/
    /// bash/MCP, whatever `[permissions]` allow; its deny list still
    /// applies). A finding on a file neither in the brief nor inspected is
    /// dropped. Also enabled by `[agent] reviewer_tools = true`. With
    /// `--repo`, the drive's worktree. Off by default: the reviewer stays
    /// one text turn.
    #[arg(long, action = clap::ArgAction::SetTrue)]
    reviewer_tools: bool,
    /// Harness verify command (docket #2), repeatable: after every fix
    /// turn the harness runs each in order in the drive's workdir (stopping
    /// at the first failure) via the platform shell, BEFORE the reviewer.
    /// Results reach the reviewer as evidence; a nonzero exit, timeout or
    /// spawn failure turns a passing verdict into a fail fed back to the
    /// fixer. Operator-trusted: NOT checked by the fixer's tool Gate.
    /// Replaces `[agent] verify_cmds` when given.
    #[arg(long = "verify-cmd", value_name = "CMD")]
    verify_cmd: Vec<String>,
    /// Per-command timeout for the verify gate in seconds (default:
    /// `[agent] verify_timeout_secs`, 600). Expiry kills the process tree.
    #[arg(long, value_name = "SECS")]
    verify_timeout_secs: Option<u64>,
    /// Ask the reviewer in round 1 of a CREATE job too (docket #7 opt-out).
    /// By default, when the artifact file is missing and no content was
    /// given, the harness answers round 1 with the creation finding the
    /// reviewer is prescribed to emit (no reviewer call; the round still
    /// counts toward `--max-rounds`) and the fixer starts at once. An
    /// artifact set seeds only when every member is a missing declared
    /// path and the diff (if any) is empty. Also off with `[agent]
    /// create_seed = false`.
    #[arg(long, action = clap::ArgAction::SetTrue)]
    no_create_seed: bool,
    /// What a non-passing in-place run does with the files the fixer's
    /// `write`/`edit` calls changed (docket #8; `--tools`, not `--repo`):
    /// `rollback` restores each to its pre-drive bytes (deleting files and
    /// empty dirs the drive created) unless someone changed it after the
    /// drive's last write (a reported conflict, left as-is), archiving the
    /// drive's versions under `.polymorph/attempts/`; also on Ctrl-C.
    /// `keep` leaves the tree as the drive left it. Default: `[agent]
    /// drive_on_fail` (rollback) for `drive`, keep for `run`. Files a
    /// fixer `bash` command changed are rolled back only where change
    /// detection saw it and git proves the original; the rest are reported
    /// (`NOT RESTORABLE` / `LEFT BEHIND` via git status).
    #[arg(long = "on-fail", value_name = "MODE", value_parser = ["rollback", "keep"])]
    on_fail: Option<String>,
}

/// `review eval-ab` flags (boxed into the variant: chunk-12
/// `large_enum_variant` precedent — nesting alone never shrinks variants).
#[derive(Debug, Args)]
struct ReviewEvalAbArgs {
    /// Goal both arms hold the artifact to.
    #[arg(long)]
    goal: String,
    /// Path to the artifact file under review (read-only; shared with both
    /// reviewers so findings name real files).
    #[arg(long)]
    artifact_file: std::path::PathBuf,
    /// Acceptance criteria text (may be empty).
    #[arg(long, default_value = "")]
    criteria: String,
    /// Loop mode for both arms: act (artifact) or plan (plan document).
    #[arg(long, default_value = "act")]
    mode: String,
    /// Path scope for D1b chain pins (both arms; unpinned paths use chains).
    #[arg(long)]
    scope: Option<String>,
    /// Review-pack file (default: muse-spark-1.3 review pack, read-only).
    #[arg(long, default_value = "prompts/muse/muse-spark-1.3/review.md")]
    review_pack: std::path::PathBuf,
    /// Executor-pack file for act mode (default: glm-5.3 act pack).
    /// Ignored in plan mode (see --plan-pack).
    #[arg(long, default_value = "prompts/glm/glm-5.3/act.md")]
    exec_pack: std::path::PathBuf,
    /// Executor-pack file for plan mode (default: glm-5.3 plan pack).
    /// Ignored in act mode (see --exec-pack).
    #[arg(long, default_value = "prompts/glm/glm-5.3/plan.md")]
    plan_pack: std::path::PathBuf,
    /// Reviewer model for arm A (bare names qualify through claiming
    /// entries, chunk-13 `qualify_model`; qualified specs pass through).
    #[arg(long)]
    review_model_a: String,
    /// Reviewer model for arm B (same resolution as --review-model-a).
    #[arg(long)]
    review_model_b: String,
    /// Max reviewer→executor rounds PER ARM (spend guard: worst case is
    /// 2 arms × max_rounds rounds × 2 turns; default 2 bounds a probe to
    /// 8 turns. Surviving-strike threshold stays at the LoopConfig
    /// default of 3).
    #[arg(long, default_value_t = 2)]
    max_rounds: u32,
}

/// `review bench` flags (boxed into the variant: chunk-12
/// `large_enum_variant` precedent — nesting alone never shrinks variants).
#[derive(Debug, Args)]
struct ReviewPlanArgs {
    /// PLAN.md input file (read-only; never overwritten — see --out).
    /// Always parsed: the title comes from the file even on resume
    /// (checkpoint precedent — the file is the human artifact).
    #[arg(long)]
    plan: std::path::PathBuf,
    /// Where to write the updated PLAN.md (REQUIRED: the input is never
    /// overwritten by default).
    #[arg(long)]
    out: std::path::PathBuf,
    /// Checkpoint JSON path (opt-in): resume from it when it exists, save
    /// the end state there when the drive completes.
    #[arg(long)]
    checkpoint: Option<std::path::PathBuf>,
    /// Session id for plan persistence (chunk 51): resume the graph from
    /// the session store when no checkpoint file applies, and save the end
    /// state there when the drive completes. Checkpoint files keep working.
    #[arg(long)]
    session: Option<String>,
    /// Max reviewer→executor rounds per task (spend guard).
    #[arg(long, default_value_t = 2)]
    max_rounds: u32,
    /// Review-pack file (default: muse-spark-1.3 review pack, read-only).
    #[arg(long, default_value = "prompts/muse/muse-spark-1.3/review.md")]
    review_pack: std::path::PathBuf,
    /// Executor-pack file for plan mode (default: glm-5.3 plan pack).
    #[arg(long, default_value = "prompts/glm/glm-5.3/plan.md")]
    plan_pack: std::path::PathBuf,
}

/// `review bench` flags (boxed into the variant: chunk-12
/// `large_enum_variant` precedent — nesting alone never shrinks variants).
#[derive(Debug, Args)]
struct ReviewBenchArgs {
    /// Corpus TOML (default: the admitted F6 fixture set; read-only input).
    #[arg(
        long,
        default_value = "crates/polymorph-review/tests/agent_bench/agent_bench_corpus.toml"
    )]
    corpus: std::path::PathBuf,
    /// Record dir (REQUIRED: a bench run that leaves no replayable artifact
    /// failed its purpose): one `*.bench.json` per task × arm. Created when
    /// missing.
    #[arg(long)]
    record: std::path::PathBuf,
    /// Score only this task id (bounded runs: probes without full spend).
    #[arg(long)]
    only: Option<String>,
    /// Arms to run: `loop` (reviewer loop, tools on) | `single` (one Act
    /// turn, tools on — tools held constant so the delta prices REVIEW) |
    /// `both` (default).
    #[arg(long, default_value = "both")]
    arm: String,
    /// Max reviewer→executor rounds per loop case (spend guard).
    #[arg(long, default_value_t = 2)]
    max_rounds: u32,
    /// Max model→tools→results feeds per fix turn / single turn.
    #[arg(long, default_value_t = 2)]
    max_tool_rounds: u32,
    /// Minimum resolve rate to hold the gate green (default 1.0).
    #[arg(long, default_value_t = 1.0)]
    min_rate: f64,
    /// Review-pack file (default: muse-spark-1.3 review pack, read-only).
    #[arg(long, default_value = "prompts/muse/muse-spark-1.3/review.md")]
    review_pack: std::path::PathBuf,
    /// Executor-pack file (default: glm-5.3 act pack, read-only).
    #[arg(long, default_value = "prompts/glm/glm-5.3/act.md")]
    exec_pack: std::path::PathBuf,
    /// Tool profile: `scoped-v1` (manifest-scoped writes, cargo-only shell)
    /// | `scoped-v1-bash` (same writes, bare-bash shell — discriminator for
    /// shell-phrasing vs systematic tool failure). Stamped on records.
    #[arg(long, default_value = "scoped-v1")]
    tool_profile: String,
    /// Tools-off axis (chunk 41): run the SINGLE arm with no tools
    /// (text-only control — prices tooling with review held constant).
    /// Requires `--arm single` (loop never sees this flag); records stamp
    /// `{profile}+single-notools` so boards stay attributable.
    #[arg(long, action = clap::ArgAction::SetTrue)]
    single_no_tools: bool,
    /// Loop-worktree mode (chunk 44): run the LOOP fixer in per-turn kept
    /// Branch worktrees merged back into the case repo (prices isolation
    /// with tools held constant). Requires a loop arm (`--arm loop` or
    /// `both`); records stamp `{profile}+loop-worktree` so boards stay
    /// attributable. Default off (InPlace loop — c39-c43 boards comparable).
    #[arg(long, action = clap::ArgAction::SetTrue)]
    loop_worktree: bool,
    /// Preamble ablation (chunk 45): `full` (default, chunk-39 block) |
    /// `minimal` (fence-ban-only — prices the example + op-shape lines).
    /// Records stamp `{profile}+preamble-min` so boards stay attributable.
    #[arg(long, default_value = "full")]
    preamble: String,
}

/// `eval` subcommands (Epic F5b/F5c). `run` is the only live one (needs
/// key); `check` replays `--record` JSONs offline (CI gate, no key).
#[derive(Debug, Subcommand)]
enum EvalCmd {
    /// Run one live model turn against an expected end state and score it.
    /// Single attempt on `--model` (exec-override precedent: qualified +
    /// injected, no fallback — fallback would blur attribution). One case
    /// per invocation; batching is Chunk 21's sweep (later).
    Run {
        #[command(flatten)]
        args: Box<EvalRunArgs>,
    },
    /// Re-score recorded model outputs offline and gate the board.
    /// File or dir (dir = sorted `*.eval.json`); red gate bails non-zero
    /// (`review check` precedent — CI must fail the build, not print red).
    Check {
        #[command(flatten)]
        args: Box<EvalCheckArgs>,
    },
    /// Sweep the B6 edit corpus live: one model turn per scorable task,
    /// one v2 `*.eval.json` per case for `eval check` to gate. Single
    /// attempt per case on `--model` (no fallback — attribution over
    /// robustness). Rejection tasks skip NAMED (byte-compare cannot score
    /// rejection). Needs provider auth like `exec`.
    Sweep {
        #[command(flatten)]
        args: Box<EvalSweepArgs>,
    },
}

/// `eval run` flags (boxed into the variant: chunk-12 `large_enum_variant`
/// precedent — nesting alone never shrinks variants).
#[derive(Debug, Args)]
struct EvalRunArgs {
    /// Natural-language change request (hand-written per sampled task).
    #[arg(long)]
    instruction: String,
    /// Path to the initial file (read-only input; scored copy runs isolated).
    #[arg(long)]
    input: std::path::PathBuf,
    /// Path to the expected end-state file (byte-exact comparison).
    #[arg(long)]
    expected: std::path::PathBuf,
    /// Subject model (REQUIRED: eval must name its subject; no chain default).
    /// Bare names qualify through claiming entries (chunk-13 path).
    #[arg(long)]
    model: String,
    /// System-pack file (default: glm-5.3 edit pack, read-only).
    #[arg(long, default_value = "prompts/glm/glm-5.3/edit.md")]
    system_pack: std::path::PathBuf,
    /// Eval format arm (default `hashline`): `str-replace` scores the
    /// baseline rival through its own pack + scorer (chunk 28; the TBM-05
    /// delta). The default `--system-pack` follows this flag unless
    /// overridden explicitly (see `resolve_system_pack`).
    #[arg(long, default_value = "hashline")]
    format: String,
    /// Optional record dir: writes one JSON recording (case + output +
    /// outcome) for CI replay. Created when missing.
    #[arg(long)]
    record: Option<std::path::PathBuf>,
}

/// `eval check` flags (boxed into the variant: chunk-12 `large_enum_variant`
/// precedent — nesting alone never shrinks variants).
#[derive(Debug, Args)]
struct EvalCheckArgs {
    /// Recording file or dir of `*.eval.json` recordings (read-only inputs;
    /// every case scores in its own tempdir, auto-removed).
    #[arg(long)]
    path: std::path::PathBuf,
    /// Minimum pass rate to hold the gate green (default 1.0: the
    /// edit-corpus 100% precedent — one regression goes red, never hides
    /// inside a percentage).
    #[arg(long, default_value_t = 1.0)]
    min_rate: f64,
}

/// `eval sweep` flags (boxed into the variant: chunk-12 `large_enum_variant`
/// precedent — nesting alone never shrinks variants).
#[derive(Debug, Args)]
struct EvalSweepArgs {
    /// Corpus TOML (default: the committed B6 fixture; read-only input).
    #[arg(
        long,
        default_value = "crates/polymorph-tools/tests/eval_b6/edit_corpus.toml"
    )]
    corpus: std::path::PathBuf,
    /// Subject model (REQUIRED: eval must name its subject; no chain default).
    /// Bare names qualify through claiming entries (chunk-13 path).
    #[arg(long)]
    model: String,
    /// Record dir (REQUIRED: a sweep that leaves no CI artifact failed its
    /// purpose): one `*.eval.json` per scored case. Created when missing.
    #[arg(long)]
    record: std::path::PathBuf,
    /// System-pack file (default: glm-5.3 edit pack, read-only).
    #[arg(long, default_value = "prompts/glm/glm-5.3/edit.md")]
    system_pack: std::path::PathBuf,
    /// Eval format arm (default `hashline`): `str-replace` sweeps the
    /// baseline rival (own pack default, untagged prompts, str scorer +
    /// D1 named skips for deletion/rename shapes). Records stamp it;
    /// `eval check` dispatches per record.
    #[arg(long, default_value = "hashline")]
    format: String,
    /// Score only this task id (bounded runs: proofs without full spend).
    #[arg(long)]
    only: Option<String>,
    /// Max attempts per case (default 2: one retry; 1 reproduces the
    /// single-shot behavior). Retries keep the same model with fault
    /// feedback (no fallback — attribution preserved); the board scores
    /// final attempts, first-attempt board prints separately (pass@k).
    #[arg(long, default_value_t = 2)]
    max_attempts: u32,
    /// Repeat the whole matrix N times (default 1 = chunk-24 behavior).
    /// Run N records into `<record>/run-N/` (each replayable by `check`
    /// as-is); per-task pass rates + pooled board print at the end.
    #[arg(long, default_value_t = 1)]
    runs: u32,
    /// Minimum pass rate to hold the gate green (default 1.0).
    #[arg(long, default_value_t = 1.0)]
    min_rate: f64,
}

/// Explicit runtime instead of `#[tokio::main]`, for a bounded shutdown.
/// Dropping a runtime waits for every blocking task, and on Windows Tokio
/// reads child pipes on blocking threads: one stranded grandchild holding a
/// pipe (a wrapper-launched LSP server) kept `review drive` alive for an hour
/// after it printed its verdict. `async_main` still returns normally, so log
/// guards and async destructors run first; only work still stuck after that
/// is abandoned after [`SHUTDOWN_GRACE`].
fn main() -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(async_main());
    runtime.shutdown_timeout(SHUTDOWN_GRACE);
    result
}

const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

async fn async_main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    if let Some(shell) = cli.completions {
        let mut cmd = Cli::command();
        generate(shell, &mut cmd, "polymorph", &mut std::io::stdout());
        return Ok(());
    }

    // Split logging (Epic C4): in TUI mode (bare `polymorph`), stderr layer is disabled
    // so background crate warnings never corrupt the raw alternate terminal screen.
    // In CLI modes, stderr ANSI prints for humans; JSON rolling file logs for forensics.
    let (subscriber, _log_guards) =
        tracing_setup::build_subscriber(&tracing_setup::default_log_dir(), cli.command.is_some())?;
    tracing::subscriber::set_global_default(subscriber)?;

    // Hoisted before the by-value `cli.command` match: afterwards `cli` is
    // partially moved, so whole-`cli` borrows must not appear in the arms
    // (traps #5: clone-then-move; disjoint field use like `cli.db` stays fine).
    let overrides = cli_overrides(&cli);
    match cli.command {
        None => {
            // Substrate CLI: no interactive surface. Bare invocation prints
            // the command surface and exits cleanly.
            use clap::CommandFactory;
            Cli::command().print_help()?;
            println!();
            Ok(())
        }
        Some(Commands::Exec {
            prompt,
            model,
            system,
            scope,
        }) => run_exec(prompt, model, system, scope, overrides).await,
        Some(Commands::Mcp {}) => polymorph_mcp::serve_stdio().await,
        Some(Commands::Sessions { cmd }) => {
            let db = cli.db.clone().unwrap_or_else(default_db_path);
            run_sessions(cmd, &db).await
        }
        Some(Commands::Config { cmd }) => {
            let (user_path, project_path) = polymorph_core::config::default_paths();
            run_config(cmd, &user_path, &project_path, &overrides)
        }
        Some(Commands::Providers { cmd }) => {
            let (user_path, project_path) = polymorph_core::config::default_paths();
            let cfg = polymorph_core::config::load_with_cli(&user_path, &project_path, &overrides)?;
            run_providers(cmd, &cfg)
        }
        Some(Commands::Models { cmd }) => run_models(cmd),
        Some(Commands::Delegate { cmd }) => run_delegate(cmd).await,
        Some(Commands::Review { cmd }) => {
            let db = cli.db.clone().unwrap_or_else(default_db_path);
            run_review(cmd, &overrides, &db).await
        }
        Some(Commands::Eval { cmd }) => run_eval(cmd, &overrides).await,
        Some(Commands::UpdateCatalog { url }) => run_update_catalog(url.as_deref()).await,
    }
}

/// CLI-layer overrides from global flags (Epic D1f): only `Some` fields win.
fn cli_overrides(cli: &Cli) -> polymorph_core::config::CliOverrides {
    polymorph_core::config::CliOverrides {
        debug: cli.debug.then_some(true),
    }
}

/// Default session DB: `./.polymorph/sessions.db`.
fn default_db_path() -> std::path::PathBuf {
    std::path::PathBuf::from(".polymorph").join("sessions.db")
}

/// `sessions`: list / switch / delete against the real store (Epic C).
/// Output is human-table on stdout; errors name the missing id.
async fn run_sessions(cmd: SessionsCmd, db: &std::path::Path) -> anyhow::Result<()> {
    use polymorph_core::sessions::Store;
    let store = Store::open(db).await.map_err(|e| {
        // DB errors must never leak key material — StoreError holds none,
        // but state the invariant where the boundary is crossed.
        anyhow::anyhow!("cannot open session DB {}: {e}", db.display())
    })?;
    match cmd {
        SessionsCmd::List => {
            let sessions = store.list_sessions().await?;
            let current = store.current_session().await?.unwrap_or_default();
            if sessions.is_empty() {
                println!("no sessions");
            }
            for s in sessions {
                let mark = if s.id == current { "*" } else { " " };
                println!("{mark} {}  {}  (updated {})", s.id, s.title, s.updated_at);
            }
            Ok(())
        }
        SessionsCmd::Create { title } => {
            // Creation only (no auto-switch): switch explicitly, same
            // separation the TUI picker observes (chunk 77).
            let session = store.create_session(&title).await?;
            println!("created {}  {}", session.id, session.title);
            Ok(())
        }
        SessionsCmd::Switch { id } => {
            store.switch_session(&id).await?;
            println!("switched to {id}");
            Ok(())
        }
        SessionsCmd::Delete { id } => {
            store.delete_session(&id).await?;
            println!("deleted {id}");
            Ok(())
        }
    }
}

/// `config`: show the merged config / set one agent-editable key (Epic D2).
/// Pure helpers (`render_config`, `apply_config_set`) take explicit paths
/// so tests run against tempdirs; this wrapper supplies default paths.
fn run_config(
    cmd: ConfigCmd,
    user_path: &std::path::Path,
    project_path: &std::path::Path,
    overrides: &polymorph_core::config::CliOverrides,
) -> anyhow::Result<()> {
    match cmd {
        ConfigCmd::Show => {
            print!("{}", render_config(user_path, project_path, overrides)?);
            Ok(())
        }
        ConfigCmd::Set { key, value } => {
            let msg = apply_config_set(user_path, project_path, &key, &value)?;
            println!("{msg}");
            Ok(())
        }
    }
}

/// Render the merged (user < project < env < CLI) config as TOML.
fn render_config(
    user_path: &std::path::Path,
    project_path: &std::path::Path,
    overrides: &polymorph_core::config::CliOverrides,
) -> anyhow::Result<String> {
    let cfg = polymorph_core::config::load_with_cli(user_path, project_path, overrides)?;
    toml::to_string(&cfg).map_err(|e| anyhow::anyhow!("cannot render config: {e}"))
}

/// Keys the agent may edit via `config set` (Epic D2, config.md rule 3).
/// `providers.*` and `pins` need file edits (multi-field shapes — refused
/// here with this help rather than half-written).
const CONFIG_SET_HELP: &str = "supported keys: options.debug (true|false), \
    roles.<role> (comma-separated models; role = default|smol|slow|plan|act|review|commit|vision|task|advisor|tiny), \
    permissions.allow / permissions.deny (comma-separated; empty clears). \
    providers.* and pins need file edits.";

/// Pure TOML edit (Epic D2): apply one `key = value` to `existing_toml`.
/// Rewrites the file's TOML (comments are NOT preserved — documented: the
/// agent path optimizes for machine-roundtrip, humans keep comments by
/// editing the file directly).
fn apply_set(existing_toml: &str, key: &str, value: &str) -> anyhow::Result<String> {
    let mut doc: toml::Value = if existing_toml.trim().is_empty() {
        toml::Value::Table(Default::default())
    } else {
        toml::from_str(existing_toml)
            .map_err(|e| anyhow::anyhow!("existing config is not valid TOML: {e}"))?
    };
    let table = doc
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("config root must be a table"))?;
    let mut parts = key.split('.');
    let (section, name) = match (parts.next(), parts.next(), parts.next()) {
        (Some(s), Some(n), None) => (s, n),
        _ => anyhow::bail!("bad key {key:?}. {CONFIG_SET_HELP}"),
    };
    let new_value: toml::Value = match (section, name) {
        ("options", "debug") => match value {
            "true" => toml::Value::Boolean(true),
            "false" => toml::Value::Boolean(false),
            _ => anyhow::bail!("options.debug must be true|false, got {value:?}"),
        },
        ("roles", role) => {
            if polymorph_core::Role::parse(role).is_none() {
                anyhow::bail!("unknown role {role:?}. {CONFIG_SET_HELP}");
            }
            let items: Vec<toml::Value> = value
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| toml::Value::String(s.to_string()))
                .collect();
            if items.is_empty() {
                anyhow::bail!("roles.{role} must be given at least one model");
            }
            toml::Value::Array(items)
        }
        ("permissions", "allow" | "deny") => toml::Value::Array(
            value
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| toml::Value::String(s.to_string()))
                .collect(),
        ),
        _ => anyhow::bail!("unsupported key {key:?}. {CONFIG_SET_HELP}"),
    };
    let section_table = table
        .entry(section.to_string())
        .or_insert(toml::Value::Table(Default::default()));
    let section_table = section_table.as_table_mut().ok_or_else(|| {
        anyhow::anyhow!("config section [{section}] exists but is not a table — fix by hand")
    })?;
    section_table.insert(name.to_string(), new_value);
    toml::to_string(&doc).map_err(|e| anyhow::anyhow!("cannot serialize config: {e}"))
}

/// Apply one `config set` to the project file (created with parents when
/// missing), then re-read through the real loader as the live-reload proof
/// (the re-read error names the file; a set that cannot be read back fails).
fn apply_config_set(
    _user_path: &std::path::Path,
    project_path: &std::path::Path,
    key: &str,
    value: &str,
) -> anyhow::Result<String> {
    let existing = if project_path.exists() {
        std::fs::read_to_string(project_path)
            .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", project_path.display()))?
    } else {
        String::new()
    };
    let updated = apply_set(&existing, key, value)?;
    if let Some(parent) = project_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| anyhow::anyhow!("cannot create {}: {e}", parent.display()))?;
    }
    std::fs::write(project_path, updated)
        .map_err(|e| anyhow::anyhow!("cannot write {}: {e}", project_path.display()))?;
    // Live-reload proof: the file we just wrote must load back cleanly.
    let missing = std::path::Path::new("/nonexistent-polymorph-user-config.toml");
    polymorph_core::config::load(missing, project_path).map_err(|e| {
        anyhow::anyhow!(
            "wrote {} but it fails to reload: {e}",
            project_path.display()
        )
    })?;
    Ok(format!(
        "set {key} (project config: {})",
        project_path.display()
    ))
}

/// `providers`: list what is configured (Epic D). Names and key SOURCE names
/// only — key VALUES never appear here (grep-guard companion: this function
/// holds no secret, only field names).
fn run_providers(
    cmd: ProvidersCmd,
    cfg: &polymorph_core::config::PolymorphConfig,
) -> anyhow::Result<()> {
    match cmd {
        ProvidersCmd::List => {
            print!("{}", render_providers(cfg));
            Ok(())
        }
        ProvidersCmd::Add { name } => anyhow::bail!(
            "`providers add {name}` is not guided in Chunk 7: provider entries are \
            multi-field ([[providers]] id/base_url/models/key sources) and a skeleton \
            would misroute. Add the entry to the project config file by hand, or set \
            chains/keys via `config set` (roles.<role>, never key values)."
        ),
    }
}

fn render_providers(cfg: &polymorph_core::config::PolymorphConfig) -> String {
    if cfg.providers.is_empty() {
        return "no providers configured (ambient env only)\n".to_string();
    }
    let mut out = String::new();
    for p in &cfg.providers {
        let mut sources = Vec::new();
        if p.api_key_cmd.is_some() {
            sources.push("cmd".to_string());
        }
        for env in p.api_key_envs.iter().chain(p.api_key_env.iter()) {
            sources.push(format!("env:{env}"));
        }
        if sources.is_empty() {
            sources.push("none".to_string());
        }
        out.push_str(&format!(
            "{}  {}  models:[{}]  key:{}\n",
            p.id,
            p.base_url,
            p.models.join(","),
            sources.join(",")
        ));
    }
    out
}

/// `models`: list the catalog (fallback ∪ cache). `update-catalog` refreshes.
fn run_models(cmd: ModelsCmd) -> anyhow::Result<()> {
    match cmd {
        ModelsCmd::List => {
            print!("{}", render_models(Some(&catalog_cache_path()))?);
            Ok(())
        }
    }
}

fn render_models(cache: Option<&std::path::Path>) -> anyhow::Result<String> {
    let cat = polymorph_llm::catalog::Catalog::load(cache)?;
    let mut out = String::new();
    for (id, info) in cat.iter() {
        out.push_str(&format!(
            "{}  {}  {}\n",
            id,
            info.provider.as_deref().unwrap_or("?"),
            info.context
                .map(|c| c.to_string())
                .as_deref()
                .unwrap_or("?")
        ));
    }
    Ok(out)
}

/// Cache file: `<user-config-dir>/models.dev.json` (next to config.toml).
fn catalog_cache_path() -> std::path::PathBuf {
    let (user_file, _) = polymorph_core::config::default_paths();
    user_file
        .parent()
        .map(|d| d.join("models.dev.json"))
        .unwrap_or_else(|| std::path::PathBuf::from("models.dev.json"))
}

/// `update-catalog`: fetch models.dev → write cache (Epic D1e). Network
/// failure names the URL; an unrecognized shape refuses to clobber.
async fn run_update_catalog(url: Option<&str>) -> anyhow::Result<()> {
    let url = url.unwrap_or(polymorph_llm::catalog::MODELS_DEV_URL);
    let dest = catalog_cache_path();
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| anyhow::anyhow!("cannot create {}: {e}", parent.display()))?;
    }
    let n = polymorph_llm::catalog::update_catalog(url, &dest).await?;
    println!("catalog: {n} models cached at {}", dest.display());
    Ok(())
}

/// `delegate run`: fan-out demo over echo runners (Epic E1g). One process,
/// one bus, no LLM, no daemon: N children run 2 echo turns each, outcomes
/// print as typed results. With `--repo`, each child gets a REAL detached
/// git worktree under a pid-scoped temp parent (auto-removed).
async fn run_delegate(cmd: DelegateCmd) -> anyhow::Result<()> {
    use polymorph_delegate::{Bus, EchoRunner, SpawnSpec, WorkdirSpec, WorktreeMode};
    use std::sync::Arc;
    match cmd {
        DelegateCmd::Run {
            children,
            task,
            role,
            repo,
        } => {
            if children == 0 || children > 8 {
                anyhow::bail!("--children must be 1–8 (got {children})");
            }
            let role = match role.as_deref() {
                None => polymorph_core::Role::Default,
                Some(name) => polymorph_core::Role::parse(name).ok_or_else(|| {
                    anyhow::anyhow!("unknown role {name:?} (see `config set` help for names)")
                })?,
            };
            // Worktree parent lives until fan-out completes (guards the trees).
            let wt_parent =
                std::env::temp_dir().join(format!("polymorph-delegate-{}", std::process::id()));
            let mut specs = Vec::new();
            for _ in 0..children {
                let workdir = match &repo {
                    None => WorkdirSpec::InPlace(std::env::temp_dir()),
                    Some(repo) => {
                        if !polymorph_delegate::worktree::is_git_repo(repo).await {
                            anyhow::bail!("--repo {} is not a git repo", repo.display());
                        }
                        std::fs::create_dir_all(&wt_parent).map_err(|e| {
                            anyhow::anyhow!("cannot create {}: {e}", wt_parent.display())
                        })?;
                        WorkdirSpec::Worktree {
                            repo: repo.clone(),
                            parent: wt_parent.clone(),
                            mode: WorktreeMode::Detached,
                        }
                    }
                };
                let mut spec = SpawnSpec::new(role, &task, workdir);
                spec.tools = vec!["read".into(), "grep".into()];
                specs.push(spec);
            }
            let bus = Bus::flat();
            let runner: Arc<dyn polymorph_delegate::TurnRunner> =
                Arc::new(EchoRunner::new(2, "demo turn complete"));
            let outcomes = bus.fan_out(specs, runner, None).await?;
            for (id, out) in &outcomes {
                println!(
                    "{id}  {:?}  turns={} tools={} wall={:.2}s  {}  {}",
                    out.result.status,
                    out.turns_used,
                    out.tool_calls,
                    out.wall_secs,
                    out.result.summary,
                    out.result.data
                );
            }
            let _ = std::fs::remove_dir(&wt_parent); // empty after auto-remove; ignore.
            Ok(())
        }
    }
}

/// `review demo|check|run` (Epic F1d + F2a). Scripted demo + verdict-file
/// checker + the live ACT-mode loop over Bus-backed LLM children.
async fn run_review(
    cmd: ReviewCmd,
    overrides: &polymorph_core::config::CliOverrides,
    db: &std::path::Path,
) -> anyhow::Result<()> {
    use polymorph_core::{Finding, LoopConfig, Severity, Verdict, VerdictKind};
    use polymorph_review::{
        run_loop, Executor, ExecutorInput, LoopMode, Outcome, Reviewer, ReviewerError,
        ReviewerInput,
    };

    struct DemoReviewer {
        script: String,
        seen: std::sync::atomic::AtomicU32,
    }
    #[async_trait::async_trait]
    impl Reviewer for DemoReviewer {
        async fn review(&self, _input: ReviewerInput) -> Result<Verdict, ReviewerError> {
            use std::sync::atomic::Ordering;
            let n = self.seen.fetch_add(1, Ordering::Relaxed);
            let fail = |issue: String| Verdict {
                verdict: VerdictKind::Fail,
                findings: vec![Finding {
                    severity: Severity::P1,
                    file: "demo.rs".into(),
                    line: Some(1),
                    issue,
                    proposed_fix: "apply the demo fix".into(),
                }],
                confidence: 0.8,
            };
            let pass = Verdict {
                verdict: VerdictKind::Pass,
                findings: vec![],
                confidence: 0.9,
            };
            match self.script.as_str() {
                "pass" => Ok(pass),
                "fail-once" => Ok(if n == 0 {
                    fail("demo bug".into())
                } else {
                    pass
                }),
                "never" => Ok(fail(format!("demo bug round {}", n + 1))),
                "survivor" => Ok(fail("stubborn demo bug".into())),
                _ => Err(ReviewerError::Invalid("unreachable".into())),
            }
        }
    }
    struct DemoExecutor;
    #[async_trait::async_trait]
    impl Executor for DemoExecutor {
        async fn execute(&self, input: ExecutorInput) -> anyhow::Result<String> {
            Ok(format!("{} [fix applied]", input.artifact))
        }
    }

    match cmd {
        ReviewCmd::Demo { mode, script } => {
            let mode = LoopMode::parse(&mode)
                .ok_or_else(|| anyhow::anyhow!("unknown mode {mode:?} (plan|act)"))?;
            if !matches!(script.as_str(), "pass" | "fail-once" | "never" | "survivor") {
                anyhow::bail!("unknown script {script:?} (pass|fail-once|never|survivor)");
            }
            let reviewer = DemoReviewer {
                script,
                seen: std::sync::atomic::AtomicU32::new(0),
            };
            let report = run_loop(
                &LoopConfig::default(),
                "demo goal",
                "demo criteria",
                "demo artifact v1",
                mode,
                &reviewer,
                &DemoExecutor,
            )
            .await;
            for r in &report.rounds_log {
                println!("round {}: {:?} findings={}", r.round, r.verdict, r.findings);
                for issue in &r.issues {
                    println!("  - {issue}");
                }
            }
            match &report.outcome {
                Outcome::Passed { rounds } => println!(
                    "PASSED in {rounds} round{}",
                    if *rounds == 1 { "" } else { "s" }
                ),
                Outcome::Escalated { reason, rounds } => {
                    println!("ESCALATED after {rounds} rounds: {reason:?}")
                }
            }
            Ok(())
        }
        ReviewCmd::Check { path } => {
            let text = std::fs::read_to_string(&path)
                .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", path.display()))?;
            match polymorph_review::parse_verdict(&text) {
                Ok(v) => {
                    println!(
                        "valid: {:?} findings={} confidence={}",
                        v.verdict,
                        v.findings.len(),
                        v.confidence
                    );
                    for f in &v.findings {
                        println!(
                            "  {:?} {}:{} {}",
                            f.severity,
                            f.file,
                            f.line.map(|l| l.to_string()).unwrap_or_else(|| "-".into()),
                            f.issue
                        );
                    }
                    Ok(())
                }
                Err(e) => anyhow::bail!("invalid verdict: {e}"),
            }
        }
        ReviewCmd::Run { args } => run_review_live(args, overrides).await,
        ReviewCmd::Drive { args, events_json } => {
            run_review_drive(args, events_json, overrides).await
        }
        ReviewCmd::EvalAb { args } => {
            let ReviewEvalAbArgs {
                goal,
                artifact_file,
                criteria,
                mode,
                scope,
                review_pack,
                exec_pack,
                plan_pack,
                review_model_a,
                review_model_b,
                max_rounds,
            } = *args;
            run_review_eval_ab(
                goal,
                artifact_file,
                criteria,
                mode,
                scope,
                review_pack,
                exec_pack,
                plan_pack,
                review_model_a,
                review_model_b,
                max_rounds,
                overrides,
            )
            .await
        }
        ReviewCmd::Bench { args } => {
            let ReviewBenchArgs {
                corpus,
                record,
                only,
                arm,
                max_rounds,
                max_tool_rounds,
                min_rate,
                review_pack,
                exec_pack,
                tool_profile,
                single_no_tools,
                loop_worktree,
                preamble,
            } = *args;
            run_bench_live(
                corpus,
                record,
                only,
                arm,
                max_rounds,
                max_tool_rounds,
                min_rate,
                review_pack,
                exec_pack,
                tool_profile,
                single_no_tools,
                loop_worktree,
                preamble,
                overrides,
            )
            .await
        }
        ReviewCmd::BenchCheck { path, min_rate } => run_bench_check(path, min_rate),
        ReviewCmd::PlanShow { plan, session } => run_plan_show(plan, db, session.as_deref()).await,
        ReviewCmd::PlanDrive { args } => {
            let ReviewPlanArgs {
                plan,
                out,
                checkpoint,
                session,
                max_rounds,
                review_pack,
                plan_pack,
            } = *args;
            run_plan_drive(
                plan,
                out,
                checkpoint,
                session,
                max_rounds,
                review_pack,
                plan_pack,
                db,
                overrides,
            )
            .await
        }
    }
}

/// `review bench`: live resolve-rate runner (Epic F6). Drives each admitted
/// corpus task through the selected arms in pid-scoped temp copies and scores
/// the hidden suite after every case. Loop arm mirrors `run_review_live`
/// construction (Bus stack, tools on, act mode); single arm is one Act spawn
/// with the same fixer and tools (tools held constant — the delta prices
/// REVIEW). Spend guards: `--max-rounds` per loop case, `--max-tool-rounds`
/// per fix/single turn. Red gate bails non-zero below `--min-rate`.
#[allow(clippy::too_many_arguments)]
async fn run_bench_live(
    corpus: std::path::PathBuf,
    record: std::path::PathBuf,
    only: Option<String>,
    arm: String,
    max_rounds: u32,
    max_tool_rounds: u32,
    min_rate: f64,
    review_pack: std::path::PathBuf,
    exec_pack: std::path::PathBuf,
    tool_profile: String,
    single_no_tools: bool,
    loop_worktree: bool,
    preamble: String,
    overrides: &polymorph_core::config::CliOverrides,
) -> anyhow::Result<()> {
    use polymorph_review::{
        AgenticFixRunner, BenchArm, BusExecutor, BusReviewer, LlmTarget, ReviewTurnRunner,
    };
    use std::sync::Arc;
    // Fail-fast (creates nothing): unknown arm/profile, unreadable corpus,
    // unknown --only all bail before any spend or record dir. The tools-off
    // flag needs the single arm (loop never sees it — a loop-only run with
    // the flag is a caller error, loud here instead of a confounded board).
    let arms = polymorph_review::parse_bench_arms(&arm)?;
    if single_no_tools && !arms.contains(&BenchArm::Single) {
        anyhow::bail!("--single-no-tools needs `--arm single` (got `--arm {arm}`)");
    }
    if loop_worktree && !arms.contains(&BenchArm::Loop) {
        anyhow::bail!("--loop-worktree needs `--arm loop` or `both` (got `--arm {arm}`)");
    }
    // Preamble ablation (chunk 45): unknown values bail pre-spend like the
    // profile rule above (F5d precedent).
    let minimal_preamble = polymorph_review::parse_preamble(&preamble)?;
    let shell_rule = polymorph_review::profile_shell_rule(&tool_profile)?;
    let corpus_text = std::fs::read_to_string(&corpus)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", corpus.display()))?;
    let manifest = polymorph_review::load_corpus(&corpus_text)?;
    let mut tasks: Vec<&polymorph_review::Task> = manifest.task.iter().collect();
    if let Some(id) = &only {
        tasks.retain(|t| &t.id == id);
        if tasks.is_empty() {
            anyhow::bail!("unknown task {id:?} (corpus holds {})", manifest.task.len());
        }
    }
    let review_system = polymorph_review::load_pack(&review_pack)?;
    let exec_system = polymorph_review::load_pack(&exec_pack)?;
    let (user_path, project_path) = polymorph_core::config::default_paths();
    let cfg = polymorph_core::config::load_with_cli(&user_path, &project_path, overrides)?;
    let router = polymorph_llm::RoleRouter::from_config(&cfg);
    let state = std::sync::Arc::new(tokio::sync::Mutex::new(polymorph_llm::FallbackState::new(
        std::time::Duration::from_secs(300),
    )));
    let bus = std::sync::Arc::new(polymorph_delegate::Bus::flat());
    // MCP deliberately NOT wired here (chunk 55): the bench measures the
    // fixed V1 floor — apparatus changes would void every prior board.
    // Docket #9 bounding stays off (apparatus stability, the docket #7
    // bonus precedent): bench boards keep the unbounded tool results.
    let registry = {
        let mut r = polymorph_tools::registry::v1_registry();
        r.set_output_spill(polymorph_tools::output::SpillConfig::off());
        Arc::new(r)
    };
    // Bench-owned Gate (plan-chunk36 §1): per-task fixture-scoped rules built
    // from the manifest — operator config is READ for its denies only, never
    // written. The default-profile run (c35) is the documented baseline.
    let operator_denies = cfg.permissions.deny.clone();
    let tool_names: Vec<String> = registry.names().iter().map(|s| s.to_string()).collect();
    // Tools-off axis (chunk 41): loop-invariant single-arm tool list (empty
    // under the flag) — the loop arm always keeps the full list.
    let single_tools = polymorph_review::single_tool_list(single_no_tools, &tool_names);
    std::fs::create_dir_all(&record)
        .map_err(|e| anyhow::anyhow!("cannot create {}: {e}", record.display()))?;
    // Pid- + sequence-scoped work parent (traps #47: pid alone races
    // in-process callers); one subdir per task, removed with the parent on
    // EVERY exit via the guard below.
    static BENCH_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = BENCH_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let parent = std::env::temp_dir().join(format!("polymorph-bench-{}-{seq}", std::process::id()));
    std::fs::create_dir_all(&parent)
        .map_err(|e| anyhow::anyhow!("cannot create {}: {e}", parent.display()))?;
    struct RemoveOnDrop(std::path::PathBuf);
    impl Drop for RemoveOnDrop {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _guard = RemoveOnDrop(parent.clone());
    let mk_fixer = |gate: polymorph_tools::Gate, tools: Vec<String>| {
        let mut fixer = AgenticFixRunner::live_with_preamble(
            polymorph_llm::LlmClient::with_fresh_conversation(),
            LlmTarget::Managed {
                router: router.clone(),
                providers: cfg.providers.clone(),
                state: state.clone(),
                role: polymorph_core::Role::Act,
                scope: None,
            },
            exec_system.clone(),
            registry.clone(),
            gate.clone(),
            tools,
            max_tool_rounds,
            polymorph_review::select_preamble(minimal_preamble),
        );
        // Apparatus stability: the bench keeps chunk 154's single
        // verification bonus (docket #7 scaling stays off its boards).
        fixer.max_bonus_tool_rounds = 1;
        fixer.history = polymorph_review::ToolHistoryBudget::off();
        Arc::new(fixer)
    };
    // Turns rule (stated, honest): loop arm turns = 2 × loop rounds
    // (review + fix per round); single arm = 1 turn. Tool calls count
    // separately from the outcome ledger.
    let mut resolved: std::collections::BTreeMap<&str, u32> = std::collections::BTreeMap::new();
    let mut ran: std::collections::BTreeMap<&str, u32> = std::collections::BTreeMap::new();
    for task in &tasks {
        for bench_arm in &arms {
            let dir = parent.join(format!("{}-{}", task.id, bench_arm.as_str()));
            std::fs::create_dir_all(&dir)
                .map_err(|e| anyhow::anyhow!("cannot create {}: {e}", dir.display()))?;
            // Chunk 43: the single/tools-on arm materializes a repo and works
            // a merged Branch worktree (merge-back live); loop + tools-off
            // arms stay on plain copies (comparability + no tools to merge).
            // Chunk 44: `--loop-worktree` graduates the loop arm the same way
            // (repo + per-turn kept Branch merges); default off keeps c39-c43
            // boards comparable.
            let single_worktree = matches!(bench_arm, BenchArm::Single) && !single_no_tools;
            let loop_repo = matches!(bench_arm, BenchArm::Loop) && loop_worktree;
            if single_worktree || loop_repo {
                polymorph_review::materialize_repo(&dir, &task.files)?;
            } else {
                polymorph_review::write_files(&dir, &task.files)?;
            }
            // Per-task scoped Gate: manifest paths only, profiled shell,
            // operator denies kept (bench_gate_shell is pure + unit-tested).
            let (allow, deny) =
                polymorph_review::bench_gate_shell(task, &operator_denies, shell_rule);
            let task_gate = polymorph_tools::Gate::new(allow, deny);
            // Admission re-verified at run time: a GREEN start means the
            // fixture rotted — quarantine with a named skip, never score.
            let (pre_out, pre_ok) =
                polymorph_review::cargo_test(&dir, polymorph_review::CARGO_DEADLINE);
            if pre_ok {
                println!(
                    "skip {} [{}]: fixture already green (quarantined)",
                    task.id,
                    bench_arm.as_str()
                );
                continue;
            }
            // The fixer finds the repo through the artifact text (F2b
            // ARTIFACT-PATH precedent): goal + criteria carry the goal,
            // the REPO DIR block carries the path into every brief.
            let goal = format!(
                "{}\n\nRepository copy at {}. Make `cargo test` pass there without touching the tests.\nUse RELATIVE paths exactly as listed (e.g. src/main.rs) for every tool call — never absolute paths; the workdir is already the repo copy.",
                task.instruction,
                dir.display()
            );
            let artifact = format!(
                "REPO DIR:\n{}\n\nFAILING OUTPUT:\n{}",
                dir.display(),
                pre_out
                    .lines()
                    .rev()
                    .take(80)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<Vec<_>>()
                    .join("\n")
            );
            let (verdict_words, turns, tool_calls, issues) = match bench_arm {
                BenchArm::Loop => {
                    let reviewer = BusReviewer {
                        bus: bus.clone(),
                        runner: Arc::new(ReviewTurnRunner {
                            client: polymorph_llm::LlmClient::with_fresh_conversation(),
                            target: LlmTarget::Managed {
                                router: router.clone(),
                                providers: cfg.providers.clone(),
                                state: state.clone(),
                                role: polymorph_core::Role::Review,
                                scope: None,
                            },
                            system: review_system.clone(),
                            pinned: Default::default(),
                        }),
                        workdir: polymorph_delegate::WorkdirSpec::InPlace(dir.clone()),
                        artifact_path: None,
                    };
                    let outcomes = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
                    let executor = BusExecutor {
                        bus: bus.clone(),
                        runner: mk_fixer(task_gate.clone(), tool_names.clone()),
                        workdir: polymorph_delegate::WorkdirSpec::InPlace(dir.clone()),
                        tools: tool_names.clone(),
                        tool_outcomes: outcomes.clone(),
                        // Chunk 44: opt-in per-turn Branch merges (repo is a
                        // `materialize_repo` repo exactly when `loop_repo`;
                        // reviewer stays InPlace — only fix turns merge).
                        merge: if loop_repo {
                            Some(polymorph_review::LoopMergeConfig {
                                repo: dir.clone(),
                                parent: parent.clone(),
                                message: format!("polymorph bench {}.loop", task.id),
                            })
                        } else {
                            None
                        },
                        // Bench boards must not move under the chunk-110 loop
                        // change (apparatus stability) — stays None.
                        artifact_path: None,
                        peripherals: None,
                        artifact_set: None,
                    };
                    let loop_cfg = polymorph_core::LoopConfig {
                        max_iterations: max_rounds,
                        ..Default::default()
                    };
                    let report = polymorph_review::run_loop(
                        &loop_cfg,
                        &goal,
                        "cargo test: all tests pass",
                        &artifact,
                        polymorph_review::LoopMode::Act,
                        &reviewer,
                        &executor,
                    )
                    .await;
                    let words = match &report.outcome {
                        polymorph_review::Outcome::Passed { rounds } => format!("Passed-{rounds}"),
                        polymorph_review::Outcome::Escalated { reason, rounds } => {
                            format!("Escalated-{reason:?}-{rounds}")
                        }
                    };
                    let flat_issues: Vec<String> = report
                        .rounds_log
                        .iter()
                        .flat_map(|r| r.issues.clone())
                        .collect();
                    let n_tools = outcomes.lock().map(|l| l.len() as u32).unwrap_or(0);
                    (
                        words,
                        2 * report.rounds_log.len() as u32,
                        n_tools,
                        flat_issues,
                    )
                }
                BenchArm::Single => {
                    // Chunk 43: tools-on single works a kept Branch worktree
                    // (merged before scoring); tools-off stays InPlace.
                    let workdir = if single_worktree {
                        polymorph_delegate::WorkdirSpec::Worktree {
                            repo: dir.clone(),
                            parent: parent.clone(),
                            mode: polymorph_delegate::worktree::WorktreeMode::Branch(None),
                        }
                    } else {
                        polymorph_delegate::WorkdirSpec::InPlace(dir.clone())
                    };
                    let mut spec = polymorph_delegate::SpawnSpec::new(
                        polymorph_core::Role::Act,
                        goal.clone(),
                        workdir,
                    );
                    spec.tools = single_tools.clone();
                    spec.keep_worktree = single_worktree;
                    let handle = bus
                        .spawn(
                            spec,
                            mk_fixer(task_gate.clone(), single_tools.clone()),
                            None,
                        )
                        .await?;
                    let outcome = handle.join().await;
                    match &outcome.result.status {
                        polymorph_delegate::FinishStatus::Completed => {}
                        polymorph_delegate::FinishStatus::Stopped { reason } => {
                            println!(
                                "note {} [single]: child stopped ({reason:?}) — scoring anyway",
                                task.id
                            );
                        }
                        polymorph_delegate::FinishStatus::Failed { error } => {
                            println!(
                                "note {} [single]: child failed ({error}) — scoring anyway",
                                task.id
                            );
                        }
                    }
                    // Merge-back (chunk 43): a kept tree merges into the
                    // case repo BEFORE scoring (single child → conflict
                    // impossible; any merge trouble scores the tree as-is,
                    // loudly, like stopped/failed children above).
                    if single_worktree {
                        match outcome.kept {
                            Some(kept) => {
                                let branch = kept.branch().unwrap_or("?").to_string();
                                match polymorph_delegate::worktree::merge_back(
                                    kept,
                                    &format!("polymorph bench {}.single", task.id),
                                )
                                .await
                                {
                                    Ok(report) => println!(
                                        "note {} [single]: merged {} {}",
                                        task.id,
                                        report.branch,
                                        report.commit.chars().take(8).collect::<String>()
                                    ),
                                    Err(e) => println!(
                                        "note {} [single]: merge-back {branch} troubled ({e}) — scoring anyway",
                                        task.id
                                    ),
                                }
                            }
                            None => println!(
                                "note {} [single]: no kept worktree — scoring anyway",
                                task.id
                            ),
                        }
                    }
                    // Turns rule: the bus counts model turns per child
                    // (turns_used) — the single arm is one spawn by
                    // construction; tool calls come from the ledger.
                    (
                        "Single-shot".to_string(),
                        outcome.turns_used,
                        outcome.tool_calls as u32,
                        Vec::new(),
                    )
                }
            };
            let (ok, summary) = polymorph_review::score_dir(&dir);
            let agreement = match bench_arm {
                BenchArm::Loop => (verdict_words.starts_with("Passed")) == ok,
                BenchArm::Single => true, // no verdict to agree with.
            };
            let names: Vec<String> = task.files.keys().cloned().collect();
            let files = polymorph_review::snapshot_files(&dir, &names)?;
            let record_out = polymorph_review::BenchRecord {
                task: task.id.clone(),
                arm: bench_arm.as_str().to_string(),
                profile: {
                    // Runner/axis stamps compose (chunk 43/44), then the chunk-45
                    // preamble suffix rides outermost — all replay-opaque.
                    let base = match bench_arm {
                        // Chunk 43: worktree-merged singles stamp the runner;
                        // tools-off singles keep the axis stamp; chunk 44: loop
                        // worktrees stamp theirs; plain loop keeps the base.
                        BenchArm::Single if single_no_tools => {
                            polymorph_review::single_record_profile(&tool_profile, true)
                        }
                        BenchArm::Single => {
                            polymorph_review::worktree_record_profile(&tool_profile)
                        }
                        BenchArm::Loop if loop_worktree => {
                            polymorph_review::loop_worktree_record_profile(&tool_profile)
                        }
                        _ => tool_profile.clone(),
                    };
                    polymorph_review::preamble_record_suffix(&base, minimal_preamble)
                },
                resolved: ok,
                turns,
                tool_calls,
                verdict: verdict_words,
                agreement,
                issues,
                files,
            };
            polymorph_review::write_record(&record, &record_out)?;
            *ran.entry(bench_arm.as_str()).or_insert(0) += 1;
            if ok {
                *resolved.entry(bench_arm.as_str()).or_insert(0) += 1;
            }
            println!(
                "{} [{}]: {} (turns={turns} tools={tool_calls} agreement={agreement} :: {summary})",
                task.id,
                bench_arm.as_str(),
                if ok { "Resolved" } else { "Unresolved" },
            );
        }
    }
    // Board + delta + strict gate (F5d precedent: variance must hurt).
    // Profile stamped on every line: boards attribute to the selected profile.
    println!("profile: {tool_profile}");
    let mut reds: Vec<String> = Vec::new();
    for bench_arm in &arms {
        let r = resolved.get(bench_arm.as_str()).copied().unwrap_or(0);
        let n = ran.get(bench_arm.as_str()).copied().unwrap_or(0);
        let rate = if n == 0 { 0.0 } else { r as f64 / n as f64 };
        println!(
            "arm {}: {r}/{n} resolved ({:.0}%)",
            bench_arm.as_str(),
            rate * 100.0
        );
        if rate < min_rate {
            reds.push(bench_arm.as_str().to_string());
        }
    }
    if arms.len() == 2 {
        let l = resolved.get("loop").copied().unwrap_or(0) as i64;
        let s = resolved.get("single").copied().unwrap_or(0) as i64;
        println!("delta (loop-single): {:+}", l - s);
    }
    if !reds.is_empty() {
        anyhow::bail!(
            "bench gate red below --min-rate {min_rate}: {}",
            reds.join(", ")
        );
    }
    Ok(())
}

/// `review bench-check`: offline replay of `*.bench.json` records (F5c
/// precedent). Materializes recorded files per record, re-runs the hidden
/// suite, and requires the same resolved bit; mismatches and corrupt files
/// are hard errors naming the file. Red gate bails non-zero.
fn run_bench_check(path: std::path::PathBuf, min_rate: f64) -> anyhow::Result<()> {
    let mut files: Vec<std::path::PathBuf> = if path.is_dir() {
        let found: Vec<std::path::PathBuf> = std::fs::read_dir(&path)
            .map_err(|e| anyhow::anyhow!("cannot list {}: {e}", path.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("json"))
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.ends_with(".bench.json"))
            })
            .collect();
        found
    } else if path.is_file() {
        vec![path]
    } else {
        anyhow::bail!("cannot read {}: no such file or directory", path.display());
    };
    files.sort();
    if files.is_empty() {
        anyhow::bail!("bench check: no records (empty board fails)");
    }
    static BENCH_CHECK_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = BENCH_CHECK_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let parent = std::env::temp_dir().join(format!(
        "polymorph-bench-check-{}-{seq}",
        std::process::id()
    ));
    std::fs::create_dir_all(&parent)
        .map_err(|e| anyhow::anyhow!("cannot create {}: {e}", parent.display()))?;
    struct RemoveOnDrop(std::path::PathBuf);
    impl Drop for RemoveOnDrop {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _guard = RemoveOnDrop(parent.clone());
    let mut resolved = 0u32;
    for file in &files {
        let record = polymorph_review::load_record(file)?;
        let stem = file
            .file_stem()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "case".to_string());
        let dir = parent.join(stem);
        std::fs::create_dir_all(&dir)
            .map_err(|e| anyhow::anyhow!("cannot create {}: {e}", dir.display()))?;
        // check_record replays OR hard-errors naming the task (never a
        // silent score) — the `?` below is the gate machinery.
        let replayed = polymorph_review::check_record(&record, &dir)?;
        if replayed {
            resolved += 1;
        }
        println!(
            "{} [{}]: {} (replayed)",
            record.task,
            record.arm,
            if replayed { "Resolved" } else { "Unresolved" }
        );
    }
    let n = files.len() as u32;
    let rate = resolved as f64 / n as f64;
    println!(
        "bench check: {resolved}/{n} resolved ({:.0}%)",
        rate * 100.0
    );
    if rate < min_rate {
        anyhow::bail!("bench gate red below --min-rate {min_rate}: {resolved}/{n}");
    }
    Ok(())
}

/// `review plan show`: parse + validate a PLAN.md file and print the board
/// (Epic G surfacing, chunk 48). Offline, no key. Parse faults exit 1
/// naming the line (the fault text already does — F5c precedent).
/// Plan source for show/drive (chunk 51): checkpoint wins when present
/// (resume), else the session store when `--session` names a session with
/// a saved plan, else the PLAN.md file. The title ALWAYS comes from the
/// file (checkpoint precedent — the file is the human artifact, the store
/// holds run state). Corrupt state is a hard error; a live session with no
/// saved plan falls back to the file (absence is data — same as an absent
/// checkpoint file). Ghost sessions: hard error when `init_session` is
/// false (read-only `plan-show`); created titled with the plan title when
/// true (`plan-drive` init-on-first-use — checkpoint-file precedent: an
/// absent slot means fresh, saved at the end; there is no other CLI path
/// that creates sessions).
#[derive(Debug, PartialEq)]
enum PlanSource {
    Checkpoint,
    Session,
    File,
}

async fn resolve_plan_graph(
    db: &std::path::Path,
    plan: &std::path::Path,
    checkpoint: Option<&std::path::Path>,
    session: Option<&str>,
    init_session: bool,
) -> anyhow::Result<(String, polymorph_tasks::TaskGraph, PlanSource)> {
    let text = std::fs::read_to_string(plan)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", plan.display()))?;
    let (title, file_graph) = polymorph_tasks::plan::parse(&text)?;
    if let Some(path) = checkpoint {
        if path.exists() {
            let graph = polymorph_tasks::TaskGraph::load_json(path)?;
            println!("resumed from {}", path.display());
            return Ok((title, graph, PlanSource::Checkpoint));
        }
    }
    if let Some(sid) = session {
        let store = polymorph_core::sessions::Store::open(db)
            .await
            .map_err(|e| anyhow::anyhow!("cannot open session DB {}: {e}", db.display()))?;
        match store.load_plan(sid).await {
            Ok(Some(graph)) => {
                println!("resumed plan from session {sid}");
                return Ok((title, graph, PlanSource::Session));
            }
            Ok(None) => {}
            Err(polymorph_core::sessions::StoreError::NoSession(_)) if init_session => {
                store.create_session_with_id(sid, &title).await?;
                println!("created session {sid}");
            }
            Err(e) => return Err(anyhow::anyhow!("{e}")),
        }
    }
    Ok((title, file_graph, PlanSource::File))
}

async fn run_plan_show(
    plan: std::path::PathBuf,
    db: &std::path::Path,
    session: Option<&str>,
) -> anyhow::Result<()> {
    let (title, graph, _) = resolve_plan_graph(db, &plan, None, session, false).await?;
    println!("# Plan: {title}");
    for task in graph.ordered() {
        let deps = if task.deps.is_empty() {
            "none".to_string()
        } else {
            task.deps.join(", ")
        };
        println!("- [{}] {:?} (deps: {})", task.id, task.status, deps);
    }
    let ready: Vec<&str> = graph.ready().iter().map(|t| t.id.as_str()).collect();
    println!(
        "{} tasks, {} ready: {}",
        graph.len(),
        ready.len(),
        if ready.is_empty() {
            "—".to_string()
        } else {
            ready.join(", ")
        }
    );
    Ok(())
}

/// `review plan drive`: drive a PLAN.md file through per-task reviewer loops
/// with executor write-back (Epic G drive slice live, chunk 48). Text-only
/// plan fixer (tools orthogonal — bounded first drive); InPlace cwd workdir
/// (no files touched: the PLAN.md itself is only read, `--out` written).
/// Checkpoint resumes when it exists (opt-in file, never implicit). Needs
/// key, like `run`. Exit 0 on a completed drive (the report judges, run
/// precedent — not a CI gate). Spend guard: `--max-rounds` per task.
#[allow(clippy::too_many_arguments)]
async fn run_plan_drive(
    plan: std::path::PathBuf,
    out: std::path::PathBuf,
    checkpoint: Option<std::path::PathBuf>,
    session: Option<String>,
    max_rounds: u32,
    review_pack: std::path::PathBuf,
    plan_pack: std::path::PathBuf,
    db: &std::path::Path,
    overrides: &polymorph_core::config::CliOverrides,
) -> anyhow::Result<()> {
    use polymorph_review::{BusReviewer, FixTurnRunner, LlmTarget, ReviewTurnRunner};
    use std::sync::Arc;
    // Source graph: checkpoint > session store > PLAN.md file (chunk 51
    // resolver — the file always supplies the title, checkpoint precedent;
    // drive initializes its slot, show stays read-only). Every boundary
    // validates — corrupt state is a hard error naming the fault, never a
    // partial drive.
    let (title, graph, _) =
        resolve_plan_graph(db, &plan, checkpoint.as_deref(), session.as_deref(), true).await?;
    let review_system = polymorph_review::load_pack(&review_pack)?;
    let exec_system = polymorph_review::load_pack(&plan_pack)?;
    let (user_path, project_path) = polymorph_core::config::default_paths();
    let cfg = polymorph_core::config::load_with_cli(&user_path, &project_path, overrides)?;
    let router = polymorph_llm::RoleRouter::from_config(&cfg);
    let state = std::sync::Arc::new(tokio::sync::Mutex::new(polymorph_llm::FallbackState::new(
        std::time::Duration::from_secs(300),
    )));
    let bus = std::sync::Arc::new(polymorph_delegate::Bus::flat());
    let workdir = polymorph_delegate::WorkdirSpec::InPlace(std::env::current_dir()?);
    let reviewer = BusReviewer {
        bus: bus.clone(),
        runner: Arc::new(ReviewTurnRunner {
            client: polymorph_llm::LlmClient::with_fresh_conversation(),
            target: LlmTarget::Managed {
                router: router.clone(),
                providers: cfg.providers.clone(),
                state: state.clone(),
                role: polymorph_core::Role::Review,
                scope: None,
            },
            system: review_system,
            pinned: Default::default(),
        }),
        workdir: workdir.clone(),
        artifact_path: Some(plan.clone()),
    };
    let executor = polymorph_review::BusExecutor {
        bus,
        runner: Arc::new(FixTurnRunner {
            client: polymorph_llm::LlmClient::with_fresh_conversation(),
            target: LlmTarget::Managed {
                router,
                providers: cfg.providers.clone(),
                state,
                role: polymorph_core::Role::Plan,
                scope: None,
            },
            system: exec_system,
            pinned: Default::default(),
        }),
        workdir,
        tools: Vec::new(),
        tool_outcomes: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        merge: None,
        // Plan-drive owns file state via write-back machinery — stays None.
        artifact_path: None,
        peripherals: None,
        artifact_set: None,
    };
    let loop_cfg = polymorph_core::LoopConfig {
        max_iterations: max_rounds,
        ..Default::default()
    };
    let rep =
        polymorph_review::drive_plan_with_writeback(&loop_cfg, &title, graph, &reviewer, &executor)
            .await?;
    for task in &rep.tasks {
        // Words derive from the FINAL status (a write-back refusal flips
        // Passed→Failed after the loop — the loop verdict alone would lie).
        let status = rep
            .graph
            .get(&task.id)
            .map(|t| t.status)
            .unwrap_or(polymorph_tasks::TaskStatus::Failed);
        println!(
            "{}: {}",
            task.id,
            polymorph_review::task_words(status, &task.report.outcome)
        );
        if let Some(err) = &task.writeback_error {
            println!("  write-back refused (old text kept): {err}");
        }
    }
    if !rep.merge_sequence.is_empty() {
        println!("merge order: {}", rep.merge_sequence.join(" -> "));
    }
    if !rep.unfinished.is_empty() {
        println!("unfinished: {}", rep.unfinished.join(", "));
    }
    // The input is never overwritten: the updated PLAN.md goes to --out,
    // the run state to --checkpoint when given and to the session store
    // when --session names one (all explicit, all opt-in).
    std::fs::write(&out, polymorph_tasks::plan::render(&title, &rep.graph))
        .map_err(|e| anyhow::anyhow!("cannot write {}: {e}", out.display()))?;
    println!("wrote {}", out.display());
    if let Some(path) = &checkpoint {
        rep.graph.save_json(path)?;
        println!("checkpoint {}", path.display());
    }
    if let Some(sid) = &session {
        let store = polymorph_core::sessions::Store::open(db)
            .await
            .map_err(|e| anyhow::anyhow!("cannot open session DB {}: {e}", db.display()))?;
        store.save_plan(sid, &rep.graph).await?;
        println!("saved plan to session {sid}");
    }
    Ok(())
}

/// `review run`: live plan/act loop (Epic F2a + F2b + F3b). Bus-backed LLM
/// reviewer + fixer children over the D1-managed role chains; needs provider
/// auth (the same ambient/injected keys as `exec`). Reviewer always rides
/// Review and stays tool-free; the fixer rides Act in act mode, Plan in plan
/// mode (`--plan-pack` system), wielding registry tools only behind `--tools`
/// (text-only fixing stays the default). One process, one bus, one shared
/// fallback state (cooldowns persist across rounds). `--scope` selects D1b
/// pins; `--repo` runs the whole drive in one detached git worktree of the
/// repo (removed at drive end; the operator's tree is never written, merge-
/// back = G). Spend guards: `--max-rounds`
/// caps LLM rounds (each round = 1 review turn + ≤1 fix turn);
/// `--max-tool-rounds` caps model→tools→results feeds per fix turn.
/// Live-loop parts shared by `run` and `drive` (chunk 141 extraction:
/// mechanical move of `run_review_live`'s wiring — behavior identical).
struct LiveLoopParts {
    reviewer: polymorph_review::HarnessGateReviewer<polymorph_review::BusReviewer>,
    executor: polymorph_review::BusExecutor,
    loop_cfg: polymorph_core::LoopConfig,
    goal: String,
    criteria: String,
    artifact: String,
    mode: polymorph_review::LoopMode,
    wt_parent: std::path::PathBuf,
    /// Round-1 CREATE/START seed (docket #7): `Some` replaces the round-1
    /// reviewer call with the harness-authored finding.
    seed: Option<polymorph_review::RoundSeed>,
    /// Restore ledger of a rollback run (docket #8): `Some` only for
    /// in-place `--tools` runs whose `--on-fail` resolves to rollback.
    restore: Option<std::sync::Arc<polymorph_review::RestoreLedger>>,
    /// The tool reviewer (docket #4): `Some` only with reviewer tools on;
    /// its per-call reports annotate the board and `--events-json`.
    review_tools: Option<std::sync::Arc<polymorph_review::ToolReviewTurnRunner>>,
    /// The `--repo` drive worktree: `Some` for the whole drive, removed by
    /// [`Self::finish`] (or its own drop on every other exit).
    drive_tree: Option<polymorph_delegate::DriveWorktree>,
}

impl LiveLoopParts {
    /// End of a drive: remove the `--repo` drive tree (a failed removal
    /// warns; the result was already reported), then the worktree parent.
    fn finish(&mut self) {
        if let Some(tree) = self.drive_tree.take() {
            let path = tree.path().display().to_string();
            match tree.close() {
                Ok(()) => tracing::info!(path = path.as_str(), "--repo drive worktree removed"),
                Err(e) => tracing::warn!(
                    path = path.as_str(),
                    "--repo drive worktree removal failed: {e:#}"
                ),
            }
        }
        scrub_worktree_parent(&self.wt_parent);
    }
    /// The tool reviewer's per-call reports so far (docket #4; empty with
    /// reviewer tools off).
    fn review_reports(&self) -> Vec<polymorph_review::ReviewToolReport> {
        self.review_tools
            .as_ref()
            .map(|r| r.reports())
            .unwrap_or_default()
    }

    /// The one loop call `run`, `drive` and the TUI driver share.
    async fn run(&self) -> polymorph_review::LoopReport {
        polymorph_review::run_loop_seeded(
            &self.loop_cfg,
            &self.goal,
            &self.criteria,
            &self.artifact,
            self.mode,
            &self.reviewer,
            &self.executor,
            self.seed.clone(),
        )
        .await
    }

    /// [`Self::run`] under the docket-#8 rollback guard: a pass keeps every
    /// edit, any other outcome rolls back (`Some` report), and so does a
    /// panic or a dropped future (the guard's drop). `cli`: Ctrl-C drops the
    /// in-flight turn, rolls back, prints the summary and errors; a TUI
    /// (`false`) owns its keys and screen, so no Ctrl-C race and drop
    /// summaries go to the log only. Without a ledger this is exactly
    /// [`Self::run`] (Ctrl-C keeps its default kill), except that a CLI
    /// `--repo` drive catches Ctrl-C to remove its worktree.
    async fn run_guarded(
        &self,
        cli: bool,
    ) -> anyhow::Result<(
        polymorph_review::LoopReport,
        Option<polymorph_review::RollbackReport>,
    )> {
        let Some(ledger) = &self.restore else {
            // `--repo` (never a ledger: the operator's tree is untouched):
            // Ctrl-C drops the in-flight turn and errors out, so the drive
            // tree's guard removes it instead of the default kill leaking
            // it.
            if cli && self.drive_tree.is_some() {
                tokio::select! {
                    report = self.run() => return Ok((report, None)),
                    Ok(()) = tokio::signal::ctrl_c() => {
                        println!("INTERRUPTED (Ctrl-C)");
                        anyhow::bail!("interrupted (Ctrl-C); --repo drive worktree discarded");
                    }
                }
            }
            return Ok((self.run().await, None));
        };
        let guard = polymorph_review::RollbackGuard::new(ledger.clone());
        let guard = if cli { guard } else { guard.quiet() };
        let report = if cli {
            tokio::select! {
                report = self.run() => report,
                // `Ok` only: a handler that failed to install disables the
                // branch instead of reading as an interrupt.
                Ok(()) = tokio::signal::ctrl_c() => {
                    let rollback = guard.rollback();
                    println!("INTERRUPTED (Ctrl-C)");
                    for line in rollback.summary_lines() {
                        println!("{line}");
                    }
                    anyhow::bail!("interrupted (Ctrl-C); in-place edits rolled back");
                }
            }
        } else {
            self.run().await
        };
        match report.outcome {
            polymorph_review::Outcome::Passed { .. } => {
                guard.commit();
                Ok((report, None))
            }
            polymorph_review::Outcome::Escalated { .. } => {
                let rollback = guard.rollback();
                Ok((report, Some(rollback)))
            }
        }
    }
}

/// Every other exit (an error after `prepare_live_loop`, Ctrl-C, a panic,
/// a dropped TUI drive) still removes the `--repo` drive tree and the
/// emptied parent; after [`LiveLoopParts::finish`] it is a no-op.
impl Drop for LiveLoopParts {
    fn drop(&mut self) {
        self.finish();
    }
}

/// `[agent]` output bounding of a registry's `bash` tool (docket #9).
fn output_spill(
    agent: &polymorph_core::config::AgentConfig,
) -> polymorph_tools::output::SpillConfig {
    polymorph_tools::output::SpillConfig {
        inline_bytes: agent.tool_output_inline_bytes,
        keep_files: agent.tool_output_keep_files,
    }
}

/// `[agent]` tool-history budget of a fixer (docket #9).
fn tool_history(
    agent: &polymorph_core::config::AgentConfig,
) -> polymorph_review::ToolHistoryBudget {
    polymorph_review::ToolHistoryBudget {
        budget_bytes: agent.tool_history_budget_bytes,
        keep_rounds: agent.tool_history_keep_rounds,
    }
}

/// Which command builds the loop (docket #8): drives default to `[agent]
/// drive_on_fail` and hide their own `--events-json` ledger from fixer
/// scans; `review run` keeps a failed attempt unless `--on-fail` asks.
#[derive(Debug, Default, Clone, Copy)]
struct LoopEntry<'a> {
    drive: bool,
    events_json: Option<&'a std::path::Path>,
}

/// `--on-fail` > (drives: `[agent] drive_on_fail`) > keep. The config
/// value is free text, so a typo refuses the run pre-spend.
fn resolve_on_fail(
    flag: Option<&str>,
    drive: bool,
    configured: &str,
) -> anyhow::Result<polymorph_review::OnFail> {
    let value = match flag {
        Some(v) => v,
        None if drive => configured,
        None => "keep",
    };
    polymorph_review::OnFail::parse(value).ok_or_else(|| {
        anyhow::anyhow!("[agent] drive_on_fail must be \"rollback\" or \"keep\" (got {value:?})")
    })
}

/// Build reviewer + executor + loop config from `ReviewRunArgs` (the
/// wiring half of the old `run_review_live`; the run/print half stays
/// with the callers).
async fn prepare_live_loop(
    args: &ReviewRunArgs,
    overrides: &polymorph_core::config::CliOverrides,
    entry: LoopEntry<'_>,
) -> anyhow::Result<LiveLoopParts> {
    use polymorph_review::{
        AgenticFixRunner, BusExecutor, BusReviewer, FixTurnRunner, LlmTarget, ReviewTurnRunner,
    };
    use std::sync::Arc;
    let ReviewRunArgs {
        goal,
        artifact_file,
        artifact_files,
        diff,
        criteria,
        mode,
        scope,
        repo,
        review_pack,
        exec_pack,
        plan_pack,
        max_rounds,
        tools,
        max_tool_rounds,
        lsp_error_gate,
        reviewer_tools,
        verify_cmd,
        verify_timeout_secs,
        no_create_seed,
        on_fail,
    } = args;
    let mode = polymorph_review::LoopMode::parse(mode)
        .ok_or_else(|| anyhow::anyhow!("unknown mode {mode:?} (plan|act)"))?;
    // Docket #5: `--artifact-files` / `--diff` grade a set (with
    // `--artifact-file` as its first member); neither = the single-file
    // path below, byte for byte.
    let artifact_set = live_artifact_set(artifact_file.as_deref(), artifact_files, diff.as_deref());
    // Single-file mode: clap guarantees the path (required unless a set
    // flag is present). Set mode reads the set once the workdir is known.
    let artifact_file = match (&artifact_set, artifact_file) {
        (None, Some(path)) => Some(path),
        (None, None) => anyhow::bail!("--artifact-file, --artifact-files or --diff is required"),
        (Some(_), _) => None,
    };
    // `--repo`: ONE detached worktree for the whole drive (every fix turn,
    // verify command and reviewer works in it), so the set, peripheral
    // files, reviewer tools, spill files and the CREATE check all read the
    // fixer's live edits, while the operator's tree and index are never
    // written. Removed when the drive ends (`LiveLoopParts::finish`), or by
    // the guard's drop on any error, panic or cancel. Paths re-spell
    // repo-relative; the `--diff` default base pins to the start commit.
    let wt_parent = std::env::temp_dir().join(format!("polymorph-review-{}", std::process::id()));
    let drive_tree = match repo {
        None => None,
        Some(repo) => Some(open_drive_tree(repo, &wt_parent).await?),
    };
    let (artifact_file, artifact_set) = match &drive_tree {
        None => (artifact_file.cloned(), artifact_set),
        Some(tree) => rebase_on_drive_tree(tree, artifact_file, artifact_set)?,
    };
    let artifact = match &artifact_file {
        None => String::new(),
        Some(path) => {
            let full = match &drive_tree {
                Some(tree) => tree.path().join(path),
                None => path.clone(),
            };
            match std::fs::read_to_string(full.as_path()) {
                Ok(s) => s,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
                Err(e) => anyhow::bail!("cannot read {}: {e}", full.display()),
            }
        }
    };
    let review_system = polymorph_review::load_pack(review_pack)?;
    // Executor system is mode-routed: act rewrites artifacts, plan rewrites
    // plan documents (PLAN.md convention — G owns the artifact, F owns loop).
    let exec_pack_path = match mode {
        polymorph_review::LoopMode::Act => exec_pack,
        polymorph_review::LoopMode::Plan => plan_pack,
    };
    let exec_system = polymorph_review::load_pack(exec_pack_path)?;
    let (user_path, project_path) = polymorph_core::config::default_paths();
    let cfg = polymorph_core::config::load_with_cli(&user_path, &project_path, overrides)?;
    let router = polymorph_llm::RoleRouter::from_config(&cfg);
    let state = std::sync::Arc::new(tokio::sync::Mutex::new(polymorph_llm::FallbackState::new(
        std::time::Duration::from_secs(300),
    )));
    let bus = std::sync::Arc::new(polymorph_delegate::Bus::flat());
    // Every child works in place: the cwd, or the `--repo` drive tree.
    let workdir = polymorph_delegate::WorkdirSpec::InPlace(match &drive_tree {
        Some(tree) => tree.path().to_path_buf(),
        None => std::env::current_dir()?,
    });
    // LSP error gate (docket #3): flag or `[agent]` toggle, fixer-side
    // record + reviewer-side veto. Only `--tools` fixers consult LSP, so
    // without it the gate would never fire — say so instead of arming it.
    let lsp_gate = if *lsp_error_gate || cfg.agent.lsp_error_gate {
        if *tools {
            Some(Arc::new(polymorph_review::HarnessGate::default()))
        } else {
            tracing::warn!("lsp error gate needs --tools (text-only fixers record no LSP); off");
            None
        }
    } else {
        None
    };
    // Verify-command gate (docket #2): a non-empty flag list replaces the
    // `[agent]` list. Blank commands and a zero timeout bail pre-spend
    // (`cmd /C ""` would "pass" while verifying nothing).
    let verify_cmds = if verify_cmd.is_empty() {
        cfg.agent.verify_cmds.clone()
    } else {
        verify_cmd.clone()
    };
    if let Some(i) = verify_cmds.iter().position(|c| c.trim().is_empty()) {
        anyhow::bail!("verify command #{} is empty", i + 1);
    }
    let verify_timeout = verify_timeout_secs.unwrap_or(cfg.agent.verify_timeout_secs);
    if verify_timeout == 0 {
        anyhow::bail!("verify timeout must be at least 1 second");
    }
    // The gate starts seeded with the missing-evidence veto while the
    // declared commands have never run, so a round-1 PASS cannot stand
    // without verification ever executing; the first fix turn clears it.
    let verify_gate = (!verify_cmds.is_empty()).then(|| {
        Arc::new(polymorph_review::HarnessGate::with_unrun_verify(
            &verify_cmds,
        ))
    });
    // Reviewer tools (docket #4): flag or `[agent]` toggle.
    let reviewer_tools = *reviewer_tools || cfg.agent.reviewer_tools;
    if reviewer_tools && cfg.agent.reviewer_tool_rounds == 0 {
        anyhow::bail!("[agent] reviewer_tool_rounds must be at least 1");
    }
    // Docket #5: the set's first rendering is the round-1 artifact; git
    // and glob faults refuse the run here, before any spend.
    let (artifact, set_seed) = match (&artifact_set, &workdir) {
        (Some(set), polymorph_delegate::WorkdirSpec::InPlace(dir)) => {
            let rendered = set.render(dir)?;
            if let Some(g) = rendered.empty_globs.first() {
                anyhow::bail!(
                    "--artifact-files glob {g:?} matched no file under {}",
                    dir.display()
                );
            }
            if !*tools && *max_rounds > 1 {
                tracing::warn!(
                    "artifact set review grades files on disk; a text-only fixer (no --tools) cannot change them"
                );
            }
            (rendered.text.clone(), rendered.create_seed())
        }
        _ => (artifact, None),
    };
    let opted_out = *no_create_seed || !cfg.agent.create_seed;
    let seed = match &artifact_file {
        Some(path) => live_create_seed(opted_out, &workdir, path, &artifact),
        // Set CREATE rule: every member a missing declared path, no diff.
        None => set_seed.filter(|_| !opted_out),
    };
    // Rollback ledger (docket #8): only where a failed drive can leave
    // edits behind — the operator's tree and a fixer with file tools
    // (text-only fixers never write; a `--repo` drive tree is discarded
    // whole, so the operator's tree has nothing to roll back).
    let on_fail_mode = resolve_on_fail(on_fail.as_deref(), entry.drive, &cfg.agent.drive_on_fail)?;
    let restore = match (&workdir, on_fail_mode, *tools && drive_tree.is_none()) {
        (
            polymorph_delegate::WorkdirSpec::InPlace(dir),
            polymorph_review::OnFail::Rollback,
            true,
        ) => Some(Arc::new(
            polymorph_review::RestoreLedger::new(dir.clone())
                .with_command_change_cap(cfg.agent.command_change_max_files),
        )),
        (_, polymorph_review::OnFail::Rollback, _) if on_fail.is_some() => {
            tracing::warn!(
                "--on-fail rollback is a no-op here: only in-place --tools runs edit the tree (--repo never writes it)"
            );
            None
        }
        _ => None,
    };
    // Tool scope for `--tools`: the full V1 floor, Gate-enforced per call.
    // Text-only (default) records an empty scope, as before.
    // MCP servers (chunk 55): enrolled from config when `--tools` is on —
    // fail-soft per server, names join the model allowlist, the unchanged
    // Gate still enforces every call.
    let mut registry = polymorph_tools::registry::v1_registry();
    registry.set_output_spill(output_spill(&cfg.agent));
    let mut tool_names: Vec<String> = registry.names();
    let mut lsp_languages: Vec<String> = Vec::new();
    if *tools {
        tool_names
            .extend(polymorph_tools::mcp::wire_mcp_servers(&mut registry, &cfg.mcp_servers).await);
        // LSP servers (chunk 95 + full-roster config): enrolled from
        // `[[lsp_servers]]` when `--tools` is on — fail-soft per entry
        // (warn + continue), lazy spawn on first write. Bench stays
        // unwired (apparatus stability, the MCP-stated reason).
        lsp_languages = polymorph_tools::lsp::wire_lsp_servers(&registry, &cfg.lsp_servers).await;
        // DAP adapters (chunk 108): enrolled from config when `--tools`
        // is on — fail-soft per entry (warn + continue), DAP tool names
        // are already in the V1 floor so only specs enroll here. Bench
        // deliberately stays unwired (apparatus stability); ACP serve
        // stays unwired (NOT here, stated). The unchanged Gate still
        // enforces every `dap_<op>:<program>` call.
        let _enrolled = polymorph_tools::dap::wire_dap_adapters(&registry, &cfg.dap_adapters).await;
        // RenderDoc binary (chunk 112): single enroll from `[renderdoc]`,
        // fail-soft (warn past a bad path); the `rdoc_capture` name rides
        // the V1 floor already. Bench/ACP stay unwired (108 precedent).
        let _rdoc = polymorph_tools::renderdoc::wire_renderdoc(&registry, &cfg.renderdoc.cmd).await;
        // Web backend (chunk 120, P1b): enroll `[web]` (fail-soft like
        // rdoc); the `web_search`/`web_extract` names ride the V1 floor
        // already. Bench/ACP stay unwired (108 precedent).
        let _web = polymorph_tools::web::wire_web(&registry, &cfg.web).await;
    } else if reviewer_tools {
        // Docket #4: a tool reviewer's `lsp_*` lookups need the servers
        // even when the fixer is text-only (which never consults them).
        lsp_languages = polymorph_tools::lsp::wire_lsp_servers(&registry, &cfg.lsp_servers).await;
    }
    // Script hooks (chunk 87): bad `[[hooks]]` config bails pre-spend
    // (fail-closed — a typo'd hook never silently degrades to ambient).
    registry
        .apply_script_hooks(&cfg.hooks)
        .map_err(|e| anyhow::anyhow!("invalid [[hooks]] config: {e:#}"))?;
    // Docket #8: the drive's own events ledger (any name — a rerun usually
    // reuses the last run's path) never surfaces in fixer `grep`/`glob`.
    if let Some(path) = entry.events_json {
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()?.join(path)
        };
        registry.hide_from_scans(path);
    }
    // The drive tree's marker file is harness state, never project content.
    if let Some(tree) = &drive_tree {
        registry.hide_from_scans(tree.path().join(polymorph_delegate::WORKTREE_MARKER));
    }
    let registry = std::sync::Arc::new(registry);
    // Reviewer (docket #4): opt-in read-only tool loop over the shared
    // registry (same LSP pool, spill policy, hooks and scan hiding); off =
    // the single text turn, byte-identical.
    let review_target = LlmTarget::Managed {
        router: router.clone(),
        providers: cfg.providers.clone(),
        state: state.clone(),
        role: polymorph_core::Role::Review,
        scope: scope.clone(),
    };
    let (review_runner, review_tools): (Arc<dyn polymorph_delegate::TurnRunner>, _) =
        if reviewer_tools {
            let runner = std::sync::Arc::new(polymorph_review::ToolReviewTurnRunner::live(
                polymorph_llm::LlmClient::with_fresh_conversation(),
                review_target,
                review_system,
                registry.clone(),
                &cfg.permissions,
                !lsp_languages.is_empty(),
                cfg.agent.reviewer_tool_rounds,
                tool_history(&cfg.agent),
            ));
            (runner.clone(), Some(runner))
        } else {
            let runner = std::sync::Arc::new(ReviewTurnRunner {
                client: polymorph_llm::LlmClient::with_fresh_conversation(),
                target: review_target,
                system: review_system,
                pinned: Default::default(),
            });
            (runner, None)
        };
    let reviewer = polymorph_review::HarnessGateReviewer {
        inner: BusReviewer {
            bus: bus.clone(),
            runner: review_runner,
            workdir: workdir.clone(),
            // Set mode: no single path (the set text names every file).
            artifact_path: artifact_file.clone(),
        },
        // Verify first: its output is post-turn ground truth, while LSP
        // snapshots date from each file's last write.
        gates: verify_gate.iter().chain(lsp_gate.iter()).cloned().collect(),
    };
    let gate = polymorph_tools::Gate::from_config(&cfg.permissions);
    let runner: Arc<dyn polymorph_delegate::TurnRunner> = if *tools {
        let mut fixer = AgenticFixRunner::live(
            polymorph_llm::LlmClient::with_fresh_conversation(),
            LlmTarget::Managed {
                router: router.clone(),
                providers: cfg.providers.clone(),
                state: state.clone(),
                role: mode.executor_role(),
                scope: scope.clone(),
            },
            exec_system,
            registry,
            gate,
            tool_names.clone(),
            *max_tool_rounds,
        );
        fixer.max_bonus_tool_rounds = cfg.agent.max_bonus_tool_rounds;
        fixer.history = tool_history(&cfg.agent);
        fixer.command_changes = cfg.agent.command_change_max_files;
        fixer.restore = restore.clone();
        // S advisor (chunk 126): project toggle (`[advisor] enabled`);
        // off = pre-S behavior exactly (bench ablation below unwired).
        if cfg.advisor.enabled {
            fixer.advisor = Some(polymorph_review::AdvisorRunner::managed(
                router,
                cfg.providers.clone(),
                state,
            ));
        }
        match lsp_gate {
            Some(gate) => Arc::new(polymorph_review::LspGateRunner {
                inner: Arc::new(fixer),
                gate,
            }),
            None => Arc::new(fixer),
        }
    } else {
        Arc::new(FixTurnRunner {
            client: polymorph_llm::LlmClient::with_fresh_conversation(),
            target: LlmTarget::Managed {
                router,
                providers: cfg.providers.clone(),
                state,
                // Mode-routed (chunk-12 fix — was hardcoded Act, which sent
                // plan-fix turns down the act chain): Plan in plan mode.
                role: mode.executor_role(),
                scope: scope.clone(),
            },
            system: exec_system,
            pinned: Default::default(),
        })
    };
    // Outermost decorator: the verify commands run after the whole fix
    // turn (and its LSP record), before the reviewer sees the result.
    let runner: Arc<dyn polymorph_delegate::TurnRunner> = match verify_gate {
        Some(gate) => Arc::new(polymorph_review::VerifyGateRunner {
            inner: runner,
            gate,
            cmds: verify_cmds,
            timeout: std::time::Duration::from_secs(verify_timeout),
        }),
        None => runner,
    };
    // Docket #8: after each whole turn (verify commands included), adopt
    // the tracked files' state, so command-made edits roll back too.
    let runner: Arc<dyn polymorph_delegate::TurnRunner> = match &restore {
        Some(ledger) => Arc::new(polymorph_review::RestoreRefreshRunner {
            inner: runner,
            ledger: ledger.clone(),
        }),
        None => runner,
    };
    let executor = BusExecutor {
        bus,
        runner,
        workdir,
        tools: if *tools { tool_names } else { Vec::new() },
        tool_outcomes: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        merge: None,
        // Empty-text fallback (chunk 110): the file always exists here
        // (read at fn start), so grading file work on blank child text
        // is honest in every mode; non-empty text keeps exact behavior.
        artifact_path: artifact_file,
        // Docket #1: the reviewer grades every file the fixer
        // mutated, not only the artifact file.
        peripherals: Some(Default::default()),
        // Docket #5: `Some` grades the whole set from disk each round.
        artifact_set,
    };
    let loop_cfg = polymorph_core::LoopConfig {
        max_iterations: *max_rounds,
        ..Default::default()
    };
    Ok(LiveLoopParts {
        reviewer,
        executor,
        loop_cfg,
        goal: goal.clone(),
        criteria: criteria.clone(),
        artifact,
        mode,
        wt_parent,
        seed,
        restore,
        review_tools,
        drive_tree,
    })
}

/// Open the `--repo` drive tree under the pid-scoped temp parent (refused
/// when `repo` is not a git repo). Drive-unique ids: one process can run
/// several drives (the TUI).
async fn open_drive_tree(
    repo: &std::path::Path,
    wt_parent: &std::path::Path,
) -> anyhow::Result<polymorph_delegate::DriveWorktree> {
    static DRIVE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    if !polymorph_delegate::worktree::is_git_repo(repo).await {
        anyhow::bail!("--repo {} is not a git repo", repo.display());
    }
    std::fs::create_dir_all(wt_parent)
        .map_err(|e| anyhow::anyhow!("cannot create {}: {e}", wt_parent.display()))?;
    let id = format!(
        "drive-{}",
        DRIVE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let tree = polymorph_delegate::DriveWorktree::create(repo, wt_parent, &id).await?;
    tracing::info!(path = %tree.path().display(), start = tree.start_commit(), "--repo drive worktree");
    Ok(tree)
}

/// Re-spell the operator's paths for the `--repo` drive tree: the artifact
/// file and every declared set member become repo-relative (resolved
/// against the tree, like every tool path), and the `--diff` default base
/// (`HEAD`) pins to the drive's start commit, so a fixer `git commit` in
/// the tree cannot hide its own changes.
fn rebase_on_drive_tree(
    tree: &polymorph_delegate::DriveWorktree,
    artifact_file: Option<&std::path::PathBuf>,
    artifact_set: Option<polymorph_review::ArtifactSet>,
) -> anyhow::Result<(
    Option<std::path::PathBuf>,
    Option<polymorph_review::ArtifactSet>,
)> {
    let artifact_file = match artifact_file {
        Some(path) => Some(std::path::PathBuf::from(repo_relative(
            tree.repo(),
            &path.to_string_lossy(),
        )?)),
        None => None,
    };
    let artifact_set = match artifact_set {
        Some(mut set) => {
            for raw in &mut set.declared {
                *raw = repo_relative(tree.repo(), raw)?;
            }
            if set.diff_base.as_deref() == Some("HEAD") {
                set.diff_base = Some(tree.start_commit().to_string());
            }
            Some(set)
        }
        None => None,
    };
    Ok((artifact_file, artifact_set))
}

/// One `--repo` path, repo-relative: a relative path already is (the repo
/// root is the drive's workdir); an absolute one must lie inside the repo
/// (a missing file, a CREATE job, resolves through its parent). Outside
/// refuses: the drive would read or write the operator's files.
fn repo_relative(repo: &std::path::Path, raw: &str) -> anyhow::Result<String> {
    let path = std::path::Path::new(raw);
    if !path.is_absolute() {
        return Ok(raw.to_string());
    }
    let canon = |p: &std::path::Path| std::fs::canonicalize(p).ok();
    let full = canon(path).or_else(|| Some(canon(path.parent()?)?.join(path.file_name()?)));
    let rel = canon(repo).zip(full).and_then(|(root, full)| {
        full.strip_prefix(&root)
            .ok()
            .map(|rel| rel.to_string_lossy().replace('\\', "/"))
    });
    rel.ok_or_else(|| {
        anyhow::anyhow!(
            "{raw} is outside --repo {}: with --repo, name files inside the repo (relative paths are repo-relative)",
            repo.display()
        )
    })
}

/// The review scope the set flags ask for (docket #5): `None` without
/// `--artifact-files` and `--diff` (single-file mode, unchanged); else
/// `--artifact-file` first, then each `--artifact-files` value, plus the
/// diff base.
fn live_artifact_set(
    artifact_file: Option<&std::path::Path>,
    artifact_files: &[String],
    diff: Option<&str>,
) -> Option<polymorph_review::ArtifactSet> {
    if artifact_files.is_empty() && diff.is_none() {
        return None;
    }
    let declared = artifact_file
        .map(|p| p.to_string_lossy().into_owned())
        .into_iter()
        .chain(artifact_files.iter().cloned())
        .collect();
    Some(polymorph_review::ArtifactSet {
        declared,
        diff_base: diff.map(str::to_string),
    })
}

/// CREATE seed decision (docket #7): exactly when the reviewer brief would
/// carry the CREATE note (missing file, empty text) and the operator has
/// not opted out. An existing file, even empty, stays review-first (the
/// chunk-166 ruling).
fn live_create_seed(
    opted_out: bool,
    workdir: &polymorph_delegate::WorkdirSpec,
    artifact_file: &std::path::Path,
    artifact: &str,
) -> Option<polymorph_review::RoundSeed> {
    if opted_out {
        return None;
    }
    polymorph_review::create_job_seed(workdir, artifact_file, artifact)
}

/// Print one loop board (the `run` dialect, verbatim — shared so `run`
/// and `drive` cannot drift apart). `review_tools`: the tool reviewer's
/// per-call reports (docket #4; empty = reviewer tools off, the board
/// byte-identical), each printed under the round it reviewed.
fn print_loop_report(
    report: &polymorph_review::LoopReport,
    review_tools: &[polymorph_review::ReviewToolReport],
) {
    let reviewed = reviewed_round_reports(report, review_tools);
    for (r, tools) in report.rounds_log.iter().zip(reviewed) {
        println!(
            "round {}: {:?} findings={}{}",
            r.round,
            r.verdict,
            r.findings,
            round_suffix(r)
        );
        for issue in &r.issues {
            println!("  - {issue}");
        }
        for line in tools.map(|t| t.board_lines()).unwrap_or_default() {
            println!("{line}");
        }
    }
    match &report.outcome {
        polymorph_review::Outcome::Passed { rounds } => println!(
            "PASSED in {rounds} round{}",
            if *rounds == 1 { "" } else { "s" }
        ),
        polymorph_review::Outcome::Escalated { reason, rounds } => {
            println!("ESCALATED after {rounds} rounds: {reason:?}")
        }
    }
}

/// The tool reviewer report of each round (docket #4), in round order:
/// one report per reviewer call, so they pair off with the rounds that
/// called the reviewer (every round but a harness-seeded one), oldest
/// first. `None` for seeded rounds and when reviewer tools are off.
fn reviewed_round_reports<'a>(
    report: &polymorph_review::LoopReport,
    review_tools: &'a [polymorph_review::ReviewToolReport],
) -> Vec<Option<&'a polymorph_review::ReviewToolReport>> {
    let mut calls = review_tools.iter();
    report
        .rounds_log
        .iter()
        .map(|r| {
            if r.synthesized.is_some() {
                None
            } else {
                calls.next()
            }
        })
        .collect()
}

/// Attach each reviewed round's tool report to its `verdict` event as one
/// additive `reviewer_tools` object (docket #4), so existing consumers
/// keep parsing; nothing changes with reviewer tools off.
fn attach_review_tools(
    events: &mut serde_json::Value,
    report: &polymorph_review::LoopReport,
    review_tools: &[polymorph_review::ReviewToolReport],
) {
    let reviewed = reviewed_round_reports(report, review_tools);
    let verdicts = events
        .as_array_mut()
        .into_iter()
        .flatten()
        .filter(|e| e["type"] == "verdict");
    for (event, tools) in verdicts.zip(reviewed) {
        if let (Some(event), Some(tools)) = (event.as_object_mut(), tools) {
            event.insert("reviewer_tools".to_string(), tools.to_json());
        }
    }
}

/// Board/event marker of a round whose verdict was not a parsed model
/// reply: a harness-seeded round (docket #7; names the seed rule, CREATE
/// or START) or an Invalid round (says why). Empty for every other round,
/// so their boards and event files stay byte-identical.
fn round_suffix(r: &polymorph_review::RoundRecord) -> String {
    match (r.synthesized, invalid_reason(r)) {
        (Some(kind), _) => format!(
            " (harness-synthesized {} finding; no reviewer call)",
            kind.label()
        ),
        (None, Some(reason)) => format!(" (unparseable reviewer verdict: {reason})"),
        (None, None) => String::new(),
    }
}

/// Bound on an Invalid round's reason on the board and in events.
const MAX_INVALID_REASON_CHARS: usize = 200;

/// An Invalid round's reason as one bounded line (`None` otherwise).
fn invalid_reason(r: &polymorph_review::RoundRecord) -> Option<String> {
    let detail = r
        .invalid
        .as_deref()?
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let mut reason: String = detail.chars().take(MAX_INVALID_REASON_CHARS).collect();
    if detail.chars().count() > MAX_INVALID_REASON_CHARS {
        reason.push('\u{2026}');
    }
    Some(reason)
}

/// Remove the pid worktree parent (empty by now — ignore errors). The bus
/// auto-removes each worktree; this is the parent-dir half (F2b precedent).
fn scrub_worktree_parent(wt_parent: &std::path::Path) {
    let _ = std::fs::remove_dir(wt_parent);
}

/// Map a loop report 1:1 onto drive events (LlmResponder precedent —
/// cross-crate composition lives in CLI; manual JSON mapping beside it
/// keeps tui serde-free). Counts must match observed rounds (asserted
/// live in 140, unit-pinned here).
fn report_to_events(
    report: &polymorph_review::LoopReport,
    max_rounds: u32,
) -> Vec<polymorph_drive::DriveEvent> {
    use polymorph_drive::DriveEvent;
    let mut events = Vec::new();
    for r in &report.rounds_log {
        events.push(DriveEvent::RoundStarted {
            round: r.round as usize,
            max_rounds: max_rounds as usize,
        });
        events.push(DriveEvent::Verdict {
            round: r.round as usize,
            passed: matches!(r.verdict, polymorph_review::RoundVerdict::Pass),
            summary: format!("findings={}{}", r.findings, round_suffix(r)),
            invalid: invalid_reason(r),
            synthesized: r.synthesized.map(|k| k.as_str().to_string()),
        });
        for issue in &r.issues {
            events.push(DriveEvent::Finding {
                location: String::new(),
                message: issue.clone(),
            });
        }
    }
    match &report.outcome {
        polymorph_review::Outcome::Passed { .. } => events.push(DriveEvent::Finished {
            verdict: "PASSED".into(),
        }),
        polymorph_review::Outcome::Escalated { reason, .. } => events.push(DriveEvent::Halted {
            reason: format!("{reason:?}"),
        }),
    }
    events
}

/// Render events as JSON (the `--events-json` file shape — drive machine
/// seam; the protocol types stay serde-free, mapping lives here).
fn events_json_value(events: &[polymorph_drive::DriveEvent]) -> serde_json::Value {
    use polymorph_drive::DriveEvent;
    serde_json::Value::Array(
        events
            .iter()
            .map(|e| match e {
                DriveEvent::RoundStarted { round, max_rounds } => serde_json::json!({
                    "type": "round_started", "round": round, "max_rounds": max_rounds
                }),
                DriveEvent::Verdict {
                    round,
                    passed,
                    summary,
                    invalid,
                    synthesized,
                } => {
                    let mut v = serde_json::json!({
                        "type": "verdict", "round": round, "passed": passed, "summary": summary
                    });
                    // Additive keys, present only when they apply (an
                    // ordinary verdict event stays byte-identical).
                    if let Some(reason) = invalid {
                        v["invalid"] = serde_json::json!(true);
                        v["parse_error"] = serde_json::json!(reason);
                    }
                    if let Some(kind) = synthesized {
                        v["synthesized"] = serde_json::json!(kind);
                    }
                    v
                }
                DriveEvent::Finding { location, message } => serde_json::json!({
                    "type": "finding", "location": location, "message": message
                }),
                DriveEvent::DiffAvailable { status, stat } => serde_json::json!({
                    "type": "diff", "status": status, "stat": stat
                }),
                DriveEvent::PlanProgress { title, tasks } => serde_json::json!({
                    "type": "plan", "title": title,
                    "tasks": tasks.iter().map(|t| serde_json::json!({
                        "id": t.id, "title": t.title, "status": t.status
                    })).collect::<Vec<_>>()
                }),
                DriveEvent::Budget {
                    rounds_used,
                    rounds_max,
                    tools_used,
                    tools_max,
                } => {
                    serde_json::json!({
                        "type": "budget", "rounds_used": rounds_used,
                        "rounds_max": rounds_max, "tools_used": tools_used,
                        "tools_max": tools_max
                    })
                }
                DriveEvent::Halted { reason } => {
                    serde_json::json!({"type": "halted", "reason": reason})
                }
                DriveEvent::Finished { verdict } => {
                    serde_json::json!({"type": "finished", "verdict": verdict})
                }
            })
            .collect(),
    )
}

async fn run_review_live(
    args: Box<ReviewRunArgs>,
    overrides: &polymorph_core::config::CliOverrides,
) -> anyhow::Result<()> {
    let mut parts = prepare_live_loop(&args, overrides, LoopEntry::default()).await?;
    let (report, rollback) = parts.run_guarded(true).await?;
    print_loop_report(&report, &parts.review_reports());
    print_rollback(rollback.as_ref());
    parts.finish();
    Ok(())
}

/// Rollback summary lines on the board (docket #8; nothing when the run
/// kept its tree).
fn print_rollback(rollback: Option<&polymorph_review::RollbackReport>) {
    for line in rollback.map(|r| r.summary_lines()).unwrap_or_default() {
        println!("{line}");
    }
}

/// Attach the rollback report to the terminal `halted` event (docket #8):
/// one additive `rollback` key, so existing consumers keep parsing.
fn attach_rollback(events: &mut serde_json::Value, rollback: &polymorph_review::RollbackReport) {
    let halted = events.as_array_mut().and_then(|evs| {
        evs.iter_mut()
            .rev()
            .find(|e| e["type"] == "halted")
            .and_then(|e| e.as_object_mut())
    });
    if let Some(halted) = halted {
        halted.insert("rollback".to_string(), rollback.to_json());
    }
}

/// `review drive`: goal-as-text in (no ingestion — softened charter),
/// `DriveEvent` vec JSON out (drive machine seam), human board on stdout.
/// DAG pre-pass and merge-back explicitly out (docketed next slices).
async fn run_review_drive(
    args: Box<ReviewRunArgs>,
    events_json: Option<std::path::PathBuf>,
    overrides: &polymorph_core::config::CliOverrides,
) -> anyhow::Result<()> {
    let max_tool_rounds = args.max_tool_rounds;
    let entry = LoopEntry {
        drive: true,
        events_json: events_json.as_deref(),
    };
    let mut parts = prepare_live_loop(&args, overrides, entry).await?;
    let (report, rollback) = parts.run_guarded(true).await?;
    let review_reports = parts.review_reports();
    print_loop_report(&report, &review_reports);
    print_rollback(rollback.as_ref());
    let mut events = report_to_events(&report, parts.loop_cfg.max_iterations);
    let tools_used = parts
        .executor
        .tool_outcomes
        .lock()
        .map(|l| l.len())
        .unwrap_or(0);
    events.push(polymorph_drive::DriveEvent::Budget {
        rounds_used: report.rounds_log.len(),
        rounds_max: parts.loop_cfg.max_iterations as usize,
        tools_used,
        tools_max: max_tool_rounds as usize,
    });
    if let Some(path) = events_json {
        let mut value = events_json_value(&events);
        attach_review_tools(&mut value, &report, &review_reports);
        if let Some(rollback) = &rollback {
            attach_rollback(&mut value, rollback);
        }
        std::fs::write(&path, serde_json::to_string_pretty(&value)?)
            .map_err(|e| anyhow::anyhow!("drive: cannot write {}: {e}", path.display()))?;
    }
    match &report.outcome {
        polymorph_review::Outcome::Passed { rounds } => println!(
            "DRIVE PASSED in {rounds} round{} ({} events)",
            if *rounds == 1 { "" } else { "s" },
            events.len()
        ),
        polymorph_review::Outcome::Escalated { reason, rounds } => println!(
            "DRIVE HALTED after {rounds} rounds: {reason:?} ({}, {} events)",
            if rollback.is_some() {
                "tree rolled back"
            } else if parts.drive_tree.is_some() {
                "--repo worktree discarded"
            } else {
                "tree kept"
            },
            events.len()
        ),
    }
    parts.finish();
    // Explicit flush (kept from the concurrent Antigravity fix): normally
    // redundant on clean return, but guarantees console output lands even
    // if a downstream drop misbehaves. No std::process::exit here, though:
    // mid-future exit skips async destructors, Tokio shutdown, and
    // log-guard flush, and kills any test calling this. Propagate via
    // Result so main's return drives the exit code instead.
    use std::io::Write;
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    match &report.outcome {
        polymorph_review::Outcome::Passed { .. } => Ok(()),
        polymorph_review::Outcome::Escalated { reason, rounds } => {
            anyhow::bail!("DRIVE HALTED after {rounds} rounds: {reason:?}")
        }
    }
}

/// `review eval-ab`: the SAME seeded case through two live reviewer arms
/// (Epic F4b, TBM-15 live half). Arm A reviews on `review_model_a`, arm B
/// on `review_model_b` (both qualified through claiming entries, chunk 13;
/// qualified specs pass through untouched). Both fixers share the mode
/// executor chain text-only — the reviewer model is the isolated variable.
/// One process, one bus, one shared fallback state (cooldowns shared, same
/// as `run`). InPlace workdir only (no `--repo`: worktree isolation is
/// F2b-part1-proven and orthogonal to the reviewer-model question).
/// Spend guard: `--max-rounds` caps EACH arm (worst case 2 × max_rounds
/// rounds × 2 turns; default 2 bounds a probe to 8 turns).
#[allow(clippy::too_many_arguments)]
async fn run_review_eval_ab(
    goal: String,
    artifact_file: std::path::PathBuf,
    criteria: String,
    mode: String,
    scope: Option<String>,
    review_pack: std::path::PathBuf,
    exec_pack: std::path::PathBuf,
    plan_pack: std::path::PathBuf,
    review_model_a: String,
    review_model_b: String,
    max_rounds: u32,
    overrides: &polymorph_core::config::CliOverrides,
) -> anyhow::Result<()> {
    use polymorph_review::{BusExecutor, BusReviewer, FixTurnRunner, LlmTarget, ReviewTurnRunner};
    use std::sync::Arc;
    let mode = polymorph_review::LoopMode::parse(&mode)
        .ok_or_else(|| anyhow::anyhow!("unknown mode {mode:?} (plan|act)"))?;
    let artifact = match std::fs::read_to_string(&artifact_file) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => anyhow::bail!("cannot read {}: {e}", artifact_file.display()),
    };
    let review_system = polymorph_review::load_pack(&review_pack)?;
    let exec_pack_path = match mode {
        polymorph_review::LoopMode::Act => &exec_pack,
        polymorph_review::LoopMode::Plan => &plan_pack,
    };
    let exec_system = polymorph_review::load_pack(exec_pack_path)?;
    let (user_path, project_path) = polymorph_core::config::default_paths();
    let cfg = polymorph_core::config::load_with_cli(&user_path, &project_path, overrides)?;
    // Per-arm reviewer chains: clone the configured roles, override ONLY
    // the review key (executors keep the configured mode chain on the base
    // router — the reviewer model is the isolated variable). Qualification
    // first, so bare names ride claiming entries exactly like `exec`.
    let spec_a = polymorph_core::config::qualify_model(&cfg.providers, &review_model_a);
    let spec_b = polymorph_core::config::qualify_model(&cfg.providers, &review_model_b);
    let mut roles_a = cfg.roles.clone();
    roles_a.insert("review".to_string(), vec![spec_a.clone()]);
    let mut roles_b = cfg.roles.clone();
    roles_b.insert("review".to_string(), vec![spec_b.clone()]);
    let router_base = polymorph_llm::RoleRouter::from_config(&cfg);
    let router_a = polymorph_llm::RoleRouter::with_pins(roles_a, cfg.pins.clone());
    let router_b = polymorph_llm::RoleRouter::with_pins(roles_b, cfg.pins.clone());
    let state = std::sync::Arc::new(tokio::sync::Mutex::new(polymorph_llm::FallbackState::new(
        std::time::Duration::from_secs(300),
    )));
    let bus = std::sync::Arc::new(polymorph_delegate::Bus::flat());
    let workdir = polymorph_delegate::WorkdirSpec::InPlace(std::env::current_dir()?);
    let reviewer_a = BusReviewer {
        bus: bus.clone(),
        runner: Arc::new(ReviewTurnRunner {
            client: polymorph_llm::LlmClient::with_fresh_conversation(),
            target: LlmTarget::Managed {
                router: router_a,
                providers: cfg.providers.clone(),
                state: state.clone(),
                role: polymorph_core::Role::Review,
                scope: scope.clone(),
            },
            system: review_system.clone(),
            pinned: Default::default(),
        }),
        workdir: workdir.clone(),
        artifact_path: Some(artifact_file.clone()),
    };
    let reviewer_b = BusReviewer {
        bus: bus.clone(),
        runner: Arc::new(ReviewTurnRunner {
            client: polymorph_llm::LlmClient::with_fresh_conversation(),
            target: LlmTarget::Managed {
                router: router_b,
                providers: cfg.providers.clone(),
                state: state.clone(),
                role: polymorph_core::Role::Review,
                scope: scope.clone(),
            },
            system: review_system,
            pinned: Default::default(),
        }),
        workdir: workdir.clone(),
        artifact_path: Some(artifact_file.clone()),
    };
    // Text-only fixers on the CONFIGURED mode chain (shared): pre-F3b
    // behavior, still the right default when the reviewer is the variable.
    let mk_fixer = |router: polymorph_llm::RoleRouter| {
        Arc::new(FixTurnRunner {
            client: polymorph_llm::LlmClient::with_fresh_conversation(),
            target: LlmTarget::Managed {
                router,
                providers: cfg.providers.clone(),
                state: state.clone(),
                role: mode.executor_role(),
                scope: scope.clone(),
            },
            system: exec_system.clone(),
            pinned: Default::default(),
        })
    };
    let executor_a = BusExecutor {
        bus: bus.clone(),
        runner: mk_fixer(router_base.clone()),
        workdir: workdir.clone(),
        tools: Vec::new(),
        tool_outcomes: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        merge: None,
        // eval-ab boards stay comparable — stays None (chunk-110 scope).
        artifact_path: None,
        peripherals: None,
        artifact_set: None,
    };
    let executor_b = BusExecutor {
        bus,
        runner: mk_fixer(router_base),
        workdir,
        tools: Vec::new(),
        tool_outcomes: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        merge: None,
        // eval-ab boards stay comparable — stays None (chunk-110 scope).
        artifact_path: None,
        peripherals: None,
        artifact_set: None,
    };
    let loop_cfg = polymorph_core::LoopConfig {
        max_iterations: max_rounds,
        ..Default::default()
    };
    let case = polymorph_review::AbCase::new("eval-ab", &goal, &criteria, &artifact);
    let rep = polymorph_review::run_ab_pair(
        &loop_cfg,
        &case,
        mode,
        &format!("A:{spec_a}"),
        &reviewer_a,
        &executor_a,
        &format!("B:{spec_b}"),
        &reviewer_b,
        &executor_b,
    )
    .await;
    for (arm, r) in [("A", &rep.arm_a), ("B", &rep.arm_b)] {
        println!(
            "arm {arm} ({}): turns={} invalid={} findings_seen={} passed={} {:?}",
            r.arm, r.turns, r.invalid_rounds, r.findings_seen, r.passed, r.report.outcome,
        );
        for rd in &r.report.rounds_log {
            println!(
                "  arm {arm} round {}: {:?} findings={}",
                rd.round, rd.verdict, rd.findings
            );
            for issue in &rd.issues {
                println!("    - {issue}");
            }
        }
    }
    println!("delta_rounds (B-A): {}", rep.delta_rounds);
    Ok(())
}

/// `eval run`: one live model turn scored against an expected end state
/// (Epic F5b). Reads `--input`, mints its tag over normalized text (the
/// same convention the scorer verifies), renders the F5a prompt, runs ONE
/// turn on `--model` (qualified + injected, no fallback), scores with the
/// F5a scorer in a pid-scoped case dir (auto-removed), prints the outcome,
/// and optionally records JSON for CI replay. Needs provider auth like
/// `exec --model`. No Ctrl-C wiring (review-live precedent for short
/// bounded calls — stated, not hidden).
#[allow(clippy::too_many_arguments)]
async fn run_eval_live(
    instruction: String,
    input: std::path::PathBuf,
    expected: std::path::PathBuf,
    model: String,
    system_pack: std::path::PathBuf,
    format_raw: String,
    record: Option<std::path::PathBuf>,
    overrides: &polymorph_core::config::CliOverrides,
) -> anyhow::Result<()> {
    use polymorph_tools::hashline::{apply::normalize, SnapshotStore};
    // Bogus `--format` bails before any IO/scorer/LLM work (F5g
    // fail-fast precedent: constraint faults create nothing).
    let format =
        polymorph_tools::EvalFormat::parse(&format_raw).map_err(|e| anyhow::anyhow!("{e}"))?;
    let system_pack = resolve_system_pack(format, &system_pack);
    let input_text = std::fs::read_to_string(&input)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", input.display()))?;
    let expected_text = std::fs::read_to_string(&expected)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", expected.display()))?;
    let name = input
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "case.txt".to_string());
    // Arm-specific prompt: tagged hashline workspace vs untagged
    // str_replace workspace (tags would leak hashline concepts — D-plan).
    let prompt = match format {
        polymorph_tools::EvalFormat::Hashline => {
            let norm = normalize(input_text.as_bytes());
            let mut store = SnapshotStore::new();
            let tag = store.mint(std::path::Path::new(&name), &norm.text);
            polymorph_tools::render_eval_prompt(
                &[(name.clone(), input_text.clone(), tag)],
                &instruction,
            )
        }
        polymorph_tools::EvalFormat::StrReplace => {
            polymorph_tools::render_str_prompt(&[(name.clone(), input_text.clone())], &instruction)
        }
    };
    let system = polymorph_review::load_pack(&system_pack)?;
    let (user_path, project_path) = polymorph_core::config::default_paths();
    let cfg = polymorph_core::config::load_with_cli(&user_path, &project_path, overrides)?;
    let client = polymorph_llm::LlmClient::with_fresh_conversation();
    let mut state = polymorph_llm::FallbackState::new(std::time::Duration::from_secs(300));
    let cancel = tokio_util::sync::CancellationToken::new();
    let mut sink = String::new();
    let outcome = client
        .stream_chat_single(
            &cfg.providers,
            &mut state,
            &model,
            &system,
            &prompt,
            &cancel,
            &mut sink,
        )
        .await?;
    let case = polymorph_tools::EvalCase {
        id: name.clone(),
        files: vec![(name.clone(), input_text.clone())],
        expected: vec![(name.clone(), expected_text.clone())],
        model_output: outcome.text.clone(),
    };
    // Pid-scoped case dir (run_review_live precedent); the guard removes it
    // on EVERY exit (same strand class the sweep guard covers).
    let case_dir = std::env::temp_dir().join(format!("polymorph-eval-{}", std::process::id()));
    std::fs::create_dir_all(&case_dir)
        .map_err(|e| anyhow::anyhow!("cannot create {}: {e}", case_dir.display()))?;
    let _guard = RemoveOnDrop(case_dir.clone());
    let report = match format {
        polymorph_tools::EvalFormat::Hashline => polymorph_tools::score_case(&case, &case_dir),
        polymorph_tools::EvalFormat::StrReplace => {
            polymorph_tools::score_str_case(&case, &case_dir)
        }
    };
    let head: String = outcome.text.chars().take(300).collect();
    println!("model output (head): {head}");
    println!("score: {:?}", report.outcome);
    let board = polymorph_tools::Scoreboard {
        reports: vec![report.clone()],
    };
    match polymorph_tools::gate(&board, 1.0) {
        Ok(()) => println!("GATE: green"),
        Err(e) => println!("GATE: red ({e})"),
    }
    if let Some(dir) = record {
        std::fs::create_dir_all(&dir)
            .map_err(|e| anyhow::anyhow!("cannot create {}: {e}", dir.display()))?;
        let rec = recording_json(
            &case.id,
            &model,
            &instruction,
            &[(name.clone(), input_text.clone())],
            &[(name.clone(), expected_text.clone())],
            &outcome.text,
            &format!("{:?}", report.outcome),
            1,
            format,
        );
        let dest = dir.join(format!("{name}.eval.json"));
        std::fs::write(&dest, serde_json::to_string_pretty(&rec).unwrap())
            .map_err(|e| anyhow::anyhow!("cannot write {}: {e}", dest.display()))?;
        println!("recorded: {}", dest.display());
    }
    Ok(())
}

/// Re-score `eval run --record` JSONs offline and gate the board (Epic
/// F5c, CI replay — LLM-free). `path` is one recording or a dir of sorted
/// `*.eval.json`; every case scores in its own tempdir (auto-removed).
/// Prints per-id outcomes plus the GATE line and bails on red (`review
/// check` precedent — a CI gate that prints red but exits 0 gates nothing).
/// Corrupt recordings bail naming the file + field (loader precedent:
/// operator error, never a model score).
fn run_eval_check(path: std::path::PathBuf, min_rate: f64) -> anyhow::Result<()> {
    let mut files: Vec<std::path::PathBuf> = if path.is_dir() {
        let found: Vec<std::path::PathBuf> = std::fs::read_dir(&path)
            .map_err(|e| anyhow::anyhow!("cannot list {}: {e}", path.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("json"))
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.ends_with(".eval.json"))
            })
            .collect();
        found
    } else if path.is_file() {
        vec![path]
    } else {
        anyhow::bail!("cannot read {}: no such file or directory", path.display());
    };
    // Single-file check skips the extension filter above (the operator named
    // it); dir scans stay `*.eval.json`-only so stray files never score.
    files.sort();
    // Pid- + sequence-scoped parent (run_eval_live precedent — tempfile
    // stays dev-only); one subdir per recording, whole parent auto-removed
    // at the end. The sequence suffix keeps concurrent in-process runs
    // (tests) off each other's tempdirs — pid alone collides across
    // threads of one process (chunk-28 lesson: two `check` tests raced on
    // one parent and the loser's setup writes failed).
    static CHECK_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = CHECK_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let parent =
        std::env::temp_dir().join(format!("polymorph-eval-check-{}-{seq}", std::process::id()));
    std::fs::create_dir_all(&parent)
        .map_err(|e| anyhow::anyhow!("cannot create {}: {e}", parent.display()))?;
    // Score inside a closure so EVERY exit (loader bail included) runs past
    // the parent removal below — a corrupt recording must not strand temp.
    let scored = (|| -> anyhow::Result<polymorph_tools::Scoreboard> {
        let mut board = polymorph_tools::Scoreboard::default();
        for file in &files {
            let text = std::fs::read_to_string(file)
                .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", file.display()))?;
            // Per-record arm dispatch (chunk 28): each recording declares
            // its scorer via the `format` stamp (absent = hashline, so
            // every legacy recording replays unchanged). Scoring under the
            // non-stamped arm would forge data — the loader refuses
            // unknown stamps naming the value.
            let (case, format) = polymorph_tools::load_recording_with_format(&text)
                .map_err(|e| anyhow::anyhow!("{}: {e}", file.display()))?;
            let stem = file
                .file_stem()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "case".to_string());
            let dir = parent.join(stem);
            std::fs::create_dir_all(&dir)
                .map_err(|e| anyhow::anyhow!("cannot create {}: {e}", dir.display()))?;
            let report = match format {
                polymorph_tools::EvalFormat::Hashline => polymorph_tools::score_case(&case, &dir),
                polymorph_tools::EvalFormat::StrReplace => {
                    polymorph_tools::score_str_case(&case, &dir)
                }
            };
            println!("{} [{}]: {:?}", report.id, format, report.outcome);
            board.reports.push(report);
        }
        Ok(board)
    })();
    let _ = std::fs::remove_dir_all(&parent);
    let board = scored?;
    println!("score: {}/{} passed", board.passed(), board.total());
    match polymorph_tools::gate(&board, min_rate) {
        Ok(()) => {
            println!("GATE: green");
            Ok(())
        }
        Err(e) => {
            println!("GATE: red ({e})");
            anyhow::bail!("{e}");
        }
    }
}

/// Default system packs per eval format arm (Chunk 28): `--format` selects
/// the default unless `--system-pack` is explicitly overridden (resolved by
/// `resolve_system_pack` — an explicit non-default path always wins).
const DEFAULT_HASHLINE_PACK: &str = "prompts/glm/glm-5.3/edit.md";
const DEFAULT_STR_PACK: &str = "prompts/glm/glm-5.3/str_replace.md";

/// Resolve the system-pack path for an eval invocation: the str-replace
/// default applies when the caller left the clap default (hashline pack)
/// untouched and asked for `--format str-replace`. Pure for tests.
fn resolve_system_pack(
    format: polymorph_tools::EvalFormat,
    system_pack: &std::path::Path,
) -> std::path::PathBuf {
    use polymorph_tools::EvalFormat;
    match format {
        EvalFormat::StrReplace if system_pack == std::path::Path::new(DEFAULT_HASHLINE_PACK) => {
            std::path::PathBuf::from(DEFAULT_STR_PACK)
        }
        _ => system_pack.to_path_buf(),
    }
}

/// One recording's worth of fields (v2 shape since F5e: `files` +
/// `expected` as `{name, content}` arrays — multi-file, absence, and
/// rename all expressible; the loader still reads legacy string shapes).
/// `attempts` is provenance like `outcome` (the scorer re-derives, the
/// loader ignores it — pinned by test). `format` stamps the scoring arm
/// (chunk 28); `check` dispatches per record through
/// `load_recording_with_format`. Shared by `run` (single-element arrays,
/// 1 attempt) and `sweep` so `check` replays both with zero cases.
#[allow(clippy::too_many_arguments)]
fn recording_json(
    id: &str,
    model: &str,
    instruction: &str,
    files: &[(String, String)],
    expected: &[(String, String)],
    output: &str,
    outcome: &str,
    attempts: u32,
    format: polymorph_tools::EvalFormat,
) -> serde_json::Value {
    let pair = |(n, c): &(String, String)| serde_json::json!({"name": n, "content": c});
    serde_json::json!({
        "id": id,
        "model": model,
        "instruction": instruction,
        "files": files.iter().map(pair).collect::<Vec<_>>(),
        "expected": expected.iter().map(pair).collect::<Vec<_>>(),
        "output": output,
        "outcome": outcome,
        "attempts": attempts,
        "format": format.as_str(),
    })
}

/// Retry feedback for one scored attempt (Epic F5g, pure for tests):
/// `None` stops (Pass — terminal success); `Some(text)` retries with the
/// text appended to the next attempt's prompt. Non-leakage by
/// construction: Extract/Apply displays name faults/paths (never expected
/// bytes — verified against every `HashlineError`/`ExtractError` variant),
/// and Mismatch feedback is a CONSTANT (the verdict names the file on the
/// board line already; the diff itself never reaches the model).
fn retry_feedback(outcome: &polymorph_tools::CaseOutcome) -> Option<String> {
    use polymorph_tools::CaseOutcome;
    match outcome {
        CaseOutcome::Pass => None,
        CaseOutcome::Mismatch(_) => Some(
            "Your previous patch applied but the result did not match the task. \
             Re-read the task and emit ONE corrected hashline patch per the system prompt."
                .to_string(),
        ),
        CaseOutcome::ExtractFail(fault) | CaseOutcome::ApplyFail(fault) => Some(format!(
            "Your previous output failed ({fault}). \
             Emit ONE hashline patch per the system prompt."
        )),
    }
}

/// Retry feedback for the str_replace arm (Chunk 28): the SAME shape as
/// `retry_feedback` (attempt parity, design rule 3) with the arm's nouns.
/// Same non-leakage contract: fault text verbatim, mismatch constant.
fn retry_feedback_str(outcome: &polymorph_tools::CaseOutcome) -> Option<String> {
    use polymorph_tools::CaseOutcome;
    match outcome {
        CaseOutcome::Pass => None,
        CaseOutcome::Mismatch(_) => Some(
            "Your previous patch applied but the result did not match the task. \
             Re-read the task and emit ONE corrected str_replace patch per the system prompt."
                .to_string(),
        ),
        CaseOutcome::ExtractFail(fault) | CaseOutcome::ApplyFail(fault) => Some(format!(
            "Your previous output failed ({fault}). \
             Emit ONE str_replace patch per the system prompt."
        )),
    }
}

/// One parsed B6 corpus task (Epic F5d sweep input). `files`/`expected`
/// sorted by name; `expect_error` carries the rejection variant when the
/// corpus declares one (those tasks skip — byte-compare cannot score a
/// rejection, and a live turn mints live tags so it could never reproduce
/// the stale/corrupt setup anyway).
#[derive(Debug, Clone)]
struct SweepTask {
    id: String,
    description: String,
    instruction: Option<String>,
    files: Vec<(String, String)>,
    expected: Vec<(String, String)>,
    expect_error: Option<String>,
}

/// Parse the B6 TOML into tasks (pure over text so fixtures prove the
/// skip classes; the live test parses the committed corpus). Table shapes
/// mirror the edit-corpus driver (`eval_b6`); unknown shapes bail naming
/// the task (operator error, never a score).
fn parse_sweep_corpus(toml_text: &str) -> Result<Vec<SweepTask>, String> {
    let doc: toml::Value =
        toml::from_str(toml_text).map_err(|e| format!("corpus: invalid TOML: {e}"))?;
    let tasks = doc
        .get("task")
        .and_then(toml::Value::as_array)
        .ok_or_else(|| "corpus: no [[task]] array".to_string())?;
    let mut out = Vec::new();
    for t in tasks {
        let id = t
            .get("id")
            .and_then(toml::Value::as_str)
            .ok_or_else(|| "corpus: task missing `id`".to_string())?
            .to_string();
        let field = |name: &str| {
            t.get(name)
                .and_then(toml::Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| format!("corpus: task {id:?} missing `{name}`"))
        };
        let description = field("description")?;
        let files_table = t
            .get("files")
            .and_then(toml::Value::as_table)
            .ok_or_else(|| format!("corpus: task {id:?} missing `[task.files]`"))?;
        let mut files: Vec<(String, String)> = files_table
            .iter()
            .map(|(k, v)| {
                v.as_str()
                    .map(|s| (k.clone(), s.to_string()))
                    .ok_or_else(|| format!("corpus: task {id:?} file {k:?} not a string"))
            })
            .collect::<Result<_, _>>()?;
        files.sort();
        let expect = t
            .get("expect")
            .and_then(toml::Value::as_table)
            .ok_or_else(|| format!("corpus: task {id:?} missing `[task.expect]`"))?;
        let expect_error = expect
            .get("error")
            .and_then(toml::Value::as_str)
            .map(str::to_string);
        let mut expected = Vec::new();
        if let Some(want) = expect.get("files").and_then(toml::Value::as_table) {
            for (k, v) in want {
                expected.push((
                    k.clone(),
                    v.as_str().map(str::to_string).ok_or_else(|| {
                        format!("corpus: task {id:?} expect file {k:?} not a string")
                    })?,
                ));
            }
            expected.sort();
        }
        out.push(SweepTask {
            id,
            description,
            instruction: t
                .get("instruction")
                .and_then(toml::Value::as_str)
                .map(str::to_string),
            files,
            expected,
            expect_error,
        });
    }
    Ok(out)
}

/// One sweepable case: workspace files + expected end state as (name,
/// content) vecs (v2 recording shape — multi-file, absence, and rename
/// all expressible; only rejection tasks stay out).
#[derive(Debug, Clone)]
struct SweepCase {
    id: String,
    instruction: String,
    files: Vec<(String, String)>,
    expected: Vec<(String, String)>,
}

/// Split tasks into scorable cases + NAMED skips (nothing narrows
/// silently: every skip prints with its class at runtime). Only
/// rejection tasks skip — byte-compare cannot score a rejection, and a
/// live turn mints live tags so it could never reproduce the stale setup.
/// Under the str_replace arm (chunk 28, D1) two more shapes skip NAMED:
/// whole-file deletion (empty `expected`) and rename/create (file-name
/// sets differ) — substitution has no form for either, so forcing them
/// would measure the contract gap, not the format.
fn sweep_plan(
    tasks: Vec<SweepTask>,
    format: polymorph_tools::EvalFormat,
) -> (Vec<SweepCase>, Vec<String>) {
    let mut cases = Vec::new();
    let mut skips = Vec::new();
    for t in tasks {
        if let Some(variant) = t.expect_error {
            skips.push(format!(
                "{}: rejection-task (expects {variant}; byte-compare cannot score rejection)",
                t.id
            ));
        } else if format == polymorph_tools::EvalFormat::StrReplace && t.expected.is_empty() {
            skips.push(format!(
                "{}: str-replace-unsupported (whole-file deletion has no substitution form; D1 named-skip)",
                t.id
            ));
        } else if format == polymorph_tools::EvalFormat::StrReplace && {
            let mut have: Vec<&str> = t.files.iter().map(|(n, _)| n.as_str()).collect();
            let mut want: Vec<&str> = t.expected.iter().map(|(n, _)| n.as_str()).collect();
            have.sort_unstable();
            want.sort_unstable();
            have != want
        } {
            skips.push(format!(
                "{}: str-replace-unsupported (rename/create has no substitution form; D1 named-skip)",
                t.id
            ));
        } else {
            // Content-specified instruction when the corpus carries one;
            // description fallback keeps custom corpora working (the B6
            // live-disk test forbids relying on it for these tasks).
            let instruction = t.instruction.unwrap_or(t.description);
            cases.push(SweepCase {
                id: t.id,
                instruction,
                files: t.files,
                expected: t.expected,
            });
        }
    }
    (cases, skips)
}

/// Narrow cases to `--only` (pure for tests): hit wins, a skipped id
/// bails with its recorded reason, an unknown id names the corpus miss.
fn select_only(
    cases: Vec<SweepCase>,
    skips: &[String],
    only: Option<String>,
) -> Result<Vec<SweepCase>, String> {
    match only {
        None => Ok(cases),
        Some(want) => {
            if let Some(hit) = cases.into_iter().find(|c| c.id == want) {
                Ok(vec![hit])
            } else if let Some(reason) = skips.iter().find(|s| s.starts_with(&want)) {
                Err(format!("task {want:?} is not sweepable ({reason})"))
            } else {
                Err(format!("unknown task {want:?} (not in corpus)"))
            }
        }
    }
}

/// Per-task pass tally over repeated run boards (Epic F5h, pure for
/// tests): `(id, passed, total)` sorted by id. Boards may differ in
/// membership only if `--only` changed between runs (never in one sweep
/// — same cases every run); unknown ids merge by name regardless.
fn stability_table(boards: &[polymorph_tools::Scoreboard]) -> Vec<(String, usize, usize)> {
    let mut tally: std::collections::BTreeMap<String, (usize, usize)> =
        std::collections::BTreeMap::new();
    for board in boards {
        for report in &board.reports {
            let entry = tally.entry(report.id.clone()).or_insert((0, 0));
            entry.1 += 1;
            if report.passed() {
                entry.0 += 1;
            }
        }
    }
    tally
        .into_iter()
        .map(|(id, (passed, total))| (id, passed, total))
        .collect()
}

/// Sweep the corpus live (Epic F5d): one turn per scorable case on
/// `--model` (qualified + injected, no fallback — chunk-19 attribution
/// precedent), scored in pid-parent/run-N/case-id/attempt-M subdirs, one
/// recording per case per run for `eval check`. Skips print named (never
/// silent). `--runs N` repeats the matrix (F5h); pooled board gated.
/// Red gate bails non-zero. No Ctrl-C wiring (short bounded calls —
/// review-live precedent, stated not hidden).
async fn run_eval_sweep(
    args: EvalSweepArgs,
    overrides: &polymorph_core::config::CliOverrides,
) -> anyhow::Result<()> {
    use polymorph_tools::hashline::{apply::normalize, SnapshotStore};
    let EvalSweepArgs {
        corpus,
        model,
        record,
        system_pack,
        format: format_raw,
        only,
        max_attempts,
        runs,
        min_rate,
    } = args;
    // Bogus `--format` bails before corpus IO/records/LLM work (fail-fast
    // precedent: constraint faults create nothing).
    let format =
        polymorph_tools::EvalFormat::parse(&format_raw).map_err(|e| anyhow::anyhow!("{e}"))?;
    let system_pack = resolve_system_pack(format, &system_pack);
    if max_attempts == 0 {
        anyhow::bail!("`--max-attempts` must be at least 1");
    }
    if runs == 0 {
        anyhow::bail!("`--runs` must be at least 1");
    }
    let text = std::fs::read_to_string(&corpus)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", corpus.display()))?;
    let (cases, skips) = sweep_plan(
        parse_sweep_corpus(&text).map_err(|e| anyhow::anyhow!("{e}"))?,
        format,
    );
    println!("format: {format}");
    for s in &skips {
        println!("skip: {s}");
    }
    let cases = select_only(cases, &skips, only).map_err(|e| anyhow::anyhow!("{e}"))?;
    if cases.is_empty() {
        anyhow::bail!("nothing sweepable (all tasks skipped)");
    }
    let system = polymorph_review::load_pack(&system_pack)?;
    let (user_path, project_path) = polymorph_core::config::default_paths();
    let cfg = polymorph_core::config::load_with_cli(&user_path, &project_path, overrides)?;
    let client = polymorph_llm::LlmClient::with_fresh_conversation();
    let mut state = polymorph_llm::FallbackState::new(std::time::Duration::from_secs(300));
    let cancel = tokio_util::sync::CancellationToken::new();
    std::fs::create_dir_all(&record)
        .map_err(|e| anyhow::anyhow!("cannot create {}: {e}", record.display()))?;
    // Pid-scoped parent (check precedent); the guard removes it on EVERY
    // exit including mid-loop live bails (chunk-20 audit lesson).
    let parent = std::env::temp_dir().join(format!("polymorph-eval-sweep-{}", std::process::id()));
    std::fs::create_dir_all(&parent)
        .map_err(|e| anyhow::anyhow!("cannot create {}: {e}", parent.display()))?;
    let _guard = RemoveOnDrop(parent.clone());
    // Matrix repeats (F5h): same cases every run; one client + fallback
    // state for the whole invocation (cooldowns shared, as within a run).
    let mut run_boards: Vec<polymorph_tools::Scoreboard> = Vec::new();
    let mut run_firsts: Vec<usize> = Vec::new();
    for run in 1..=runs {
        if runs > 1 {
            println!("=== run {run}/{runs} ===");
        }
        // Run-indexed record dir: every run replays in `check` as-is
        // (flat scan per dir — no check change needed).
        let run_record = record.join(format!("run-{run}"));
        std::fs::create_dir_all(&run_record)
            .map_err(|e| anyhow::anyhow!("cannot create {}: {e}", run_record.display()))?;
        let mut board = polymorph_tools::Scoreboard::default();
        let mut first_passed: usize = 0;
        for case in &cases {
            // Arm-specific prompt: hashline mints one live tag per file
            // (F3b read-tag-mint precedent — identical tags across
            // attempts); str_replace shows plain files (no tags exist in
            // that arm — showing them would leak hashline concepts).
            let base_prompt = match format {
                polymorph_tools::EvalFormat::Hashline => {
                    let mut store = SnapshotStore::new();
                    let mut shown: Vec<(String, String, String)> = Vec::new();
                    for (name, content) in &case.files {
                        let norm = normalize(content.as_bytes());
                        let tag = store.mint(std::path::Path::new(name), &norm.text);
                        shown.push((name.clone(), content.clone(), tag));
                    }
                    polymorph_tools::render_eval_prompt(&shown, &case.instruction)
                }
                polymorph_tools::EvalFormat::StrReplace => polymorph_tools::render_str_prompt(
                    &case
                        .files
                        .iter()
                        .map(|(n, c)| (n.clone(), c.clone()))
                        .collect::<Vec<_>>(),
                    &case.instruction,
                ),
            };
            // Bounded retry (pass@k): attempt N>1 appends the previous fault.
            // Each attempt scores in its own subdir (setup rewrites initials,
            // but applied renames/removals would otherwise leak across).
            let mut feedback = String::new();
            let mut attempts: u32 = 0;
            let (final_text, final_report) = loop {
                attempts += 1;
                let prompt = if feedback.is_empty() {
                    base_prompt.clone()
                } else {
                    format!("{base_prompt}\n\nRETRY FEEDBACK:\n{feedback}")
                };
                let mut sink = String::new();
                let outcome = client
                    .stream_chat_single(
                        &cfg.providers,
                        &mut state,
                        &model,
                        &system,
                        &prompt,
                        &cancel,
                        &mut sink,
                    )
                    .await?;
                let eval_case = polymorph_tools::EvalCase {
                    id: case.id.clone(),
                    files: case.files.clone(),
                    expected: case.expected.clone(),
                    model_output: outcome.text.clone(),
                };
                let dir = parent
                    .join(format!("run-{run}"))
                    .join(&case.id)
                    .join(format!("attempt-{attempts}"));
                std::fs::create_dir_all(&dir)
                    .map_err(|e| anyhow::anyhow!("cannot create {}: {e}", dir.display()))?;
                let report = match format {
                    polymorph_tools::EvalFormat::Hashline => {
                        polymorph_tools::score_case(&eval_case, &dir)
                    }
                    polymorph_tools::EvalFormat::StrReplace => {
                        polymorph_tools::score_str_case(&eval_case, &dir)
                    }
                };
                println!("{} attempt {attempts}: {:?}", report.id, report.outcome);
                if attempts == 1 && report.passed() {
                    first_passed += 1;
                }
                // Same retry SHAPE both arms (attempt parity); only the
                // nouns differ (fault text verbatim / mismatch constant).
                let feedback_for = match format {
                    polymorph_tools::EvalFormat::Hashline => retry_feedback,
                    polymorph_tools::EvalFormat::StrReplace => retry_feedback_str,
                };
                match feedback_for(&report.outcome) {
                    None => break (outcome.text, report),
                    Some(_) if attempts >= max_attempts => break (outcome.text, report),
                    Some(next) => feedback = next,
                }
            };
            println!(
                "{}: {:?} (after {attempts} attempt(s))",
                final_report.id, final_report.outcome
            );
            let rec = recording_json(
                &case.id,
                &model,
                &case.instruction,
                &case.files,
                &case.expected,
                &final_text,
                &format!("{:?}", final_report.outcome),
                attempts,
                format,
            );
            let dest = run_record.join(format!("{}.eval.json", case.id));
            std::fs::write(&dest, serde_json::to_string_pretty(&rec).unwrap())
                .map_err(|e| anyhow::anyhow!("cannot write {}: {e}", dest.display()))?;
            println!("recorded: {}", dest.display());
            board.reports.push(final_report);
        }
        println!(
            "run {run}: first-attempt {first_passed}/{} passed; score {}/{} passed, {} skipped",
            board.total(),
            board.passed(),
            board.total(),
            skips.len()
        );
        run_firsts.push(first_passed);
        run_boards.push(board);
    }
    // Pooled board (strict gate: any flip reddens — variance must hurt
    // the gate or the gate is theatre) + per-task stability table.
    let mut pooled = polymorph_tools::Scoreboard::default();
    for board in &run_boards {
        pooled.reports.extend(board.reports.iter().cloned());
    }
    let pooled_first: usize = run_firsts.iter().sum();
    let pooled_total = pooled.total();
    println!("stability ({runs} run(s)):");
    for (id, passed, total) in stability_table(&run_boards) {
        let mark = if passed == total { "stable" } else { "FLIP" };
        println!("  {id}: {passed}/{total} {mark}");
    }
    println!(
        "first-attempt pooled: {pooled_first}/{pooled_total} passed; score: {}/{} passed, {} skipped",
        pooled.passed(),
        pooled_total,
        skips.len()
    );
    let board = pooled;
    match polymorph_tools::gate(&board, min_rate) {
        Ok(()) => {
            println!("GATE: green");
            Ok(())
        }
        Err(e) => {
            println!("GATE: red ({e})");
            anyhow::bail!("{e}");
        }
    }
}

/// Removes a temp dir on drop — EVERY exit runs it, including `?` bails
/// (chunk-20 audit lesson: removal after the fallible section strands on
/// early returns; the closure wrap fixed sync code but cannot carry async
/// borrows out of its body, so the guard owns the path instead).
struct RemoveOnDrop(std::path::PathBuf);
impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `eval` dispatch (`run` and `sweep` are live; `check` replays offline).
async fn run_eval(
    cmd: EvalCmd,
    overrides: &polymorph_core::config::CliOverrides,
) -> anyhow::Result<()> {
    match cmd {
        EvalCmd::Run { args } => {
            let EvalRunArgs {
                instruction,
                input,
                expected,
                model,
                system_pack,
                format,
                record,
            } = *args;
            run_eval_live(
                instruction,
                input,
                expected,
                model,
                system_pack,
                format,
                record,
                overrides,
            )
            .await
        }
        EvalCmd::Check { args } => {
            let EvalCheckArgs { path, min_rate } = *args;
            run_eval_check(path, min_rate)
        }
        EvalCmd::Sweep { args } => run_eval_sweep(*args, overrides).await,
    }
}

/// `exec`: single streamed turn over the `default` role chain (Epic A3).
///
/// Chunk 7 (Epic D): turns run managed — path scope selects D1b pins, the
/// provider mapped to each model donates one pool key (D1c/D1d injection),
/// 429s cool down per model with restore (D1a). `--model` is the operator's
/// manual override: single attempt, injection still applies, no fallback.
///
/// Ctrl-C cancels the token: the stream fold returns its partial text with
/// `aborted: true`, no orphan request survives (the HTTP body is owned by the
/// cancelled future), and the process exits 130 (conventional SIGINT).
/// stdout carries ONLY model deltas so `exec` composes in pipes; diagnostics
/// go to stderr via tracing.
async fn run_exec(
    prompt: String,
    model: Option<String>,
    system: Option<String>,
    scope: Option<String>,
    overrides: polymorph_core::config::CliOverrides,
) -> anyhow::Result<()> {
    use polymorph_core::Role;
    use polymorph_llm::{FallbackState, LlmClient, RoleRouter, StreamSink, DEFAULT_SYSTEM};
    use tokio_util::sync::CancellationToken;

    let (user_path, project_path) = polymorph_core::config::default_paths();
    let cfg = polymorph_core::config::load_with_cli(&user_path, &project_path, &overrides)?;
    let router = RoleRouter::from_config(&cfg);
    let system = system.unwrap_or_else(|| DEFAULT_SYSTEM.to_string());

    let cancel = CancellationToken::new();
    let cancel_sig = cancel.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            cancel_sig.cancel();
        }
    });

    struct StdoutSink;
    impl StreamSink for StdoutSink {
        fn push_text(&mut self, chunk: &str) {
            use std::io::Write;
            let _ = std::io::stdout().write_all(chunk.as_bytes());
            let _ = std::io::stdout().flush();
        }
    }

    let client = LlmClient::with_fresh_conversation();
    let mut sink = StdoutSink;
    // One invocation = one fallback state (cooldowns + pool cursors).
    let mut state = FallbackState::new(std::time::Duration::from_secs(300));
    if let Some(model) = model {
        let outcome = client
            .stream_chat_single(
                &cfg.providers,
                &mut state,
                &model,
                &system,
                &prompt,
                &cancel,
                &mut sink,
            )
            .await?;
        finish_exec(outcome.aborted);
        return Ok(());
    }
    let (outcome, used) = client
        .stream_chat_managed(
            &router,
            &cfg.providers,
            &mut state,
            Role::Default,
            scope.as_deref(),
            &system,
            &prompt,
            &cancel,
            &mut sink,
        )
        .await?;
    tracing::info!(model = used, "exec turn complete");
    finish_exec(outcome.aborted);
    Ok(())
}

fn finish_exec(aborted: bool) {
    use std::io::Write;
    let _ = writeln!(std::io::stdout());
    if aborted {
        eprintln!("[aborted by user — partial output above]");
        std::process::exit(130);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn cli_tree_parses_all_subcommands() {
        for argv in [
            vec!["polymorph", "exec", "hello"],
            vec!["polymorph", "mcp"],
            vec!["polymorph", "exec", "hello", "--scope", "crates/x"],
            vec!["polymorph", "--debug", "exec", "hello"],
            vec!["polymorph", "sessions", "list"],
            vec!["polymorph", "sessions", "create", "my-title"],
            vec!["polymorph", "config", "show"],
            vec!["polymorph", "config", "set", "options.debug", "true"],
            vec!["polymorph", "providers", "list"],
            vec!["polymorph", "models", "list"],
            vec!["polymorph", "delegate", "run", "--task", "probe"],
            vec!["polymorph", "review", "demo"],
            vec![
                "polymorph",
                "review",
                "demo",
                "--mode",
                "act",
                "--script",
                "survivor",
            ],
            vec!["polymorph", "review", "check", "v.json"],
            vec![
                "polymorph",
                "review",
                "run",
                "--goal",
                "hold the line",
                "--artifact-file",
                "artifact.txt",
            ],
            vec![
                "polymorph",
                "review",
                "run",
                "--goal",
                "hold the line",
                "--artifact-file",
                "artifact.txt",
                "--criteria",
                "no nits",
                "--max-rounds",
                "2",
            ],
            vec![
                "polymorph",
                "review",
                "run",
                "--goal",
                "hold the line",
                "--artifact-file",
                "artifact.txt",
                "--mode",
                "act",
                "--scope",
                "crates/x",
                "--repo",
                ".",
            ],
            vec![
                "polymorph",
                "review",
                "run",
                "--goal",
                "hold the line",
                "--artifact-file",
                "PLAN.md",
                "--mode",
                "plan",
                "--plan-pack",
                "prompts/glm/glm-5.3/plan.md",
            ],
            vec![
                "polymorph",
                "review",
                "eval-ab",
                "--goal",
                "hold the line",
                "--artifact-file",
                "artifact.txt",
                "--review-model-a",
                "opencode_go::glm-5.3-flash",
                "--review-model-b",
                "glm-5.3",
            ],
            vec![
                "polymorph",
                "eval",
                "run",
                "--instruction",
                "Replace line 2.",
                "--input",
                "in.txt",
                "--expected",
                "want.txt",
                "--model",
                "glm-5.3",
            ],
            vec![
                "polymorph",
                "eval",
                "run",
                "--instruction",
                "Replace line 2.",
                "--input",
                "in.txt",
                "--expected",
                "want.txt",
                "--model",
                "glm-5.3",
                "--record",
                "out/",
            ],
            vec![
                "polymorph",
                "review",
                "eval-ab",
                "--goal",
                "hold the line",
                "--artifact-file",
                "artifact.txt",
                "--review-model-a",
                "a-model",
                "--review-model-b",
                "b-model",
                "--mode",
                "plan",
                "--max-rounds",
                "1",
            ],
            vec![
                "polymorph",
                "delegate",
                "run",
                "--children",
                "3",
                "--task",
                "probe",
                "--role",
                "smol",
            ],
            vec!["polymorph", "update-catalog"],
            vec![
                "polymorph",
                "update-catalog",
                "--url",
                "http://127.0.0.1:9/x",
            ],
        ] {
            assert!(Cli::try_parse_from(&argv).is_ok(), "{argv:?}");
        }
        // `review run` flags reach the parsed tree with values (F2b-part1:
        // parse layer; managed-target assembly stays live-proven, chunk-10
        // precedent — stated, not hidden).
        let cli = Cli::try_parse_from([
            "polymorph",
            "review",
            "run",
            "--goal",
            "g",
            "--artifact-file",
            "a.txt",
            "--mode",
            "act",
            "--scope",
            "crates/x",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Review {
                cmd: ReviewCmd::Run { args },
            }) => {
                assert_eq!(args.mode, "act");
                assert_eq!(args.scope.as_deref(), Some("crates/x"));
                assert!(args.repo.is_none());
            }
            other => panic!("unexpected parse: {other:?}"),
        }
        // Plan mode parses with its pack flag (F2b-part2 — both modes live).
        let cli = Cli::try_parse_from([
            "polymorph",
            "review",
            "run",
            "--goal",
            "g",
            "--artifact-file",
            "PLAN.md",
            "--mode",
            "plan",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Review {
                cmd: ReviewCmd::Run { args },
            }) => {
                assert_eq!(args.mode, "plan");
                assert_eq!(
                    args.plan_pack,
                    std::path::PathBuf::from("prompts/glm/glm-5.3/plan.md")
                );
                // Tools default OFF with 30 feed rounds (F3b opt-in).
                assert!(!args.tools);
                assert_eq!(args.max_tool_rounds, 30);
            }
            other => panic!("unexpected parse: {other:?}"),
        }
        // `--tools --max-tool-rounds` parse (F3b flag).
        let cli = Cli::try_parse_from([
            "polymorph",
            "review",
            "run",
            "--goal",
            "g",
            "--artifact-file",
            "a.txt",
            "--tools",
            "--max-tool-rounds",
            "2",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Review {
                cmd: ReviewCmd::Run { args },
            }) => {
                assert!(args.tools);
                assert_eq!(args.max_tool_rounds, 2);
            }
            other => panic!("unexpected parse: {other:?}"),
        }
        // `eval-ab` flags reach the tree with values (F4b: parse layer;
        // arm-router assembly + live pair stay live-proven, chunk-10
        // precedent — stated, not hidden).
        let cli = Cli::try_parse_from([
            "polymorph",
            "review",
            "eval-ab",
            "--goal",
            "g",
            "--artifact-file",
            "a.txt",
            "--review-model-a",
            "a-model",
            "--review-model-b",
            "b-model",
            "--mode",
            "plan",
            "--max-rounds",
            "1",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Review {
                cmd: ReviewCmd::EvalAb { args },
            }) => {
                assert_eq!(args.review_model_a, "a-model");
                assert_eq!(args.review_model_b, "b-model");
                assert_eq!(args.mode, "plan");
                assert_eq!(args.max_rounds, 1);
            }
            other => panic!("unexpected parse: {other:?}"),
        }
        // `eval-ab` defaults: act mode, max-rounds 2 (bounded two-arm spend).
        let cli = Cli::try_parse_from([
            "polymorph",
            "review",
            "eval-ab",
            "--goal",
            "g",
            "--artifact-file",
            "a.txt",
            "--review-model-a",
            "a-model",
            "--review-model-b",
            "b-model",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Review {
                cmd: ReviewCmd::EvalAb { args },
            }) => {
                assert_eq!(args.mode, "act");
                assert_eq!(args.max_rounds, 2);
            }
            other => panic!("unexpected parse: {other:?}"),
        }
        // `eval run` flags reach the tree with values (F5b: parse layer;
        // live turn + scoring stay live-proven, chunk-10 precedent).
        let cli = Cli::try_parse_from([
            "polymorph",
            "eval",
            "run",
            "--instruction",
            "Do it.",
            "--input",
            "in.txt",
            "--expected",
            "want.txt",
            "--model",
            "glm-5.3",
            "--record",
            "out/",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Eval {
                cmd: EvalCmd::Run { args },
            }) => {
                assert_eq!(args.instruction, "Do it.");
                assert_eq!(args.model, "glm-5.3");
                assert_eq!(
                    args.system_pack,
                    std::path::PathBuf::from("prompts/glm/glm-5.3/edit.md")
                );
                assert!(args.record.is_some());
            }
            other => panic!("unexpected parse: {other:?}"),
        }
        // `--model` is required (eval names its subject — no chain default).
        assert!(Cli::try_parse_from([
            "polymorph",
            "eval",
            "run",
            "--instruction",
            "Do it.",
            "--input",
            "in.txt",
            "--expected",
            "want.txt",
        ])
        .is_err());
        // `eval check` flags reach the tree with values + defaults (F5c:
        // parse layer; scoring stays live-proven by the smoke below).
        let cli = Cli::try_parse_from(["polymorph", "eval", "check", "--path", "rec/"]).unwrap();
        match cli.command {
            Some(Commands::Eval {
                cmd: EvalCmd::Check { args },
            }) => {
                assert_eq!(args.path, std::path::PathBuf::from("rec/"));
                assert_eq!(args.min_rate, 1.0);
            }
            other => panic!("unexpected parse: {other:?}"),
        }
        let cli = Cli::try_parse_from([
            "polymorph",
            "eval",
            "check",
            "--path",
            "one.eval.json",
            "--min-rate",
            "0.5",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Eval {
                cmd: EvalCmd::Check { args },
            }) => {
                assert_eq!(args.min_rate, 0.5);
            }
            other => panic!("unexpected parse: {other:?}"),
        }
        // `--path` is required (check names its recordings, not subjects).
        assert!(Cli::try_parse_from(["polymorph", "eval", "check"]).is_err());
        // `eval sweep` flags reach the tree with values + defaults (F5d:
        // parse layer; live turns stay operator/key-gated like `run`).
        let cli = Cli::try_parse_from([
            "polymorph",
            "eval",
            "sweep",
            "--model",
            "glm-5.3",
            "--record",
            "rec/",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Eval {
                cmd: EvalCmd::Sweep { args },
            }) => {
                assert_eq!(args.model, "glm-5.3");
                assert_eq!(args.record, std::path::PathBuf::from("rec/"));
                assert_eq!(args.min_rate, 1.0);
                assert!(args.only.is_none());
                assert!(args.corpus.ends_with("edit_corpus.toml"));
                assert_eq!(args.format, "hashline");
            }
            other => panic!("unexpected parse: {other:?}"),
        }
        // `--format str-replace` reaches the tree (chunk 28 baseline arm).
        let cli = Cli::try_parse_from([
            "polymorph",
            "eval",
            "sweep",
            "--model",
            "m",
            "--record",
            "r/",
            "--format",
            "str-replace",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Eval {
                cmd: EvalCmd::Sweep { args },
            }) => {
                assert_eq!(args.format, "str-replace");
            }
            other => panic!("unexpected parse: {other:?}"),
        }
        let cli = Cli::try_parse_from([
            "polymorph",
            "eval",
            "sweep",
            "--model",
            "m",
            "--record",
            "r/",
            "--only",
            "cut-delete",
            "--min-rate",
            "0.5",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Eval {
                cmd: EvalCmd::Sweep { args },
            }) => {
                assert_eq!(args.only.as_deref(), Some("cut-delete"));
                assert_eq!(args.min_rate, 0.5);
            }
            other => panic!("unexpected parse: {other:?}"),
        }
        // `--model` and `--record` are both required (named subject +
        // CI artifact are the command's purpose, not defaults).
        assert!(Cli::try_parse_from(["polymorph", "eval", "sweep", "--model", "m"]).is_err());
        assert!(Cli::try_parse_from(["polymorph", "eval", "sweep", "--record", "r/"]).is_err());
        // `--max-attempts` defaults to 2 (one retry); explicit values parse.
        let cli = Cli::try_parse_from([
            "polymorph",
            "eval",
            "sweep",
            "--model",
            "m",
            "--record",
            "r/",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Eval {
                cmd: EvalCmd::Sweep { args },
            }) => {
                assert_eq!(args.max_attempts, 2);
                assert_eq!(args.runs, 1);
            }
            other => panic!("unexpected parse: {other:?}"),
        }
        // `--runs 3` parses (multi-run aggregation, F5h).
        let cli = Cli::try_parse_from([
            "polymorph",
            "eval",
            "sweep",
            "--model",
            "m",
            "--record",
            "r/",
            "--runs",
            "3",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Eval {
                cmd: EvalCmd::Sweep { args },
            }) => assert_eq!(args.runs, 3),
            other => panic!("unexpected parse: {other:?}"),
        }
        let cli = Cli::try_parse_from([
            "polymorph",
            "eval",
            "sweep",
            "--model",
            "m",
            "--record",
            "r/",
            "--max-attempts",
            "1",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Eval {
                cmd: EvalCmd::Sweep { args },
            }) => assert_eq!(args.max_attempts, 1),
            other => panic!("unexpected parse: {other:?}"),
        }
        // Global --debug reaches the parsed tree (D1f CLI layer).
        let cli = Cli::try_parse_from(["polymorph", "--debug", "exec", "hi"]).unwrap();
        assert!(cli.debug);
        assert!(cli_overrides(&cli).debug == Some(true));
        let cli = Cli::try_parse_from(["polymorph", "exec", "hi"]).unwrap();
        assert!(!cli.debug);
        assert!(cli_overrides(&cli).debug.is_none());
    }

    #[test]
    fn eval_check_rescores_recordings_offline() {
        // F5c replay without spend: hand-written recordings (NOT model
        // runs) through the real loader + scorer + gate. Tags mint exactly
        // like score_case's setup (traps #20: normalized, terminator-free).
        use polymorph_tools::hashline::{apply::normalize, SnapshotStore};
        let live_tag = {
            let mut store = SnapshotStore::new();
            let norm = normalize(b"orig\n");
            store.mint(std::path::Path::new("a.txt"), &norm.text)
        };
        let good_patch = format!("[a.txt#{live_tag}]\nPUT 1:\n+fixed\n");
        let good = serde_json::json!({
            "id": "a.txt",
            "model": "fixture-model",
            "instruction": "fix it",
            "input": "orig\n",
            "expected": "fixed\n",
            "output": good_patch,
            "outcome": "Pass",
        })
        .to_string();
        let bad = serde_json::json!({
            "id": "b.txt",
            "model": "fixture-model",
            "instruction": "fix it",
            "input": "orig\n",
            "expected": "fixed\n",
            "output": "I would change it but the tag expired.",
            "outcome": "Pass",
        })
        .to_string();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("good.eval.json"), &good).unwrap();
        // Single file greens; the file need not end `.eval.json` (named).
        assert!(run_eval_check(dir.path().join("good.eval.json"), 1.0).is_ok());
        assert!(run_eval_check(dir.path().to_path_buf(), 1.0).is_ok());
        // Mixed board goes red naming the bad id; half-rate holds at 0.5.
        std::fs::write(dir.path().join("bad.eval.json"), &bad).unwrap();
        let err = run_eval_check(dir.path().to_path_buf(), 1.0)
            .unwrap_err()
            .to_string();
        assert!(err.contains("b.txt"), "{err}");
        assert!(run_eval_check(dir.path().to_path_buf(), 0.5).is_ok());
        // Corrupt recording bails naming the file, never scores.
        std::fs::write(dir.path().join("broken.eval.json"), "{nope").unwrap();
        let err = run_eval_check(dir.path().join("broken.eval.json"), 0.0)
            .unwrap_err()
            .to_string();
        assert!(err.contains("broken.eval.json"), "{err}");
        std::fs::remove_file(dir.path().join("broken.eval.json")).unwrap();
        // Missing path, empty dir, and stray-only dir all refuse.
        // No temp survives ANY exit (corrupt bail included — the fix):
        // scan for the pid-prefixed parents (sequence-suffixed since
        // chunk 28, so match by prefix, not exact name).
        let prefix = format!("polymorph-eval-check-{}", std::process::id());
        let strays: Vec<_> = std::fs::read_dir(std::env::temp_dir())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.starts_with(&prefix))
            })
            .collect();
        assert!(strays.is_empty(), "stranded temp: {strays:?}");
        assert!(run_eval_check(dir.path().join("ghost.json"), 1.0).is_err());
        let empty = tempfile::tempdir().unwrap();
        assert!(run_eval_check(empty.path().to_path_buf(), 0.0).is_err());
        std::fs::write(empty.path().join("notes.txt"), "stray").unwrap();
        assert!(run_eval_check(empty.path().to_path_buf(), 0.0).is_err());
    }

    #[test]
    fn sweep_plan_keeps_shapes_and_skips_only_rejection() {
        // Fixture-level proof: every shape scores, only rejection skips
        // (NOT model runs — the v2 promise at the plan layer).
        let task = |id: &str,
                    files: Vec<(&str, &str)>,
                    expected: Vec<(&str, &str)>,
                    expect_error: Option<&str>| {
            SweepTask {
                id: id.to_string(),
                description: format!("do {id}"),
                instruction: None,
                files: files
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
                expected: expected
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
                expect_error: expect_error.map(str::to_string),
            }
        };
        let tasks = vec![
            task("good", vec![("a.txt", "o\n")], vec![("a.txt", "f\n")], None),
            task("rej", vec![("j.txt", "v\n")], vec![], Some("StaleTag")),
            task(
                "multi",
                vec![("h1.txt", "x\n"), ("h2.txt", "y\n")],
                vec![("h1.txt", "X\n"), ("h2.txt", "Y\n")],
                None,
            ),
            task("rem", vec![("m.txt", "bye\n")], vec![], None),
            task("mv", vec![("n.txt", "d\n")], vec![("o.txt", "d\n")], None),
        ];
        // Hashline arm: every shape scores (only rejections skip).
        let (cases, skips) = sweep_plan(tasks.clone(), polymorph_tools::EvalFormat::Hashline);
        assert_eq!(cases.len(), 4);
        let multi = cases.iter().find(|c| c.id == "multi").unwrap();
        assert_eq!(multi.files.len(), 2);
        let rem = cases.iter().find(|c| c.id == "rem").unwrap();
        assert!(rem.expected.is_empty());
        let mv = cases.iter().find(|c| c.id == "mv").unwrap();
        assert_eq!(mv.expected, vec![("o.txt".to_string(), "d\n".to_string())]);
        assert_eq!(skips.len(), 1);
        assert!(skips[0].starts_with("rej:") && skips[0].contains("StaleTag"));
        // Str_replace arm (D1): rem + mv skip NAMED, multi still scores.
        let (cases_s, skips_s) = sweep_plan(tasks, polymorph_tools::EvalFormat::StrReplace);
        assert_eq!(cases_s.len(), 2);
        assert!(cases_s.iter().any(|c| c.id == "good"));
        assert!(cases_s.iter().any(|c| c.id == "multi"));
        assert_eq!(skips_s.len(), 3, "skips: {skips_s:?}");
        assert!(skips_s
            .iter()
            .any(|s| s.starts_with("rem:") && s.contains("str-replace-unsupported")));
        assert!(skips_s
            .iter()
            .any(|s| s.starts_with("mv:") && s.contains("str-replace-unsupported")));
    }

    #[test]
    fn retry_feedback_maps_all_outcomes_without_leaking() {
        use polymorph_tools::CaseOutcome;
        // Pass stops (terminal success — the only None).
        assert!(retry_feedback(&CaseOutcome::Pass).is_none());
        // Extract/Apply faults echo verbatim (actionable, never expected
        // bytes — the fault strings name faults/paths by construction).
        for outcome in [
            CaseOutcome::ExtractFail("no patch block: prose only".into()),
            CaseOutcome::ExtractFail("ambiguous output: 2 fenced blocks (want exactly 1)".into()),
            CaseOutcome::ApplyFail(
                "stale tag for 'a.txt': expected #ab12 but content changed".into(),
            ),
            CaseOutcome::ApplyFail("parse error at patch line 9: expected section header".into()),
        ] {
            let fb = retry_feedback(&outcome).unwrap();
            assert!(
                fb.contains("previous") && fb.contains("ONE hashline patch"),
                "{fb}"
            );
        }
        // Mismatch feedback is a CONSTANT: it must never carry expected
        // content (the leak-scan: even a hostile expected string with
        // distinctive bytes must not appear in the feedback).
        let fb = retry_feedback(&CaseOutcome::Mismatch("content differs: c.txt".into())).unwrap();
        for secret in ["héllo 🌍", "+top", "o.txt", "X\nY"] {
            assert!(!fb.contains(secret), "leak of {secret:?} in {fb:?}");
        }
        assert!(fb.contains("ONE") || fb.contains("patch"), "{fb}");
    }

    #[test]
    fn retry_feedback_str_mirrors_shape_with_own_nouns() {
        use polymorph_tools::CaseOutcome;
        // Attempt parity (design rule 3): same shape, arm's nouns.
        assert!(retry_feedback_str(&CaseOutcome::Pass).is_none());
        let fb = retry_feedback_str(&CaseOutcome::ExtractFail(
            "no patch sections: prose only".into(),
        ))
        .unwrap();
        assert!(
            fb.contains("previous") && fb.contains("ONE str_replace patch"),
            "{fb}"
        );
        assert!(!fb.contains("hashline"), "arm-noun leak: {fb}");
        let fb = retry_feedback(&CaseOutcome::ExtractFail("x".into())).unwrap();
        assert!(!fb.contains("str_replace"), "arm-noun leak: {fb}");
        // Mismatch constant carries no expected bytes (same leak-scan).
        let fb =
            retry_feedback_str(&CaseOutcome::Mismatch("content differs: c.txt".into())).unwrap();
        for secret in ["héllo 🌍", "<<<", ">>>", "[edit ", "o.txt"] {
            assert!(!fb.contains(secret), "leak of {secret:?} in {fb:?}");
        }
    }

    #[test]
    fn resolve_system_pack_follows_format_unless_overridden() {
        use polymorph_tools::EvalFormat;
        use std::path::Path;
        // Defaults follow the arm.
        assert_eq!(
            resolve_system_pack(EvalFormat::Hashline, Path::new(DEFAULT_HASHLINE_PACK)),
            std::path::PathBuf::from(DEFAULT_HASHLINE_PACK)
        );
        assert_eq!(
            resolve_system_pack(EvalFormat::StrReplace, Path::new(DEFAULT_HASHLINE_PACK)),
            std::path::PathBuf::from(DEFAULT_STR_PACK)
        );
        // An explicit non-default path always wins (even a swapped one).
        assert_eq!(
            resolve_system_pack(EvalFormat::StrReplace, Path::new("custom/pack.md")),
            std::path::PathBuf::from("custom/pack.md")
        );
        assert_eq!(
            resolve_system_pack(EvalFormat::Hashline, Path::new(DEFAULT_STR_PACK)),
            std::path::PathBuf::from(DEFAULT_STR_PACK)
        );
    }

    #[test]
    fn sweep_live_corpus_plans_thirteen_with_seven_named_skips_on_str_replace() {
        // Arm-S live gate (D1): 13 scorable + 7 named skips (5 rejections
        // + rem-file + mv-file, which have no substitution form). Same
        // fixture, same instructions — only the arm moved.
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../");
        let text = std::fs::read_to_string(
            root.join("crates/polymorph-tools/tests/eval_b6/edit_corpus.toml"),
        )
        .unwrap();
        let (cases, skips) = sweep_plan(
            parse_sweep_corpus(&text).unwrap(),
            polymorph_tools::EvalFormat::StrReplace,
        );
        assert_eq!(
            cases.len(),
            13,
            "cases: {:?}",
            cases.iter().map(|c| &c.id).collect::<Vec<_>>()
        );
        assert_eq!(skips.len(), 7, "skips: {skips:?}");
        assert!(skips
            .iter()
            .any(|s| s.starts_with("rem-file:") && s.contains("str-replace-unsupported")));
        assert!(skips
            .iter()
            .any(|s| s.starts_with("mv-file:") && s.contains("str-replace-unsupported")));
        assert!(cases
            .iter()
            .all(|c| c.id != "rem-file" && c.id != "mv-file"));
        // Multi-file still scores (multiple sections); crlf/bom stay in
        // (they will refuse as MissingOld live — the measured effect).
        assert!(cases.iter().any(|c| c.id == "multi-file"));
        assert!(cases.iter().any(|c| c.id == "crlf-preserved"));
        // Instruction neutrality, str_replace direction (slice 4): zero
        // baseline syntax in the shared instructions (else the rival runs
        // a copy test). Hashline direction stays pinned by the sibling test.
        for case in &cases {
            for leak in ["old_string", "[edit ", "<<<", ">>>", "anchor"] {
                assert!(
                    !case.instruction.contains(leak),
                    "{} leaks {leak:?}",
                    case.id
                );
            }
        }
    }

    #[test]
    fn eval_check_dispatches_per_record_format() {
        use polymorph_tools::hashline::{apply::normalize, SnapshotStore};
        use polymorph_tools::EvalFormat;
        let dir = tempfile::tempdir().unwrap();
        // Hashline record with a live tag (passes under score_case).
        let norm = normalize(b"orig\n");
        let mut store = SnapshotStore::new();
        let tag = store.mint(std::path::Path::new("a.txt"), &norm.text);
        let h_patch = format!("[a.txt#{tag}]\nPUT 1:\n+fixed\n");
        let h_rec = recording_json(
            "h-case",
            "fixture-model",
            "change orig to fixed",
            &[("a.txt".to_string(), "orig\n".to_string())],
            &[("a.txt".to_string(), "fixed\n".to_string())],
            &h_patch,
            "Pass",
            1,
            EvalFormat::Hashline,
        );
        // Str_replace record: passes under score_str_case, but under the
        // hashline scorer it would ApplyFail (parse error) — so a green
        // gate proves per-record dispatch, not a lucky shared scorer.
        let s_out = "[edit a.txt]\n<<<\norig\n>>>\nfixed\n";
        let s_rec = recording_json(
            "s-case",
            "fixture-model",
            "change orig to fixed",
            &[("a.txt".to_string(), "orig\n".to_string())],
            &[("a.txt".to_string(), "fixed\n".to_string())],
            s_out,
            "Pass",
            1,
            EvalFormat::StrReplace,
        );
        std::fs::write(dir.path().join("h-case.eval.json"), h_rec.to_string()).unwrap();
        std::fs::write(dir.path().join("s-case.eval.json"), s_rec.to_string()).unwrap();
        assert!(run_eval_check(dir.path().to_path_buf(), 1.0).is_ok());
        // Unknown `format` stamps bail naming the file (never a score).
        let mut bad: serde_json::Value = serde_json::from_str(&s_rec.to_string()).unwrap();
        bad["format"] = serde_json::json!("unified-diff");
        std::fs::write(dir.path().join("bad.eval.json"), bad.to_string()).unwrap();
        let err = run_eval_check(dir.path().join("bad.eval.json"), 0.0)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("bad.eval.json") && err.contains("unified-diff"),
            "{err}"
        );
    }

    #[test]
    fn stability_table_tallies_passes_by_id_sorted() {
        use polymorph_tools::{CaseOutcome, CaseReport, Scoreboard};
        let board = |rows: Vec<(&str, bool)>| Scoreboard {
            reports: rows
                .into_iter()
                .map(|(id, pass)| CaseReport {
                    id: id.to_string(),
                    outcome: if pass {
                        CaseOutcome::Pass
                    } else {
                        CaseOutcome::Mismatch("x".into())
                    },
                })
                .collect(),
        };
        let boards = vec![
            board(vec![("b", true), ("a", true)]),
            board(vec![("b", false), ("a", true)]),
        ];
        let table = stability_table(&boards);
        // Sorted by id regardless of board order; flips tallied honestly.
        assert_eq!(
            table,
            vec![("a".to_string(), 2, 2), ("b".to_string(), 1, 2)]
        );
        assert!(stability_table(&[]).is_empty());
    }

    #[test]
    fn sweep_select_only_covers_all_three_branches() {
        let case = |id: &str| SweepCase {
            id: id.to_string(),
            instruction: String::new(),
            files: vec![("a.txt".to_string(), String::new())],
            expected: vec![("a.txt".to_string(), String::new())],
        };
        let cases = vec![case("a"), case("b")];
        let skips = vec!["rej: rejection-task (expects StaleTag; ...)".to_string()];
        assert_eq!(select_only(cases.clone(), &skips, None).unwrap().len(), 2);
        assert_eq!(
            select_only(cases.clone(), &skips, Some("b".into())).unwrap()[0].id,
            "b"
        );
        assert!(select_only(cases.clone(), &skips, Some("rej".into()))
            .unwrap_err()
            .contains("not sweepable"));
        assert!(select_only(cases, &skips, Some("ghost".into()))
            .unwrap_err()
            .contains("unknown task"));
    }

    #[test]
    fn sweep_corpus_parse_rejects_bad_shapes_naming_tasks() {
        assert!(parse_sweep_corpus("not toml{{{")
            .unwrap_err()
            .contains("invalid TOML"));
        assert!(parse_sweep_corpus("[other]\nx = 1\n")
            .unwrap_err()
            .contains("[[task]]"));
        // Missing description names the task (operator error, never a score).
        let bad = "[[task]]\nid = \"ghost\"\n[task.files]\n\"a.txt\" = \"x\\n\"\n[task.expect.files]\n\"a.txt\" = \"x\\n\"\n";
        assert!(parse_sweep_corpus(bad).unwrap_err().contains("ghost"));
        // `instruction` is optional (fallback: description) and verbatim.
        let ok = "[[task]]\nid = \"t\"\ndescription = \"d\"\ninstruction = 'Do X.'\n[task.files]\n\"a.txt\" = \"x\\n\"\n[task.expect.files]\n\"a.txt\" = \"y\\n\"\n";
        let tasks = parse_sweep_corpus(ok).unwrap();
        assert_eq!(tasks[0].instruction.as_deref(), Some("Do X."));
        let nodesc = "[[task]]\nid = \"t\"\ndescription = \"d\"\n[task.files]\n\"a.txt\" = \"x\\n\"\n[task.expect.files]\n\"a.txt\" = \"y\\n\"\n";
        assert!(parse_sweep_corpus(nodesc).unwrap()[0].instruction.is_none());
    }

    #[test]
    fn sweep_live_corpus_plans_fifteen_with_five_rejection_skips() {
        // Live gate: the committed B6 fixture plans 15 scorable + 5
        // rejection skips (NOTHING else narrows). CWD-independent.
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../");
        let text = std::fs::read_to_string(
            root.join("crates/polymorph-tools/tests/eval_b6/edit_corpus.toml"),
        )
        .unwrap();
        let (cases, skips) = sweep_plan(
            parse_sweep_corpus(&text).unwrap(),
            polymorph_tools::EvalFormat::Hashline,
        );
        assert_eq!(
            cases.len(),
            15,
            "cases: {:?}",
            cases.iter().map(|c| &c.id).collect::<Vec<_>>()
        );
        assert_eq!(skips.len(), 5, "skips: {skips:?}");
        assert!(skips.iter().all(|s| s.contains("rejection-task")));
        // Every scorable case carries an explicit content-specified
        // instruction (never the strawman description): spot-check the
        // previously unknowable answers are now derivable from the text.
        // (Descriptions lack X/o.txt/emoji/ordering, so these asserts
        // prove the instruction — not the fallback — is wired.)
        let by_id = |id: &str| cases.iter().find(|c| c.id == id).unwrap();
        assert!(by_id("replace-range").instruction.contains("`X`"));
        assert!(by_id("mv-file").instruction.contains("o.txt"));
        assert!(by_id("unicode-content").instruction.contains("🌍"));
        assert!(by_id("cut-paste-move")
            .instruction
            .contains("`b`, `c`, `d`, `a`"));
        // No instruction leaks hashline syntax (else the eval is a copy test).
        for case in &cases {
            for leak in ["[a.txt#", "[h1.txt#", "PUT ", "CUT ", "REM\n", "MV "] {
                assert!(
                    !case.instruction.contains(leak),
                    "{} leaks {leak:?}",
                    case.id
                );
            }
        }
        // The previously-deferred shapes now plan: multi-file (2 inputs),
        // REM (empty expect), MV (renamed expect).
        let multi = cases.iter().find(|c| c.id == "multi-file").unwrap();
        assert_eq!(multi.files.len(), 2);
        let rem = cases.iter().find(|c| c.id == "rem-file").unwrap();
        assert!(rem.expected.is_empty());
        let mv = cases.iter().find(|c| c.id == "mv-file").unwrap();
        assert_eq!(mv.expected[0].0, "o.txt");
        // CRLF/BOM fixtures survive TOML parsing byte-exact (the shapes a
        // live model must reproduce — parse must not normalize them away).
        let crlf = cases.iter().find(|c| c.id == "crlf-preserved").unwrap();
        let crlf_input = &crlf.files[0].1;
        assert!(crlf_input.contains("\r\n"), "{}", crlf_input.escape_debug());
        assert!(crlf.expected[0].1.contains("\r\n"));
        let bom = cases.iter().find(|c| c.id == "bom-preserved").unwrap();
        assert!(bom.files[0].1.starts_with("\u{FEFF}"));
        // The v2 writer's bytes load back with identity + file vecs intact
        // (NOT a model run — proves sweep records replay in `check`).
        let first = &cases[0];
        let rec = recording_json(
            &first.id,
            "fixture-model",
            &first.instruction,
            &first.files,
            &first.expected,
            "placeholder output",
            "ExtractFail",
            2,
            polymorph_tools::EvalFormat::Hashline,
        );
        let loaded = polymorph_tools::load_recording(&rec.to_string()).unwrap();
        assert_eq!(loaded.id, first.id);
        assert_eq!(loaded.files, first.files);
        assert_eq!(loaded.expected, first.expected);
        // No v2 shape leaks into legacy: the writer emits no `input` /
        // `expected`-string / `path` keys for the old loader to trip on.
        assert!(rec.get("input").is_none());
        assert!(rec.get("path").is_none());
    }

    #[test]
    fn eval_ab_arm_routers_override_only_review_chain() {
        // F4b core without spend: per-arm role maps override ONLY the
        // review key; every other chain (act, plan, default) is shared.
        use polymorph_core::Role;
        use std::collections::BTreeMap;
        let mut base: BTreeMap<String, Vec<String>> = BTreeMap::new();
        base.insert("review".to_string(), vec!["base-reviewer".to_string()]);
        base.insert("act".to_string(), vec!["shared-fixer".to_string()]);
        let mut roles_a = base.clone();
        roles_a.insert("review".to_string(), vec!["a-model".to_string()]);
        let mut roles_b = base.clone();
        roles_b.insert("review".to_string(), vec!["b-model".to_string()]);
        let router_a = polymorph_llm::RoleRouter::new(roles_a);
        let router_b = polymorph_llm::RoleRouter::new(roles_b);
        assert_eq!(router_a.chain(Role::Review), vec!["a-model".to_string()]);
        assert_eq!(router_b.chain(Role::Review), vec!["b-model".to_string()]);
        assert_eq!(router_a.chain(Role::Act), vec!["shared-fixer".to_string()]);
        assert_eq!(router_b.chain(Role::Act), vec!["shared-fixer".to_string()]);
    }

    #[test]
    fn apply_set_supported_keys() {
        // Fresh file: debug flip.
        let out = apply_set("", "options.debug", "true").unwrap();
        assert!(out.contains("debug = true"));
        // Role chain set + list trimming.
        let out = apply_set("", "roles.default", "a, b ,,c").unwrap();
        assert!(out.contains("default = [\"a\", \"b\", \"c\"]"), "{out}");
        // Permissions (empty clears).
        let out = apply_set("", "permissions.deny", "").unwrap();
        assert!(out.contains("deny = []"), "{out}");
        // Existing sections survive the edit.
        let out = apply_set("[options]\ndebug = false\n", "roles.review", "r1").unwrap();
        assert!(out.contains("debug = false"));
        assert!(out.contains("r1"));
    }

    #[test]
    fn apply_set_refusals_name_the_help() {
        for (k, v) in [
            ("options.debug", "maybe"),
            ("roles.frobnicate", "a"),
            ("roles.default", " , "),
            ("providers.oa", "x"),
            ("pins", "x"),
            ("bogus", "x"),
            ("a.b.c", "x"),
        ] {
            let err = apply_set("", k, v).unwrap_err();
            assert!(
                err.to_string().contains("supported keys") || err.to_string().contains("must be"),
                "{k}: {err}"
            );
        }
        // Corrupt existing file refused, not overwritten.
        assert!(apply_set("not toml {{{", "options.debug", "true").is_err());
    }

    #[test]
    fn config_set_round_trips_through_the_real_loader() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("u.toml");
        let project = dir.path().join(".polymorph").join("config.toml");
        let msg = apply_config_set(&user, &project, "roles.default", "m1,m2").unwrap();
        assert!(msg.contains("roles.default"), "{msg}");
        // Live-reload proof at the CLI layer too: the loader sees the edit.
        let cfg = polymorph_core::config::load(&user, &project).unwrap();
        assert_eq!(
            cfg.chain_for(polymorph_core::Role::Default),
            vec!["m1".to_string(), "m2".to_string()]
        );
        // Deny-beats-allow survives the agent path (Gate semantics owner: tools).
        apply_config_set(&user, &project, "permissions.deny", "bash:rm").unwrap();
        let cfg = polymorph_core::config::load(&user, &project).unwrap();
        assert!(cfg.permissions.deny.iter().any(|d| d == "bash:rm"));
        // show renders the merged file.
        let shown = render_config(
            &user,
            &project,
            &polymorph_core::config::CliOverrides::default(),
        )
        .unwrap();
        assert!(shown.contains("m1"));
    }

    #[tokio::test]
    async fn plan_show_renders_board_and_refuses_faults() {
        // Chunk 48 surfacing: a valid PLAN.md boards offline; missing and
        // corrupt inputs fail naming the path/line (never silent, never
        // partial). Chunk 51: the show path is async (session store); with
        // no --session the DB is never touched (any path goes).
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("unused.db");
        let plan = dir.path().join("PLAN.md");
        std::fs::write(
            &plan,
            "# Plan: t\n\n## [b] Second\nDeps: a\nCriteria:\nx\n\n## [a] First\nDeps: none\nCriteria:\ny\n",
        )
        .unwrap();
        run_plan_show(plan, &db, None).await.unwrap();
        let err = run_plan_show(dir.path().join("nope.md"), &db, None)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("nope.md"), "{err}");
        let bad = dir.path().join("bad.md");
        std::fs::write(&bad, "## [a] A\nDeps: none\n").unwrap();
        let err = run_plan_show(bad, &db, None).await.unwrap_err().to_string();
        assert!(err.contains("title"), "{err}");
    }

    #[tokio::test]
    async fn plan_source_prefers_checkpoint_then_session_then_file() {
        // Chunk-51 resolver contract: checkpoint > session > file; title
        // always from the file; ghosts and corrupt rows are hard errors.
        use polymorph_core::sessions::Store;
        use polymorph_tasks::{Task, TaskGraph};
        fn graph_with(id: &str, title: &str) -> TaskGraph {
            let mut g = TaskGraph::new();
            g.add_task(Task::new(id, title, "c\n", vec![])).unwrap();
            g
        }
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("s.db");
        let plan = dir.path().join("PLAN.md");
        std::fs::write(
            &plan,
            "# Plan: file-title\n\n## [a] File task\nDeps: none\nCriteria:\nfc\n",
        )
        .unwrap();
        let cp = dir.path().join("cp.json");
        graph_with("z", "Checkpoint task").save_json(&cp).unwrap();
        let store = Store::open(&db).await.unwrap();
        let sess = store.create_session("s").await.unwrap();
        store
            .save_plan(&sess.id, &graph_with("m", "Stored task"))
            .await
            .unwrap();
        // Checkpoint wins over everything; title still from the file.
        let (title, g, src) = resolve_plan_graph(&db, &plan, Some(&cp), Some(&sess.id), false)
            .await
            .unwrap();
        assert_eq!(
            (title.as_str(), src),
            ("file-title", PlanSource::Checkpoint)
        );
        assert!(g.get("z").is_some());
        // Session wins over the file when no checkpoint applies.
        let (title, g, src) = resolve_plan_graph(&db, &plan, None, Some(&sess.id), false)
            .await
            .unwrap();
        assert_eq!((title.as_str(), src), ("file-title", PlanSource::Session));
        assert_eq!(g.get("m").unwrap().title, "Stored task");
        // A live session with no saved plan falls back to the file.
        let fresh = store.create_session("fresh").await.unwrap();
        let (title, g, src) = resolve_plan_graph(&db, &plan, None, Some(&fresh.id), false)
            .await
            .unwrap();
        assert_eq!((title.as_str(), src), ("file-title", PlanSource::File));
        assert!(g.get("a").is_some());
        // Ghost sessions refuse on the read-only path (never strand work on
        // a dead id); the drive path initializes instead (init-on-first-use).
        let err = resolve_plan_graph(&db, &plan, None, Some("ghost"), false)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("ghost"), "{err}");
        let (title, g, src) = resolve_plan_graph(&db, &plan, None, Some("fresh-slot"), true)
            .await
            .unwrap();
        assert_eq!((title.as_str(), src), ("file-title", PlanSource::File));
        assert!(g.get("a").is_some());
        let back = Store::open(&db)
            .await
            .unwrap()
            .load_plan("fresh-slot")
            .await
            .unwrap();
        assert!(back.is_none(), "init creates the session, stores no plan");
    }

    #[test]
    fn providers_list_names_sources_never_values() {
        let mut cfg = polymorph_core::config::PolymorphConfig::default();
        cfg.providers.push(polymorph_core::config::ProviderEntry {
            id: "oa".into(),
            base_url: "https://example.invalid".into(),
            api_key_env: Some("POLYMORPH_TEST_RENDER_ENV".into()),
            api_key_cmd: Some("echo must-not-appear".into()),
            models: vec!["m".into()],
            ..Default::default()
        });
        let out = render_providers(&cfg);
        assert!(out.contains("oa"));
        assert!(out.contains("env:POLYMORPH_TEST_RENDER_ENV"));
        assert!(out.contains("cmd"));
        assert!(!out.contains("must-not-appear-value"));
        // Empty registry says ambient-only.
        let empty = polymorph_core::config::PolymorphConfig::default();
        assert!(render_providers(&empty).contains("ambient"));
    }

    #[test]
    fn models_list_serves_fallback_without_cache() {
        let out = render_models(None).unwrap();
        assert!(out.contains("glm-5.3"));
        assert!(out.contains("muse-spark-1.3"));
    }

    #[tokio::test]
    async fn sessions_commands_hit_the_real_store() {
        use polymorph_core::sessions::Store;
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("s.db");
        // Seed one session straight through the store.
        let id = Store::open(&db)
            .await
            .unwrap()
            .create_session("seed")
            .await
            .unwrap()
            .id;
        // list / switch / delete all succeed against the same file.
        run_sessions(SessionsCmd::List, &db).await.unwrap();
        run_sessions(SessionsCmd::Switch { id: id.clone() }, &db)
            .await
            .unwrap();
        run_sessions(SessionsCmd::Delete { id: id.clone() }, &db)
            .await
            .unwrap();
        // Ghost ids fail with useful errors, not panics.
        assert!(
            run_sessions(SessionsCmd::Switch { id: "ghost".into() }, &db)
                .await
                .is_err()
        );
        assert!(run_sessions(SessionsCmd::Delete { id }, &db).await.is_err());
    }

    /// Verify a prompt-pack manifest (Epic A6/F5 gate — the FIRST code
    /// behind the `prompts.toml` ENFORCED header): every listed pack exists
    /// with matching sha256, and every `.md` under the packs dir is listed
    /// (no shadow packs). Pure over injected read/list fns so fixtures prove
    /// both accept and reject directions; the live test runs it on disk.
    fn verify_prompt_manifest(
        manifest_toml: &str,
        read_file: &dyn Fn(&str) -> Option<Vec<u8>>,
        list_md_files: &dyn Fn() -> Vec<String>,
    ) -> anyhow::Result<()> {
        use sha2::{Digest, Sha256};
        let doc: toml::Value = manifest_toml
            .parse()
            .map_err(|e| anyhow::anyhow!("manifest is not valid TOML: {e}"))?;
        let packs = doc
            .get("packs")
            .and_then(toml::Value::as_table)
            .ok_or_else(|| anyhow::anyhow!("manifest has no [packs.*] rows"))?;
        let mut listed = std::collections::BTreeSet::new();
        for (name, row) in packs {
            let path = row
                .get("path")
                .and_then(toml::Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("pack {name} has no path"))?;
            let want = row
                .get("sha256")
                .and_then(toml::Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("pack {name} has no sha256"))?;
            let bytes =
                read_file(path).ok_or_else(|| anyhow::anyhow!("pack {name} missing: {path}"))?;
            let mut hasher = Sha256::new();
            hasher.update(&bytes);
            let got = format!("{:x}", hasher.finalize());
            if got != want {
                anyhow::bail!("pack {name} drifted: {path} (manifest stale)");
            }
            listed.insert(path.to_string());
        }
        for found in list_md_files() {
            if !listed.contains(&found) {
                anyhow::bail!("shadow pack (unlisted .md): {found}");
            }
        }
        Ok(())
    }

    /// Recursive `.md` collector returning manifest-style (`/`-joined)
    /// paths relative to `root` (std only — no walkdir dep for one gate).
    fn collect_pack_md_files(prompts_dir: &std::path::Path, root: &std::path::Path) -> Vec<String> {
        fn walk(dir: &std::path::Path, root: &std::path::Path, out: &mut Vec<String>) {
            let entries = std::fs::read_dir(dir).unwrap();
            for entry in entries {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, root, out);
                } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
                    let rel = path.strip_prefix(root).unwrap();
                    out.push(
                        rel.components()
                            .map(|c| c.as_os_str().to_string_lossy().into_owned())
                            .collect::<Vec<_>>()
                            .join("/"),
                    );
                }
            }
        }
        let mut out = Vec::new();
        walk(prompts_dir, root, &mut out);
        out
    }

    fn manifest_fixture_sha(body: &str) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(body.as_bytes());
        format!("{:x}", hasher.finalize())
    }

    #[test]
    fn prompt_manifest_gate_rejects_every_drift_direction() {
        let body = "# pack\nBe strict.\n";
        let good_hex = manifest_fixture_sha(body);
        let manifest =
            format!("[packs.demo]\npath = \"prompts/demo/r.md\"\nsha256 = \"{good_hex}\"\n");
        let read_ok = |_: &str| Some(body.as_bytes().to_vec());
        let list_ok = || vec!["prompts/demo/r.md".to_string()];
        // Accept direction: exact manifest passes.
        verify_prompt_manifest(&manifest, &read_ok, &list_ok).unwrap();
        // Tampered bytes fail, naming the pack.
        let read_evil = |_: &str| Some(b"# pack\nBe lax.\n".to_vec());
        let err = verify_prompt_manifest(&manifest, &read_evil, &list_ok).unwrap_err();
        assert!(err.to_string().contains("demo"), "{err}");
        // Missing file fails, naming the path.
        let read_gone = |_: &str| None;
        let err = verify_prompt_manifest(&manifest, &read_gone, &list_ok).unwrap_err();
        assert!(err.to_string().contains("prompts/demo/r.md"), "{err}");
        // Stale hash row fails.
        let stale = manifest.replace(&good_hex, &"0".repeat(64));
        let err = verify_prompt_manifest(&stale, &read_ok, &list_ok).unwrap_err();
        assert!(err.to_string().contains("drifted"), "{err}");
        // Shadow (unlisted) .md fails.
        let list_shadow = || {
            vec![
                "prompts/demo/r.md".to_string(),
                "prompts/demo/evil.md".to_string(),
            ]
        };
        let err = verify_prompt_manifest(&manifest, &read_ok, &list_shadow).unwrap_err();
        assert!(err.to_string().contains("evil.md"), "{err}");
    }

    #[test]
    fn prompt_manifest_matches_disk() {
        // Live gate: the committed manifest describes the committed packs.
        // CWD-independent (absolute from the crate manifest dir).
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../");
        let text = std::fs::read_to_string(root.join("prompts.toml")).unwrap();
        let prompts_dir = root.join("crates/polymorph-prompts/prompts");
        verify_prompt_manifest(&text, &|p| std::fs::read(root.join(p)).ok(), &|| {
            collect_pack_md_files(&prompts_dir, &root)
        })
        .unwrap();
    }

    #[test]
    fn plan_pack_demands_bare_section() {
        // Chunk-49 pin: the plan fixer must emit a bare `## [id]` section
        // that `apply_section` accepts (c48 refused prose). Pin phrases,
        // not the full file — prose may evolve, the contract may not.
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../");
        let text = std::fs::read_to_string(
            root.join("crates/polymorph-prompts/prompts/glm/glm-5.3/plan.md"),
        )
        .unwrap();
        for phrase in [
            "ONE task section",
            "ONLY the rewritten section",
            "exactly one",
            "## [id]",
            "Status:",
            "Deps:",
            "Criteria:",
            "no code fences",
            "no `# Plan:` title",
            "no extra sections",
            "## [a] First step",
        ] {
            assert!(text.contains(phrase), "plan pack missing {phrase:?}");
        }
    }

    /// Scripted reviewer (F1 pattern, local copy): pops one verdict per
    /// round; exhaustion panics (test bug, loud — loop.rs precedent).
    struct DriveReviewer {
        script:
            std::sync::Mutex<Vec<Result<polymorph_core::Verdict, polymorph_review::ReviewerError>>>,
    }

    #[async_trait::async_trait]
    impl polymorph_review::Reviewer for DriveReviewer {
        async fn review(
            &self,
            _input: polymorph_review::ReviewerInput,
        ) -> Result<polymorph_core::Verdict, polymorph_review::ReviewerError> {
            self.script.lock().unwrap().pop().expect("script exhausted")
        }
    }

    struct DriveExecutor {
        fix_to: String,
    }

    #[async_trait::async_trait]
    impl polymorph_review::Executor for DriveExecutor {
        async fn execute(&self, _input: polymorph_review::ExecutorInput) -> anyhow::Result<String> {
            Ok(self.fix_to.clone())
        }
    }

    fn drive_fail(issue: &str) -> polymorph_core::Verdict {
        polymorph_core::Verdict {
            verdict: polymorph_core::VerdictKind::Fail,
            findings: vec![polymorph_core::Finding {
                severity: polymorph_core::Severity::P1,
                file: "a.rs".into(),
                line: Some(1),
                issue: issue.into(),
                proposed_fix: "fix".into(),
            }],
            confidence: 0.8,
        }
    }

    fn drive_pass() -> polymorph_core::Verdict {
        polymorph_core::Verdict {
            verdict: polymorph_core::VerdictKind::Pass,
            findings: vec![],
            confidence: 0.9,
        }
    }

    async fn drive_report(
        script: Vec<Result<polymorph_core::Verdict, polymorph_review::ReviewerError>>,
        max: u32,
    ) -> polymorph_review::LoopReport {
        let r = DriveReviewer {
            script: std::sync::Mutex::new(script.into_iter().rev().collect()),
        };
        let e = DriveExecutor {
            fix_to: "fixed".into(),
        };
        let cfg = polymorph_core::LoopConfig {
            max_iterations: max,
            ..Default::default()
        };
        polymorph_review::run_loop(
            &cfg,
            "goal",
            "criteria",
            "artifact",
            polymorph_review::LoopMode::Act,
            &r,
            &e,
        )
        .await
    }

    #[tokio::test]
    async fn drive_events_fail_then_pass() {
        // Genuine loop output (fail → fix → pass), translated exactly.
        let rep = drive_report(vec![Ok(drive_fail("bug")), Ok(drive_pass())], 3).await;
        assert!(matches!(
            rep.outcome,
            polymorph_review::Outcome::Passed { .. }
        ));
        let evs = report_to_events(&rep, 3);
        // RoundStarted + Verdict × 2 rounds + 1 finding + Finished.
        assert_eq!(evs.len(), 2 + 2 + 1 + 1);
        let v = events_json_value(&evs);
        let arr = v.as_array().unwrap();
        assert_eq!(arr[0]["type"], serde_json::json!("round_started"));
        assert_eq!(arr[1]["type"], serde_json::json!("verdict"));
        assert_eq!(arr[1]["passed"], serde_json::json!(false));
        assert_eq!(arr[2]["type"], serde_json::json!("finding"));
        assert_eq!(arr[5]["type"], serde_json::json!("finished"));
        assert_eq!(arr[5]["verdict"], serde_json::json!("PASSED"));
    }

    #[tokio::test]
    async fn drive_events_escalate_maps_halted() {
        let rep = drive_report(
            vec![
                Ok(drive_fail("x")),
                Ok(drive_fail("x")),
                Ok(drive_fail("x")),
            ],
            2,
        )
        .await;
        assert!(matches!(
            rep.outcome,
            polymorph_review::Outcome::Escalated { .. }
        ));
        let evs = report_to_events(&rep, 2);
        let v = events_json_value(&evs);
        let last = v.as_array().unwrap().last().unwrap();
        assert_eq!(last["type"], serde_json::json!("halted"));
        assert!(
            last["reason"].as_str().unwrap().contains("MaxIterations"),
            "{last}"
        );
    }

    #[test]
    fn drive_flags_parse() {
        // `review drive` = ReviewRunArgs + --events-json (141 charter).
        let cli = Cli::try_parse_from([
            "polymorph",
            "review",
            "drive",
            "--goal",
            "g",
            "--artifact-file",
            "a.txt",
            "--events-json",
            "ev.json",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Review {
                cmd: ReviewCmd::Drive { args, events_json },
            }) => {
                assert_eq!(args.goal, "g");
                assert_eq!(events_json.unwrap(), std::path::PathBuf::from("ev.json"));
                // Docket #2: the verify gate is off unless asked for.
                assert!(args.verify_cmd.is_empty());
                assert_eq!(args.verify_timeout_secs, None);
                // Docket #8: no flag = the per-command default.
                assert_eq!(args.on_fail, None);
            }
            other => panic!("wrong command: {other:?}"),
        }
    }

    /// Docket #8: `--on-fail` takes exactly rollback|keep; the flag beats
    /// config, drives default to `[agent] drive_on_fail`, `run` keeps.
    #[test]
    fn on_fail_flag_and_defaults() {
        use polymorph_review::OnFail;
        let parse = |mode: &str| {
            Cli::try_parse_from([
                "polymorph",
                "review",
                "run",
                "--goal",
                "g",
                "--artifact-file",
                "a.txt",
                "--on-fail",
                mode,
            ])
        };
        let Ok(Cli {
            command:
                Some(Commands::Review {
                    cmd: ReviewCmd::Run { args },
                }),
            ..
        }) = parse("keep")
        else {
            panic!("--on-fail keep parses");
        };
        assert_eq!(args.on_fail.as_deref(), Some("keep"));
        assert!(parse("discard").is_err());
        assert_eq!(
            resolve_on_fail(None, true, "rollback").unwrap(),
            OnFail::Rollback
        );
        assert_eq!(resolve_on_fail(None, true, "keep").unwrap(), OnFail::Keep);
        assert_eq!(
            resolve_on_fail(None, false, "rollback").unwrap(),
            OnFail::Keep
        );
        assert_eq!(
            resolve_on_fail(Some("rollback"), false, "keep").unwrap(),
            OnFail::Rollback
        );
        assert_eq!(
            resolve_on_fail(Some("keep"), true, "rollback").unwrap(),
            OnFail::Keep
        );
        let err = resolve_on_fail(None, true, "rollbak")
            .unwrap_err()
            .to_string();
        assert!(err.contains("drive_on_fail"), "{err}");
    }

    /// Docket #8: the rollback report rides the `halted` event as one
    /// additive key; other events and a passing file are untouched.
    #[test]
    fn rollback_attaches_to_the_halted_event_only() {
        let report = polymorph_review::LoopReport {
            outcome: polymorph_review::Outcome::Escalated {
                reason: polymorph_review::EscalateReason::MaxIterations,
                rounds: 1,
            },
            rounds_log: Vec::new(),
            surviving: Vec::new(),
        };
        let mut events = report_to_events(&report, 3);
        events.push(polymorph_drive::DriveEvent::Budget {
            rounds_used: 1,
            rounds_max: 3,
            tools_used: 0,
            tools_max: 30,
        });
        let mut v = events_json_value(&events);
        let rollback = polymorph_review::RollbackReport {
            restored: vec!["a.rs".into()],
            ..Default::default()
        };
        attach_rollback(&mut v, &rollback);
        assert_eq!(v[0]["type"], "halted");
        assert_eq!(v[0]["reason"], "MaxIterations");
        assert_eq!(v[0]["rollback"]["restored"][0], "a.rs");
        assert!(v[1].get("rollback").is_none());
    }

    /// Docket #2: `--verify-cmd` repeats in order (quotes and spaces kept
    /// verbatim for the shell), `--verify-timeout-secs` is optional.
    #[test]
    fn verify_cmd_flags_parse_in_order() {
        let cli = Cli::try_parse_from([
            "polymorph",
            "review",
            "run",
            "--goal",
            "g",
            "--artifact-file",
            "a.txt",
            "--verify-cmd",
            "cargo build",
            "--verify-cmd",
            "cargo test -- --exact \"a b\"",
            "--verify-timeout-secs",
            "90",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Review {
                cmd: ReviewCmd::Run { args },
            }) => {
                assert_eq!(
                    args.verify_cmd,
                    ["cargo build", "cargo test -- --exact \"a b\""]
                );
                assert_eq!(args.verify_timeout_secs, Some(90));
            }
            other => panic!("wrong command: {other:?}"),
        }
    }

    /// Docket #7: a seeded round's verdict event says it was
    /// harness-synthesized; unseeded drives keep byte-identical events.
    #[tokio::test]
    async fn drive_events_mark_synthesized_round_only() {
        let cfg = polymorph_core::LoopConfig {
            max_iterations: 3,
            ..Default::default()
        };
        let run = |seed: Option<polymorph_review::RoundSeed>| {
            let cfg = cfg.clone();
            async move {
                let r = DriveReviewer {
                    script: std::sync::Mutex::new(vec![Ok(drive_pass()), Ok(drive_fail("bug"))]),
                };
                let e = DriveExecutor {
                    fix_to: "fixed".into(),
                };
                polymorph_review::run_loop_seeded(
                    &cfg,
                    "goal",
                    "criteria",
                    "",
                    polymorph_review::LoopMode::Act,
                    &r,
                    &e,
                    seed,
                )
                .await
            }
        };
        let plain = drive_report(vec![Ok(drive_fail("bug")), Ok(drive_pass())], 3).await;
        let plain_json = events_json_value(&report_to_events(&plain, 3)).to_string();
        let unseeded = run(None).await;
        assert_eq!(
            events_json_value(&report_to_events(&unseeded, 3)).to_string(),
            plain_json
        );
        assert!(
            plain_json.contains("\"summary\":\"findings=1\""),
            "{plain_json}"
        );
        assert!(!plain_json.contains("synthesized"), "{plain_json}");
        // Seeded: round 1 is the harness finding, then the reviewer's fail;
        // the label and the additive `synthesized` key name the seed rule.
        for (kind, label, wire) in [
            (polymorph_review::SeedKind::Create, "CREATE", "create"),
            (polymorph_review::SeedKind::Start, "START", "start"),
        ] {
            let seeded = run(Some(polymorph_review::RoundSeed {
                kind,
                findings: drive_fail("create a.rs").findings,
            }))
            .await;
            let v = events_json_value(&report_to_events(&seeded, 3));
            let arr = v.as_array().unwrap();
            assert_eq!(arr[1]["type"], serde_json::json!("verdict"));
            assert_eq!(arr[1]["round"], serde_json::json!(1));
            assert_eq!(
                arr[1]["summary"],
                serde_json::json!(format!(
                    "findings=1 (harness-synthesized {label} finding; no reviewer call)"
                ))
            );
            assert_eq!(arr[1]["synthesized"], serde_json::json!(wire));
            assert!(arr[1].get("invalid").is_none());
            assert_eq!(arr[4]["type"], serde_json::json!("verdict"));
            assert_eq!(arr[4]["summary"], serde_json::json!("findings=1"));
            assert!(arr[4].get("synthesized").is_none());
        }
    }

    /// An Invalid round's `verdict` event carries `invalid: true` and a
    /// bounded one-line `parse_error`, and its summary says why; every
    /// other verdict event keeps exactly its four keys.
    #[tokio::test]
    async fn drive_events_mark_invalid_round_with_reason() {
        let long = format!("verdict is not JSON:\n{}", "x".repeat(400));
        let report = drive_report(
            vec![
                Err(polymorph_review::ReviewerError::Invalid(long)),
                Ok(drive_fail("bug")),
                Ok(drive_pass()),
            ],
            3,
        )
        .await;
        let v = events_json_value(&report_to_events(&report, 3));
        let arr = v.as_array().unwrap();
        assert_eq!(arr[1]["type"], serde_json::json!("verdict"));
        assert_eq!(arr[1]["passed"], serde_json::json!(false));
        assert_eq!(arr[1]["invalid"], serde_json::json!(true));
        let reason = arr[1]["parse_error"].as_str().unwrap();
        assert!(reason.starts_with("verdict is not JSON: xxx"), "{reason}");
        assert!(reason.ends_with('\u{2026}'), "{reason}");
        assert_eq!(reason.chars().count(), MAX_INVALID_REASON_CHARS + 1);
        assert_eq!(
            arr[1]["summary"],
            serde_json::json!(format!(
                "findings=0 (unparseable reviewer verdict: {reason})"
            ))
        );
        assert!(arr[1].get("synthesized").is_none());
        let verdicts: Vec<_> = arr.iter().filter(|e| e["type"] == "verdict").collect();
        assert_eq!(verdicts.len(), 3);
        for ok in &verdicts[1..] {
            assert_eq!(ok.as_object().unwrap().len(), 4, "{ok}");
        }
        assert_eq!(verdicts[1]["summary"], serde_json::json!("findings=1"));
    }

    /// Docket #7: the seed follows the CREATE-note condition and both
    /// opt-outs (`--no-create-seed`, `[agent] create_seed = false`).
    #[test]
    fn live_create_seed_follows_create_condition_and_opt_out() {
        let dir = tempfile::tempdir().unwrap();
        let wd = polymorph_delegate::WorkdirSpec::InPlace(dir.path().to_path_buf());
        let ghost = std::path::Path::new("ghost.rs");
        let seed = live_create_seed(false, &wd, ghost, "").expect("seed");
        assert_eq!(seed.kind, polymorph_review::SeedKind::Create);
        assert_eq!(seed.findings.len(), 1);
        assert_eq!(seed.findings[0].file, "ghost.rs");
        assert!(live_create_seed(true, &wd, ghost, "").is_none());
        assert!(live_create_seed(false, &wd, ghost, "body").is_none());
        std::fs::write(dir.path().join("empty.rs"), "").unwrap();
        assert!(live_create_seed(false, &wd, std::path::Path::new("empty.rs"), "").is_none());
        let cli = Cli::try_parse_from([
            "polymorph",
            "review",
            "drive",
            "--goal",
            "g",
            "--artifact-file",
            "a.txt",
            "--no-create-seed",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Review {
                cmd: ReviewCmd::Drive { args, .. },
            }) => assert!(args.no_create_seed),
            other => panic!("wrong command: {other:?}"),
        }
        let cli = Cli::try_parse_from([
            "polymorph",
            "review",
            "run",
            "--goal",
            "g",
            "--artifact-file",
            "a.txt",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Review {
                cmd: ReviewCmd::Run { args },
            }) => assert!(!args.no_create_seed),
            other => panic!("wrong command: {other:?}"),
        }
    }

    /// Docket #5: `--artifact-files` repeats, `--diff` takes an optional
    /// base (HEAD when bare), and either one makes `--artifact-file`
    /// optional; with none of the three the run is refused by clap.
    #[test]
    fn artifact_set_flags_parse() {
        let parse = |extra: &[&str]| {
            let mut argv = vec!["polymorph", "review", "drive", "--goal", "g"];
            argv.extend_from_slice(extra);
            Cli::try_parse_from(argv).map(|cli| match cli.command {
                Some(Commands::Review {
                    cmd: ReviewCmd::Drive { args, .. },
                }) => args,
                other => panic!("wrong command: {other:?}"),
            })
        };
        let args = parse(&[
            "--artifact-files",
            "a.rs",
            "--artifact-files",
            "src/*.rs",
            "--diff",
        ])
        .unwrap();
        assert_eq!(args.artifact_file, None);
        assert_eq!(args.artifact_files, ["a.rs", "src/*.rs"]);
        assert_eq!(args.diff.as_deref(), Some("HEAD"));
        let args = parse(&["--diff", "main", "--artifact-file", "x.rs"]).unwrap();
        assert_eq!(args.diff.as_deref(), Some("main"));
        assert_eq!(args.artifact_file, Some(std::path::PathBuf::from("x.rs")));
        let args = parse(&["--diff", "--max-rounds", "2"]).unwrap();
        assert_eq!(args.diff.as_deref(), Some("HEAD"));
        assert_eq!(args.max_rounds, 2);
        // Single-file mode is unchanged: no set flags, no set.
        let args = parse(&["--artifact-file", "x.rs"]).unwrap();
        assert!(args.artifact_files.is_empty() && args.diff.is_none());
        assert!(parse(&[]).is_err());
    }

    /// Docket #5: no set flag → `None` (single-file path, byte for byte);
    /// otherwise `--artifact-file` leads, then `--artifact-files` in order.
    #[test]
    fn live_artifact_set_orders_members() {
        let file = std::path::Path::new("main.rs");
        assert_eq!(live_artifact_set(Some(file), &[], None), None);
        let set = live_artifact_set(Some(file), &["b.rs".into(), "c/*.rs".into()], None).unwrap();
        assert_eq!(set.declared, ["main.rs", "b.rs", "c/*.rs"]);
        assert_eq!(set.diff_base, None);
        let set = live_artifact_set(None, &[], Some("HEAD~1")).unwrap();
        assert!(set.declared.is_empty());
        assert_eq!(set.diff_base.as_deref(), Some("HEAD~1"));
    }

    /// Docket #4: `--reviewer-tools` parses (off by default).
    #[test]
    fn reviewer_tools_flag_parses() {
        let parse = |extra: &[&str]| {
            let mut argv = vec!["polymorph", "review", "run", "--goal", "g"];
            argv.extend_from_slice(extra);
            match Cli::try_parse_from(argv).unwrap().command {
                Some(Commands::Review {
                    cmd: ReviewCmd::Run { args },
                }) => args,
                other => panic!("wrong command: {other:?}"),
            }
        };
        assert!(!parse(&["--artifact-file", "a.rs"]).reviewer_tools);
        assert!(parse(&["--artifact-file", "a.rs", "--reviewer-tools"]).reviewer_tools);
    }

    /// `--repo` test repo: local identity, byte-stable checkouts, a
    /// committed baseline (`src/main.rs`, `src/lib.rs`, an empty
    /// `empty.rs`, `docs/a.md`) plus operator state the drive must never
    /// touch: a staged edit and an untracked file.
    fn repo_fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.name", "t"],
            &["config", "user.email", "t@t"],
            &["config", "core.autocrlf", "false"],
        ] {
            git_out(d, args);
        }
        std::fs::create_dir_all(d.join("src")).unwrap();
        std::fs::create_dir_all(d.join("docs")).unwrap();
        std::fs::write(d.join("src/main.rs"), "fn main() { old(); }\n").unwrap();
        std::fs::write(d.join("src/lib.rs"), "pub fn old() {}\n").unwrap();
        std::fs::write(d.join("empty.rs"), "").unwrap();
        std::fs::write(d.join("docs/a.md"), "# a\n").unwrap();
        git_out(d, &["add", "-A"]);
        git_out(d, &["commit", "-q", "-m", "base"]);
        std::fs::write(d.join("docs/a.md"), "# a (operator wip)\n").unwrap();
        git_out(d, &["add", "docs/a.md"]);
        std::fs::write(d.join("scratch.txt"), "operator notes\n").unwrap();
        dir
    }

    fn git_out(dir: &std::path::Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Operator-visible repo state: status, index, every fixture file.
    fn operator_state(dir: &std::path::Path) -> Vec<String> {
        let mut state = vec![
            git_out(dir, &["status", "--porcelain"]),
            git_out(dir, &["diff", "--cached"]),
            git_out(dir, &["rev-parse", "HEAD"]),
        ];
        for f in [
            "src/main.rs",
            "src/lib.rs",
            "empty.rs",
            "docs/a.md",
            "scratch.txt",
        ] {
            state.push(std::fs::read_to_string(dir.join(f)).unwrap());
        }
        for f in ["src/helper.rs", "src/extra.rs", ".polymorph"] {
            assert!(!dir.join(f).exists(), "{f} leaked into the operator tree");
        }
        state
    }

    /// No worktree but the main one and no branch but `main`.
    fn assert_no_worktree_litter(dir: &std::path::Path) {
        let list = git_out(dir, &["worktree", "list"]);
        assert_eq!(list.lines().count(), 1, "{list}");
        assert_eq!(git_out(dir, &["branch", "--list"]).trim(), "* main");
    }

    /// `review run` args for a `--repo` drive on `repo` (packs by absolute
    /// path, so the test never depends on the cwd).
    fn repo_run_args(repo: &std::path::Path, extra: &[&str]) -> Box<ReviewRunArgs> {
        let pack = |rel: &str| {
            format!(
                "{}/../../crates/polymorph-prompts/prompts/{rel}",
                env!("CARGO_MANIFEST_DIR")
            )
        };
        let (review, act, plan) = (
            pack("muse/muse-spark-1.3/review.md"),
            pack("glm/glm-5.3/act.md"),
            pack("glm/glm-5.3/plan.md"),
        );
        let repo = repo.to_string_lossy().into_owned();
        let mut argv = vec![
            "polymorph",
            "review",
            "run",
            "--goal",
            "call new()",
            "--repo",
            &repo,
            "--review-pack",
            &review,
            "--exec-pack",
            &act,
            "--plan-pack",
            &plan,
        ];
        argv.extend_from_slice(extra);
        match Cli::try_parse_from(argv).unwrap().command {
            Some(Commands::Review {
                cmd: ReviewCmd::Run { args },
            }) => args,
            other => panic!("wrong command: {other:?}"),
        }
    }

    /// One recorded child turn: its workdir, brief, and the files it read.
    #[derive(Debug, Clone)]
    struct SeenTurn {
        workdir: std::path::PathBuf,
        brief: String,
        reads: Vec<(String, Option<String>)>,
    }

    fn read_all(workdir: &std::path::Path, files: &[String]) -> Vec<(String, Option<String>)> {
        files
            .iter()
            .map(|f| (f.clone(), std::fs::read_to_string(workdir.join(f)).ok()))
            .collect()
    }

    /// Reviewer stand-in (what a tool reviewer reads, from its workdir):
    /// answers the scripted verdicts in order, reading `files` (plus the
    /// spill path once the fixer has made one) every call.
    struct RepoReviewer {
        verdicts: std::sync::Mutex<std::collections::VecDeque<&'static str>>,
        files: Vec<String>,
        spill: std::sync::Arc<std::sync::Mutex<Option<String>>>,
        seen: std::sync::Mutex<Vec<SeenTurn>>,
    }

    #[async_trait::async_trait]
    impl polymorph_delegate::TurnRunner for RepoReviewer {
        async fn run_turn(
            &self,
            ctx: polymorph_delegate::TurnContext,
        ) -> anyhow::Result<polymorph_delegate::TurnOutput> {
            let mut files = self.files.clone();
            files.extend(self.spill.lock().unwrap().clone());
            self.seen.lock().unwrap().push(SeenTurn {
                reads: read_all(&ctx.workdir, &files),
                workdir: ctx.workdir.clone(),
                brief: ctx.brief.clone(),
            });
            let verdict = self.verdicts.lock().unwrap().pop_front().unwrap();
            Ok(polymorph_delegate::TurnOutput {
                text: verdict.to_string(),
                summary: "stub review".to_string(),
                tool_calls: 0,
                done: true,
                data: Some(serde_json::json!({ "verdict_text": verdict })),
            })
        }
    }

    /// Fixer stand-in with file tools: turn 1 rewrites `src/main.rs`,
    /// creates `src/helper.rs` and spills a tool output (as `bash` does);
    /// later turns only read (round 1's edits and the spill must persist).
    struct RepoFixer {
        spill: std::sync::Arc<std::sync::Mutex<Option<String>>>,
        seen: std::sync::Mutex<Vec<SeenTurn>>,
    }

    #[async_trait::async_trait]
    impl polymorph_delegate::TurnRunner for RepoFixer {
        async fn run_turn(
            &self,
            ctx: polymorph_delegate::TurnContext,
        ) -> anyhow::Result<polymorph_delegate::TurnOutput> {
            let first = self.seen.lock().unwrap().is_empty();
            let mut files = vec!["src/main.rs".to_string(), "src/helper.rs".to_string()];
            files.extend(self.spill.lock().unwrap().clone());
            self.seen.lock().unwrap().push(SeenTurn {
                reads: read_all(&ctx.workdir, &files),
                workdir: ctx.workdir.clone(),
                brief: ctx.brief.clone(),
            });
            let mut mutated: Vec<&str> = Vec::new();
            if first {
                std::fs::write(ctx.workdir.join("src/main.rs"), "fn main() { new(); }\n")?;
                std::fs::write(ctx.workdir.join("src/helper.rs"), "pub fn new() {}\n")?;
                let shown = polymorph_tools::output::save_text(
                    &ctx.workdir,
                    "log",
                    "SPILLED TOOL OUTPUT\n",
                    200,
                )?;
                *self.spill.lock().unwrap() = Some(shown);
                mutated = vec!["src/main.rs", "src/helper.rs"];
            }
            Ok(polymorph_delegate::TurnOutput {
                text: "done".to_string(),
                summary: "stub fix".to_string(),
                tool_calls: 0,
                done: true,
                data: Some(serde_json::json!({
                    "artifact": "edited the files",
                    "mutated_files": first,
                    "mutated_paths": mutated,
                })),
            })
        }
    }

    const REPO_PASS: &str = r#"{"verdict":"pass","findings":[],"confidence":0.9}"#;
    const REPO_FAIL_1: &str = r#"{"verdict":"fail","findings":[{"severity":"P1","file":"src/main.rs","line":1,"issue":"main calls old()","proposed_fix":"call new()"}],"confidence":0.8}"#;
    const REPO_FAIL_2: &str = r#"{"verdict":"fail","findings":[{"severity":"P2","file":"src/helper.rs","line":1,"issue":"helper lacks docs","proposed_fix":"add a doc line"}],"confidence":0.8}"#;

    /// `--repo` end to end through the real CLI wiring (stub models in
    /// place of the live runners): one drive tree serves every fix turn and
    /// review; the artifact grades from it, peripheral files carry content
    /// (no SKIPPED), the reviewer reads the fixer's edits and the round-1
    /// spill, round 2's fixer still sees round 1's edits, rollback stays
    /// off, and finishing leaves the operator's tree, index and branches
    /// exactly as they were, with no worktree behind.
    #[tokio::test]
    async fn repo_drive_shares_one_live_tree_and_leaves_operator_untouched() {
        let repo = repo_fixture();
        let before = operator_state(repo.path());
        let args = repo_run_args(
            repo.path(),
            &[
                "--artifact-file",
                "src/main.rs",
                "--reviewer-tools",
                "--tools",
                "--on-fail",
                "rollback",
            ],
        );
        let entry = LoopEntry {
            drive: true,
            events_json: None,
        };
        let mut parts = prepare_live_loop(&args, &Default::default(), entry)
            .await
            .unwrap();
        // Docket #8: the operator's tree is never written, so no ledger.
        assert!(parts.restore.is_none());
        let tree = parts.drive_tree.as_ref().unwrap().path().to_path_buf();
        assert!(tree.join("src/main.rs").exists());
        assert_eq!(parts.artifact, "fn main() { old(); }\n");
        let spill = std::sync::Arc::new(std::sync::Mutex::new(None));
        let reviewer = std::sync::Arc::new(RepoReviewer {
            verdicts: std::sync::Mutex::new([REPO_FAIL_1, REPO_FAIL_2, REPO_PASS].into()),
            files: vec!["src/main.rs".into(), "src/helper.rs".into()],
            spill: spill.clone(),
            seen: Default::default(),
        });
        let fixer = std::sync::Arc::new(RepoFixer {
            spill: spill.clone(),
            seen: Default::default(),
        });
        parts.reviewer.inner.runner = reviewer.clone();
        parts.executor.runner = fixer.clone();
        let report = parts.run().await;
        assert_eq!(
            report.outcome,
            polymorph_review::Outcome::Passed { rounds: 3 },
            "{report:?}"
        );
        let reviews = reviewer.seen.lock().unwrap().clone();
        let fixes = fixer.seen.lock().unwrap().clone();
        assert_eq!((reviews.len(), fixes.len()), (3, 2));
        for turn in reviews.iter().chain(&fixes) {
            assert_eq!(turn.workdir, tree, "every child works in the drive tree");
        }
        let spilled = spill.lock().unwrap().clone().unwrap();
        assert!(spilled.starts_with(".polymorph/tool-output/"), "{spilled}");
        // Round 2 grades round 1's edits from the tree: artifact from
        // disk, the peripheral file with content, never SKIPPED.
        let brief = &reviews[1].brief;
        assert!(brief.contains("fn main() { new(); }"), "{brief}");
        assert!(
            brief.contains(polymorph_review::PERIPHERAL_HEADER),
            "{brief}"
        );
        assert!(brief.contains("--- src/helper.rs: FULL"), "{brief}");
        assert!(!brief.contains(": SKIPPED ("), "{brief}");
        // Harness evidence stats the finding's file in the tree too.
        assert!(brief.contains("- src/main.rs: EXISTS (1 lines)"), "{brief}");
        // The reviewer reads the fixer's edits and its spill by path.
        let read = |t: &SeenTurn, f: &str| {
            t.reads
                .iter()
                .find(|(p, _)| p == f)
                .and_then(|(_, c)| c.clone())
        };
        assert_eq!(
            read(&reviews[1], "src/helper.rs").unwrap(),
            "pub fn new() {}\n"
        );
        assert_eq!(
            read(&reviews[1], &spilled).unwrap(),
            "SPILLED TOOL OUTPUT\n"
        );
        // The next fix round starts from round 1's tree, spill included.
        assert_eq!(
            read(&fixes[1], "src/main.rs").unwrap(),
            "fn main() { new(); }\n"
        );
        assert_eq!(read(&fixes[1], &spilled).unwrap(), "SPILLED TOOL OUTPUT\n");
        // Round 1 saw the operator's HEAD content, not its staged wip.
        assert_eq!(
            read(&reviews[0], "src/main.rs").unwrap(),
            "fn main() { old(); }\n"
        );
        parts.finish();
        assert!(!tree.exists());
        assert_no_worktree_litter(repo.path());
        assert_eq!(operator_state(repo.path()), before);
    }

    /// `--diff` + `--artifact-files` under `--repo`: the set is read from
    /// the drive tree after every fix turn, the default base is the drive's
    /// start commit, the tree's marker file is never a member, and an
    /// escalated drive discards the tree with the operator's state intact.
    #[tokio::test]
    async fn repo_drive_grades_diff_set_and_discards_on_escalate() {
        let repo = repo_fixture();
        let before = operator_state(repo.path());
        let start = git_out(repo.path(), &["rev-parse", "--short", "HEAD"]);
        let start = start.trim();
        let args = repo_run_args(
            repo.path(),
            &[
                "--diff",
                "--artifact-files",
                "docs/*.md",
                "--max-rounds",
                "2",
            ],
        );
        let mut parts = prepare_live_loop(&args, &Default::default(), LoopEntry::default())
            .await
            .unwrap();
        let tree = parts.drive_tree.as_ref().unwrap().path().to_path_buf();
        // Round 1: the committed doc (not the operator's staged wip), and
        // an empty diff vs the start commit.
        assert!(
            parts
                .artifact
                .contains(&format!("worktree diff vs {start}: 0 changed")),
            "{}",
            parts.artifact
        );
        assert!(parts.artifact.contains("1 | # a\n"), "{}", parts.artifact);
        let spill = std::sync::Arc::new(std::sync::Mutex::new(None));
        let reviewer = std::sync::Arc::new(RepoReviewer {
            verdicts: std::sync::Mutex::new([REPO_FAIL_1, REPO_FAIL_1].into()),
            files: Vec::new(),
            spill: spill.clone(),
            seen: Default::default(),
        });
        let fixer = std::sync::Arc::new(RepoFixer {
            spill,
            seen: Default::default(),
        });
        parts.reviewer.inner.runner = reviewer.clone();
        parts.executor.runner = fixer;
        let report = parts.run().await;
        assert!(
            matches!(report.outcome, polymorph_review::Outcome::Escalated { .. }),
            "{report:?}"
        );
        let reviews = reviewer.seen.lock().unwrap().clone();
        let brief = &reviews[1].brief;
        assert!(
            brief.contains(&format!("- src/main.rs: MODIFIED vs {start}")),
            "{brief}"
        );
        assert!(brief.contains("- src/helper.rs: UNTRACKED"), "{brief}");
        assert!(brief.contains("1 | fn main() { new(); }"), "{brief}");
        assert!(
            !brief.contains(polymorph_delegate::WORKTREE_MARKER),
            "{brief}"
        );
        assert!(!brief.contains(".polymorph/"), "{brief}");
        parts.finish();
        assert!(!tree.exists());
        assert_no_worktree_litter(repo.path());
        assert_eq!(operator_state(repo.path()), before);
    }

    /// CREATE detection reads the drive tree (docket #7 under `--repo`): a
    /// committed empty file stays review-first, a missing one seeds; an
    /// absolute path inside the repo re-spells repo-relative. A drive that
    /// errors out (here: dropped unfinished) still removes its tree.
    #[tokio::test]
    async fn repo_create_seed_sees_the_tree_and_drop_cleans_up() {
        let repo = repo_fixture();
        let before = operator_state(repo.path());
        let empty = repo.path().join("empty.rs").to_string_lossy().into_owned();
        let parts = prepare_live_loop(
            &repo_run_args(repo.path(), &["--artifact-file", &empty]),
            &Default::default(),
            LoopEntry::default(),
        )
        .await
        .unwrap();
        assert!(parts.seed.is_none(), "existing empty file is review-first");
        assert_eq!(
            parts.executor.artifact_path.as_deref(),
            Some(std::path::Path::new("empty.rs"))
        );
        let tree = parts.drive_tree.as_ref().unwrap().path().to_path_buf();
        drop(parts);
        assert!(!tree.exists(), "the guard removes the tree on any exit");
        assert_no_worktree_litter(repo.path());
        let parts = prepare_live_loop(
            &repo_run_args(repo.path(), &["--artifact-file", "src/new.rs"]),
            &Default::default(),
            LoopEntry::default(),
        )
        .await
        .unwrap();
        let seed = parts.seed.as_ref().expect("missing file seeds CREATE");
        assert_eq!(seed.findings[0].file, "src/new.rs");
        drop(parts);
        assert_no_worktree_litter(repo.path());
        assert_eq!(operator_state(repo.path()), before);
    }

    /// `--repo` path rules: relative stays, absolute inside re-spells
    /// (missing files through their parent), outside refuses; a non-repo
    /// refuses before any tree exists.
    #[tokio::test]
    async fn repo_paths_respell_and_refuse_outside() {
        let repo = repo_fixture();
        let root = repo.path();
        assert_eq!(repo_relative(root, "src/main.rs").unwrap(), "src/main.rs");
        let abs = root.join("src").join("main.rs");
        assert_eq!(
            repo_relative(root, &abs.to_string_lossy()).unwrap(),
            "src/main.rs"
        );
        let missing = root.join("src").join("new.rs");
        assert_eq!(
            repo_relative(root, &missing.to_string_lossy()).unwrap(),
            "src/new.rs"
        );
        let outside = tempfile::tempdir().unwrap();
        let err = repo_relative(root, &outside.path().join("x.rs").to_string_lossy()).unwrap_err();
        assert!(err.to_string().contains("outside --repo"), "{err}");
        let parent = outside.path().join("wt");
        let err = open_drive_tree(outside.path(), &parent).await.unwrap_err();
        assert!(err.to_string().contains("is not a git repo"), "{err}");
        assert!(!parent.exists());
    }

    /// Docket #4: tool reviewer reports pair off with the rounds that
    /// called the reviewer (a seeded round has none) and ride the
    /// `verdict` events as one additive key; off, events are unchanged.
    #[tokio::test]
    async fn review_tool_reports_attach_to_reviewed_rounds_only() {
        let cfg = polymorph_core::LoopConfig {
            max_iterations: 3,
            ..Default::default()
        };
        let r = DriveReviewer {
            script: std::sync::Mutex::new(vec![Ok(drive_pass()), Ok(drive_fail("bug"))]),
        };
        let e = DriveExecutor {
            fix_to: "fixed".into(),
        };
        let report = polymorph_review::run_loop_seeded(
            &cfg,
            "goal",
            "criteria",
            "",
            polymorph_review::LoopMode::Act,
            &r,
            &e,
            Some(polymorph_review::RoundSeed {
                kind: polymorph_review::SeedKind::Create,
                findings: drive_fail("create a.rs").findings,
            }),
        )
        .await;
        assert_eq!(report.rounds_log.len(), 3);
        let tools = |calls: u32| polymorph_review::ReviewToolReport {
            calls,
            tool_rounds: 1,
            inspected: vec!["src/b.rs".into()],
            ..Default::default()
        };
        let reports = [tools(1), tools(2)];
        let paired = reviewed_round_reports(&report, &reports);
        assert_eq!(paired.len(), 3);
        assert!(paired[0].is_none());
        assert_eq!(paired[1].map(|t| t.calls), Some(1));
        assert_eq!(paired[2].map(|t| t.calls), Some(2));
        let plain = events_json_value(&report_to_events(&report, 3));
        let mut off = plain.clone();
        attach_review_tools(&mut off, &report, &[]);
        assert_eq!(off, plain);
        let mut on = plain.clone();
        attach_review_tools(&mut on, &report, &reports);
        let verdicts: Vec<&serde_json::Value> = on
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["type"] == "verdict")
            .collect();
        assert!(verdicts[0].get("reviewer_tools").is_none());
        assert_eq!(verdicts[1]["reviewer_tools"]["calls"], 1);
        assert_eq!(verdicts[2]["reviewer_tools"]["calls"], 2);
        assert_eq!(verdicts[2]["reviewer_tools"]["inspected"][0], "src/b.rs");
        // Additive only: every original key keeps its value.
        for (a, b) in plain.as_array().unwrap().iter().zip(on.as_array().unwrap()) {
            for (k, v) in a.as_object().unwrap() {
                assert_eq!(&b[k], v, "{k}");
            }
        }
    }

    /// Pins the exact bytes `review drive --events-json` writes: every
    /// `DriveEvent` variant through `events_json_value` + pretty JSON (the
    /// same two calls `run_review_drive` makes), so the S0 crate extraction
    /// can prove byte-identical output. Regenerate with
    /// `POLYMORPH_UPDATE_GOLDEN=1`.
    #[test]
    fn events_json_golden() {
        use polymorph_drive::DriveEvent;
        use polymorph_drive::TaskRow;
        let events = vec![
            DriveEvent::RoundStarted {
                round: 1,
                max_rounds: 3,
            },
            DriveEvent::Verdict {
                round: 1,
                passed: false,
                summary: "findings=1".to_string(),
                invalid: None,
                synthesized: None,
            },
            DriveEvent::Verdict {
                round: 2,
                passed: false,
                summary: "findings=0 (unparseable reviewer verdict: not JSON)".to_string(),
                invalid: Some("not JSON".to_string()),
                synthesized: Some("create".to_string()),
            },
            DriveEvent::Finding {
                location: "src/main.rs:1".to_string(),
                message: "demo finding".to_string(),
            },
            DriveEvent::DiffAvailable {
                status: "M src/main.rs".to_string(),
                stat: " 1 file changed, 1 insertion(+)".to_string(),
            },
            DriveEvent::PlanProgress {
                title: "demo plan".to_string(),
                tasks: vec![TaskRow {
                    id: "t1".to_string(),
                    title: "first task".to_string(),
                    status: "pending".to_string(),
                }],
            },
            DriveEvent::Budget {
                rounds_used: 2,
                rounds_max: 3,
                tools_used: 4,
                tools_max: 30,
            },
            DriveEvent::Halted {
                reason: "MaxIterations".to_string(),
            },
            DriveEvent::Finished {
                verdict: "PASSED".to_string(),
            },
        ];
        let rendered = serde_json::to_string_pretty(&events_json_value(&events)).unwrap();
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/drive-events.golden.json");
        if std::env::var_os("POLYMORPH_UPDATE_GOLDEN").is_some() {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&path, rendered.as_bytes()).unwrap();
            return;
        }
        let recorded = std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!(
                "cannot read golden fixture {}: {e} \
                 (regenerate it with POLYMORPH_UPDATE_GOLDEN=1 cargo test -p polymorph-swe \
                 events_json_golden)",
                path.display()
            )
        });
        assert_eq!(
            rendered,
            recorded,
            "events JSON drifted from {}",
            path.display()
        );
    }
}
