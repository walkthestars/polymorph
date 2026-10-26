//! Task model + dependency DAG.
//!
//! The plan-mode substrate the reviewer loop drives: tasks with dependency
//! edges, topological ready-order, and cycle rejection. Validation lives at
//! the boundary (`parse`/`load` re-validate everything from disk — never
//! trust the file), while [`TaskGraph::add_task`] refuses only what it can
//! judge locally (duplicates, empty/self/unknown deps). Forward references
//! are impossible through `add_task` (deps must already exist), so cycle
//! detection belongs to the whole-graph validators class:
//! validation in the validation phase, never mid-write.

use std::collections::{BTreeMap, VecDeque};

/// Lifecycle of one plan task. `Done` is terminal: resume never resurrects
/// completed work (a completed task that needs redo is a NEW task).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    Pending,
    Running,
    Done,
    Failed,
}

impl TaskStatus {
    /// Strict vocabulary (verdict-boundary precedent): unknown words are
    /// refused with the valid set named, never guessed.
    pub fn parse(word: &str) -> Result<TaskStatus, GraphError> {
        match word {
            "pending" => Ok(TaskStatus::Pending),
            "running" => Ok(TaskStatus::Running),
            "done" => Ok(TaskStatus::Done),
            "failed" => Ok(TaskStatus::Failed),
            _ => Err(GraphError::BadStatus {
                word: word.to_string(),
            }),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            TaskStatus::Pending => "pending",
            TaskStatus::Running => "running",
            TaskStatus::Done => "done",
            TaskStatus::Failed => "failed",
        }
    }
}

/// One plan task: goal + acceptance criteria + lifecycle + dependencies.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Task {
    pub id: String,
    pub title: String,
    pub criteria: String,
    pub status: TaskStatus,
    pub deps: Vec<String>,
}

impl Task {
    pub fn new(
        id: impl Into<String>,
        title: impl Into<String>,
        criteria: impl Into<String>,
        deps: Vec<String>,
    ) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            criteria: criteria.into(),
            status: TaskStatus::Pending,
            deps,
        }
    }
}

/// Whole-graph failures. Typed, never sniffed: every variant names the task
/// (and dep) at fault.
#[derive(Debug, PartialEq, thiserror::Error)]
pub enum GraphError {
    #[error("task id must not be empty")]
    EmptyId,
    #[error("duplicate task id {id:?}")]
    Duplicate { id: String },
    #[error("task {id:?} depends on itself")]
    SelfDep { id: String },
    #[error("task {id:?} depends on unknown task {dep:?}")]
    UnknownDep { id: String, dep: String },
    #[error("dependency cycle: {path}")]
    Cycle { path: String },
    #[error("unknown task id {id:?}")]
    UnknownTask { id: String },
    #[error("task {id:?} is Done and cannot move to {to:?}")]
    Terminal { id: String, to: String },
    #[error("unknown status word {word:?} (want pending|running|done|failed)")]
    BadStatus { word: String },
}

/// An ordered task DAG. Insertion order is the stable tiebreak everywhere
/// (boards and files stay diff-stable across runs).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TaskGraph {
    tasks: Vec<Task>,
}

impl TaskGraph {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.tasks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    pub fn get(&self, id: &str) -> Option<&Task> {
        self.tasks.iter().find(|t| t.id == id)
    }

    pub(crate) fn get_mut(&mut self, id: &str) -> Option<&mut Task> {
        self.tasks.iter_mut().find(|t| t.id == id)
    }

    /// Insertion-order view (stable rendering and boards).
    pub fn ordered(&self) -> &[Task] {
        &self.tasks
    }

    /// Push without local validation — `plan::parse` ONLY, which runs
    /// whole-graph [`Self::validate`] before returning (forward refs from
    /// files cannot survive `add_task`). Never use elsewhere.
    pub(crate) fn push_unchecked(&mut self, task: Task) {
        self.tasks.push(task);
    }

    fn pos(&self, id: &str) -> Option<usize> {
        self.tasks.iter().position(|t| t.id == id)
    }

