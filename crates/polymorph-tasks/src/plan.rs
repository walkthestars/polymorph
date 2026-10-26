//! PLAN.md artifact.
//!
//! The human- and model-readable plan file: the planner writes it, the
//! reviewer reviews it, the executor rewrites it (observable, editable,
//! versioned, never a black-box transcript). Render and
//! parse are strict inverses: `parse(render(g)) == g` is tested, and parse
//! faults name the line (precedent — corrupt files are hard errors).
//! [`apply_section`] writes executor rewrite fragments back
//! into the graph under the same strictness.

use crate::graph::{Task, TaskGraph, TaskStatus};

/// PLAN.md parse failures. Every variant names the line at fault.
#[derive(Debug, PartialEq, thiserror::Error)]
pub enum PlanError {
    #[error("line {line}: {msg}")]
    Parse { line: usize, msg: String },
}

fn bad(line: usize, msg: &str) -> PlanError {
    PlanError::Parse {
        line,
        msg: msg.to_string(),
    }
}

/// Render a whole graph as PLAN.md. Title is caller-owned (goal text);
/// statuses ride as `Status:` lines so resume restores them exactly.
pub fn render(title: &str, graph: &TaskGraph) -> String {
    let mut out = format!("# Plan: {title}\n");
    for task in graph.ordered() {
        out.push_str(&render_section(task));
    }
    out
}

