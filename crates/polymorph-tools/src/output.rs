//! Bounded tool output (docket #9): spill-to-file plus a line digest.
//!
//! A `bash` stream larger than the inline threshold is written in FULL to
//! an indexed scratch file under `<workdir>/.polymorph/tool-output/` while
//! it is read, and the model gets a digest instead of the raw text: the
//! first lines, the last lines, and the diagnostic-looking lines from the
//! middle (each with its following context), with every gap named by its
//! line range and the file to page it from (`read` with offset/limit, or
//! `grep` with `dir` = the file). Never a blind byte cut: lines are kept
//! whole, an overlong line is char-cut with an explicit marker. Memory
//! stays bounded while reading: past the threshold only the head, a tail
//! ring and the kept diagnostic lines are held.
//!
//! The same diagnostic heuristic and line cleaner serve the review verify
//! gate's digest (docket #2), whose rendering they must keep byte-identical.

use std::collections::VecDeque;
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Directory (under [`crate::fs::HARNESS_DIR`]) holding spill files.
pub const TOOL_OUTPUT_DIR: &str = "tool-output";

/// Default per-stream inline threshold, bytes (`[agent]
/// tool_output_inline_bytes`): streams at or under it reach the model
/// unchanged.
pub const DEFAULT_INLINE_BYTES: usize = 16 * 1024;

/// Default number of files kept in the spill directory (`[agent]
/// tool_output_keep_files`); older ones are pruned when a new one is made.
pub const DEFAULT_KEEP_FILES: usize = 200;

/// Digest shape: first lines, last lines, diagnostic lines from the middle
/// (each followed by up to [`CONTEXT_LINES`] lines), warnings on a separate
/// smaller budget so a warning flood cannot crowd out the errors.
const HEAD_LINES: usize = 20;
const TAIL_LINES: usize = 40;
const MAX_DIAG_LINES: usize = 25;
const MAX_WARN_LINES: usize = 10;
const CONTEXT_LINES: usize = 1;
/// Per-line bound in the digest (chars, never a byte index).
const LINE_CHARS: usize = 300;
/// Bytes of one line held while reading: a gigantic line (no newline for
/// megabytes) never grows memory past this. Enough for [`LINE_CHARS`]
/// chars at any UTF-8 width.
const LINE_KEEP_BYTES: usize = 2048;

/// Spill policy of a registry's `bash` tool. `inline_bytes == 0` turns
/// spilling off (pre-#9 behavior: every stream inline, unbounded).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpillConfig {
    pub inline_bytes: usize,
    pub keep_files: usize,
}

impl Default for SpillConfig {
    fn default() -> Self {
        Self {
            inline_bytes: DEFAULT_INLINE_BYTES,
            keep_files: DEFAULT_KEEP_FILES,
        }
    }
}

impl SpillConfig {
    /// Spilling off (bench apparatus stability).
    pub fn off() -> Self {
        Self {
            inline_bytes: 0,
            ..Self::default()
        }
    }

    pub fn enabled(&self) -> bool {
        self.inline_bytes > 0
    }
}

/// Diagnostic-line heuristic (case-insensitive substrings: compiler
/// errors, test-runner failures, panics, tracebacks, unknown-command
/// shells). False positives only cost a kept line. Shared with the verify
/// gate digest (docket #2) — changing it changes that rendering.
pub fn is_diagnostic_line(line: &str) -> bool {
    const MARKERS: [&str; 9] = [
        "error",
        "fail",
        "panicked",
        "exception",
        "fatal",
        "traceback",
        "cannot find",
        "not recognized",
        "not found",
    ];
    let lower = line.to_ascii_lowercase();
    MARKERS.iter().any(|m| lower.contains(m))
}

fn is_warning_line(line: &str) -> bool {
    line.to_ascii_lowercase().contains("warning")
}

