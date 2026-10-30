//! Script-command hooks: external commands consulted at the `Registry::call` boundary, after the
//! in-process handlers (cheap first, spawn cost second).
//!
//! Event delivery is environment (`POLYMORPH_HOOK_EVENT=pre|post|stop`, `POLYMORPH_HOOK_TOOL`,
//! `POLYMORPH_HOOK_ARGS` as JSON, `POLYMORPH_HOOK_OK` + `POLYMORPH_HOOK_DETAIL` on post) — portable
//! across `cmd` and `sh` without quoting layers.
//!
//! Exit-code semantics (fail-closed): 0 = Allow; 2 = Deny with trimmed
//! stderr (capped) as the reason; any other code, timeout, or spawn
//! failure = Deny naming the condition. Post/Stop scripts run for
//! observation (stderr traced); their verdicts are ignored — outcome
//! shapes stay frozen (routing script output into the loop is a queued
//! refinement, stated not silent). Prompt-action (ask-user) needs an
//! interactive prompter seam for surfaces with a human: queued.

use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Which boundary point fires the script. String vocabulary matches the
/// `[[hooks]]` config `event` field (`pre`/`post`/`stop`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptOn {
    Pre,
    Post,
    Stop,
}

impl ScriptOn {
    pub fn parse(word: &str) -> anyhow::Result<Self> {
        match word {
            "pre" => Ok(Self::Pre),
            "post" => Ok(Self::Post),
            "stop" => Ok(Self::Stop),
            _ => anyhow::bail!("unknown hook event {word:?} (available: pre, post, stop)"),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pre => "pre",
            Self::Post => "post",
            Self::Stop => "stop",
        }
    }
}

/// One script hook: event point + shell command + timeout. Declared in
/// config (`[[hooks]]`), validated by [`validate_entry`], run by [`run_hook`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptHook {
    pub on: ScriptOn,
    pub command: String,
    pub timeout: Duration,
}

/// Bash-tool timeout discipline, mirrored (exec.rs caps at 600 s).
pub const MAX_HOOK_TIMEOUT_SECS: u64 = 600;

/// Validate one config entry into a runnable hook. Refusals name the field
/// (fail-closed at attach time — bad config bails pre-spend, F5d precedent).
pub fn validate_entry(entry: &polymorph_core::config::HookEntry) -> anyhow::Result<ScriptHook> {
    let on = ScriptOn::parse(&entry.event)?;
    if entry.command.trim().is_empty() {
        anyhow::bail!("hook command must be non-empty");
    }
    let secs = entry.timeout_secs.unwrap_or(30);
    if secs == 0 || secs > MAX_HOOK_TIMEOUT_SECS {
        anyhow::bail!(
            "hook timeout must be 1..={} seconds (got {secs})",
            MAX_HOOK_TIMEOUT_SECS
        );
    }
    Ok(ScriptHook {
        on,
        command: entry.command.clone(),
        timeout: Duration::from_secs(secs),
    })
}

/// Stderr cap for deny reasons (scripts must not flood transcripts).
pub const MAX_STDERR_CHARS: usize = 500;

/// Run one script hook for a pre-event. Returns the verdict (see module
/// docs for exit-code semantics). Post/Stop callers use [`run_observe`].
pub async fn run_hook(
    hook: &ScriptHook,
    tool: &str,
    args_json: &str,
    cancel: &CancellationToken,
) -> super::HookDecision {
    match run_script(hook, "pre", tool, args_json, None, cancel).await {
        Ok(out) => match out.exit_code {
            // Debug-level: visible in the JSON rolling logs (file floor is
            // DEBUG), never on stderr — per-call forensics without noise.
            0 => {
                tracing::debug!(tool, "script hook allow");
                super::HookDecision::Allow
            }
            2 => super::HookDecision::Deny {
                reason: format!("hook-script: {}", capped(&out.stderr)),
            },
            code => super::HookDecision::Deny {
                reason: format!("hook-script: exit {code}"),
            },
        },
        Err(err) => super::HookDecision::Deny {
            reason: format!("hook-script: {err:#}"),
        },
    }
}