/// Render one task's section (the per-task review artifact the
/// driver shows the reviewer — same bytes as in the full file).
pub fn render_section(task: &Task) -> String {
    let mut out = format!("\n## [{}] {}\n", task.id, task.title);
    out.push_str(&format!("Status: {}\n", task.status.as_str()));
    if task.deps.is_empty() {
        out.push_str("Deps: none\n");
    } else {
        out.push_str(&format!("Deps: {}\n", task.deps.join(", ")));
    }
    out.push_str("Criteria:\n");
    for line in task.criteria.lines() {
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Parse PLAN.md back into a graph. Sections in any file order (forward refs
/// allowed — validation runs whole-graph at the end); duplicate
/// sections, unknown statuses, and malformed headers are hard errors naming
/// the line. A missing title is refused, never defaulted.
pub fn parse(text: &str) -> Result<(String, TaskGraph), PlanError> {
    let (title, sections) = split_sections(text)?;
    let title = title.ok_or_else(|| bad(0, "missing `# Plan:` title"))?;
    if sections.is_empty() {
        return Err(bad(0, "no `## [id]` sections"));
    }
    let mut graph = TaskGraph::new();
    // First pass: tasks with no edges (file order may forward-reference).
    for sec in &sections {
        if graph.get(&sec.id).is_some() {
            return Err(bad(sec.line, &format!("duplicate section [{}]", sec.id)));
        }
        let status = section_status(sec)?;
        let mut task = Task::new(sec.id.clone(), sec.name.clone(), String::new(), Vec::new());
        task.status = status;
        // Bypass add_task (it refuses forward deps); edges land in pass two
        // and whole-graph validate() judges them.
        graph.push_unchecked(task);
    }
    for sec in &sections {
        let deps = sec.deps.clone().unwrap_or_default();
        let task = graph.get_mut(&sec.id).expect("just pushed");
        task.deps = deps;
        task.criteria = sec.criteria.join("\n");
        if !task.criteria.is_empty() {
            task.criteria.push('\n');
        }
    }
    graph.validate().map_err(|e| bad(0, &e.to_string()))?;
    Ok((title, graph))
}

/// Write one executor rewrite fragment back into the graph.
/// Strict contract, atomic outcome:
/// - The fragment must hold EXACTLY one section and it must be this task's
///   (extra sections = hallucinated scope, refused naming the extra id).
/// - Title/criteria/deps apply; the Status line is validated but NEVER
///   applied (the verdict owns lifecycle — a rewrite cannot mark Done).
/// - Empty dep entries are refused (model output is held stricter than
///   human files, stated).
/// - The patched graph re-validates whole (cycles/unknown deps from the
///   rewrite roll back); failures keep the old task byte-identical and the
///   caller marks the task Failed.
pub fn apply_section(
    graph: &mut TaskGraph,
    task_id: &str,
    fragment: &str,
) -> Result<(), PlanError> {
    graph
        .get(task_id)
        .ok_or_else(|| bad(0, &format!("unknown task [{task_id}]")))?;
    let (_, sections) = split_sections(fragment)?;
    if sections.is_empty() {
        return Err(bad(0, &format!("fragment holds no [{task_id}] section")));
    }
    if sections.len() > 1 {
        let extra = sections
            .iter()
            .find(|s| s.id != task_id)
            .map(|s| s.id.clone())
            .unwrap_or_else(|| sections[0].id.clone());
        return Err(bad(0, &format!("fragment holds extra section [{extra}]")));
    }
    let sec = &sections[0];
    if sec.id != task_id {
        return Err(bad(
            sec.line,
            &format!("fragment holds [{}], want [{task_id}]", sec.id),
        ));
    }
    // Status vocabulary checked, value dropped (verdict owns lifecycle).
    let _ = section_status(sec)?;
    for dep in sec.deps.clone().unwrap_or_default() {
        if dep.trim().is_empty() {
            return Err(bad(sec.line, "empty dep entry"));
        }
    }
    // Atomic: patch a clone, validate whole, commit only on success.
    let mut next = graph.clone();
    let task = next.get_mut(task_id).expect("validated present above");
    task.title = sec.name.clone();
    task.deps = sec.deps.clone().unwrap_or_default();
    task.criteria = sec.criteria.join("\n");
    if !task.criteria.is_empty() {
        task.criteria.push('\n');
    }
    next.validate()
        .map_err(|e| bad(0, &format!("task [{task_id}] rewrite invalid: {e}")))?;
    *graph = next;
    Ok(())
}

fn section_status(sec: &RawSection) -> Result<TaskStatus, PlanError> {
    match &sec.status {
        Some((ln, word)) => TaskStatus::parse(word).map_err(|_| {
            bad(
                *ln,
                &format!("unknown status {word:?} (want pending|running|done|failed)"),
            )
        }),
        None => Ok(TaskStatus::Pending),
    }
}

/// Line loop shared by parse() and apply_section(): shape in, raw sections
/// out, whole-graph validation later (the loop judges shape,
/// never graph).
fn split_sections(text: &str) -> Result<(Option<String>, Vec<RawSection>), PlanError> {
    let mut title: Option<String> = None;
    // Raw sections first (file order), validation whole-graph at the end.
    let mut sections: Vec<RawSection> = Vec::new();
    let mut current: Option<RawSection> = None;
    let mut in_criteria = false;
    for (i, raw) in text.lines().enumerate() {
        let line = i + 1;
        let trimmed = raw.trim();
        if let Some(t) = trimmed.strip_prefix("# Plan:") {
            if title.is_some() {
                return Err(bad(line, "duplicate plan title"));
            }
            let t = t.trim().to_string();
            if t.is_empty() {
                return Err(bad(line, "empty plan title"));
            }
            title = Some(t);
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("## [") {
            if let Some(mut cur) = current.take() {
                cur.trim_trailing_blanks();
                sections.push(cur);
            }
            let end = rest
                .find(']')
                .ok_or_else(|| bad(line, "malformed section header (want `## [id] title`)"))?;
            let id = rest[..end].trim().to_string();
            if id.is_empty() {
                return Err(bad(line, "empty section id"));
            }
            let name = rest[end + 1..].trim().to_string();
            if name.is_empty() {
                return Err(bad(line, "empty section title"));
            }
            current = Some(RawSection::new(line, id, name));
            in_criteria = false;
            continue;
        }
        let Some(cur) = current.as_mut() else {
            if trimmed.is_empty() {
                continue;
            }
            // A `##`-looking line that failed header parse is malformed
            // (never silently skipped — the section it meant is missing).
            if trimmed.starts_with("##") {
                return Err(bad(line, "malformed section header (want `## [id] title`)"));
            }
            if trimmed.starts_with('#') {
                continue;
            }
            return Err(bad(line, "content outside any `## [id]` section"));
        };
        if let Some(rest) = trimmed.strip_prefix("Status:") {
            if in_criteria {
                cur.criteria.push(raw.to_string());
                continue;
            }
            if cur.status.is_some() {
                return Err(bad(line, "duplicate Status line"));
            }
            cur.status = Some((line, rest.trim().to_string()));
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("Deps:") {
            if in_criteria {
                cur.criteria.push(raw.to_string());
                continue;
            }
            if cur.deps.is_some() {
                return Err(bad(line, "duplicate Deps line"));
            }
            let rest = rest.trim();
            cur.deps = Some(if rest == "none" || rest.is_empty() {
                Vec::new()
            } else {
                rest.split(',').map(|s| s.trim().to_string()).collect()
            });
            continue;
        }
        if trimmed == "Criteria:" && !in_criteria {
            in_criteria = true;
            continue;
        }
        if in_criteria {
            cur.criteria.push(raw.to_string());
        } else if !trimmed.is_empty() {
            return Err(bad(line, "expected Status:/Deps:/Criteria: line"));
        }
    }
    if let Some(mut cur) = current.take() {
        cur.trim_trailing_blanks();
        sections.push(cur);
    }
    Ok((title, sections))
}

struct RawSection {
    line: usize,
    id: String,
    name: String,
    status: Option<(usize, String)>,
    deps: Option<Vec<String>>,
    criteria: Vec<String>,
}

impl RawSection {
    fn new(line: usize, id: String, name: String) -> Self {
        Self {
            line,
            id,
            name,
            status: None,
            deps: None,
            criteria: Vec::new(),
        }
    }

    /// Drop separator blank lines absorbed while collecting criteria, so
    /// `parse(render(g)) == g` holds byte-exact. A criteria that genuinely
    /// ends in blank lines normalizes (whitespace-only loss, documented).
    fn trim_trailing_blanks(&mut self) {
        while self.criteria.last().is_some_and(|l| l.trim().is_empty()) {
            self.criteria.pop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::TaskStatus;

    fn sample() -> (String, TaskGraph) {
        let mut g = TaskGraph::new();
        let mut a = Task::new("a", "First", "ship it\n", vec![]);
        a.status = TaskStatus::Done;
        g.add_task(a).unwrap();
        g.add_task(Task::new("b", "Second", "multi\nline\n", vec!["a".into()]))
            .unwrap();
        ("demo goal".into(), g)
    }

    #[test]
    fn render_parse_round_trips() {
        let (title, g) = sample();
        let text = render(&title, &g);
        let (title2, g2) = parse(&text).unwrap();
        assert_eq!(title2, title);
        assert_eq!(g2, g);
        // Statuses survive the file (resume restores them exactly).
        assert_eq!(g2.get("a").unwrap().status, TaskStatus::Done);
        assert_eq!(g2.get("b").unwrap().deps, vec!["a".to_string()]);
        assert_eq!(g2.get("b").unwrap().criteria, "multi\nline\n");
    }

    #[test]
    fn parse_accepts_forward_refs() {
        // File order need not be topological — validation is whole-graph.
        let text = "# Plan: t\n\n## [b] Second\nDeps: a\nCriteria:\nx\n\n## [a] First\nDeps: none\nCriteria:\ny\n";
        let (_, g) = parse(text).unwrap();
        assert_eq!(g.topo_order().unwrap(), vec!["a", "b"]);
    }

    #[test]
    fn parse_faults_name_the_line() {
        // Missing title.
        let err = parse("## [a] A\nDeps: none\n").unwrap_err().to_string();
        assert!(err.contains("title"), "{err}");
        // Unknown status word, line named.
        let err = parse("# Plan: t\n## [a] A\nStatus: someday\nDeps: none\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("line 3"), "{err}");
        assert!(err.contains("someday"), "{err}");
        // Duplicate section — the OFFENDING (second) line is named.
        let err = parse("# Plan: t\n## [a] A\nDeps: none\n## [a] A2\nDeps: none\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("line 4"), "{err}");
        // Malformed header.
        let err = parse("# Plan: t\n## a] A\n").unwrap_err().to_string();
        assert!(err.contains("line 2"), "{err}");
        // Cycle surfaces whole-graph (line 0 = file-level verdict).
        let err = parse("# Plan: t\n## [a] A\nDeps: b\nCriteria:\n## [b] B\nDeps: a\nCriteria:\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("cycle"), "{err}");
        // Empty file.
        assert!(parse("").is_err());
    }

    #[test]
    fn apply_section_patches_title_criteria_deps_not_status() {
        let (_, mut g) = sample();
        // Fragment carries Status: done for a Pending task — vocabulary
        // checked, value dropped (the verdict owns lifecycle).
        let fragment = "## [b] Second, revised\nStatus: done\nDeps: a\nCriteria:\nnew words\n";
        apply_section(&mut g, "b", fragment).unwrap();
        let b = g.get("b").unwrap();
        assert_eq!(b.title, "Second, revised");
        assert_eq!(b.criteria, "new words\n");
        assert_eq!(b.deps, vec!["a".to_string()]);
        assert_eq!(b.status, TaskStatus::Pending);
        assert!(g.validate().is_ok());
    }

    #[test]
    fn apply_section_refuses_and_keeps_old_bytes() {
        let (_, mut g) = sample();
        let before = g.clone();
        // Wrong id.
        let err = apply_section(&mut g, "b", "## [z] Other\nDeps: none\nCriteria:\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("[z]") && err.contains("[b]"), "{err}");
        // Extra section (hallucinated scope).
        let err = apply_section(
            &mut g,
            "b",
            "## [b] B\nDeps: a\nCriteria:\n## [c] C\nDeps: none\nCriteria:\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("[c]"), "{err}");
        // Empty fragment.
        assert!(apply_section(&mut g, "b", "nothing here\n").is_err());
        // Unknown task.
        assert!(apply_section(&mut g, "ghost", "## [ghost] G\nDeps: none\nCriteria:\n").is_err());
        // Bad status word.
        let err = apply_section(
            &mut g,
            "b",
            "## [b] B\nStatus: someday\nDeps: a\nCriteria:\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("someday"), "{err}");
        // Deps creating a cycle (b already depended-on by nobody; make a
        // depend on b instead: patch a to dep on b while b deps on a).
        let mut g2 = TaskGraph::new();
        g2.add_task(Task::new("a", "A", "c", vec![])).unwrap();
        g2.add_task(Task::new("b", "B", "c", vec!["a".into()]))
            .unwrap();
        let err = apply_section(&mut g2, "a", "## [a] A\nDeps: b\nCriteria:\nc\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("cycle"), "{err}");
        assert_eq!(g2.get("a").unwrap().deps, Vec::<String>::new());
        // Untouched throughout (atomicity — every refusal above kept bytes).
        assert_eq!(g, before);
    }

    #[test]
    fn pack_example_applies_clean_prose_refused() {
        // Pack↔applier agreement: the `plan.md` example shape
        // must apply (or the pack teaches a shape the applier refuses),
        // and the prose failure must still refuse.
        let mut g = TaskGraph::new();
        g.add_task(Task::new("a", "First", "old\n", vec![]))
            .unwrap();
        let example =
            "## [a] First step\nStatus: pending\nDeps: none\nCriteria:\nship it, tested\n";
        apply_section(&mut g, "a", example).unwrap();
        assert_eq!(g.get("a").unwrap().title, "First step");
        assert_eq!(g.get("a").unwrap().criteria, "ship it, tested\n");
        // Live refusal shape: prose without a section header.
        assert!(apply_section(&mut g, "a", "looks good, ship it\n").is_err());
        // Fenced section is prose to the applier (no bare `## [id]` line).
        assert!(apply_section(&mut g, "a", "```\n## [a] X\n```\n").is_err());
    }
}
