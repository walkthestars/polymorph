//! Hashline property matrix. Deterministic xorshift64* (fixed seed): 300 iterations, no network, no
//! timing dependence.
//!
//! Per iteration, three properties:
//! 1. VALID: random patch against a random doc applies to EXACTLY the content
//!    computed by an independent bottom-up model (descending application needs
//!    no index shifting, so the model is trivially auditable).
//! 2. STALE: one mutated doc line makes the same patch fail (StaleTag — the
//!    store binds content, so ANY mutation invalidates). Pure-core inputs are
//!    `&`-borrowed (non-mutation structural); disk-level untouched-proof is
//!    `disk_atomicity_second_file_missing` in the unit suite.
//! 3. ROUNDTRIP: `parse(serialize(patch)) == patch` on every generated patch.

use polymorph_tools::hashline::{
    apply_to_strings, parse, serialize, FileSection, Op, Patch, SnapshotStore,
};
use std::collections::BTreeMap;
use std::path::Path;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % (n as u64).max(1)) as usize
    }
}

const POOL: &[&str] = &[
    "fn main() {",
    "}",
    "    let x = 1;",
    "",
    "+sigil line",
    "[brack#et]",
    "é unicode",
    "PUT 1:",
    "trailing  ",
    "\ttabbed",
    "CUT this",
    "0123456789",
];

/// Generated op in ORIGINAL numbering + its bottom-up model effect.
#[derive(Debug)]
enum GenOp {
    Replace {
        start: usize,
        end: usize,
        body: Vec<String>,
    },
    Insert {
        point: usize,
        body: Vec<String>,
    }, // point p: lines strictly before
    Cut {
        start: usize,
        end: usize,
    },
}

fn rand_lines(rng: &mut Rng, max: usize) -> Vec<String> {
    let n = rng.below(max + 1);
    (0..n)
        .map(|_| POOL[rng.below(POOL.len())].to_string())
        .collect()
}