/// Run one script hook for a post/stop event (observation only): stderr is
/// traced for forensics; the verdict is always Allow (ignored by callers).
pub async fn run_observe(
    hook: &ScriptHook,
    kind: &str,
    tool: &str,
    args_json: &str,
    ok: bool,
    detail: &str,
    cancel: &CancellationToken,
) {
    match run_script(hook, kind, tool, args_json, Some((ok, detail)), cancel).await {
        Ok(out) => {
            if !out.stderr.trim().is_empty() {
                tracing::info!(
                    tool,
                    kind,
                    stderr = capped(&out.stderr).as_str(),
                    "hook script stderr"
                );
            }
        }
        Err(err) => {
            tracing::warn!(
                tool,
                kind,
                error = format!("{err:#}").as_str(),
                "hook script failed"
            );
        }
    }
}

struct ScriptOut {
    exit_code: i32,
    stderr: String,
}

async fn run_script(
    hook: &ScriptHook,
    kind: &str,
    tool: &str,
    args_json: &str,
    post: Option<(bool, &str)>,
    cancel: &CancellationToken,
) -> anyhow::Result<ScriptOut> {
    // Env delivery (no argv quoting layers): child inherits process env PLUS these overrides,
    // applied PER-CHILD. Values are data (a tool name / JSON blob), never secrets — HookEvents
    // carry shapes, not values.
    let mut env = vec![
        ("POLYMORPH_HOOK_EVENT", kind),
        ("POLYMORPH_HOOK_TOOL", tool),
        ("POLYMORPH_HOOK_ARGS", args_json),
    ];
    if let Some((ok, detail)) = post {
        env.push(("POLYMORPH_HOOK_OK", if ok { "1" } else { "0" }));
        env.push(("POLYMORPH_HOOK_DETAIL", detail));
    }
    let out = super::exec::run_bash_with_env(&hook.command, hook.timeout, cancel, &env)
        .await
        .map_err(|e| anyhow::anyhow!("{e:#}"))?;
    Ok(ScriptOut {
        exit_code: out.exit_code,
        stderr: out.stderr,
    })
}

