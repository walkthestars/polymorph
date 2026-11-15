//! `ais-cert-v1` — the artifact of value.
//!
//! A certificate is self-contained: it carries the normalized property, the
//! exact source digests it was proved against, the annotations that were
//! checked, the pinned backend (name, version, binary digest), one entry per
//! obligation with the digest of its exact SMT-LIB2 query, and a base64 payload
//! holding those queries verbatim, plus the entry, seed and solver timeout
//! needed to re-derive those queries. A third party re-checks it by re-deriving
//! the obligations from the source, verifying the pinned backend, re-running
//! the vacuity guards a proof needed (a vacuous specification or a
//! contradictory invariant set discharges every obligation for free, so the
//! Full re-check refuses both), and replaying the payload through the pinned
//! solver — they never have to trust this crate,
//! and they never have to trust the model that proposed the annotations.
//!
//! Residual trust (stated, not hidden): the re-check trusts the *solver binary's
//! verdict on a query the re-checker can read*. That is the smallest trusted
//! base available without a proof-producing backend; see `docs/PROOF.md`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::ir::Program;
use crate::types::{Annotation, Obligation, ObligationStatus, Property, AIS_VERSION};

/// Certificate format tag.
pub const CERT_FORMAT: &str = "ais-cert-v1";

/// `sha256:` of arbitrary bytes.
pub fn sha256_bytes(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("sha256:{}", hex(&h.finalize()))
}

/// `sha256:` of a string's UTF-8 bytes.
pub fn sha256_tag(text: &str) -> String {
    sha256_bytes(text.as_bytes())
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Serde guard for the re-derivation fields: empty and zero values are
/// skipped, so a body that omits them serializes without those keys and a
/// certificate that never recorded them keeps its canonical body (and its
/// digest).
fn is_zero(v: &u64) -> bool {
    *v == 0
}

/// Base64 (standard alphabet, padded). Hand-rolled so the crate keeps a
/// dependency-free certificate layer.
pub fn b64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Decode standard base64; `None` on any malformed input (never partially
/// applies a certificate).
pub fn b64_decode(text: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let bytes: Vec<u8> = text.bytes().filter(|c| !c.is_ascii_whitespace()).collect();
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for quad in bytes.chunks(4) {
        let pad = quad.iter().filter(|c| **c == b'=').count();
        let mut n = 0u32;
        for (i, c) in quad.iter().enumerate() {
            let v = if *c == b'=' {
                if i < 2 {
                    return None;
                }
                0
            } else {
                u32::from(val(*c)?)
            };
            n |= v << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if pad < 2 {
            out.push((n >> 8) as u8);
        }
        if pad < 1 {
            out.push(n as u8);
        }
    }
    Some(out)
}

/// One source file's pinned bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourcePin {
    /// Display path.
    pub path: String,
    /// `sha256:` of the file's exact bytes.
    pub digest: String,
    /// Length in bytes.
    pub bytes: u64,
}

/// The backend, pinned. A different binary means a different certificate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendPin {
    /// Backend family name (`smtlib2`).
    pub name: String,
    /// Self-reported version line.
    pub version: String,
    /// `sha256:` of the executable.
    pub binary_digest: String,
    /// The exact command template, so a re-checker knows the invocation.
    pub command: Vec<String>,
}

/// One obligation, pinned by the digest of its exact query text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CertObligation {
    /// Stable id.
    pub id: String,
    /// Function.
    pub function: String,
    /// Kind (`init`, `preserve`, `exit`, `assert`, `variant`, `pre`).
    pub kind: String,
    /// Status the checker reached.
    pub status: ObligationStatus,
    /// `sha256:` of the exact SMT-LIB2 text.
    pub smt_sha256: String,
}

/// How a third party re-checks this certificate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecheckRecipe {
    /// The command to run.
    pub command: String,
    /// What a valid certificate must report.
    pub expected: String,
    /// What the replay must observe per obligation for the certificate to hold.
    pub obligation_rule: String,
}

/// The digest-covered body of a certificate (everything but the payload blob).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CertificateBody {
    /// Format tag.
    pub format: String,
    /// Primitive version.
    pub ais_version: String,
    /// The normalized property.
    pub property: Property,
    /// Entry function the proof targeted. A re-derivation input; empty on
    /// certificates that do not record it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub entry: String,
    /// Solver seed the queries were built with (`:random-seed`). A
    /// re-derivation input; zero is taken literally.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub seed: u64,
    /// Solver timeout in seconds the queries were built with (`:timeout`). A
    /// re-derivation input; zero means "not recorded".
    #[serde(default, skip_serializing_if = "is_zero")]
    pub solver_timeout_s: u64,
    /// `sha256:` over the concatenated, ordered source bytes.
    pub source_digest: String,
    /// Per-file pins.
    pub source_files: Vec<SourcePin>,
    /// The final annotations.
    pub annotations: Vec<Annotation>,
    /// The backend pin.
    pub backend: BackendPin,
    /// One entry per obligation.
    pub obligations: Vec<CertObligation>,
    /// `sha256:` of the full checker transcript.
    pub transcript_digest: String,
    /// How to replay.
    pub recheck: RecheckRecipe,
}

