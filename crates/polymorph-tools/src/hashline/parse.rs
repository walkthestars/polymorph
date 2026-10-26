//! Hashline text grammar: parse + serialize.
//!
//! Strict subset, documented in `hashline/mod.rs`. The parser is intentionally
//! NOT lenient beyond the listed range separators — every rejection is a typed
//! `Parse`/`Unsupported` error naming the patch line, so fixer loops get
//! actionable feedback instead of silent misapplication.

use super::{FileSection, HashlineError, Op, Patch, MAX_RANGE_SPAN};

/// Parse patch text into a [`Patch`]. Empty/whitespace-only input is an error
/// (a no-op patch is a model mistake, and `apply` separately reports no-op
/// EDITS as diagnostics — the two must not be conflated).
pub fn parse(text: &str) -> Result<Patch, HashlineError> {
    let lines: Vec<&str> = text.lines().collect();
    let mut sections: Vec<FileSection> = Vec::new();
    let mut i = 0;
    // Skip leading blanks.
    while i < lines.len() && lines[i].trim().is_empty() {
        i += 1;
    }
    if i >= lines.len() {
        return Err(HashlineError::Parse {
            line: 1,
            msg: "empty patch (no sections)".into(),
        });
    }
    while i < lines.len() {
        if lines[i].trim().is_empty() {
            i += 1;
            continue;
        }
        let (path, tag, next) = parse_header(&lines, i)?;
        i = next;
        let mut ops = Vec::new();
        while i < lines.len() && !lines[i].trim().is_empty() && !is_header(lines[i]) {
            let (op, body_required) = parse_op_line(&lines, i)?;
            i += 1; // consume the op line; body lines follow
                    // Consume `+` body lines when the op takes a body.
            let mut body: Vec<String> = Vec::new();
            if body_required {
                while i < lines.len() && lines[i].starts_with('+') {
                    body.push(lines[i][1..].to_string());
                    i += 1;
                }
                if body.is_empty() {
                    return Err(HashlineError::EmptyBody { path: path.clone() });
                }
            } else if i < lines.len() && lines[i].starts_with('+') {
                return Err(HashlineError::Parse {
                    line: i + 1,
                    msg: "body lines (`+...`) given for a bodyless op (CUT/REM/MV/paste)".into(),
                });
            }
            // Bodyless ops pass through untouched; body-taking ops get the
            // collected body attached (possibly empty -> EmptyBody below).
            if body_required {
                ops.push(attach_body(op, body, &path)?);
            } else {
                ops.push(op);
            }
        }
        if ops.is_empty() {
            return Err(HashlineError::Parse {
                line: i.max(1),
                msg: format!("section '[{path}]' has no ops"),
            });
        }
        sections.push(FileSection { path, tag, ops });
    }
    Ok(Patch { sections })
}

fn is_header(line: &str) -> bool {
    let t = line.trim();
    t.starts_with('[') && t.ends_with(']') && t.contains('#')
}

fn parse_header(lines: &[&str], i: usize) -> Result<(String, String, usize), HashlineError> {
    let t = lines[i].trim();
    let err = |msg: &str| HashlineError::Parse {
        line: i + 1,
        msg: msg.into(),
    };
    if !t.starts_with('[') || !t.ends_with(']') {
        return Err(err("expected section header `[PATH#TAG]`"));
    }
    let inner = &t[1..t.len() - 1];
    // Split on the LAST '#' so paths containing '#' still parse.
    let hash = inner
        .rfind('#')
        .ok_or_else(|| err("header missing `#TAG`"))?;
    let (path, tag) = (&inner[..hash], &inner[hash + 1..]);
    if path.is_empty() {
        return Err(err("header has an empty PATH"));
    }
    if tag.len() != 4
        || !tag
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err(err("TAG must be exactly 4 lowercase hex chars"));
    }
    Ok((path.to_string(), tag.to_string(), i + 1))
}