/// One output line made safe for a digest: tabs become spaces, other
/// control characters (ANSI escapes, `\r`) are dropped, leading
/// indentation is kept, overflow past `max_chars` is cut on a char
/// boundary and marked `...`.
pub fn clean_line(line: &str, max_chars: usize) -> String {
    let clean: String = line
        .chars()
        .filter_map(|c| match c {
            '\t' => Some(' '),
            c if c.is_control() => None,
            c => Some(c),
        })
        .collect();
    let clean = clean.trim_end();
    if clean.chars().count() > max_chars {
        let mut cut: String = clean.chars().take(max_chars.saturating_sub(3)).collect();
        cut.push_str("...");
        cut
    } else {
        clean.to_string()
    }
}

/// `<workdir>/.polymorph/tool-output`.
pub fn spill_dir(workdir: &Path) -> PathBuf {
    workdir.join(crate::fs::HARNESS_DIR).join(TOOL_OUTPUT_DIR)
}

/// Workdir-relative `/`-separated spelling of a spill file: what the
/// model passes back to `read`/`grep` (relative paths join the workdir).
fn shown_path(name: &str) -> String {
    format!("{}/{TOOL_OUTPUT_DIR}/{name}", crate::fs::HARNESS_DIR)
}

/// Unique, time-ordered file stem: `<unix-millis, 13 digits>-<pid>-<seq>`.
/// The process-wide sequence keeps concurrent calls apart, the pid keeps
/// concurrent processes on one workdir apart.
pub fn next_stem() -> String {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("{millis:013}-{}-{seq}", std::process::id())
}

/// Whether `name` is a file this module made (`<13 digits>-…` with a
/// `.log`/`.txt` extension): pruning never touches anything else.
fn is_spill_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() > 14
        && bytes[..13].iter().all(u8::is_ascii_digit)
        && bytes[13] == b'-'
        && (name.ends_with(".log") || name.ends_with(".txt"))
}

/// Cleanup policy: keep the newest `keep` spill files in `dir` (names
/// sort by creation time), delete older ones. Only regular files whose
/// names match [`is_spill_name`] are candidates, and never one of the
/// current run's (`protect`: its `<stem>.` prefix — a run's stderr spill
/// must not prune its own stdout spill); errors are ignored (a concurrent
/// pruner may have removed the same file).
pub fn prune(dir: &Path, keep: usize, protect: &str) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| is_spill_name(n) && (protect.is_empty() || !n.starts_with(protect)))
        .collect();
    if names.len() <= keep {
        return;
    }
    names.sort_unstable_by(|a, b| b.cmp(a));
    for name in &names[keep..] {
        let _ = std::fs::remove_file(dir.join(name));
    }
}

/// Create a new spill file `<stem>.<suffix>` under `workdir`'s spill dir:
/// the dir (with a `*` `.gitignore`, so neither `git status` nor
/// gitignore-aware tools see it) on first use, older files pruned to make
/// room. Returns the open file, its path and its shown spelling.
fn create_spill_file(
    workdir: &Path,
    name: &str,
    keep_files: usize,
) -> std::io::Result<(std::fs::File, PathBuf, String)> {
    let dir = spill_dir(workdir);
    std::fs::create_dir_all(&dir)?;
    let ignore = dir.join(".gitignore");
    if !ignore.exists() {
        let _ = std::fs::write(&ignore, "*\n");
    }
    let stem = name.split('.').next().unwrap_or(name);
    prune(&dir, keep_files.saturating_sub(1), &format!("{stem}."));
    let path = dir.join(name);
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    Ok((file, path, shown_path(name)))
}

/// Save `text` as a new spill file `<fresh stem>.<suffix>` (history
/// compaction keeps an evicted tool result retrievable this way). Returns
/// the shown (workdir-relative) path.
pub fn save_text(
    workdir: &Path,
    suffix: &str,
    text: &str,
    keep_files: usize,
) -> std::io::Result<String> {
    let name = format!("{}.{suffix}", next_stem());
    let (mut file, _, shown) = create_spill_file(workdir, &name, keep_files)?;
    file.write_all(text.as_bytes())?;
    file.flush()?;
    Ok(shown)
}