/// The whole certificate: body plus the payload it commits to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Certificate {
    /// Format tag.
    pub format: String,
    /// The digest-covered body.
    pub body: CertificateBody,
    /// `sha256:` of the canonical body JSON.
    pub digest: String,
    /// Base64 of the payload (a JSON array of `{id, smt}`).
    pub blob: String,
    /// `sha256:` of the decoded payload bytes.
    pub blob_digest: String,
}

/// One query carried by the certificate payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PayloadQuery {
    /// Obligation id.
    pub id: String,
    /// The exact SMT-LIB2 text.
    pub smt: String,
}

impl Certificate {
    /// Assemble a certificate from the pieces the driver holds.
    #[allow(clippy::too_many_arguments)]
    pub fn build(
        property: Property,
        entry: String,
        seed: u64,
        solver_timeout_s: u64,
        source_digest: String,
        source_files: Vec<SourcePin>,
        annotations: Vec<Annotation>,
        backend: BackendPin,
        obligations: &[Obligation],
        statuses: &BTreeMap<String, ObligationStatus>,
        transcript: &str,
        recheck_command: &str,
    ) -> Result<Certificate, String> {
        let payload: Vec<PayloadQuery> = obligations
            .iter()
            .map(|o| PayloadQuery {
                id: o.id.clone(),
                smt: o.smt.clone(),
            })
            .collect();
        let payload_bytes = serde_json::to_vec(&payload).map_err(|e| e.to_string())?;
        let blob = b64_encode(&payload_bytes);
        let blob_digest = sha256_bytes(&payload_bytes);
        let body = CertificateBody {
            format: CERT_FORMAT.to_string(),
            ais_version: AIS_VERSION.to_string(),
            property,
            entry,
            seed,
            solver_timeout_s,
            source_digest,
            source_files,
            annotations,
            backend,
            obligations: obligations
                .iter()
                .map(|o| CertObligation {
                    id: o.id.clone(),
                    function: o.function.clone(),
                    kind: o.kind.clone(),
                    status: statuses
                        .get(&o.id)
                        .copied()
                        .unwrap_or(ObligationStatus::Error),
                    smt_sha256: sha256_tag(&o.smt),
                })
                .collect(),
            transcript_digest: sha256_tag(transcript),
            recheck: RecheckRecipe {
                command: recheck_command.to_string(),
                expected: "proved".to_string(),
                obligation_rule:
                    "re-derive the payload from the source, property, annotations, entry, seed and \
                     timeout and require byte equality; require every function's precondition \
                     conjunction and every loop's invariant conjunction to be `sat`; then replay \
                     every obligation with status `discharged` and require `unsat` — a payload \
                     mismatch, a vacuous or contradictory specification, or a \
                     `sat`/`unknown`/timeout replay invalidates the certificate"
                        .to_string(),
            },
        };
        let canonical = serde_json::to_string(&body).map_err(|e| e.to_string())?;
        let digest = sha256_tag(&canonical);
        Ok(Certificate {
            format: CERT_FORMAT.to_string(),
            body,
            digest,
            blob,
            blob_digest,
        })
    }

    /// The canonical body JSON the digest covers.
    pub fn canonical_body(&self) -> Result<String, String> {
        serde_json::to_string(&self.body).map_err(|e| e.to_string())
    }

    /// Recompute the self-digest and compare. Cheap integrity gate before any
    /// replay work.
    pub fn digest_ok(&self) -> bool {
        self.canonical_body()
            .map(|c| sha256_tag(&c) == self.digest)
            .unwrap_or(false)
    }

    /// Decode the payload and check it against the committed digests.
    pub fn payload(&self) -> Result<Vec<PayloadQuery>, String> {
        let bytes = b64_decode(&self.blob).ok_or_else(|| "blob is not valid base64".to_string())?;
        if sha256_bytes(&bytes) != self.blob_digest {
            return Err("blob digest does not match the decoded payload".into());
        }
        serde_json::from_slice(&bytes).map_err(|e| format!("payload is not a query array: {e}"))
    }

    /// One line for a ledger event.
    pub fn summary(&self) -> String {
        format!(
            "{} obligations, backend {} {} ({}), digest {}",
            self.body.obligations.len(),
            self.body.backend.name,
            self.body.backend.version,
            &self.body.backend.binary_digest
                [..std::cmp::min(19, self.body.backend.binary_digest.len())],
            &self.digest[..std::cmp::min(19, self.digest.len())]
        )
    }
}