/// Parse one op line. Returns (op-without-body, takes-body). Body lines are
/// consumed by the caller.
fn parse_op_line(lines: &[&str], i: usize) -> Result<(Op, bool), HashlineError> {
    let t = lines[i].trim();
    let err = |msg: String| HashlineError::Parse { line: i + 1, msg };
    if let Some(rest) = t.strip_prefix("PUT") {
        let rest = rest.trim();
        // PUT with trailing ':' takes a body; without ':' must be a register paste.
        if let Some(target) = rest.strip_suffix(':') {
            let target = target.trim();
            if target.contains('@') {
                return Err(err("PUT with @register cannot also take a body".into()));
            }
            return parse_put_target(target, i).map(|op| (op, true));
        }
        // Register paste: `PUT >N @name`.
        let (target, reg) = rest
            .rsplit_once(char::is_whitespace)
            .ok_or_else(|| err("PUT without ':' must be `PUT >N @name`".into()))?;
        let name = reg.strip_prefix('@').ok_or_else(|| {
            err("PUT without ':' must be `PUT >N @name` (register starts with @)".into())
        })?;
        if name.is_empty() || name.contains(char::is_whitespace) {
            return Err(err("bad register name (one @word, no spaces)".into()));
        }
        let after = parse_insert_point(target.trim(), i, '>')?;
        return Ok((
            Op::Paste {
                after,
                register: name.into(),
            },
            false,
        ));
    }
    if let Some(rest) = t.strip_prefix("CUT") {
        let rest = rest.trim();
        // Optional trailing `@name` register.
        let (target, register) = match rest.rsplit_once(char::is_whitespace) {
            Some((tgt, reg)) if reg.starts_with('@') => (tgt, Some(reg[1..].to_string())),
            _ => (rest, None),
        };
        if let Some(r) = &register {
            if r.is_empty() || r.contains(char::is_whitespace) {
                return Err(err("bad register name (one @word, no spaces)".into()));
            }
        }
        let (start, end) = parse_range(target, i)?;
        return Ok((
            Op::Cut {
                start,
                end,
                register,
            },
            false,
        ));
    }
    if t == "REM" {
        return Ok((Op::RemoveFile, false));
    }
    if let Some(dest) = t.strip_prefix("MV") {
        let dest = dest.trim();
        if dest.is_empty() {
            return Err(err("MV needs a destination (`MV new/path`)".into()));
        }
        return Ok((Op::MoveFile { dest: dest.into() }, false));
    }
    Err(err(format!("unknown op (expected PUT/CUT/REM/MV): {t}")))
}

/// `PUT <target>:` targets: `<N` (insert before), `>N` (insert after),
/// `N` / `N<sep>M` (replace). `N*` is recognized but Unsupported.
fn parse_put_target(target: &str, i: usize) -> Result<Op, HashlineError> {
    let err = |msg: String| HashlineError::Parse { line: i + 1, msg };
    if let Some(n) = target.strip_prefix('<') {
        let at = parse_line_no(n.trim(), i)?;
        return Ok(Op::InsertBefore {
            at,
            body: Vec::new(),
        });
    }
    if let Some(n) = target.strip_prefix('>') {
        let at = parse_line_no(n.trim(), i)?;
        return Ok(Op::InsertAfter {
            at,
            body: Vec::new(),
        });
    }
    if target.ends_with('*') {
        return Err(HashlineError::Unsupported(format!(
            "syntactic-block op `{target}` needs tree-sitter support"
        )));
    }
    let (start, end) = parse_range(target, i).map_err(|_| {
        err(format!(
            "bad PUT target `{target}` (want <N, >N, N, or N.=M)"
        ))
    })?;
    Ok(Op::Replace {
        start,
        end,
        body: Vec::new(),
    })
}

/// `PUT >N` insert point (register paste form).
fn parse_insert_point(target: &str, i: usize, dir: char) -> Result<u32, HashlineError> {
    let err = HashlineError::Parse {
        line: i + 1,
        msg: format!("bad insert target `{target}` (want `{dir}N`)"),
    };
    let n = target.strip_prefix(dir).ok_or(err)?;
    parse_line_no(n.trim(), i)
}

fn parse_line_no(s: &str, i: usize) -> Result<u32, HashlineError> {
    s.parse::<u32>()
        .map_err(|_| HashlineError::Parse {
            line: i + 1,
            msg: format!("bad line number `{s}` (want a positive integer)"),
        })
        .and_then(|n| {
            if n == 0 {
                Err(HashlineError::Parse {
                    line: i + 1,
                    msg: "line numbers start at 1 (0 is invalid)".into(),
                })
            } else {
                Ok(n)
            }
        })
}

