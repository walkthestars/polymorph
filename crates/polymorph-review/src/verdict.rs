//! Verdict text boundary (Epic F1a).
//!
//! LLMs emit text; the loop runs on [`polymorph_core::Verdict`]. This module
//! is the ONLY text→verdict path: strict `serde_json` object parse, then
//! semantic validation. Prose is refused, never scraped (0-master §2.6:
//! deterministic verdict shape — never parse prose).
//!
//! Field VALUES are read tolerantly where the intent is unambiguous (live
//! drive: one finding's `"line": "    return ..."` voided a whole verdict
//! and its round): `verdict`/`severity` ignore case and padding,
//! `confidence` takes a numeric string, and `line` takes `12`, `"12"`,
//! `"12-15"`, `"L12"` — anything else is an unknown location (`None`), its
//! text kept in the issue. A missing key, a non-object, an empty `fail` or
//! an unknown variant stays invalid.

use polymorph_core::{Finding, Severity, Verdict, VerdictKind};

/// Why verdict text was rejected. Every variant names the fault so the
/// caller (F2's LLM reviewer, `review check`) can report it exactly.
#[derive(Debug, Clone, PartialEq)]
pub enum VerdictError {
    /// Blank/whitespace-only input.
    Empty,
    /// Not JSON at all (carries serde's message for the report, never
    /// matched on for control flow).
    NotJson(String),
    /// A required top-level key is absent.
    MissingField(&'static str),
    /// A field has the wrong JSON type.
    BadType {
        field: &'static str,
        expected: &'static str,
    },
    /// `verdict` is not `pass`/`fail` (carries the offending rendering).
    BadVariant(String),
    /// `fail` with zero findings — a reviewer must justify failure.
    FailWithoutFindings,
    /// Finding `index` has an absent/non-string/blank `field`
    /// (`file`, `issue`, `proposed_fix`) or an unknown `severity`.
    EmptyFinding { index: usize, field: &'static str },
    /// `severity` value is not P0–P3 (carries index + offending rendering).
    BadSeverity { index: usize, got: String },
    /// `confidence` is not a finite number in 0..=1 (offending rendering).
    BadConfidence(String),
}

impl std::fmt::Display for VerdictError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VerdictError::Empty => write!(f, "verdict is empty"),
            VerdictError::NotJson(e) => write!(f, "verdict is not JSON: {e}"),
            VerdictError::MissingField(k) => write!(f, "verdict misses required key {k:?}"),
            VerdictError::BadType { field, expected } => {
                write!(f, "verdict key {field:?} must be {expected}")
            }
            VerdictError::BadVariant(got) => {
                write!(f, "verdict must be \"pass\"|\"fail\" (got {got})")
            }
            VerdictError::FailWithoutFindings => {
                write!(f, "verdict \"fail\" must carry at least one finding")
            }
            VerdictError::EmptyFinding { index, field } => {
                write!(f, "finding {index} has empty {field:?}")
            }
            VerdictError::BadSeverity { index, got } => {
                write!(f, "finding {index} severity must be P0–P3 (got {got})")
            }
            VerdictError::BadConfidence(got) => {
                write!(f, "confidence must be a number in 0..=1 (got {got})")
            }
        }
    }
}

impl std::error::Error for VerdictError {}

fn bad_type(field: &'static str, expected: &'static str) -> VerdictError {
    VerdictError::BadType { field, expected }
}