/// One named check in a replay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecheckCheck {
    /// What was checked.
    pub name: String,
    /// Whether it held.
    pub ok: bool,
    /// Detail a human can act on.
    pub detail: String,
}

/// The outcome of re-checking a certificate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecheckOutcome {
    /// True only when every check held and every obligation replayed `unsat`.
    pub ok: bool,
    /// The certificate's own verdict line (`proved` / `failed`).
    pub verdict: String,
    /// Every check performed, in order.
    pub checks: Vec<RecheckCheck>,
}

impl RecheckOutcome {
    /// The first failing check, if any.
    pub fn first_failure(&self) -> Option<&RecheckCheck> {
        self.checks.iter().find(|c| !c.ok)
    }
}

/// How thoroughly a re-check re-derives what the certificate claims: replay
/// alone does not show why the queries exist, so the default re-derives them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecheckMode {
    /// Default: re-derive the obligations from the supplied source bytes plus
    /// the certificate's annotations, property, entry and determinism
    /// settings, and require the re-derived ordered `(id, smt)` list to be
    /// byte-for-byte identical to the decoded payload.
    Full,
    /// Skip re-derivation. Digests are still verified and the payload is
    /// still replayed, but a rewritten property (or rewritten queries) with
    /// recomputed digests is not detected; the `mode` check row says so.
    DigestOnly,
}

/// Re-check a certificate against the source bytes the caller believes it
/// describes: the [`RecheckMode::Full`] convenience over [`recheck_sources`]
/// with a single source blob. For a certificate pinned to several files
/// the blob is the joined source text; prefer [`recheck_sources`] with one
/// `(path, bytes)` pair per pinned file.
pub fn recheck(
    cert: &Certificate,
    source: Option<&[u8]>,
    checker: &crate::solver::Checker,
) -> RecheckOutcome {
    let single: Option<Vec<(String, Vec<u8>)>> =
        source.map(|bytes| vec![(String::new(), bytes.to_vec())]);
    recheck_sources(cert, single.as_deref(), checker, RecheckMode::Full)
}