    /// Add one task. Refuses empty ids, duplicates, self-deps, and deps on
    /// tasks not yet present (fail-fast at write precedent). Cycle
    /// detection is NOT here: with unknown deps refused, no `add_task` call
    /// can close a cycle, so a check would be unreachable code — cycles are
    /// caught by [`Self::validate`], which the parse/load boundaries run.
    pub fn add_task(&mut self, task: Task) -> Result<(), GraphError> {
        if task.id.trim().is_empty() {
            return Err(GraphError::EmptyId);
        }
        if self.pos(&task.id).is_some() {
            return Err(GraphError::Duplicate { id: task.id });
        }
        if task.deps.iter().any(|d| d == &task.id) {
            return Err(GraphError::SelfDep { id: task.id });
        }
        for dep in &task.deps {
            if self.pos(dep).is_none() {
                return Err(GraphError::UnknownDep {
                    id: task.id.clone(),
                    dep: dep.clone(),
                });
            }
        }
        self.tasks.push(task);
        Ok(())
    }

    /// Move a task's lifecycle. `Done` is terminal (resume-safe); `Failed`
    /// may retry to `Pending`/`Running`.
    pub fn set_status(&mut self, id: &str, to: TaskStatus) -> Result<(), GraphError> {
        let i = self
            .pos(id)
            .ok_or_else(|| GraphError::UnknownTask { id: id.to_string() })?;
        if self.tasks[i].status == TaskStatus::Done && to != TaskStatus::Done {
            return Err(GraphError::Terminal {
                id: id.to_string(),
                to: to.as_str().to_string(),
            });
        }
        self.tasks[i].status = to;
        Ok(())
    }

    /// Whole-graph validation (parse/load/topo boundaries): every dep known
    /// (forward refs from files) and the graph acyclic. First fault wins,
    /// named.
    pub fn validate(&self) -> Result<(), GraphError> {
        for task in &self.tasks {
            for dep in &task.deps {
                if dep == &task.id {
                    return Err(GraphError::SelfDep {
                        id: task.id.clone(),
                    });
                }
                if self.pos(dep).is_none() {
                    return Err(GraphError::UnknownDep {
                        id: task.id.clone(),
                        dep: dep.clone(),
                    });
                }
            }
        }
        self.find_cycle()?;
        Ok(())
    }

    /// Depth-first cycle search. Returns the cycle as `a -> b -> a`.
    fn find_cycle(&self) -> Result<(), GraphError> {
        // 0 = unvisited, 1 = on stack, 2 = finished.
        let mut mark = vec![0u8; self.tasks.len()];
        let mut stack: Vec<usize> = Vec::new();
        for root in 0..self.tasks.len() {
            if mark[root] != 0 {
                continue;
            }
            let mut work: Vec<(usize, usize)> = vec![(root, 0)];
            while let Some((node, next_dep)) = work.pop() {
                if next_dep == 0 {
                    if mark[node] == 2 {
                        continue;
                    }
                    mark[node] = 1;
                    stack.push(node);
                }
                let deps = self.tasks[node].deps.clone();
                if next_dep < deps.len() {
                    work.push((node, next_dep + 1));
                    let dep = &deps[next_dep];
                    if let Some(di) = self.pos(dep) {
                        if mark[di] == 1 {
                            let start = stack.iter().position(|&s| s == di).unwrap_or(0);
                            let mut path: Vec<String> = stack[start..]
                                .iter()
                                .map(|&s| self.tasks[s].id.clone())
                                .collect();
                            path.push(self.tasks[di].id.clone());
                            return Err(GraphError::Cycle {
                                path: path.join(" -> "),
                            });
                        }
                        if mark[di] == 0 {
                            work.push((di, 0));
                        }
                    }
                } else {
                    mark[node] = 2;
                    stack.pop();
                }
            }
        }
        Ok(())
    }

    /// Tasks ready to run now: `Pending` with every dep `Done`, in insertion
    /// order. The scheduler's only query — reviewers and executors never
    /// walk edges themselves.
    pub fn ready(&self) -> Vec<&Task> {
        self.tasks
            .iter()
            .filter(|t| {
                t.status == TaskStatus::Pending
                    && t.deps
                        .iter()
                        .all(|d| self.get(d).is_some_and(|g| g.status == TaskStatus::Done))
            })
            .collect()
    }