/// Range `N` / `N<sep>M` with separators `.=`, `=`, `-`, `..`, `…`.
/// `N*` is recognized but Unsupported (tree-sitter).
fn parse_range(s: &str, i: usize) -> Result<(u32, u32), HashlineError> {
    let err = |msg: String| HashlineError::Parse { line: i + 1, msg };
    let s = s.trim();
    if s.ends_with('*') {
        return Err(HashlineError::Unsupported(format!(
            "syntactic-block range `{s}` needs tree-sitter support"
        )));
    }
    // Longest separators first so `..` / `.=` win over `=` / `-` / `.`.
    for sep in [".=", "..", "…", "=", "-", "."] {
        if let Some((a, b)) = s.split_once(sep) {
            // Guard: a lone `N` contains no separator — but `N-M` splitting on
            // `-` is right; watch out for empty halves (`-3`, `3-`).
            if a.is_empty() || b.is_empty() {
                continue;
            }
            // `.` alone would split floats/paths weirdly — only accept when
            // both halves are pure integers (checked below anyway).
            let start = parse_line_no(a.trim(), i)?;
            let end = parse_line_no(b.trim(), i)?;
            if end < start {
                return Err(err(format!("inverted range `{s}` (end before start)")));
            }
            check_span(s, start, end)?;
            return Ok((start, end));
        }
    }
    // Single line.
    let n = parse_line_no(s, i)?;
    Ok((n, n))
}

fn check_span(raw: &str, start: u32, end: u32) -> Result<(), HashlineError> {
    if end.saturating_sub(start).saturating_add(1) > MAX_RANGE_SPAN {
        return Err(HashlineError::RangeTooLarge(raw.into()));
    }
    Ok(())
}

/// Attach a parsed body to a body-taking op.
fn attach_body(op: Op, body: Vec<String>, path: &str) -> Result<Op, HashlineError> {
    match op {
        Op::Replace { start, end, .. } => Ok(Op::Replace { start, end, body }),
        Op::InsertBefore { at, .. } => Ok(Op::InsertBefore { at, body }),
        Op::InsertAfter { at, .. } => Ok(Op::InsertAfter { at, body }),
        _ => Err(HashlineError::Parse {
            line: 0,
            msg: format!("internal: body attached to bodyless op in '{path}'"),
        }),
    }
}

/// Serialize a [`Patch`] back to text. `parse(serialize(p)) == p` is tested
/// (roundtrip pin) and the output doubles as prompt-pack examples.
pub fn serialize(patch: &Patch) -> String {
    let mut out = String::new();
    for sec in &patch.sections {
        out.push_str(&format!("[{}#{}]\n", sec.path, sec.tag));
        for op in &sec.ops {
            match op {
                Op::Replace { start, end, body } => {
                    if start == end {
                        out.push_str(&format!("PUT {start}:\n"));
                    } else {
                        out.push_str(&format!("PUT {start}.={end}:\n"));
                    }
                    push_body(&mut out, body);
                }
                Op::InsertBefore { at, body } => {
                    out.push_str(&format!("PUT <{at}:\n"));
                    push_body(&mut out, body);
                }
                Op::InsertAfter { at, body } => {
                    out.push_str(&format!("PUT >{at}:\n"));
                    push_body(&mut out, body);
                }
                Op::Paste { after, register } => {
                    out.push_str(&format!("PUT >{after} @{register}\n"));
                }
                Op::Cut {
                    start,
                    end,
                    register,
                } => {
                    if start == end {
                        out.push_str(&format!("CUT {start}"));
                    } else {
                        out.push_str(&format!("CUT {start}.={end}"));
                    }
                    if let Some(r) = register {
                        out.push_str(&format!(" @{r}"));
                    }
                    out.push('\n');
                }
                Op::RemoveFile => out.push_str("REM\n"),
                Op::MoveFile { dest } => out.push_str(&format!("MV {dest}\n")),
            }
        }
    }
    out
}