/// Re-check a certificate against the source files the caller believes it
/// describes. Independent of the run that produced it: it recomputes every
/// digest, verifies the pinned backend binary, binds the certificate to
/// the supplied source files, re-derives the obligations from the source
/// and the certificate's own recorded inputs ([`RecheckMode::Full`]), and
/// re-runs the driver's vacuity guards on the re-derived per-function sets
/// (every function's precondition conjunction and every loop's invariant
/// conjunction must be `sat` under the pinned checker — a forged certificate
/// over a vacuous specification or a contradictory invariant set replays
/// clean, so replay alone cannot catch it), and replays each discharged
/// obligation through the pinned checker, requiring `unsat`.
///
/// `sources` carries one `(path, bytes)` pair per source file. When the pairs
/// match the certificate's per-file pins one-for-one, each pin's sha256 and
/// byte length are verified individually and the file bytes are joined with
/// `\n` in the certificate's pin order (matched by path when every pin path is
/// present among the supplied files, otherwise in the supplied order) before
/// `source_digest` is verified over the join. When one blob is supplied for a
/// multi-file certificate, the join digest is verified and the `source_files`
/// check reports that the per-file pins were not individually checked — it
/// never guesses and never panics. [`RecheckMode::Full`] without any source
/// fails the source check: a full re-check cannot re-derive without the source.
pub fn recheck_sources(
    cert: &Certificate,
    sources: Option<&[(String, Vec<u8>)]>,
    checker: &crate::solver::Checker,
    mode: RecheckMode,
) -> RecheckOutcome {
    let mut checks = Vec::new();
    let mut ok = true;
    let mut push = |name: &str, good: bool, detail: String| {
        if !good {
            ok = false;
        }
        checks.push(RecheckCheck {
            name: name.to_string(),
            ok: good,
            detail,
        });
    };

    push(
        "format",
        cert.format == CERT_FORMAT && cert.body.format == CERT_FORMAT,
        format!("certificate advertises {:?}", cert.format),
    );

    let digest_ok = cert.digest_ok();
    push(
        "self_digest",
        digest_ok,
        if digest_ok {
            format!("body digest matches {}", cert.digest)
        } else {
            "body digest does not match the canonical body".to_string()
        },
    );

    // A replay only means something under the solver binary that proved
    // the certificate. A different binary fails the re-check here — never a
    // silent replay under some other solver.
    let backend_ok = checker.binary_digest == cert.body.backend.binary_digest;
    push(
        "backend",
        backend_ok,
        if backend_ok {
            format!(
                "checker binary matches the pinned digest {}",
                cert.body.backend.binary_digest
            )
        } else {
            format!(
                "checker binary hashes to {}, but the certificate pins {}: the replay must run \
                 under the solver that proved it",
                checker.binary_digest, cert.body.backend.binary_digest
            )
        },
    );

    // Bind the certificate to the caller's source files: per-file pins are
    // verified, so a multi-file certificate re-checks from the files it names.
    let binding = bind_source(cert, sources, mode);
    for (name, good, detail) in binding.rows {
        push(name, good, detail);
    }

    let payload = cert.payload();
    match &payload {
        Err(e) => push("payload", false, e.clone()),
        Ok(queries) => {
            let mut digest_matches = true;
            let mut missing = Vec::new();
            for q in queries {
                let entry = cert.body.obligations.iter().find(|o| o.id == q.id);
                match entry {
                    None => missing.push(format!("{} has no obligation entry", q.id)),
                    Some(o) if o.smt_sha256 != sha256_tag(&q.smt) => {
                        digest_matches = false;
                        missing.push(format!("{} query digest mismatch", q.id));
                    }
                    Some(_) => {}
                }
            }
            push(
                "payload",
                digest_matches && missing.is_empty(),
                if digest_matches && missing.is_empty() {
                    format!("{} queries decode and match their digests", queries.len())
                } else {
                    missing.join("; ")
                },
            );
        }
    }

    // Re-derive the obligations from the source the certificate is bound
    // to — replay alone cannot catch a rewritten property with recomputed
    // digests — or record explicitly that they were not re-derived.
    match mode {
        RecheckMode::DigestOnly => push(
            "mode",
            true,
            "digest-only: the payload was not re-derived from the source; a rewritten property \
             (or rewritten queries) with recomputed digests is not detected — re-run with the \
             source files for a full re-check"
                .to_string(),
        ),
        RecheckMode::Full => {
            // Hold on to the per-function sets — the vacuity guards below
            // re-run over them.
            let mut rederived_sets: Option<Vec<(String, crate::vc::VcSet)>> = None;
            let derived = match &binding.joined {
                None => Err(
                    "no source was supplied, so the payload could not be re-derived; supply the \
                     source files (or the joined text) or re-run with --digest-only"
                        .to_string(),
                ),
                Some(bytes) => rederive(cert, &String::from_utf8_lossy(bytes)),
            };
            match derived {
                Err(e) => push("payload_rederived", false, e),
                Ok(derived) => {
                    rederived_sets = Some(derived.per_function);
                    match serde_json::to_vec(&derived.payload) {
                        Err(e) => push(
                            "payload_rederived",
                            false,
                            format!("the re-derived payload could not be serialized: {e}"),
                        ),
                        Ok(encoded) => match b64_decode(&cert.blob) {
                            None => push(
                                "payload_rederived",
                                false,
                                "the payload did not decode, so it cannot be compared to the \
                                 re-derived obligations"
                                    .to_string(),
                            ),
                            Some(decoded) => {
                                let identical = decoded == encoded;
                                push(
                                    "payload_rederived",
                                    identical,
                                    if identical {
                                        format!(
                                            "{} obligations re-derived from the source; the payload \
                                             is byte-identical",
                                            derived.payload.len()
                                        )
                                    } else {
                                        format!(
                                            "the re-derived obligations differ from the payload: {}",
                                            payload_difference(
                                                &derived.payload,
                                                payload.as_deref().ok()
                                            )
                                        )
                                    },
                                );
                            }
                        },
                    }
                }
            }
            // Re-run the driver's vacuity guards over what was re-derived,
            // under the pinned checker. Replay alone cannot catch a certificate
            // over a vacuous specification or a contradictory invariant set —
            // every obligation is `unsat` for free under either — so the guards
            // are part of a full re-check exactly as they were part of the run
            // that produced the certificate. When re-derivation itself failed
            // there are no sets to guard, and both rows fail honestly rather
            // than silently skipping checks a full re-check claims to perform.
            let guards = vacuity_guard_rows(cert, checker, rederived_sets.as_deref());
            for (name, good, detail) in guards {
                push(name, good, detail);
            }
        }
    }

    // Replay: only discharged obligations must be `unsat`.
    if let Ok(queries) = &payload {
        let mut replayed = 0u32;
        let mut bad = Vec::new();
        for q in queries {
            let Some(entry) = cert.body.obligations.iter().find(|o| o.id == q.id) else {
                continue;
            };
            if entry.status != ObligationStatus::Discharged {
                continue;
            }
            replayed += 1;
            let outcome = crate::solver::run_verbatim(checker, &q.smt);
            match outcome {
                Some(run) if run.sat == crate::solver::Sat::Unsat => {}
                Some(run) => bad.push(format!("{} → {:?}", q.id, run.sat)),
                None => bad.push(format!("{} → no verdict on replay", q.id)),
            }
        }
        push(
            "replay",
            bad.is_empty(),
            if bad.is_empty() {
                format!("{replayed} discharged obligations replayed `unsat`")
            } else {
                format!("replay disagreed: {}", bad.join(", "))
            },
        );
    }

    // A certificate over nothing is not a proof. An empty obligation
    // set discharges vacuously, so the `obligations` check fails on it before
    // the all-discharged question even arises — the re-check refuses the
    // artifact for the same reason the driver refuses to prove such a unit.
    let empty_obligations = cert.body.obligations.is_empty();
    let all_discharged = !empty_obligations
        && cert
            .body
            .obligations
            .iter()
            .all(|o| o.status == ObligationStatus::Discharged);
    push(
        "obligations",
        all_discharged,
        if empty_obligations {
            "no obligations: a certificate over an empty obligation set is not a proof — the \
             empty set discharges vacuously, so the certificate is refused"
                .to_string()
        } else if all_discharged {
            format!(
                "all {} obligations are discharged",
                cert.body.obligations.len()
            )
        } else {
            let pending: Vec<&str> = cert
                .body
                .obligations
                .iter()
                .filter(|o| o.status != ObligationStatus::Discharged)
                .map(|o| o.id.as_str())
                .collect();
            format!("not discharged: {}", pending.join(", "))
        },
    );

    let recheck_ok = cert.digest_ok() && all_discharged && ok;
    RecheckOutcome {
        ok: recheck_ok,
        verdict: if recheck_ok {
            "proved".into()
        } else {
            "failed".into()
        },
        checks,
    }
}