fn capped(s: &str) -> String {
    let trimmed = s.trim();
    if trimmed.chars().count() <= MAX_STDERR_CHARS {
        return trimmed.to_string();
    }
    let head: String = trimmed.chars().take(MAX_STDERR_CHARS).collect();
    format!("{head}…[{MAX_STDERR_CHARS} chars]")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cancel() -> CancellationToken {
        CancellationToken::new()
    }

    #[test]
    fn event_vocabulary_strict() {
        assert_eq!(ScriptOn::parse("pre").unwrap(), ScriptOn::Pre);
        assert_eq!(ScriptOn::parse("post").unwrap(), ScriptOn::Post);
        assert_eq!(ScriptOn::parse("stop").unwrap(), ScriptOn::Stop);
        for bad in ["", "PRE", "prompt", "allow"] {
            let err = ScriptOn::parse(bad).unwrap_err().to_string();
            assert!(err.contains("pre, post, stop"), "{bad}: {err}");
        }
    }

    #[test]
    fn entry_validation() {
        let good = polymorph_core::config::HookEntry {
            event: "pre".to_string(),
            command: "cmd /C exit 0".to_string(),
            timeout_secs: None,
        };
        let hook = validate_entry(&good).unwrap();
        assert_eq!(hook.timeout, Duration::from_secs(30));
        for bad in [
            polymorph_core::config::HookEntry {
                event: "prompt".to_string(),
                command: "x".to_string(),
                timeout_secs: None,
            },
            polymorph_core::config::HookEntry {
                event: "pre".to_string(),
                command: "   ".to_string(),
                timeout_secs: None,
            },
            polymorph_core::config::HookEntry {
                event: "pre".to_string(),
                command: "x".to_string(),
                timeout_secs: Some(0),
            },
            polymorph_core::config::HookEntry {
                event: "pre".to_string(),
                command: "x".to_string(),
                timeout_secs: Some(601),
            },
        ] {
            validate_entry(&bad).unwrap_err();
        }
    }

    // NOTE: script-execution tests below shell out via `cmd /C` (Windows host + windows-only CI —
    // same platform note as the exec suite). Keep commands quote-free and fast (well under
    // timeouts).

    #[tokio::test]
    async fn exit_zero_allows() {
        let hook = ScriptHook {
            on: ScriptOn::Pre,
            command: "cmd /C exit 0".to_string(),
            timeout: Duration::from_secs(10),
        };
        assert_eq!(
            run_hook(&hook, "read", "{}", &cancel()).await,
            super::super::HookDecision::Allow
        );
    }

    #[tokio::test]
    async fn exit_two_denies_with_stderr() {
        let hook = ScriptHook {
            on: ScriptOn::Pre,
            command: "cmd /C echo no-reads-here 1>&2 & exit 2".to_string(),
            timeout: Duration::from_secs(10),
        };
        let decision = run_hook(&hook, "read", "{}", &cancel()).await;
        match decision {
            super::super::HookDecision::Deny { reason } => {
                assert!(reason.contains("no-reads-here"), "{reason}")
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn other_exit_denies_naming_code() {
        let hook = ScriptHook {
            on: ScriptOn::Pre,
            command: "cmd /C exit 5".to_string(),
            timeout: Duration::from_secs(10),
        };
        let decision = run_hook(&hook, "read", "{}", &cancel()).await;
        match decision {
            super::super::HookDecision::Deny { reason } => {
                assert!(reason.contains("exit 5"), "{reason}")
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_command_denies() {
        let hook = ScriptHook {
            on: ScriptOn::Pre,
            command: "cmd /C no-such-binary-xyz".to_string(),
            timeout: Duration::from_secs(10),
        };
        // `cmd /C` itself spawns (exit 1 on unknown inner command) → deny
        // naming the code, never a hang, never a pass.
        let decision = run_hook(&hook, "read", "{}", &cancel()).await;
        assert!(
            matches!(decision, super::super::HookDecision::Deny { .. }),
            "{decision:?}"
        );
    }

    #[tokio::test]
    async fn env_delivers_tool_name() {
        // Delivery proof: the script decides on the TOOL var.
        let hook = ScriptHook {
            on: ScriptOn::Pre,
            command: "cmd /C if %POLYMORPH_HOOK_TOOL%==read exit 2".to_string(),
            timeout: Duration::from_secs(10),
        };
        let decision = run_hook(&hook, "read", "{}", &cancel()).await;
        assert!(
            matches!(decision, super::super::HookDecision::Deny { .. }),
            "{decision:?}"
        );
    }

    #[tokio::test]
    async fn concurrent_hooks_observe_own_tool() {
        // Race pin. 16 concurrent runs with DISTINCT tools; each script denies iff it sees its OWN
        // name.
        let mut set = tokio::task::JoinSet::new();
        for i in 0..16u32 {
            let tool = format!("race-tool-{i}");
            set.spawn(async move {
                let hook = ScriptHook {
                    on: ScriptOn::Pre,
                    command: format!("cmd /C if %POLYMORPH_HOOK_TOOL%=={tool} exit 2"),
                    timeout: Duration::from_secs(30),
                };
                let decision = run_hook(&hook, &tool, "{}", &cancel()).await;
                (tool, decision)
            });
        }
        while let Some(joined) = set.join_next().await {
            let (tool, decision) = joined.unwrap();
            assert!(
                matches!(decision, super::super::HookDecision::Deny { .. }),
                "{tool} observed another run's env"
            );
        }
    }

    #[tokio::test]
    async fn observe_never_denies() {
        let hook = ScriptHook {
            on: ScriptOn::Post,
            command: "cmd /C exit 2".to_string(),
            timeout: Duration::from_secs(10),
        };
        // Observation path has no verdict by construction (returns unit).
        run_observe(&hook, "post", "read", "{}", true, "keys:path", &cancel()).await;
    }
}
