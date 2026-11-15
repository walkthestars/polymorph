//! `polymorph-ais` — Automated Invariant Synthesis as a verifier-judged primitive.
//!
//! Design axiom: **the LLM is untrusted; only the verifier's verdict counts.**
//! Nothing in this crate trusts a proposer. A [`crate::types::Verdict::Proved`]
//! exists only when a deterministic checker discharged every proof obligation,
//! and the artifact of value is the resulting [`crate::cert::Certificate`],
//! which a third party can re-check without this crate's cooperation.
//!
//! Five components, one loop ([`crate::driver`]):
//!
//! 1. **Proposer** ([`crate::propose`]) — untrusted, possibly an LLM. Here it is
//!    either a deterministic candidate generator (Houdini-style guess-and-check)
//!    or any caller-supplied [`crate::propose::Proposer`]; the crate never calls
//!    a model itself.
//! 2. **Elaborator** ([`crate::ir`] + [`crate::vc`]) — parses the candidate
//!    annotation text into the core IR and turns it into verification conditions.
//!    Malformed text is a proposer fault, never a proof failure.
//! 3. **Checker** ([`crate::solver`] + [`crate::backend`]) — a sandboxed SMT
//!    subprocess. It is the only source of truth.
//! 4. **Analyzer** ([`crate::classify`]) — maps a failed obligation onto the
//!    failure taxonomy, so the refiner gets a class and a counterexample rather
//!    than a solver log.
//! 5. **Certificate** ([`crate::cert`]) — self-contained, digests pinned to the
//!    exact bytes proven, replayable by [`crate::cert::recheck`].
//!
//! Scope (v1, declared in [`crate::backend::Capabilities`]): a small imperative
//! core language with integer scalars, integer arrays, `while` loops with
//! candidate invariants and optional termination variants, `if`, `assert`, and
//! function contracts. Concurrency, heaps, floating point and pointers are
//! outside the fragment and are refused at admission rather than approximated.

pub mod backend;
pub mod bench;
pub mod cert;
pub mod classify;
pub mod driver;
pub mod eval;
pub mod ir;
pub mod propose;
pub mod smt;
pub mod solver;
pub mod types;
pub mod vc;

pub use backend::{Backend, Capabilities};
pub use cert::{Certificate, RecheckOutcome};
pub use driver::{run, DriverConfig, DriverOutcome};
pub use ir::{Function, Program};
pub use propose::{
    Feedback, HeuristicProposer, LoopBrief, Proposal, ProposalContext, Proposer, ScriptedProposer,
};
pub use types::{FailureClass, Property, PropertyKind, Request, Response, Verdict};