/// Differential replay: run every discharged obligation of `cert` under a
/// *second* solver and require it to agree (`unsat`). This is extra evidence,
/// not a trust upgrade: both solvers are trusted on queries the re-checker can
/// read, and a disagreement (a `sat`, an `unknown`, a timeout or an error from
/// the second solver) fails the row so a human looks before relying on the
/// certificate. The row is named `differential`.
pub fn differential(cert: &Certificate, second: &crate::solver::Checker) -> RecheckCheck {
    let queries = match cert.payload() {
        Ok(q) => q,
        Err(e) => {
            return RecheckCheck {
                name: "differential".into(),
                ok: false,
                detail: format!("the payload did not decode, so nothing was replayed: {e}"),
            }
        }
    };
    let mut agreed = 0u32;
    let mut disagreed = Vec::new();
    for q in &queries {
        let discharged = cert
            .body
            .obligations
            .iter()
            .any(|o| o.id == q.id && o.status == ObligationStatus::Discharged);
        if !discharged {
            continue;
        }
        match crate::solver::run_verbatim(second, &q.smt) {
            Some(run) if run.sat == crate::solver::Sat::Unsat => agreed += 1,
            Some(run) => disagreed.push(format!("{} → {:?}", q.id, run.sat)),
            None => disagreed.push(format!("{} → no run", q.id)),
        }
    }
    let total = agreed + u32::try_from(disagreed.len()).unwrap_or(u32::MAX);
    let ok = disagreed.is_empty() && total > 0;
    RecheckCheck {
        name: "differential".into(),
        ok,
        detail: if total == 0 {
            "no discharged obligations to replay".into()
        } else if ok {
            format!(
                "{agreed}/{total} discharged obligations also `unsat` under {} ({})",
                second.version, second.binary_digest
            )
        } else {
            format!(
                "{agreed}/{total} agree under {}; disagreements: {}",
                second.version,
                disagreed.join(", ")
            )
        },
    }
}

impl RecheckOutcome {
    /// Append an extra check row (e.g. [`differential`]); a failing row fails
    /// the whole re-check.
    pub fn push_check(&mut self, check: RecheckCheck) {
        if !check.ok {
            self.ok = false;
            self.verdict = "failed".into();
        }
        self.checks.push(check);
    }
}