/// Where one stream spills: `<workdir>/.polymorph/tool-output/<stem>.<label>.log`.
#[derive(Debug, Clone)]
pub struct SpillTarget {
    pub workdir: PathBuf,
    pub stem: String,
    /// Stream label (`stdout`/`stderr`): file suffix and digest wording.
    pub label: &'static str,
    pub inline_bytes: usize,
    pub keep_files: usize,
}

/// One finished stream.
#[derive(Debug, Clone)]
pub enum CapturedStream {
    /// At or under the threshold (or spilling off): the lossy-decoded
    /// text, exactly what the pre-#9 capture returned.
    Inline(String),
    Spilled(SpilledStream),
}

/// A stream past the threshold: the digest the model sees plus where the
/// full bytes went.
#[derive(Debug, Clone)]
pub struct SpilledStream {
    pub digest: String,
    /// Workdir-relative spelling (`.polymorph/tool-output/…`), `None` when
    /// the file could not be created.
    pub path: Option<String>,
    pub abs_path: Option<PathBuf>,
    pub total_bytes: u64,
    pub total_lines: u64,
    /// Spill write failure: the omitted lines are lost (or, with a path,
    /// only partly saved).
    pub error: Option<String>,
}

impl CapturedStream {
    /// [`Self::text`], owned (no copy of an inline stream).
    pub fn into_text(self) -> String {
        match self {
            CapturedStream::Inline(s) => s,
            CapturedStream::Spilled(s) => s.digest,
        }
    }

    /// What the model reads for this stream: the text, or the digest.
    pub fn text(&self) -> &str {
        match self {
            CapturedStream::Inline(s) => s,
            CapturedStream::Spilled(s) => &s.digest,
        }
    }

    pub fn is_empty(&self) -> bool {
        matches!(self, CapturedStream::Inline(s) if s.is_empty())
    }

    /// `{path, total_bytes, total_lines[, error]}` for spilled streams.
    pub fn spill_json(&self) -> Option<serde_json::Value> {
        let CapturedStream::Spilled(s) = self else {
            return None;
        };
        let mut v = serde_json::json!({
            "path": s.path,
            "total_bytes": s.total_bytes,
            "total_lines": s.total_lines,
        });
        if let Some(e) = &s.error {
            v["error"] = serde_json::Value::String(e.clone());
        }
        Some(v)
    }
}

/// Incremental capture of one child stream. Under the threshold it
/// buffers bytes (the inline case must stay byte-identical); past it, it
/// writes everything to the spill file and keeps only the digest state.
pub struct StreamCapture {
    target: Option<SpillTarget>,
    buf: Vec<u8>,
    total: u64,
    spill: Option<Spill>,
}

struct Spill {
    file: Option<std::io::BufWriter<std::fs::File>>,
    path: Option<PathBuf>,
    shown: Option<String>,
    error: Option<String>,
    digest: LineDigest,
}

impl StreamCapture {
    /// `None` = never spill (full in-memory capture, pre-#9 behavior).
    pub fn new(target: Option<SpillTarget>) -> Self {
        Self {
            target: target.filter(|t| t.inline_bytes > 0),
            buf: Vec::new(),
            total: 0,
            spill: None,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.total += bytes.len() as u64;
        if let Some(spill) = &mut self.spill {
            spill.write(bytes);
            return;
        }
        let Some(target) = &self.target else {
            self.buf.extend_from_slice(bytes);
            return;
        };
        if self.buf.len() + bytes.len() <= target.inline_bytes {
            self.buf.extend_from_slice(bytes);
            return;
        }
        let mut spill = Spill::open(target);
        let buffered = std::mem::take(&mut self.buf);
        spill.write(&buffered);
        spill.write(bytes);
        self.spill = Some(spill);
    }

    pub fn finish(self) -> CapturedStream {
        let Some(mut spill) = self.spill else {
            return CapturedStream::Inline(String::from_utf8_lossy(&self.buf).into_owned());
        };
        if let Some(mut file) = spill.file.take() {
            if let Err(e) = file.flush() {
                spill.error = Some(format!("write failed: {e}"));
            }
        }
        spill.digest.end();
        let label = self.target.as_ref().map(|t| t.label).unwrap_or("output");
        let inline_bytes = self.target.as_ref().map(|t| t.inline_bytes).unwrap_or(0);
        let digest = spill.digest.render(
            label,
            inline_bytes,
            self.total,
            spill.shown.as_deref(),
            spill.error.as_deref(),
        );
        CapturedStream::Spilled(SpilledStream {
            digest,
            path: spill.shown,
            abs_path: spill.path,
            total_bytes: self.total,
            total_lines: spill.digest.lines,
            error: spill.error,
        })
    }
}

impl Spill {
    fn open(target: &SpillTarget) -> Self {
        let name = format!("{}.{}.log", target.stem, target.label);
        let (file, path, shown, error) =
            match create_spill_file(&target.workdir, &name, target.keep_files) {
                Ok((f, p, s)) => (
                    Some(std::io::BufWriter::with_capacity(64 * 1024, f)),
                    Some(p),
                    Some(s),
                    None,
                ),
                Err(e) => (None, None, None, Some(e.to_string())),
            };
        Self {
            file,
            path,
            shown,
            error,
            digest: LineDigest::default(),
        }
    }