    /// Full topological order (dependency order — the merge-back
    /// rule for multi-child merges). Insertion order breaks ties.
    pub fn topo_order(&self) -> Result<Vec<String>, GraphError> {
        self.validate()?;
        let mut indeg: BTreeMap<&str, usize> = BTreeMap::new();
        for task in &self.tasks {
            indeg.insert(task.id.as_str(), task.deps.len());
        }
        let mut queue: VecDeque<&str> = self
            .tasks
            .iter()
            .filter(|t| t.deps.is_empty())
            .map(|t| t.id.as_str())
            .collect();
        let mut order = Vec::with_capacity(self.tasks.len());
        while let Some(id) = queue.pop_front() {
            order.push(id.to_string());
            for task in &self.tasks {
                if task.deps.iter().any(|d| d == id) {
                    let e = indeg.get_mut(task.id.as_str()).unwrap();
                    *e -= 1;
                    if *e == 0 {
                        queue.push_back(task.id.as_str());
                    }
                }
            }
        }
        Ok(order)
    }

    /// Order a completed-task subset for merging (dependency
    /// order, caller stops at the first conflict — no partial merges).
    /// Unknown ids are refused (fail-fast — silently dropping a branch
    /// would lose work).
    pub fn merge_order(&self, ids: &[String]) -> Result<Vec<String>, GraphError> {
        for id in ids {
            if self.pos(id).is_none() {
                return Err(GraphError::UnknownTask { id: id.clone() });
            }
        }
        let order = self.topo_order()?;
        Ok(order.into_iter().filter(|id| ids.contains(id)).collect())
    }

    /// Pause: serialize the whole run state (tasks + statuses + edges).
    pub fn save_json(&self, path: &std::path::Path) -> anyhow::Result<()> {
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(path, text)
            .map_err(|e| anyhow::anyhow!("cannot write {}: {e}", path.display()))?;
        Ok(())
    }