/// The result of binding a certificate to the caller's source files: the
/// joined bytes a full re-check re-derives from, plus the check rows to report.
struct SourceBinding {
    /// The joined source bytes, when any were supplied.
    joined: Option<Vec<u8>>,
    /// Check rows, in report order.
    rows: Vec<(&'static str, bool, String)>,
}

/// Verify the certificate's source binding against the caller's files.
///
/// Per-file pins are checked one-for-one when the caller supplies as many
/// files as the certificate pins: each pin's sha256 and byte length must
/// match. The join order is the certificate's pin order when every pin path is
/// present among the supplied files, otherwise the supplied order. A single
/// blob for a multi-file certificate (or any count that does not match) checks
/// only the join digest and reports that the pins were not individually
/// checked — it never guesses and never panics.
fn bind_source(
    cert: &Certificate,
    sources: Option<&[(String, Vec<u8>)]>,
    mode: RecheckMode,
) -> SourceBinding {
    let pins = &cert.body.source_files;
    let Some(pairs) = sources.filter(|p| !p.is_empty()) else {
        // No source supplied. Digest-only declines honestly; a full re-check
        // cannot re-derive without the source, so it fails here.
        let good = mode == RecheckMode::DigestOnly;
        return SourceBinding {
            joined: None,
            rows: vec![(
                "source_digest",
                good,
                if good {
                    "no source supplied; source binding was not re-checked".to_string()
                } else {
                    "no source supplied: a full re-check re-derives the payload from the source, \
                     so the source files (or the joined text) must be supplied — or re-run with \
                     --digest-only"
                        .to_string()
                },
            )],
        };
    };
    if pins.len() != pairs.len() {
        // Most commonly one blob for a multi-file certificate: the join digest
        // is still verified, and the per-file pins are reported as not
        // individually checked rather than guessed at (or panicked on).
        let join = join_files(pairs.iter().map(|pair| pair.1.as_slice()));
        let got = sha256_bytes(&join);
        return SourceBinding {
            joined: Some(join),
            rows: vec![
                (
                    "source_files",
                    true,
                    format!(
                        "per-file pins were not individually checked: the certificate pins {} \
                         file(s) and {} blob(s) was supplied",
                        pins.len(),
                        pairs.len()
                    ),
                ),
                (
                    "source_digest",
                    got == cert.body.source_digest,
                    format!(
                        "supplied source hashes to {got}; certificate binds {}",
                        cert.body.source_digest
                    ),
                ),
            ],
        };
    }
    // One file per pin: match by path when every pin path is present among the
    // supplied files, otherwise take the supplied order positionally.
    let by_path = pins
        .iter()
        .all(|pin| pairs.iter().any(|pair| pair.0 == pin.path));
    let ordered: Vec<&(String, Vec<u8>)> = if by_path {
        pins.iter()
            .map(|pin| {
                pairs
                    .iter()
                    .find(|pair| pair.0 == pin.path)
                    .expect("every pin path was confirmed present")
            })
            .collect()
    } else {
        pairs.iter().collect()
    };
    let mut problems = Vec::new();
    for (pin, bytes) in pins
        .iter()
        .zip(ordered.iter().map(|file| file.1.as_slice()))
    {
        let got = sha256_bytes(bytes);
        let got_len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if got != pin.digest || got_len != pin.bytes {
            problems.push(format!(
                "{} hashes to {got} ({got_len} bytes), the certificate pins {} ({} bytes)",
                pin.path, pin.digest, pin.bytes
            ));
        }
    }
    let join = join_files(ordered.iter().map(|file| file.1.as_slice()));
    let got = sha256_bytes(&join);
    SourceBinding {
        joined: Some(join),
        rows: vec![
            (
                "source_files",
                problems.is_empty(),
                if problems.is_empty() {
                    format!(
                        "{} source pins verified (digest and length), joined in pin order",
                        pins.len()
                    )
                } else {
                    format!("per-file pin mismatch: {}", problems.join("; "))
                },
            ),
            (
                "source_digest",
                got == cert.body.source_digest,
                format!(
                    "joined source hashes to {got}; certificate binds {}",
                    cert.body.source_digest
                ),
            ),
        ],
    }
}

/// Join per-file bytes with `\n`, exactly as the driver joined the unit's
/// files when it computed `source_digest`.
fn join_files<'a>(blobs: impl Iterator<Item = &'a [u8]>) -> Vec<u8> {
    let mut out = Vec::new();
    for (i, blob) in blobs.enumerate() {
        if i > 0 {
            out.push(b'\n');
        }
        out.extend_from_slice(blob);
    }
    out
}

/// The product of a Full re-derivation: the per-function
/// [`VcSet`](crate::vc::VcSet)s the vacuity guards re-run, plus the re-derived
/// payload the certificate's own payload is compared to.
struct Rederived {
    /// One set per function, in source order: every function is elaborated,
    /// not only the entry.
    per_function: Vec<(String, crate::vc::VcSet)>,
    /// The merged, ordered `(id, smt)` list, exactly as
    /// [`Certificate::build`] serialized it.
    payload: Vec<PayloadQuery>,
}