/// Parse + validate verdict text into a typed [`Verdict`].
pub fn parse_verdict(text: &str) -> Result<Verdict, VerdictError> {
    if text.trim().is_empty() {
        return Err(VerdictError::Empty);
    }
    let v: serde_json::Value =
        serde_json::from_str(text).map_err(|e| VerdictError::NotJson(e.to_string()))?;
    let obj = v
        .as_object()
        .ok_or_else(|| VerdictError::NotJson("top level must be an object".to_string()))?;

    let verdict = match obj.get("verdict") {
        None => return Err(VerdictError::MissingField("verdict")),
        Some(serde_json::Value::String(s)) if s.trim().eq_ignore_ascii_case("pass") => {
            VerdictKind::Pass
        }
        Some(serde_json::Value::String(s)) if s.trim().eq_ignore_ascii_case("fail") => {
            VerdictKind::Fail
        }
        Some(other) => return Err(VerdictError::BadVariant(other.to_string())),
    };

    let findings_raw = match obj.get("findings") {
        None => return Err(VerdictError::MissingField("findings")),
        Some(serde_json::Value::Array(a)) => a,
        Some(_) => return Err(bad_type("findings", "an array")),
    };

    let confidence = match obj.get("confidence") {
        None => return Err(VerdictError::MissingField("confidence")),
        Some(raw) => {
            let c = match raw {
                serde_json::Value::Number(n) => n.as_f64(),
                serde_json::Value::String(s) => s.trim().parse::<f64>().ok(),
                _ => None,
            };
            match c {
                Some(c) if c.is_finite() && (0.0..=1.0).contains(&c) => c as f32,
                _ => return Err(VerdictError::BadConfidence(raw.to_string())),
            }
        }
    };

    let mut findings = Vec::with_capacity(findings_raw.len());
    for (index, raw) in findings_raw.iter().enumerate() {
        findings.push(parse_finding(index, raw)?);
    }
    if verdict == VerdictKind::Fail && findings.is_empty() {
        return Err(VerdictError::FailWithoutFindings);
    }
    Ok(Verdict {
        verdict,
        findings,
        confidence,
    })
}

fn text_field(
    finding: &serde_json::Map<String, serde_json::Value>,
    index: usize,
    field: &'static str,
) -> Result<String, VerdictError> {
    match finding.get(field) {
        Some(serde_json::Value::String(s)) if !s.trim().is_empty() => Ok(s.clone()),
        _ => Err(VerdictError::EmptyFinding { index, field }),
    }
}

fn parse_finding(index: usize, raw: &serde_json::Value) -> Result<Finding, VerdictError> {
    let obj = raw
        .as_object()
        .ok_or_else(|| bad_type("findings[]", "objects"))?;
    let severity = match obj.get("severity") {
        Some(serde_json::Value::String(s)) => match s.trim().to_ascii_uppercase().as_str() {
            "P0" => Severity::P0,
            "P1" => Severity::P1,
            "P2" => Severity::P2,
            "P3" => Severity::P3,
            _ => {
                return Err(VerdictError::BadSeverity {
                    index,
                    got: s.clone(),
                });
            }
        },
        other => {
            return Err(VerdictError::BadSeverity {
                index,
                got: other
                    .map(|v| v.to_string())
                    .unwrap_or("<missing>".to_string()),
            });
        }
    };
    let file = text_field(obj, index, "file")?;
    let line = obj.get("line").and_then(lenient_line);
    let mut issue = text_field(obj, index, "issue")?;
    // A non-location `line` string is usually the quoted code the brief
    // asks for: keep it in the issue (bounded) unless already there.
    if let Some(serde_json::Value::String(s)) = obj.get("line") {
        let quoted = s.trim();
        if line.is_none() && !quoted.is_empty() && !issue.contains(quoted) {
            let mut q: String = quoted.chars().take(MAX_QUOTED_LINE_CHARS).collect();
            if quoted.chars().count() > MAX_QUOTED_LINE_CHARS {
                q.push('\u{2026}');
            }
            issue.push_str(&format!(" (quoted: {q})"));
        }
    }
    let proposed_fix = text_field(obj, index, "proposed_fix")?;
    Ok(Finding {
        severity,
        file,
        line,
        issue,
        proposed_fix,
    })
}

/// Bound on a non-location `line` string carried into the issue.
const MAX_QUOTED_LINE_CHARS: usize = 200;

