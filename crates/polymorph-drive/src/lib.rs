//! Drive machine-seam protocol (S0): the headless half of the
//! fire-and-forget circuit, moved verbatim out of `polymorph-tui` so
//! non-TUI consumers can route and classify without the render stack.
//!
//! Build-intent classification ([`classify_intent`]), NL loop routing
//! ([`classify_loop`]), the model-discriminator vocabulary
//! ([`parse_discriminator`] + [`DISCRIMINATOR_SYSTEM`]), the [`Driver`]
//! contract emitting [`DriveEvent`]s, [`TaskRow`] plan rows, and the
//! status-line [`budget_line`]. Rendering and state application stay in
//! `polymorph-tui` (`App::apply_drive_event`), which re-exports every item
//! here unchanged.

/// What the operator wants, post-gate (the ask-picker confirms Build;
/// denial/headless falls back to Chat — Q fail-closed precedent).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriveIntent {
    PlanFirst {
        goal: String,
    },
    DiveIn {
        goal: String,
    },
    Chat {
        prompt: String,
    },
    /// Review one file through the loop (NL "review <target>").
    ReviewFile {
        target: String,
    },
    /// Drive one plan file per-task (NL "drive PLAN.md").
    PlanFile {
        file: String,
    },
}

/// Human line for an intent (transcript + persistence).
pub fn intent_text(intent: &DriveIntent) -> String {
    match intent {
        DriveIntent::PlanFirst { goal } | DriveIntent::DiveIn { goal } => goal.clone(),
        DriveIntent::Chat { prompt } => prompt.clone(),
        DriveIntent::ReviewFile { target } => format!("review {target}"),
        DriveIntent::PlanFile { file } => format!("drive {file}"),
    }
}

/// Where a natural-language line should execute (NL→loop routing). The
/// TUI types what it wants; the loop runs where it belongs: quick turns
/// stay chat, goals drive, file reviews review, plan files plan-drive.
/// Eval/bench stay CLI-only (seeded corpora, not conversation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopIntent {
    Chat { prompt: String },
    Drive { goal: String },
    Review { target: String },
    PlanDrive { file: String },
}

/// Route one prompt line (case-insensitive, whole-word verbs reuse the
/// `classify_intent` gate — learned intent stays refused, c22 class).
/// Spend-safe by construction: empty review targets and bare keywords
/// fall back to Chat (cheap), never into a billed loop.
pub fn classify_loop(prompt: &str) -> LoopIntent {
    let trimmed = prompt.trim();
    if trimmed.is_empty() {
        return LoopIntent::Chat {
            prompt: prompt.to_string(),
        };
    }
    let lower = trimmed.to_ascii_lowercase();
    // "review <target>" — needs a named file, else it is just chat.
    // The prefix is pure ASCII, so byte-slicing the original is safe.
    // Word boundary required: "reviewer" / "reviewed" stay chat.
    if lower.starts_with("review")
        && lower["review".len()..]
            .chars()
            .next()
            .map(|c| !c.is_alphanumeric())
            .unwrap_or(true)
    {
        let rest = trimmed["review".len().min(trimmed.len())..]
            .trim()
            .trim_start_matches([':', '-', ' '])
            .trim();
        if !rest.is_empty() {
            return LoopIntent::Review {
                target: rest.to_string(),
            };
        }
        return LoopIntent::Chat {
            prompt: prompt.to_string(),
        };
    }
    // Plan files drive per-task: first .md token wins, else PLAN.md.
    if lower.contains("plan.md") || (lower.contains("plan") && lower.contains("drive")) {
        let file = trimmed
            .split_whitespace()
            .find(|t| t.to_ascii_lowercase().ends_with(".md"))
            .unwrap_or("PLAN.md")
            .trim_matches(|c| "\"'`,.:".contains(c))
            .to_string();
        return LoopIntent::PlanDrive { file };
    }
    // Build verbs drive; everything else chats.
    match classify_intent(prompt) {
        IntentClass::Build => LoopIntent::Drive {
            goal: trimmed.to_string(),
        },
        IntentClass::Chat => LoopIntent::Chat {
            prompt: prompt.to_string(),
        },
    }
}

/// Pre-gate classification (D5: dumb-deliberate keyword set, listed here
/// and under test — learned intent is the refused c22 class).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntentClass {
    Build,
    Chat,
}

/// Build verbs + start words (both lists pinned by gate-matrix tests).
const BUILD_VERBS: &[&str] = &[
    "build",
    "implement",
    "create",
    "make",
    "add",
    "write",
    "fix",
    "refactor",
];
const START_WORDS: &[&str] = &["start", "begin", "proceed", "continue", "go"];