fn run_matrix(iterations: usize) {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for iter in 0..iterations {
        // Random doc, 0..12 lines, CANONICALIZED to the text convention:
        // trailing "" lines alias the newline terminator (join appends it,
        // split strips it), so `split(join(doc)) != doc` for docs ending in
        // "". The disk layer always canonicalizes via `normalize()`; the
        // pure core rightly assumes canonical inputs — generating them is
        // the test's job, and an uncanonical input is caller error, not an
        // applier bug.
        let mut doc = rand_lines(&mut rng, 12);
        while doc.last().is_some_and(|l| l.is_empty()) {
            doc.pop();
        }
        let n = doc.len();

        // Random non-overlapping ops with clean edges (fuzz targets the CORE
        // paths; edge-sharing and registers are pinned by unit tests).
        let mut used: Vec<(usize, usize)> = Vec::new(); // affected [s,e], 1-based
        let mut used_points: Vec<usize> = Vec::new();
        let mut gen_ops: Vec<GenOp> = Vec::new();
        let op_count = rng.below(3) + 1;
        for _ in 0..op_count {
            let kind = rng.below(3);
            if n == 0 {
                // Empty doc: only InsertBefore(1) is valid.
                gen_ops.push(GenOp::Insert {
                    point: 0,
                    body: vec![POOL[rng.below(POOL.len())].to_string()],
                });
                break;
            }
            match kind {
                0 | 2 => {
                    // Replace or Cut over a random span 1..=3.
                    let span = rng.below(3) + 1;
                    let start = rng.below(n) + 1;
                    let end = (start + span - 1).min(n);
                    if used.iter().any(|(s, e)| start <= *e && *s <= end) {
                        continue;
                    }
                    // Symmetric guard: no earlier insert gap may lie strictly
                    // inside the new range (generation is sequential, so the
                    // range must check the inserts, not just vice versa).
                    if used_points.iter().any(|p| *p > start - 1 && *p < end) {
                        continue;
                    }
                    used.push((start, end));
                    if kind == 0 {
                        let mut body = rand_lines(&mut rng, 3);
                        if body.is_empty() {
                            body.push("pad".into());
                        }
                        gen_ops.push(GenOp::Replace { start, end, body });
                    } else {
                        gen_ops.push(GenOp::Cut { start, end });
                    }
                }
                _ => {
                    // Insert at gap point 0..=n. Legal iff the gap is not
                    // strictly inside a range (edges p==s-1 / p==e are the
                    // legal attachment points — mirror of the applier rule).
                    let p = rng.below(n + 1);
                    if used.iter().any(|(s, e)| p > s - 1 && p < *e) {
                        continue;
                    }
                    if used_points.contains(&p) {
                        continue; // keep same-point ordering out of the model
                    }
                    used_points.push(p);
                    let mut body = rand_lines(&mut rng, 3);
                    if body.is_empty() {
                        body.push("pad".into());
                    }
                    gen_ops.push(GenOp::Insert { point: p, body });
                }
            }
        }
        if gen_ops.is_empty() {
            continue;
        }

        // Independent model: apply BOTTOM-UP (descending anchors need no
        // shifting). Tie-break (the subtle case): an insert at gap p == s-1
        // shares sort key (p+1 == s) with the range below it, and the RANGE
        // must apply first — the insert shifts every line at index >= p,
        // which includes the range's own lines. (The real applier emits gap
        // content before the range body: same final order.) No other key
        // ties are generatable (same-gap inserts, same-start ranges excluded).
        let mut expected = doc.clone();
        let mut keyed: Vec<(usize, bool, usize)> = gen_ops
            .iter()
            .enumerate()
            .map(|(idx, op)| {
                let (key, is_range) = match op {
                    GenOp::Replace { start, .. } | GenOp::Cut { start, .. } => (*start, true),
                    // Gap p sorts as line p+1.
                    GenOp::Insert { point, .. } => (*point + 1, false),
                };
                (key, is_range, idx)
            })
            .collect();
        // Descending keys, ties ranges-first. Fully ordered: stability moot.
        keyed.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
        for (_, _, idx) in keyed {
            match &gen_ops[idx] {
                GenOp::Replace { start, end, body } => {
                    expected.splice(start - 1..*end, body.iter().cloned());
                }
                GenOp::Cut { start, end } => {
                    expected.drain(start - 1..*end);
                }
                GenOp::Insert { point, body } => {
                    let mut tail = expected.split_off(*point);
                    expected.extend(body.iter().cloned());
                    expected.append(&mut tail);
                }
            }
        }

        // Build the real Patch (1-based) + serialize/parse roundtrip pin.
        let mut ops = Vec::new();
        for op in &gen_ops {
            match op {
                GenOp::Replace { start, end, body } => ops.push(Op::Replace {
                    start: *start as u32,
                    end: *end as u32,
                    body: body.clone(),
                }),
                GenOp::Insert { point, body } => {
                    // Gap p: express as InsertBefore(p+1), except p==n which
                    // is also InsertBefore(n+1) — always valid (1..=n+1).
                    ops.push(Op::InsertBefore {
                        at: (*point + 1) as u32,
                        body: body.clone(),
                    });
                }
                GenOp::Cut { start, end } => ops.push(Op::Cut {
                    start: *start as u32,
                    end: *end as u32,
                    register: None,
                }),
            }
        }
        let patch = Patch {
            sections: vec![FileSection {
                path: "f".into(),
                tag: "0000".into(), // replaced below with a minted tag
                ops,
            }],
        };
        assert_eq!(
            parse(&serialize(&patch)).unwrap(),
            patch,
            "iter {iter}: roundtrip"
        );

        // VALID property.
        let doc_text = doc.join("\n");
        let mut store = SnapshotStore::new();
        let tag = store.mint(Path::new("f"), &doc_text);
        let mut patch = patch;
        patch.sections[0].tag = tag;
        let files: BTreeMap<String, String> =
            [("f".to_string(), doc_text.clone())].into_iter().collect();
        let out = apply_to_strings(&patch, &files, &store).unwrap_or_else(|e| {
            panic!("iter {iter}: valid patch rejected: {e}\nDOC={doc:?}\nGEN={gen_ops:?}")
        });
        if out["f"].new_text.as_deref().unwrap() != expected.join("\n") {
            eprintln!("iter {iter} DOC={doc:?}");
            eprintln!("iter {iter} GEN={gen_ops:?}");
            eprintln!("iter {iter} PATCH=\n{}", serialize(&patch));
        }
        assert_eq!(
            out["f"].new_text.as_deref().unwrap(),
            &expected.join("\n"),
            "iter {iter}: content mismatch"
        );

        // STALE property: mutate one line (or append when empty).
        let mut stale = doc.clone();
        if stale.is_empty() {
            stale.push("intruder".into());
        } else {
            let at = rng.below(stale.len());
            stale[at].push_str("~mut");
        }
        let stale_text = stale.join("\n");
        let stale_files: BTreeMap<String, String> =
            [("f".to_string(), stale_text)].into_iter().collect();
        assert!(
            apply_to_strings(&patch, &stale_files, &store).is_err(),
            "iter {iter}: mutated doc accepted the old tag"
        );
    }
}

#[test]
fn hashline_property_matrix_300() {
    run_matrix(300);
}