    fn write(&mut self, bytes: &[u8]) {
        if let Some(file) = &mut self.file {
            if let Err(e) = file.write_all(bytes) {
                self.error = Some(format!("write failed: {e}"));
                self.file = None;
            }
        }
        self.digest.push(bytes);
    }
}

/// Streaming line digest: complete lines flow head → tail ring; a line
/// leaving the ring is a middle line, kept only when diagnostic-looking
/// (or as the context after one).
#[derive(Default)]
struct LineDigest {
    /// Current (unterminated) line: at most [`LINE_KEEP_BYTES`] held.
    cur: Vec<u8>,
    cur_len: u64,
    /// Lines completed so far (1-based number of the last one).
    lines: u64,
    head: Vec<(u64, String)>,
    tail: VecDeque<(u64, String)>,
    kept: Vec<(u64, String)>,
    diags: usize,
    warns: usize,
    /// Diagnostic/warning lines in the middle past their budgets.
    unshown_diags: u64,
    context_left: usize,
    saw_nul: bool,
}

impl LineDigest {
    fn push(&mut self, mut bytes: &[u8]) {
        if bytes.contains(&0) {
            self.saw_nul = true;
        }
        while let Some(pos) = bytes.iter().position(|&b| b == b'\n') {
            self.extend(&bytes[..pos]);
            self.end_line();
            bytes = &bytes[pos + 1..];
        }
        self.extend(bytes);
    }

    fn extend(&mut self, bytes: &[u8]) {
        self.cur_len += bytes.len() as u64;
        let room = LINE_KEEP_BYTES.saturating_sub(self.cur.len());
        self.cur.extend_from_slice(&bytes[..room.min(bytes.len())]);
    }

    /// Flush a final unterminated line (`str::lines` counts it too, so the
    /// numbers match a later `read` of the spill file).
    fn end(&mut self) {
        if self.cur_len > 0 {
            self.end_line();
        }
    }

    fn end_line(&mut self) {
        let cut = self.cur_len > self.cur.len() as u64;
        let raw = if cut {
            trim_partial_char(&self.cur)
        } else {
            &self.cur[..]
        };
        let mut text = clean_line(&String::from_utf8_lossy(raw), LINE_CHARS);
        if cut {
            text.push_str(&format!(" [line cut: {} bytes in full]", self.cur_len));
        }
        self.cur.clear();
        self.cur_len = 0;
        self.lines += 1;
        let n = self.lines;
        if self.head.len() < HEAD_LINES {
            self.head.push((n, text));
            return;
        }
        self.tail.push_back((n, text));
        if self.tail.len() > TAIL_LINES {
            if let Some((m, t)) = self.tail.pop_front() {
                self.middle(m, t);
            }
        }
    }