fn push_body(out: &mut String, body: &[String]) {
    for line in body {
        out.push('+');
        out.push_str(line);
        out.push('\n');
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_one(text: &str) -> FileSection {
        let p = parse(text).unwrap();
        assert_eq!(p.sections.len(), 1);
        p.sections.into_iter().next().unwrap()
    }

    #[test]
    fn replace_range_with_body() {
        let s = parse_one("[a.rs#ab12]\nPUT 2.=3:\n+x\n+y\n");
        assert_eq!(s.path, "a.rs");
        assert_eq!(s.tag, "ab12");
        assert_eq!(
            s.ops,
            vec![Op::Replace {
                start: 2,
                end: 3,
                body: vec!["x".into(), "y".into()]
            }]
        );
    }

    #[test]
    fn insert_forms_and_single_line_replace() {
        let s = parse_one("[a.rs#0000]\nPUT <1:\n+top\nPUT >5:\n+bot\nPUT 3:\n+mid\n");
        assert_eq!(
            s.ops,
            vec![
                Op::InsertBefore {
                    at: 1,
                    body: vec!["top".into()]
                },
                Op::InsertAfter {
                    at: 5,
                    body: vec!["bot".into()]
                },
                Op::Replace {
                    start: 3,
                    end: 3,
                    body: vec!["mid".into()]
                },
            ]
        );
    }

    #[test]
    fn cut_register_paste_rem_mv() {
        let s = parse_one("[a.rs#0000]\nCUT 2.=3 @tmp\nPUT >9 @tmp\nREM\n");
        assert!(matches!(
            &s.ops[0],
            Op::Cut { start: 2, end: 3, register: Some(r) } if r == "tmp"
        ));
        assert!(matches!(
            &s.ops[1],
            Op::Paste { after: 9, register: r } if r == "tmp"
        ));
        assert!(matches!(s.ops[2], Op::RemoveFile));
        let s = parse_one("[a.rs#0000]\nMV b.rs\n");
        assert!(matches!(s.ops[0], Op::MoveFile { .. }));
    }

    #[test]
    fn range_separator_leniency() {
        // All six spellings of 2..=3.
        for target in ["2.=3", "2=3", "2-3", "2..3", "2…3", "2.3"] {
            let s = parse_one(&format!("[a.rs#0000]\nCUT {target}\n"));
            assert!(
                matches!(
                    s.ops[0],
                    Op::Cut {
                        start: 2,
                        end: 3,
                        ..
                    }
                ),
                "{target}"
            );
        }
    }

    #[test]
    fn syntactic_block_is_unsupported_not_misapplied() {
        let e = parse("[a.rs#0000]\nPUT 4*:\n+x\n").unwrap_err();
        assert!(matches!(e, HashlineError::Unsupported(_)));
        let e = parse("[a.rs#0000]\nCUT 4*\n").unwrap_err();
        assert!(matches!(e, HashlineError::Unsupported(_)));
    }

    #[test]
    fn body_sigil_rules() {
        // `+` alone = empty line; `++x` = content `+x`.
        let s = parse_one("[a.rs#0000]\nPUT 1:\n+\n++x\n");
        assert_eq!(
            s.ops,
            vec![Op::Replace {
                start: 1,
                end: 1,
                body: vec!["".into(), "+x".into()]
            }]
        );
        // Empty body rejected (use CUT).
        assert!(matches!(
            parse("[a.rs#0000]\nPUT 1:\n"),
            Err(HashlineError::EmptyBody { .. })
        ));
        // Body after bodyless op rejected.
        assert!(parse("[a.rs#0000]\nCUT 1\n+x\n").is_err());
    }

    #[test]
    fn malformed_headers_rejected() {
        for bad in [
            "PUT 1:\n+x\n",               // no section at all
            "[a.rs]\nPUT 1:\n+x\n",       // missing tag
            "[a.rs#XYZ1]\nPUT 1:\n+x\n",  // non-hex tag
            "[a.rs#ab12",                 // unclosed
            "[#ab12]\nPUT 1:\n+x\n",      // empty path
            "[a.rs#ab123]\nPUT 1:\n+x\n", // 5-hex tag
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
        // Path containing '#' splits on the LAST one.
        let s = parse_one("[we#ird.rs#ab12]\nPUT 1:\n+x\n");
        assert_eq!(s.path, "we#ird.rs");
    }

    #[test]
    fn bad_ops_rejected_with_line_numbers() {
        let e = parse("[a.rs#0000]\nPUT 1:\n+x\nFROBNICATE\n").unwrap_err();
        match e {
            HashlineError::Parse { line, .. } => assert_eq!(line, 4),
            _ => panic!("want Parse, got {e:?}"),
        }
        assert!(parse("[a.rs#0000]\nPUT 0:\n+x\n").is_err()); // line 0
        assert!(parse("[a.rs#0000]\nPUT 5.=2:\n").is_err()); // inverted
        assert!(parse("[a.rs#0000]\nPUT 1:\n+x\n").is_ok());
    }

    #[test]
    fn range_cap_enforced() {
        assert!(matches!(
            parse("[a.rs#0000]\nCUT 1.=100001\n"),
            Err(HashlineError::RangeTooLarge(_))
        ));
    }

    #[test]
    fn serialize_roundtrips() {
        let text = "[a.rs#ab12]\nPUT 2.=3:\n+x\n+\n++y\nPUT <1:\n+top\nCUT 9 @r\nPUT >9 @r\n";
        let p = parse(text).unwrap();
        assert_eq!(parse(&serialize(&p)).unwrap(), p);
    }

    #[test]
    fn empty_patch_rejected() {
        assert!(parse("").is_err());
        assert!(parse("  \n \n").is_err());
    }
}