/// Classify one prompt line (case-insensitive token match — verbs/starts
/// match whole alphanumeric tokens, not substrings: `good` never fires
/// on `go`, `address` never fires on `add`. Empty/whitespace → Chat
/// (never drive on nothing).
pub fn classify_intent(prompt: &str) -> IntentClass {
    let lower = prompt.to_lowercase();
    if lower.trim().is_empty() {
        return IntentClass::Chat;
    }
    let mut has_verb = false;
    let mut has_start = false;
    for token in lower.split(|c: char| !c.is_alphanumeric()) {
        if token.is_empty() {
            continue;
        }
        if !has_verb && BUILD_VERBS.contains(&token) {
            has_verb = true;
        }
        if !has_start && START_WORDS.contains(&token) {
            has_start = true;
        }
        if has_verb || has_start {
            break;
        }
    }
    if has_verb || has_start {
        IntentClass::Build
    } else {
        IntentClass::Chat
    }
}

/// Model discriminator output: strict `WORD` or `WORD: argument` (first
/// line only). The model picks the loop; this parser enforces the
/// vocabulary — anything else is `None` and the caller falls back to
/// keywords. Spend-safe: REVIEW/PLAN without an argument are `None` (a
/// billed loop with no target is never built from a bare word).
pub fn parse_discriminator(text: &str, prompt: &str) -> Option<LoopIntent> {
    let line = text.lines().next().unwrap_or("").trim();
    let (word, arg) = match line.split_once(':') {
        Some((w, a)) => (w.trim(), a.trim()),
        None => (line, ""),
    };
    match word.to_ascii_uppercase().as_str() {
        "CHAT" => Some(LoopIntent::Chat {
            prompt: prompt.to_string(),
        }),
        "DRIVE" => Some(LoopIntent::Drive {
            goal: prompt.trim().to_string(),
        }),
        "REVIEW" if !arg.is_empty() => Some(LoopIntent::Review {
            target: arg.to_string(),
        }),
        "PLAN" if !arg.is_empty() => Some(LoopIntent::PlanDrive {
            file: arg.to_string(),
        }),
        _ => None,
    }
}

/// System prompt for the discriminator model (smol role): one word out,
/// routing decided here. Deliberately tiny — this runs on every routed
/// line, so tokens are the budget.
pub const DISCRIMINATOR_SYSTEM: &str =
    "Route the operator's line to exactly one word: CHAT, DRIVE, REVIEW, or PLAN.\n\
CHAT = question, explanation, smalltalk, or anything answerable in words.\n\
DRIVE = build, implement, create, fix, or refactor work in the workspace.\n\
REVIEW: <file> = review one named file (only when a file is named).\n\
PLAN: <file> = work through a plan file (only when a .md file is named).\n\
Reply with ONLY the word, or WORD: argument. No prose, no fences.";

/// One plan row for the tasks pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRow {
    pub id: String,
    pub title: String,
    pub status: String,
}

/// One drive-round fact, pre-rendered (backends own content, App owns
/// chrome — the diff-pane precedent, extended to rounds).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriveEvent {
    RoundStarted {
        round: usize,
        max_rounds: usize,
    },
    Verdict {
        round: usize,
        passed: bool,
        summary: String,
        /// `Some(reason)` when the reviewer reply yielded no usable verdict
        /// (an unparseable round; the reason is bounded, one line).
        invalid: Option<String>,
        /// `Some("create"|"start")` when a harness seed answered the round
        /// instead of a reviewer call (docket #7).
        synthesized: Option<String>,
    },
    Finding {
        location: String,
        message: String,
    },
    DiffAvailable {
        status: String,
        stat: String,
    },
    PlanProgress {
        title: String,
        tasks: Vec<TaskRow>,
    },
    Budget {
        rounds_used: usize,
        rounds_max: usize,
        tools_used: usize,
        tools_max: usize,
    },
    Halted {
        reason: String,
    },
    Finished {
        verdict: String,
    },
}

/// The drive contract (139c implements it live over drive_plan +
/// review run; tests use scripted vecs — seam proven before use).
pub trait Driver {
    fn drive(
        &self,
        intent: DriveIntent,
    ) -> impl std::future::Future<Output = anyhow::Result<Vec<DriveEvent>>> + Send;
    /// False = no live loop here (headless/tests): the loop routes NL
    /// loop intents back to chat turns, preserving pre-loop behavior.
    fn available(&self) -> bool {
        true
    }
    /// Model discriminator (smol role): raw routing word for this line.
    /// The loop parses it (`parse_discriminator`); `None` = fall back to
    /// keywords (keyless, unparseable, or error — the router never bills
    /// twice and never strands a line).
    fn discriminate(
        &self,
        _prompt: &str,
    ) -> impl std::future::Future<Output = Option<String>> + Send {
        async { None }
    }
}

/// No-op driver (headless/tests without key or wiring): never available,
/// so every line stays a chat turn exactly as before.
pub struct NoDriver;