/// Re-derive the certificate's obligations from the source text and the
/// certificate's own recorded inputs: parse the unit, apply the recorded
/// annotations, elaborate **every** function ([`crate::vc::elaborate_all`]),
/// and serialize the ordered `(id, smt)` list exactly as
/// [`Certificate::build`] did. Deterministic: the same source, annotations,
/// property, entry, seed and timeout always re-derive the same bytes.
///
/// The per-function sets come back alongside the payload: a full
/// re-check re-runs the driver's vacuity guards over them, because a forged
/// certificate over a vacuous specification or a contradictory invariant set
/// re-derives and replays clean — every obligation is `unsat` for free.
fn rederive(cert: &Certificate, source_text: &str) -> Result<Rederived, String> {
    if cert.body.entry.is_empty() {
        return Err(
            "the certificate records no entry function, so its obligations cannot be re-derived \
             (it predates the recorded re-derivation inputs); re-run with --digest-only"
                .to_string(),
        );
    }
    if cert.body.solver_timeout_s == 0 {
        return Err(
            "the certificate records no solver timeout, so its exact queries cannot be \
             re-derived (it predates the recorded re-derivation inputs); re-run with \
             --digest-only"
                .to_string(),
        );
    }
    let goal = crate::driver::goal_from_property(&cert.body.property)?;
    let mut program =
        Program::parse(source_text).map_err(|e| format!("the source did not re-parse: {e}"))?;
    crate::driver::apply_annotations(&mut program, &cert.body.annotations)
        .map_err(|e| format!("the recorded annotations did not re-apply: {e}"))?;
    let per_function = crate::vc::elaborate_all(
        &program,
        &cert.body.entry,
        &goal,
        cert.body.solver_timeout_s,
        cert.body.seed,
    )
    .map_err(|e| format!("the unit did not re-elaborate: {e}"))?;
    let merged = crate::driver::merge_vcs(&per_function);
    let payload = merged
        .obligations
        .iter()
        .map(|o| PayloadQuery {
            id: o.id.clone(),
            smt: o.smt.clone(),
        })
        .collect();
    Ok(Rederived {
        per_function,
        payload,
    })
}

/// The vacuity-guard rows of a Full re-check, re-run under the pinned
/// checker over the re-derived per-function sets. Replay alone cannot catch a
/// forged certificate over a vacuous specification or a contradictory invariant
/// set — under either, every obligation is `unsat` for free — so the re-check
/// performs the same probes the driver performed before it reported a proof:
///
/// * `precondition` — every function's precondition conjunction must be
///   satisfiable. `unsat` is a vacuous specification (nothing was proved);
///   `unknown`, a timeout or an error means non-vacuity was not established,
///   which is also not a proof.
/// * `consistency` — every loop's candidate invariant conjunction must be
///   satisfiable. `unsat` is a contradictory invariant set (it would discharge
///   every obligation for free); anything else means consistency was not
///   established, which is not a proof either.
///
/// When re-derivation failed there are no sets to guard, so both rows fail
/// honestly instead of silently skipping checks a full re-check claims.
fn vacuity_guard_rows(
    cert: &Certificate,
    checker: &crate::solver::Checker,
    sets: Option<&[(String, crate::vc::VcSet)]>,
) -> Vec<(&'static str, bool, String)> {
    use crate::solver::Sat;
    let Some(sets) = sets else {
        return vec![
            (
                "precondition",
                false,
                "the payload was not re-derived, so the precondition satisfiability guard could \
                 not be re-run; a full re-check must establish a non-vacuous specification \
                 before it accepts a certificate"
                    .to_string(),
            ),
            (
                "consistency",
                false,
                "the payload was not re-derived, so the invariant-consistency guard could not \
                 be re-run; a full re-check must establish a consistent invariant set before \
                 it accepts a certificate"
                    .to_string(),
            ),
        ];
    };
    let mut precondition_bad = Vec::new();
    let mut precondition_probes = 0u32;
    for (name, set) in sets {
        // A function with no preconditions has nothing to be vacuous about.
        let Some(query) = set.precondition_query(cert.body.solver_timeout_s, cert.body.seed) else {
            continue;
        };
        precondition_probes += 1;
        let run = checker.run(&query);
        let sat = &run.sat;
        match sat {
            Sat::Sat => {}
            Sat::Unsat => precondition_bad.push(format!(
                "function {name}: vacuous specification — the preconditions are unsatisfiable, \
                 so every obligation of {name} holds for free and nothing was proved"
            )),
            Sat::Unknown | Sat::Timeout => precondition_bad.push(format!(
                "function {name}: the solver answered {sat:?}, so the re-check could not \
                 establish that the specification is non-vacuous"
            )),
            Sat::Error(e) => precondition_bad.push(format!(
                "function {name}: the precondition probe errored: {e}"
            )),
        }
    }
    let mut consistency_bad = Vec::new();
    let mut consistency_probes = 0u32;
    for (_name, set) in sets {
        for c in &set.consistency {
            consistency_probes += 1;
            let run = checker.run(&c.query);
            let sat = &run.sat;
            match sat {
                Sat::Sat => {}
                Sat::Unsat => consistency_bad.push(format!(
                    "{}: contradictory invariant set — the candidate invariant conjunction is \
                     unsatisfiable, so every obligation of this loop discharges for free",
                    c.id
                )),
                Sat::Unknown | Sat::Timeout => consistency_bad.push(format!(
                    "{}: the solver answered {sat:?}, so the re-check could not establish that \
                     the invariant set is consistent",
                    c.id
                )),
                Sat::Error(e) => {
                    consistency_bad.push(format!("{}: the consistency probe errored: {e}", c.id))
                }
            }
        }
    }
    vec![
        (
            "precondition",
            precondition_bad.is_empty(),
            if precondition_bad.is_empty() {
                format!(
                    "{precondition_probes} precondition probe(s) re-run under the pinned \
                     checker; every function's specification is satisfiable"
                )
            } else {
                precondition_bad.join("; ")
            },
        ),
        (
            "consistency",
            consistency_bad.is_empty(),
            if consistency_bad.is_empty() {
                format!(
                    "{consistency_probes} invariant-consistency probe(s) re-run under the \
                     pinned checker; every loop's invariant set is satisfiable"
                )
            } else {
                consistency_bad.join("; ")
            },
        ),
    ]
}

