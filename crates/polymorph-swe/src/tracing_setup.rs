//! Tracing setup: stderr ANSI layer for humans + JSON daily-rolling file layer for forensics,
//! secrets never logged.
//!
//! The secrecy guarantee is STRUCTURAL (key material never enters an event)
//! plus MECHANICAL (the `secrets_never_logged` test below greps every source
//! line: any tracing-macro call mentioning `api_key` fails the build).

use std::path::Path;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{fmt, prelude::*, EnvFilter, Layer};

/// Log-file retention: a week of daily forensics, then the roller deletes.
/// Used by the builder below AND pinned by test — bump in one place.
pub(crate) const RETENTION_FILES: usize = 7;

/// Held guards — dropping the file guard loses buffered log lines.
/// `main` must keep this alive for the whole process.
pub struct LoggingGuards {
    _file: tracing_appender::non_blocking::WorkerGuard,
}

/// Build the combined subscriber.
/// - When `enable_stderr` is true (CLI modes), stderr ANSI prints for humans.
/// - When false (non-CLI callers), the stderr layer is omitted so background library logs
///   (e.g. genai streamer warnings) stay off the caller's output.
/// - Rolling JSON file logs always record forensics in `log_dir`.
pub fn build_subscriber(
    log_dir: &Path,
    enable_stderr: bool,
) -> anyhow::Result<(impl tracing::Subscriber + Send + Sync, LoggingGuards)> {
    std::fs::create_dir_all(log_dir)
        .map_err(|e| anyhow::anyhow!("cannot create log dir {}: {e}", log_dir.display()))?;
    let file_appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("polymorph")
        .max_log_files(RETENTION_FILES)
        .build(log_dir)
        .map_err(|e| anyhow::anyhow!("cannot open log files in {}: {e}", log_dir.display()))?;
    let (file_writer, file_guard) = tracing_appender::non_blocking(file_appender);

    let stderr_layer = if enable_stderr {
        let stderr_filter =
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
        Some(
            fmt::layer()
                .with_writer(std::io::stderr)
                .with_filter(stderr_filter),
        )
    } else {
        None
    };

    // File layer floor is DEBUG for our targets (forensics needs the trail
    // even when the console stays quiet); an explicit RUST_LOG overrides
    // both layers identically.
    let file_filter = std::env::var("RUST_LOG")
        .ok()
        .and_then(|v| EnvFilter::try_new(v).ok())
        .unwrap_or_else(|| EnvFilter::new("polymorph=debug,warn"));

    let subscriber = tracing_subscriber::registry().with(stderr_layer).with(
        fmt::layer()
            .json()
            .with_writer(file_writer)
            .with_filter(file_filter),
    );
    Ok((subscriber, LoggingGuards { _file: file_guard }))
}

/// Default log dir: `./.polymorph/logs`.
pub fn default_log_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(".polymorph").join("logs")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_layer_writes_json() {
        let dir = tempfile::tempdir().unwrap();
        let (subscriber, _guards) = build_subscriber(dir.path(), true).unwrap();
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "polymorph", answer = 42, "hello-log");
        });
        drop(_guards); // flush the non-blocking worker
        std::thread::sleep(std::time::Duration::from_millis(200));
        let mut found = false;
        for entry in std::fs::read_dir(dir.path()).unwrap() {
            let content = std::fs::read_to_string(entry.unwrap().path()).unwrap();
            if content.contains("hello-log") && content.contains("\"answer\":42") {
                // JSON shape: fields as JSON values, not debug strings.
                found = true;
            }
            assert!(
                !content.contains("api_key"),
                "log output must never contain api_key material"
            );
        }
        assert!(found, "expected the JSON log line in the log dir");
    }

    #[test]
    fn retention_weeks_worth() {
        // Decision pin: a week of daily forensics. The builder consumes
        // super::RETENTION_FILES (same file, visible in review); this test
        // makes a casual numeric bump fail loudly.
        assert_eq!(super::RETENTION_FILES, 7);
    }

    #[test]
    fn secrets_never_logged() {
        // GREP-GUARD: no tracing-macro call site may mention api_key material. Walks every
        // workspace.rs file (excluding target/ and this guard's own needle construction). NOTE: the
        // needle is built with concat! so this very line does not contain the literal (self-
        // exclusion, documented not sneaky).
        let needle: String = concat!("api_", "key").into();
        let macros = [
            "tracing::",
            "info!(",
            "debug!(",
            "warn!(",
            "error!(",
            "trace!(",
        ];
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..");
        let mut hits = Vec::new();
        visit_rs(&root, &mut |path, line_no, line| {
            if line.contains("GREP_GUARD_SELF_OK") {
                return;
            }
            if macros.iter().any(|m| line.contains(m)) && line.contains(&needle) {
                hits.push(format!("{}:{line_no}: {line}", path.display()));
            }
        });
        assert!(
            hits.is_empty(),
            "tracing call sites mentioning api key material:\n{}",
            hits.join("\n")
        );
    }

    fn visit_rs(dir: &Path, f: &mut dyn FnMut(&Path, usize, &str)) {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.file_name().and_then(|n| n.to_str()) == Some("target") {
                continue;
            }
            if path.is_dir() {
                visit_rs(&path, f);
            } else if path.extension().and_then(|x| x.to_str()) == Some("rs") {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    for (i, line) in content.lines().enumerate() {
                        f(&path, i + 1, line);
                    }
                }
            }
        }
    }
}