    /// Resume: load + re-validate (never trust disk — recording-loader
    /// precedent: corrupt files are hard errors, never partial graphs).
    pub fn load_json(path: &std::path::Path) -> anyhow::Result<TaskGraph> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", path.display()))?;
        let graph: TaskGraph = serde_json::from_str(&text)
            .map_err(|e| anyhow::anyhow!("checkpoint {}: {e}", path.display()))?;
        graph
            .validate()
            .map_err(|e| anyhow::anyhow!("checkpoint {}: {e}", path.display()))?;
        Ok(graph)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str, deps: &[&str]) -> Task {
        Task::new(
            id,
            format!("title {id}"),
            format!("criteria {id}"),
            deps.iter().map(|s| s.to_string()).collect(),
        )
    }

    #[test]
    fn chain_runs_ready_in_order() {
        let mut g = TaskGraph::new();
        g.add_task(task("a", &[])).unwrap();
        g.add_task(task("b", &["a"])).unwrap();
        g.add_task(task("c", &["b"])).unwrap();
        assert_eq!(ids(&g.ready()), vec!["a"]);
        g.set_status("a", TaskStatus::Done).unwrap();
        assert_eq!(ids(&g.ready()), vec!["b"]);
        g.set_status("b", TaskStatus::Done).unwrap();
        assert_eq!(ids(&g.ready()), vec!["c"]);
        assert!(g.validate().is_ok());
    }

    #[test]
    fn add_refuses_local_faults() {
        let mut g = TaskGraph::new();
        assert_eq!(g.add_task(task("", &[])).unwrap_err(), GraphError::EmptyId);
        g.add_task(task("a", &[])).unwrap();
        assert_eq!(
            g.add_task(task("a", &[])).unwrap_err(),
            GraphError::Duplicate { id: "a".into() }
        );
        assert_eq!(
            g.add_task(task("b", &["b"])).unwrap_err(),
            GraphError::SelfDep { id: "b".into() }
        );
        assert_eq!(
            g.add_task(task("c", &["ghost"])).unwrap_err(),
            GraphError::UnknownDep {
                id: "c".into(),
                dep: "ghost".into()
            }
        );
        // Unknown task on status move is equally loud.
        assert_eq!(
            g.set_status("ghost", TaskStatus::Done).unwrap_err(),
            GraphError::UnknownTask { id: "ghost".into() }
        );
    }

    #[test]
    fn done_is_terminal_but_failed_may_retry() {
        let mut g = TaskGraph::new();
        g.add_task(task("a", &[])).unwrap();
        g.add_task(task("b", &[])).unwrap();
        g.set_status("a", TaskStatus::Done).unwrap();
        assert_eq!(
            g.set_status("a", TaskStatus::Running).unwrap_err(),
            GraphError::Terminal {
                id: "a".into(),
                to: "running".into()
            }
        );
        // Done-to-Done is a no-op, not an error.
        g.set_status("a", TaskStatus::Done).unwrap();
        g.set_status("b", TaskStatus::Failed).unwrap();
        g.set_status("b", TaskStatus::Pending).unwrap();
        assert_eq!(g.get("b").unwrap().status, TaskStatus::Pending);
    }

    #[test]
    fn ready_skips_non_pending() {
        let mut g = TaskGraph::new();
        g.add_task(task("a", &[])).unwrap();
        g.add_task(task("b", &[])).unwrap();
        g.set_status("a", TaskStatus::Running).unwrap();
        g.set_status("b", TaskStatus::Failed).unwrap();
        // Neither Running nor Failed is schedulable — the scheduler sees an
        // empty ready set, never a half-state.
        assert!(g.ready().is_empty());
    }

    #[test]
    fn diamond_toposorts_deterministically() {
        let mut g = TaskGraph::new();
        for (id, deps) in [
            ("a", &[][..]),
            ("b", &["a"][..]),
            ("c", &["a"][..]),
            ("d", &["b", "c"][..]),
        ] {
            g.add_task(task(id, deps)).unwrap();
        }
        let order = g.topo_order().unwrap();
        assert_eq!(order.first().unwrap(), "a");
        assert_eq!(order.last().unwrap(), "d");
        // Insertion order breaks the b/c tie — stable across runs.
        assert_eq!(order, vec!["a", "b", "c", "d"]);
    }

    #[test]
    fn validate_names_cycles() {
        // Forward refs are impossible via add_task, so cycles arrive through
        // whole-graph input (parse/load) — build one by hand here.
        let mut g = TaskGraph::new();
        g.tasks.push(task("a", &["b"]));
        g.tasks.push(task("b", &["a"]));
        let err = g.validate().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("a") && msg.contains("b"), "{msg}");
        assert!(matches!(err, GraphError::Cycle { .. }));
    }

    #[test]
    fn merge_order_sorts_and_refuses_unknown() {
        let mut g = TaskGraph::new();
        for (id, deps) in [
            ("a", &[][..]),
            ("b", &["a"][..]),
            ("c", &["a"][..]),
            ("d", &["b", "c"][..]),
        ] {
            g.add_task(task(id, deps)).unwrap();
        }
        // Reversed input still merges in dependency order.
        let ordered = g
            .merge_order(&["d".to_string(), "b".to_string(), "a".to_string()])
            .unwrap();
        assert_eq!(ordered, vec!["a", "b", "d"]);
        assert!(g.merge_order(&["ghost".to_string()]).is_err());
    }

    #[test]
    fn checkpoint_round_trips_and_revalidates() {
        let dir = tempfile::tempdir().unwrap();
        let mut g = TaskGraph::new();
        g.add_task(task("a", &[])).unwrap();
        g.add_task(task("b", &["a"])).unwrap();
        g.set_status("a", TaskStatus::Done).unwrap();
        g.set_status("b", TaskStatus::Running).unwrap();
        let path = dir.path().join("run.json");
        g.save_json(&path).unwrap();
        let back = TaskGraph::load_json(&path).unwrap();
        assert_eq!(back, g);
        assert_eq!(ids(&back.ready()), Vec::<String>::new());
        // Corrupt checkpoints are hard errors naming the file, never partial
        // graphs — write one with a cycle by hand.
        let bad = dir.path().join("bad.json");
        std::fs::write(
            &bad,
            r#"{"tasks":[
                {"id":"a","title":"t","criteria":"c","status":"pending","deps":["b"]},
                {"id":"b","title":"t","criteria":"c","status":"pending","deps":["a"]}
            ]}"#,
        )
        .unwrap();
        let err = TaskGraph::load_json(&bad).unwrap_err().to_string();
        assert!(err.contains("bad.json"), "{err}");
        assert!(err.contains("cycle"), "{err}");
        // Missing files name the path too.
        let missing = TaskGraph::load_json(&dir.path().join("nope.json")).unwrap_err();
        assert!(missing.to_string().contains("nope.json"));
    }

    fn ids(tasks: &[&Task]) -> Vec<String> {
        tasks.iter().map(|t| t.id.clone()).collect()
    }
}