/// Human-readable first difference between the re-derived obligations and the
/// payload the certificate carries, for a failing `payload_rederived` row.
fn payload_difference(rederived: &[PayloadQuery], payload: Option<&[PayloadQuery]>) -> String {
    let Some(payload) = payload else {
        return "the payload did not parse as a query array".to_string();
    };
    if rederived.len() != payload.len() {
        return format!(
            "re-derivation produced {} obligations, the payload carries {}",
            rederived.len(),
            payload.len()
        );
    }
    for (i, (a, b)) in rederived.iter().zip(payload.iter()).enumerate() {
        if a.id != b.id {
            return format!(
                "obligation {i}: re-derived id {:?}, payload id {:?}",
                a.id, b.id
            );
        }
        if a.smt != b.smt {
            return format!(
                "obligation {}: the re-derived query differs from the payload's query",
                a.id
            );
        }
    }
    "the serialized forms differ".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips_every_padding_case() {
        for input in [
            b"".as_slice(),
            b"a".as_slice(),
            b"ab".as_slice(),
            b"abc".as_slice(),
            b"abcd".as_slice(),
            b"\x00\xff\x10".as_slice(),
        ] {
            let enc = b64_encode(input);
            assert_eq!(b64_decode(&enc).as_deref(), Some(input), "enc={enc}");
        }
        assert!(b64_decode("a").is_none(), "length must be a multiple of 4");
        assert!(b64_decode("====").is_none(), "padding cannot lead");
        assert!(b64_decode("ab$d").is_none(), "alphabet is enforced");
    }

    #[test]
    fn digests_are_prefixed_and_deterministic() {
        assert_eq!(sha256_tag("ais"), sha256_tag("ais"));
        assert!(sha256_tag("ais").starts_with("sha256:"));
        assert_ne!(sha256_tag("ais"), sha256_tag("ai5"));
        assert_eq!(sha256_tag("").len(), 7 + 64);
    }

    #[test]
    fn a_certificate_commits_to_its_payload_and_its_body() {
        let annotations = vec![Annotation {
            target: "loop1".into(),
            kind: "invariant".into(),
            text: "i >= 0".into(),
        }];
        let obligations = vec![crate::types::Obligation {
            id: "f.post0".into(),
            function: "f".into(),
            kind: "exit".into(),
            smt: "(assert true)\n(check-sat)\n".into(),
            probes: Vec::new(),
        }];
        let mut statuses = std::collections::BTreeMap::new();
        statuses.insert(
            "f.post0".to_string(),
            crate::types::ObligationStatus::Discharged,
        );
        let cert = Certificate::build(
            crate::types::Property::default(),
            "f".into(),
            7,
            5,
            "sha256:00".into(),
            vec![SourcePin {
                path: "u.ais".into(),
                digest: "sha256:11".into(),
                bytes: 3,
            }],
            annotations,
            BackendPin {
                name: "smtlib2".into(),
                version: "z3".into(),
                binary_digest: "sha256:22".into(),
                command: vec!["z3".into(), "{input}".into()],
            },
            &obligations,
            &statuses,
            "transcript",
            "polymorph proof recheck --cert c.json",
        )
        .unwrap();
        // Compatibility: a body with no recorded entry/seed/timeout serializes
        // without those keys, so a certificate that never recorded them keeps
        // its canonical body (and its digest).
        let mut legacy = cert.clone();
        legacy.body.entry = String::new();
        legacy.body.seed = 0;
        legacy.body.solver_timeout_s = 0;
        let canonical = legacy.canonical_body().unwrap();
        assert!(!canonical.contains("\"entry\""), "{canonical}");
        assert!(!canonical.contains("\"seed\""), "{canonical}");
        assert!(!canonical.contains("\"solver_timeout_s\""), "{canonical}");
        assert!(cert.digest_ok());
        let payload = cert.payload().expect("payload decodes");
        assert_eq!(payload.len(), 1);
        assert_eq!(payload[0].smt, obligations[0].smt);

        let mut tampered = cert.clone();
        tampered.body.annotations[0].text = "i >= 1".into();
        assert!(!tampered.digest_ok());

        let mut tampered = cert.clone();
        tampered.blob = b64_encode(b"[]");
        assert!(tampered.payload().is_err());
    }
}