/// A finding's `line` as a location, or `None` (unknown location — never a
/// verdict rejection). Numbers must be whole and fit `u32`; strings must
/// be a line reference — `12`, `L12`, `line 12`, optionally followed by a
/// range or column (`12-15`, `L12-L15`, `12:5`), first number taken. A
/// string merely CONTAINING digits (`return x + 1`) is not a location.
fn lenient_line(raw: &serde_json::Value) -> Option<u32> {
    match raw {
        serde_json::Value::Number(n) => n
            .as_u64()
            .or_else(|| {
                n.as_f64()
                    .filter(|f| f.fract() == 0.0 && *f >= 0.0)
                    .map(|f| f as u64)
            })
            .and_then(|u| u32::try_from(u).ok()),
        serde_json::Value::String(s) => line_ref(s),
        _ => None,
    }
}

/// `L12` / `line 12` / `12` with an optional `-15` / `:5` / `,15` tail.
fn line_ref(s: &str) -> Option<u32> {
    let lower = s.trim().to_ascii_lowercase();
    let body = ["lines", "line", "l"]
        .iter()
        .find_map(|p| lower.strip_prefix(p))
        .unwrap_or(&lower)
        .trim_start();
    let digits = body.len() - body.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    if digits == 0 {
        return None;
    }
    let (num, rest) = body.split_at(digits);
    let rest = rest.trim_start();
    if !rest.is_empty() {
        let tail = rest
            .strip_prefix(['-', '\u{2013}', '\u{2014}', ':', ','])?
            .trim();
        let tail = tail.strip_prefix('l').unwrap_or(tail);
        if tail.is_empty() || !tail.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
    }
    num.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASS: &str = r#"{"verdict":"pass","findings":[],"confidence":0.9}"#;
    const FAIL: &str = r#"{"verdict":"fail","findings":[{"severity":"P1","file":"src/main.rs","line":10,"issue":"unchecked unwrap","proposed_fix":"use context"}],"confidence":0.8}"#;

    #[test]
    fn valid_pass_parses() {
        let v = parse_verdict(PASS).unwrap();
        assert_eq!(v.verdict, VerdictKind::Pass);
        assert!(v.findings.is_empty());
        assert_eq!(v.confidence, 0.9);
    }

    #[test]
    fn valid_fail_parses_with_finding() {
        let v = parse_verdict(FAIL).unwrap();
        assert_eq!(v.verdict, VerdictKind::Fail);
        assert_eq!(v.findings.len(), 1);
        assert_eq!(v.findings[0].severity, Severity::P1);
        assert_eq!(v.findings[0].line, Some(10));
    }

    #[test]
    fn prose_is_refused_never_scraped() {
        let err = parse_verdict("The code looks good, verdict: pass").unwrap_err();
        assert!(matches!(err, VerdictError::NotJson(_)), "{err}");
        // Even prose CONTAINING valid JSON is refused — no extraction.
        let sneaky = "looks fine {\"verdict\":\"pass\",\"findings\":[],\"confidence\":1.0}";
        assert!(matches!(
            parse_verdict(sneaky).unwrap_err(),
            VerdictError::NotJson(_)
        ));
    }

    #[test]
    fn empty_input_refused() {
        assert_eq!(parse_verdict("").unwrap_err(), VerdictError::Empty);
        assert_eq!(parse_verdict("   \n ").unwrap_err(), VerdictError::Empty);
    }

    #[test]
    fn missing_keys_named() {
        assert_eq!(
            parse_verdict(r#"{"findings":[],"confidence":1.0}"#).unwrap_err(),
            VerdictError::MissingField("verdict")
        );
        assert_eq!(
            parse_verdict(r#"{"verdict":"pass","confidence":1.0}"#).unwrap_err(),
            VerdictError::MissingField("findings")
        );
        assert_eq!(
            parse_verdict(r#"{"verdict":"pass","findings":[]}"#).unwrap_err(),
            VerdictError::MissingField("confidence")
        );
    }

    #[test]
    fn bad_variants_name_the_value() {
        let err =
            parse_verdict(r#"{"verdict":"maybe","findings":[],"confidence":0.5}"#).unwrap_err();
        assert_eq!(err, VerdictError::BadVariant("\"maybe\"".to_string()));
        let err = parse_verdict(
            r#"{"verdict":"fail","findings":[{"severity":"P9","file":"a","issue":"i","proposed_fix":"f"}],"confidence":0.5}"#,
        )
        .unwrap_err();
        assert_eq!(
            err,
            VerdictError::BadSeverity {
                index: 0,
                got: "P9".to_string()
            }
        );
    }

    #[test]
    fn bad_confidence_refused() {
        for got in ["2.0", "-0.1", "\"high\""] {
            let text = format!(r#"{{"verdict":"pass","findings":[],"confidence":{got}}}"#);
            assert!(
                matches!(
                    parse_verdict(&text).unwrap_err(),
                    VerdictError::BadConfidence(_)
                ),
                "{got}"
            );
        }
    }

    #[test]
    fn fail_without_findings_refused() {
        let err =
            parse_verdict(r#"{"verdict":"fail","findings":[],"confidence":0.5}"#).unwrap_err();
        assert_eq!(err, VerdictError::FailWithoutFindings);
    }

    #[test]
    fn blank_finding_fields_named() {
        let text = r#"{"verdict":"fail","findings":[{"severity":"P2","file":"","issue":"i","proposed_fix":"f"}],"confidence":0.5}"#;
        assert_eq!(
            parse_verdict(text).unwrap_err(),
            VerdictError::EmptyFinding {
                index: 0,
                field: "file"
            }
        );
    }

    #[test]
    fn malformed_shapes_name_their_fault() {
        // Top-level array.
        assert!(matches!(
            parse_verdict(r#"[]"#).unwrap_err(),
            VerdictError::NotJson(_)
        ));
        // Non-object finding.
        assert!(matches!(
            parse_verdict(r#"{"verdict":"fail","findings":[42],"confidence":0.5}"#).unwrap_err(),
            VerdictError::BadType { .. }
        ));
        // Missing severity.
        assert_eq!(
            parse_verdict(
                r#"{"verdict":"fail","findings":[{"file":"a","issue":"i","proposed_fix":"f"}],"confidence":0.5}"#
            )
            .unwrap_err(),
            VerdictError::BadSeverity {
                index: 0,
                got: "<missing>".to_string()
            }
        );
        // Null line is fine (unknown location).
        let v = parse_verdict(
            r#"{"verdict":"fail","findings":[{"severity":"P3","file":"a","line":null,"issue":"i","proposed_fix":"f"}],"confidence":0.5}"#,
        )
        .unwrap();
        assert_eq!(v.findings[0].line, None);
    }

    fn with_line(line: &str) -> Finding {
        let text = format!(
            r#"{{"verdict":"fail","findings":[{{"severity":"P1","file":"a.py","line":{line},"issue":"wrong greeting","proposed_fix":"f"}}],"confidence":0.5}}"#
        );
        parse_verdict(&text)
            .unwrap_or_else(|e| panic!("{line}: {e}"))
            .findings
            .remove(0)
    }

    #[test]
    fn line_reference_forms_parse_to_first_number() {
        for (raw, want) in [
            ("12", 12),
            (r#""12""#, 12),
            (r#"" 12 ""#, 12),
            (r#""12-15""#, 12),
            (r#""12 – 15""#, 12),
            (r#""L12""#, 12),
            (r#""L12-L15""#, 12),
            (r#""line 7""#, 7),
            (r#""Lines 3-4""#, 3),
            (r#""12:5""#, 12),
            ("12.0", 12),
        ] {
            let f = with_line(raw);
            assert_eq!(f.line, Some(want), "{raw}");
            assert_eq!(f.issue, "wrong greeting", "{raw}");
        }
    }

    /// Live drive: a quoted code line in `line` voided the whole verdict.
    /// Now it is an unknown location and the quote survives in the issue.
    #[test]
    fn non_location_line_is_none_and_quote_kept() {
        let f = with_line(r#""    return \"Hello \" + name""#);
        assert_eq!(f.line, None);
        assert_eq!(f.issue, "wrong greeting (quoted: return \"Hello \" + name)");
        // Digits inside code are not a location.
        let f = with_line(r#""x = 1 + 2""#);
        assert_eq!(f.line, None);
        assert!(f.issue.ends_with("(quoted: x = 1 + 2)"), "{}", f.issue);
        // A quote the issue already carries is not repeated.
        assert_eq!(with_line(r#""wrong greeting""#).issue, "wrong greeting");
        // Long quotes are bounded.
        let long = format!(r#""{}""#, "y".repeat(500));
        let f = with_line(&long);
        assert_eq!(f.line, None);
        assert!(f.issue.ends_with("\u{2026})"), "{}", f.issue);
        assert!(f.issue.chars().count() < 260);
        // Non-string, non-u32 shapes: unknown location, issue untouched.
        for raw in ["4294967296", "-3", "1.5", "true", "[12]", "{}", r#""""#] {
            let f = with_line(raw);
            assert_eq!(f.line, None, "{raw}");
            assert_eq!(f.issue, "wrong greeting", "{raw}");
        }
    }

    #[test]
    fn casing_and_numeric_strings_tolerated() {
        let v = parse_verdict(
            r#"{"verdict":" FAIL ","findings":[{"severity":"p2","file":"a","issue":"i","proposed_fix":"f"}],"confidence":"0.75"}"#,
        )
        .unwrap();
        assert_eq!(v.verdict, VerdictKind::Fail);
        assert_eq!(v.findings[0].severity, Severity::P2);
        assert_eq!(v.confidence, 0.75);
        assert_eq!(
            parse_verdict(r#"{"verdict":"Pass","findings":[],"confidence":1}"#)
                .unwrap()
                .verdict,
            VerdictKind::Pass
        );
    }

    /// Leniency stops at values: clearly invalid verdicts stay invalid.
    #[test]
    fn lenient_fields_do_not_rescue_invalid_verdicts() {
        // A bad line never masks another fault.
        assert_eq!(
            parse_verdict(
                r#"{"verdict":"fail","findings":[{"severity":"P1","file":"","line":"x","issue":"i","proposed_fix":"f"}],"confidence":0.5}"#
            )
            .unwrap_err(),
            VerdictError::EmptyFinding {
                index: 0,
                field: "file"
            }
        );
        assert!(matches!(
            parse_verdict(r#"{"verdict":"passed","findings":[],"confidence":0.5}"#).unwrap_err(),
            VerdictError::BadVariant(_)
        ));
        assert!(matches!(
            parse_verdict(
                r#"{"verdict":"fail","findings":[{"severity":"high","file":"a","line":"3","issue":"i","proposed_fix":"f"}],"confidence":0.5}"#
            )
            .unwrap_err(),
            VerdictError::BadSeverity { .. }
        ));
        assert!(matches!(
            parse_verdict(r#"{"verdict":"pass","findings":[],"confidence":"1.5"}"#).unwrap_err(),
            VerdictError::BadConfidence(_)
        ));
        assert_eq!(
            parse_verdict(r#"{"verdict":"fail","findings":[],"confidence":"0.5"}"#).unwrap_err(),
            VerdictError::FailWithoutFindings
        );
    }

    #[test]
    fn core_verdict_json_round_trips_through_parser() {
        // What core serializes, the boundary accepts (single shape, §2.6).
        let v = Verdict {
            verdict: VerdictKind::Fail,
            findings: vec![Finding {
                severity: Severity::P0,
                file: "x.rs".into(),
                line: None,
                issue: "i".into(),
                proposed_fix: "f".into(),
            }],
            confidence: 0.7,
        };
        let s = serde_json::to_string(&v).unwrap();
        assert_eq!(parse_verdict(&s).unwrap(), v);
    }
}