impl Driver for NoDriver {
    async fn drive(&self, _intent: DriveIntent) -> anyhow::Result<Vec<DriveEvent>> {
        anyhow::bail!("no live driver wired")
    }
    fn available(&self) -> bool {
        false
    }
}

/// Status-line budget (`round 2/8 · tools 5/32` — D4 always-displayed).
pub fn budget_line(
    rounds_used: usize,
    rounds_max: usize,
    tools_used: usize,
    tools_max: usize,
) -> String {
    format!("round {rounds_used}/{rounds_max} · tools {tools_used}/{tools_max}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_matrix() {
        // Build phrasings (verbs + starts, any case).
        for p in [
            "build me a web server",
            "Implement user auth",
            "CREATE the migration",
            "make it faster",
            "add dark mode",
            "write a parser",
            "fix the login bug",
            "refactor the store",
            "where do we start",
            "let's begin",
            "proceed with the plan",
            "continue building",
        ] {
            assert_eq!(classify_intent(p), IntentClass::Build, "{p}");
        }
        // Chat (questions, smalltalk, empty — never drive on nothing).
        // Token-boundary negatives: substrings of verbs/starts must NOT fire.
        for p in [
            "what does this function do?",
            "explain lifetimes",
            "hi",
            "",
            "   ",
            "thanks",
            "good morning",
            "what is the address?",
            "explain the prefix",
            "i forgot the key",
            "open the dialog",
            "categorize this",
            "the goal is set",
        ] {
            assert_eq!(classify_intent(p), IntentClass::Chat, "{p:?}");
        }
    }

    #[test]
    fn discriminator_words_parse_strictly_or_abstain() {
        // Exact words route; prose, fences, and bare REVIEW/PLAN abstain
        // (caller falls back to keywords — never a billed loop).
        assert_eq!(
            parse_discriminator("DRIVE", "build x"),
            Some(LoopIntent::Drive {
                goal: "build x".to_string()
            })
        );
        assert_eq!(
            parse_discriminator("chat", "what is this?"),
            Some(LoopIntent::Chat {
                prompt: "what is this?".to_string()
            })
        );
        assert_eq!(
            parse_discriminator("REVIEW: src/main.rs", "review it"),
            Some(LoopIntent::Review {
                target: "src/main.rs".to_string()
            })
        );
        assert_eq!(
            parse_discriminator("PLAN: PLAN.md", "drive it"),
            Some(LoopIntent::PlanDrive {
                file: "PLAN.md".to_string()
            })
        );
        for bad in [
            "I think you should DRIVE",
            "```DRIVE```",
            "REVIEW",
            "PLAN",
            "REVIEW:",
            "",
            "LAUNCH",
        ] {
            assert_eq!(parse_discriminator(bad, "p"), None, "{bad:?}");
        }
    }

    #[test]
    fn sandwich_builds_and_picker_absorbs_it() {
        // "make me a sandwich" classifies Build (documented over-match —
        // the ask-picker confirmation absorbs it: denial → chat).
        assert_eq!(classify_intent("make me a sandwich"), IntentClass::Build);
    }

    #[test]
    fn loop_router_files_reviews_drives_and_chats() {
        // Drive: build verbs carry the whole goal.
        assert_eq!(
            classify_loop("build me a web server"),
            LoopIntent::Drive {
                goal: "build me a web server".to_string()
            }
        );
        // Review: needs a target; bare "review" stays cheap chat.
        assert_eq!(
            classify_loop("review src/main.rs"),
            LoopIntent::Review {
                target: "src/main.rs".to_string()
            }
        );
        assert_eq!(
            classify_loop("REVIEW: brief.txt"),
            LoopIntent::Review {
                target: "brief.txt".to_string()
            }
        );
        assert_eq!(
            classify_loop("review"),
            LoopIntent::Chat {
                prompt: "review".to_string()
            }
        );
        // Word boundary: reviewer/reviews/re-reviewed never bill.
        for p in [
            "the reviewer was harsh",
            "code reviews are good",
            "reviewed it",
        ] {
            assert_eq!(
                classify_loop(p),
                LoopIntent::Chat {
                    prompt: p.to_string()
                },
                "{p}"
            );
        }
        // Plan files drive per-task.
        assert_eq!(
            classify_loop("drive PLAN.md"),
            LoopIntent::PlanDrive {
                file: "PLAN.md".to_string()
            }
        );
        // Plain questions chat.
        assert_eq!(
            classify_loop("what does this do?"),
            LoopIntent::Chat {
                prompt: "what does this do?".to_string()
            }
        );
        assert_eq!(
            classify_loop(""),
            LoopIntent::Chat {
                prompt: String::new()
            }
        );
    }

    #[test]
    fn budget_line_shape() {
        assert_eq!(budget_line(2, 8, 5, 32), "round 2/8 · tools 5/32");
    }
}