    fn middle(&mut self, n: u64, text: String) {
        let budget = if is_diagnostic_line(&text) {
            Some(&mut self.diags).filter(|d| **d < MAX_DIAG_LINES)
        } else if is_warning_line(&text) {
            Some(&mut self.warns).filter(|w| **w < MAX_WARN_LINES)
        } else {
            None
        };
        let flagged = is_diagnostic_line(&text) || is_warning_line(&text);
        if let Some(count) = budget {
            *count += 1;
            self.kept.push((n, text));
            self.context_left = CONTEXT_LINES;
            return;
        }
        if flagged {
            self.unshown_diags += 1;
        }
        if self.context_left > 0 {
            self.context_left -= 1;
            self.kept.push((n, text));
        }
    }

    fn render(
        &self,
        label: &str,
        inline_bytes: usize,
        total_bytes: u64,
        shown: Option<&str>,
        error: Option<&str>,
    ) -> String {
        let total = self.lines;
        let mut out = match (shown, error) {
            (Some(path), None) => format!(
                "[{label} spilled: {total} lines, {total_bytes} bytes (over the {inline_bytes}-byte inline limit). Full text: {path} — page it with `read` (path + offset/limit) or `grep` (dir = that path). Shown below: the first and last lines plus diagnostic lines from the middle; every gap names its line range.]"
            ),
            (Some(path), Some(e)) => format!(
                "[{label} spilled: {total} lines, {total_bytes} bytes (over the {inline_bytes}-byte inline limit), but the spill file {path} is INCOMPLETE ({e}); lines missing from it are lost. Shown below: the first and last lines plus diagnostic lines from the middle.]"
            ),
            (None, e) => format!(
                "[{label}: {total} lines, {total_bytes} bytes (over the {inline_bytes}-byte inline limit); the full text could NOT be saved ({}), so omitted lines are lost. Shown below: the first and last lines plus diagnostic lines from the middle.]",
                e.unwrap_or("unknown error")
            ),
        };
        if self.saw_nul {
            out.push_str("\n[binary output (NUL bytes): `read`/`grep` refuse the spill file; inspect it with a command instead.]");
        }
        if self.unshown_diags > 0 {
            out.push_str(&format!(
                "\n[{} more diagnostic/warning-looking lines in the omitted ranges: `grep` the spill file for them.]",
                self.unshown_diags
            ));
        }
        let gap = |a: u64, b: u64| match (shown, error) {
            (Some(path), None) => format!(
                "... [lines {a}-{b} omitted ({} lines): read {path} offset={a}] ...",
                b - a + 1
            ),
            _ => format!(
                "... [lines {a}-{b} omitted ({} lines; not saved)] ...",
                b - a + 1
            ),
        };
        let mut next = 1u64;
        for (n, text) in self.head.iter().chain(&self.kept).chain(&self.tail) {
            if *n > next {
                out.push('\n');
                out.push_str(&gap(next, n - 1));
            }
            out.push('\n');
            out.push_str(text);
            next = n + 1;
        }
        if next <= total {
            out.push('\n');
            out.push_str(&gap(next, total));
        }
        out
    }
}

/// Drop an incomplete trailing UTF-8 sequence (a line cut at
/// [`LINE_KEEP_BYTES`] may end mid-char), so the cut never decodes to a
/// replacement character.
fn trim_partial_char(bytes: &[u8]) -> &[u8] {
    let len = bytes.len();
    let mut start = len;
    while start > 0 && len - start < 4 {
        start -= 1;
        if bytes[start] & 0b1100_0000 != 0b1000_0000 {
            break;
        }
    }
    if start == len {
        return bytes;
    }
    let width = match bytes[start] {
        b if b & 0b1000_0000 == 0 => 1,
        b if b & 0b1110_0000 == 0b1100_0000 => 2,
        b if b & 0b1111_0000 == 0b1110_0000 => 3,
        b if b & 0b1111_1000 == 0b1111_0000 => 4,
        _ => return bytes,
    };
    if len - start < width {
        &bytes[..start]
    } else {
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(dir: &Path, inline: usize) -> SpillTarget {
        SpillTarget {
            workdir: dir.to_path_buf(),
            stem: next_stem(),
            label: "stdout",
            inline_bytes: inline,
            keep_files: DEFAULT_KEEP_FILES,
        }
    }

    fn spilled(c: CapturedStream) -> SpilledStream {
        match c {
            CapturedStream::Spilled(s) => s,
            CapturedStream::Inline(s) => panic!("expected a spill, got inline {} bytes", s.len()),
        }
    }

    #[test]
    fn under_threshold_is_inline_and_exact() {
        let d = tempfile::tempdir().unwrap();
        let mut c = StreamCapture::new(Some(target(d.path(), 16)));
        c.push(b"0123456789");
        c.push(b"abcdef");
        match c.finish() {
            CapturedStream::Inline(s) => assert_eq!(s, "0123456789abcdef"),
            other => panic!("{other:?}"),
        }
        assert!(
            !spill_dir(d.path()).exists(),
            "no spill dir for inline output"
        );
    }

    /// A diagnostic in the middle of 50k lines survives into the digest
    /// with its context; the file holds every byte; gaps name ranges.
    #[test]
    fn middle_diagnostic_survives_and_file_is_exact() {
        let d = tempfile::tempdir().unwrap();
        let mut c = StreamCapture::new(Some(target(d.path(), 1024)));
        let mut all = Vec::new();
        for i in 1..=50_000u32 {
            let line = if i == 25_000 {
                "error[E0308]: mismatched types\r\n".to_string()
            } else if i == 25_001 {
                "  --> src/lib.rs:7:5\r\n".to_string()
            } else {
                format!("compiling unit {i}\r\n")
            };
            all.extend_from_slice(line.as_bytes());
        }
        // Odd chunking: splits lines and CRLF pairs across pushes.
        for chunk in all.chunks(777) {
            c.push(chunk);
        }
        let s = spilled(c.finish());
        assert_eq!(s.total_lines, 50_000);
        assert_eq!(s.total_bytes, all.len() as u64);
        assert!(s.error.is_none());
        assert_eq!(std::fs::read(s.abs_path.as_ref().unwrap()).unwrap(), all);
        let path = s.path.clone().unwrap();
        assert!(path.starts_with(".polymorph/tool-output/"), "{path}");
        assert!(
            s.digest.contains("error[E0308]: mismatched types"),
            "{}",
            s.digest
        );
        assert!(s.digest.contains("  --> src/lib.rs:7:5"));
        assert!(!s.digest.contains('\r'));
        assert!(
            s.digest.contains(&format!(
                "... [lines 21-24999 omitted (24979 lines): read {path} offset=21] ..."
            )),
            "{}",
            s.digest
        );
        assert!(s.digest.contains("compiling unit 1\n"));
        assert!(s.digest.ends_with("compiling unit 50000"));
        assert!(s
            .digest
            .contains("... [lines 25002-49960 omitted (24959 lines)"));
        assert!(s.digest.len() < 8 * 1024, "{}", s.digest.len());
    }

    /// One enormous line without a newline: bounded in memory and inline,
    /// cut at a char boundary with an explicit marker.
    #[test]
    fn single_giant_line_is_bounded_with_marker() {
        let d = tempfile::tempdir().unwrap();
        let mut c = StreamCapture::new(Some(target(d.path(), 1024)));
        let chunk = "é".repeat(4096); // 2-byte chars: cuts land mid-char
        for _ in 0..256 {
            c.push(chunk.as_bytes());
        }
        let s = spilled(c.finish());
        assert_eq!(s.total_lines, 1);
        assert_eq!(s.total_bytes, 256 * 8192);
        assert!(
            s.digest.contains("[line cut: 2097152 bytes in full]"),
            "{}",
            s.digest
        );
        assert!(!s.digest.contains('\u{FFFD}'));
        assert!(s.digest.len() < 4096, "{}", s.digest.len());
        let line = s.digest.lines().last().unwrap();
        assert!(line.starts_with(&"é".repeat(297)), "{line}");
    }

    #[test]
    fn unwritable_spill_dir_falls_back_with_a_clear_note() {
        let d = tempfile::tempdir().unwrap();
        // `.polymorph` is a FILE: the spill dir cannot be created.
        std::fs::write(d.path().join(".polymorph"), "x").unwrap();
        let mut c = StreamCapture::new(Some(target(d.path(), 8)));
        for i in 0..200 {
            c.push(format!("line {i}\n").as_bytes());
        }
        let s = spilled(c.finish());
        assert!(s.path.is_none());
        assert!(s.error.is_some());
        assert!(s.digest.contains("could NOT be saved"), "{}", s.digest);
        assert!(s.digest.contains("omitted (") && s.digest.contains("not saved)"));
        assert!(s.digest.contains("line 199"));
    }

    #[test]
    fn binary_output_is_noted() {
        let d = tempfile::tempdir().unwrap();
        let mut c = StreamCapture::new(Some(target(d.path(), 4)));
        c.push(&[0u8, 1, 2, 0xff, b'\n', 0, 0, b'\n']);
        let s = spilled(c.finish());
        assert!(s.digest.contains("binary output"), "{}", s.digest);
        assert_eq!(std::fs::read(s.abs_path.unwrap()).unwrap().len(), 8);
    }

    #[test]
    fn warning_flood_cannot_crowd_out_errors() {
        let d = tempfile::tempdir().unwrap();
        let mut c = StreamCapture::new(Some(target(d.path(), 64)));
        let mut text = String::new();
        for i in 0..500 {
            text.push_str(&format!("warning: unused variable {i}\n"));
        }
        text.push_str("error: real problem\n");
        for i in 0..100 {
            text.push_str(&format!("filler {i}\n"));
        }
        c.push(text.as_bytes());
        let s = spilled(c.finish());
        assert!(s.digest.contains("error: real problem"), "{}", s.digest);
        assert!(s.digest.contains("more diagnostic/warning-looking lines"));
    }

    #[test]
    fn prune_keeps_newest_and_only_touches_spill_files() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path();
        for i in 0..5u64 {
            std::fs::write(dir.join(format!("{:013}-1-{i}.stdout.log", 1000 + i)), "x").unwrap();
        }
        std::fs::write(dir.join("notes.log"), "keep").unwrap();
        std::fs::write(dir.join(".gitignore"), "*\n").unwrap();
        prune(dir, 2, "");
        let mut left: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                ".gitignore",
                "0000000001003-1-3.stdout.log",
                "0000000001004-1-4.stdout.log",
                "notes.log"
            ]
        );
        // The current run's files are never pruned, nor counted.
        prune(dir, 0, "0000000001003-1-3.");
        assert!(dir.join("0000000001003-1-3.stdout.log").exists());
        assert!(!dir.join("0000000001004-1-4.stdout.log").exists());
    }

    #[test]
    fn stems_are_unique_and_spill_dir_is_gitignored() {
        let a = next_stem();
        let b = next_stem();
        assert_ne!(a, b);
        assert!(is_spill_name(&format!("{a}.stderr.log")));
        let d = tempfile::tempdir().unwrap();
        let shown = save_text(d.path(), "result.txt", "body\n", 10).unwrap();
        assert!(shown.starts_with(".polymorph/tool-output/") && shown.ends_with(".result.txt"));
        assert_eq!(
            std::fs::read_to_string(d.path().join(&shown)).unwrap(),
            "body\n"
        );
        assert_eq!(
            std::fs::read_to_string(spill_dir(d.path()).join(".gitignore")).unwrap(),
            "*\n"
        );
    }

    #[test]
    fn trim_partial_char_cases() {
        assert_eq!(trim_partial_char(b"abc"), b"abc");
        assert_eq!(trim_partial_char("aé".as_bytes()), "aé".as_bytes());
        assert_eq!(trim_partial_char(&"aé".as_bytes()[..2]), b"a");
        assert_eq!(trim_partial_char(&"a😀".as_bytes()[..4]), b"a");
        assert_eq!(trim_partial_char(b""), b"");
    }
}
