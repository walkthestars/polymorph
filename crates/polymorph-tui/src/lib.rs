//! `polymorph-tui` — Epic J interactive surface (see `docs/plan-chunk74.md`).
//!
//! Chunk 75 (skeleton): `App` state machine + hello render + terminal wiring.
//! Chunk 76 (chat): message log + input editing + `Responder` seam for model
//! turns.
//! Chunk 77 (picker): C3 session picker over the `SessionBackend` seam (Tab
//! toggles, arrows move, Enter switches). All tests are TTY-free: pure key
//! handling, `FakeResponder`
//! submit cycles, `TestBackend` buffer-exact rendering. The tracing
//! subscriber-swap contract (D2) holds STRUCTURALLY — this crate never
//! touches the stderr/file layers, so the JSON ledger keeps recording
//! while the alternate screen is up.

use std::future::Future;

/// TUI drive mode — offline contract (Chunk 139, post-v1.0 feature).
pub mod drive;

pub mod theme;

pub use theme::Theme;

use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Margin, Position},
    widgets::{Block, Paragraph, Wrap},
    Frame, Terminal,
};

/// Crate identifier. Used by smoke tests to prove the crate links.
pub fn crate_name() -> &'static str {
    "polymorph-tui"
}

/// Who said a chat line. Rendered as a `you:`/`model:` prefix (c76 dialect;
/// richer cards land with the diff/task panes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Model,
}

/// One chat line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub role: Role,
    pub text: String,
}

/// Output from a model turn: clean reply text (for chat transcript and
/// persistence) plus structured circuit execution telemetry (for the
/// scrollable inspection panel and F5 circuit view).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TurnOutput {
    pub text: String,
    pub circuit_log: Vec<String>,
}

impl From<String> for TurnOutput {
    fn from(text: String) -> Self {
        Self {
            text,
            circuit_log: Vec::new(),
        }
    }
}

/// Supplies model turns. Implemented by the CLI over `polymorph-llm`
/// (config stays there); tests use `FakeResponder`. Owned prompt in,
/// structured turn output out — clean transcript text + inspection telemetry.
pub trait Responder {
    fn respond(&self, prompt: String) -> impl Future<Output = anyhow::Result<TurnOutput>> + Send;
    /// One cheap turn for chat intent (no Plan/Review circuit — greeting
    /// a circuit is how "hello" bills 3 turns). Default = `respond`
    /// (FakeResponder and old implementers keep working untouched).
    /// `images` stages screenshot sidecars; implementers without vision
    /// ignore them (default) rather than failing the turn.
    fn chat_turn(
        &self,
        prompt: String,
        images: Vec<std::path::PathBuf>,
    ) -> impl Future<Output = anyhow::Result<TurnOutput>> + Send {
        let _ = images;
        self.respond(prompt)
    }
}

/// Staged vision attachments: validated at stage time so a bad path
/// never reaches a billed turn.
pub const MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024;
const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "webp", "gif", "bmp"];

/// Validate one image path for staging (exists, image extension, size
/// cap). Returns the path unchanged on success — fail-closed naming
/// the fault.
pub fn validate_image(path: &str) -> anyhow::Result<std::path::PathBuf> {
    let p = std::path::PathBuf::from(path.trim().trim_matches(|c| "\"'".contains(c)));
    if p.as_os_str().is_empty() {
        anyhow::bail!("usage: /attach <image.png>");
    }
    let meta = std::fs::metadata(&p)
        .map_err(|_| anyhow::anyhow!("cannot read {} (missing?)", p.display()))?;
    if !meta.is_file() {
        anyhow::bail!("not a file: {}", p.display());
    }
    let ext_ok = p
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| IMAGE_EXTS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false);
    if !ext_ok {
        anyhow::bail!("not an image (png/jpg/webp/gif/bmp): {}", p.display());
    }
    if meta.len() > MAX_IMAGE_BYTES {
        anyhow::bail!(
            "too big ({}MB cap): {}",
            MAX_IMAGE_BYTES / 1024 / 1024,
            p.display()
        );
    }
    Ok(p)
}

/// One session row for the picker. Mirrors the `sessions list` print
/// dialect: `current` renders as `*` (same mark, same meaning).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub id: String,
    pub title: String,
    pub current: bool,
}

/// Session store access for the picker. Implemented by the CLI over `Store`;
/// tests use `FakeSessions`. Chat turns stay fresh-conversation (c76 seam)
/// — the picker manages the store's active session, it does NOT bind turns
/// (queued follow-up, stated).
pub trait SessionBackend {
    fn list(&self) -> impl Future<Output = anyhow::Result<Vec<SessionSummary>>> + Send;
    fn switch(&self, id: String) -> impl Future<Output = anyhow::Result<()>> + Send;
    fn create(&self, title: String) -> impl Future<Output = anyhow::Result<SessionSummary>> + Send {
        async move {
            Ok(SessionSummary {
                id: "s-default".to_string(),
                title,
                current: true,
            })
        }
    }
    fn rename(&self, id: String, title: String) -> impl Future<Output = anyhow::Result<()>> + Send {
        async move {
            let _ = (id, title);
            Ok(())
        }
    }
    fn delete(&self, id: String) -> impl Future<Output = anyhow::Result<()>> + Send {
        async move {
            let _ = id;
            Ok(())
        }
    }
    /// Load persisted turns for one session (oldest-first, capped).
    /// The `session_id` rides along because the backend is db-scoped while
    /// the App is session-scoped (c86 refinement of the c85 sketch).
    fn history(
        &self,
        session_id: &str,
    ) -> impl Future<Output = anyhow::Result<Vec<ChatRecord>>> + Send;
    /// Retrieve the most recent circuit execution log for this session.
    fn latest_circuit_log(
        &self,
        session_id: &str,
    ) -> impl Future<Output = anyhow::Result<Vec<String>>> + Send {
        async move {
            let _ = session_id;
            Ok(Vec::new())
        }
    }
    /// Persist one turn. Failures surface to the caller (the loop renders
    /// them fail-soft) — storage must never kill chat.
    fn append(
        &self,
        session_id: &str,
        role: TurnRole,
        text: String,
        circuit_log: &[String],
    ) -> impl Future<Output = anyhow::Result<()>> + Send;
}

/// Picker selection state. `selected` is always clamped into `sessions`
/// (empty list is safe: every action no-ops, `selected_id` is `None`).
/// Visibility lives on `Overlay`, not here — one overlay at a time by
/// construction (c78), never by cross-checking bools.
#[derive(Debug, Default)]
pub struct PickerState {
    pub sessions: Vec<SessionSummary>,
    pub selected: usize,
}

/// Which pane is on screen. Exactly one — overlays never stack (c78 rule).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Overlay {
    #[default]
    Chat,
    Picker,
    Diff,
    Tasks,
    Prompt,
    Gutter,
    Circuit,
}

/// Pure key-routing decision (chunk 146): the loop matches on this, so
/// every routing rule is headless-testable. Toggles fire from ANY overlay
/// (loop precedence preserved); overlay-local keys only in their overlay;
/// non-Press kinds are `Ignore` (the tent.txt Windows rule, moved in from
/// the loop-entry guard). Async work stays in `run()` under each arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyIntent {
    Ignore,
    TogglePicker,
    ToggleDiff,
    ToggleTasks,
    ToggleGutter,
    ToggleCircuit,
    ScrollPages(i32),
    ScrollInspection(i32),
    /// Scroll the open Diff/Tasks/Gutter content itself (right pane).
    /// Split rules, modifier-free for legacy cmd.exe: page keys move the
    /// left transcript everywhere; arrows move the right pane content
    /// (trace in F5, own content in F2/F3/F4).
    ScrollOverlay(i32),
    PickerMove(i32),
    PickerSwitch,
    PickerCreate,
    PickerDelete,
    PickerRename,
    PickerClose,
    PromptAnswer(bool),
    OverlayClose,
    Submit,
    ClearInput,
    Quit,
    TypeKey,
}

/// Read-only worktree snapshot for the diff pane: short-status plus
/// diffstat, both pre-rendered text (the backend owns the git call).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiffSnapshot {
    pub status: String,
    pub stat: String,
}

/// Worktree access for the diff pane. Implemented by the CLI over the git
/// CLI facade (ADR-007); tests use `FakeDiff`.
pub trait DiffBackend {
    fn snapshot(&self) -> impl Future<Output = anyhow::Result<DiffSnapshot>> + Send;
}

/// One plan row for the tasks pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRow {
    pub id: String,
    pub title: String,
    pub status: String,
}

/// Plan-file snapshot for the tasks pane: title + rows (empty = no plan
/// file found, rendered as an honest note, never an error).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlanSnapshot {
    pub title: String,
    pub rows: Vec<TaskRow>,
}

/// Plan-file access for the tasks pane. Implemented by the CLI over
/// `polymorph-tasks::plan::parse`; tests use `FakeTasks`.
pub trait TaskBackend {
    fn load(&self) -> impl Future<Output = anyhow::Result<PlanSnapshot>> + Send;
}

/// One diagnostics row for the gutter pane. Location is pre-rendered by
/// the backend (`file:line` dialect); the pane only lays rows out (the
/// diff-pane precedent: backends own content, the App owns chrome).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GutterRow {
    pub location: String,
    pub message: String,
}

/// Diagnostics snapshot for the gutter pane: rows, or empty = the honest
/// note (never an error — clean tree and no-server-yet render the same).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GutterSnapshot {
    pub rows: Vec<GutterRow>,
}

/// Diagnostics access for the gutter pane. Implemented by the CLI over a
/// shared pool's recorded pulls; tests use `FakeGutter`.
pub trait GutterBackend {
    fn load(&self) -> impl Future<Output = anyhow::Result<GutterSnapshot>> + Send;
}

/// Empty gutter (bare-TUI placeholder): no shared pool exists on the bare
/// path, so the pane honestly renders its empty note until the TUI shares
/// a pool with a tool loop (the chunk-90 channel trigger — same shape).
#[derive(Debug, Clone, Copy, Default)]
pub struct EmptyGutter;

impl GutterBackend for EmptyGutter {
    async fn load(&self) -> anyhow::Result<GutterSnapshot> {
        Ok(GutterSnapshot::default())
    }
}

/// Which side spoke a chat record (TUI-native vocabulary — the CLI maps
/// User→`MessageRole::User`, Model→`Assistant` per the ACP precedent, so
/// the tui crate stays core-free).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnRole {
    User,
    Model,
}

/// One persisted chat line (history in, append out).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatRecord {
    pub role: TurnRole,
    pub text: String,
}

/// How many rows a history load takes (startup bound; older rows stay
/// in-store, scrollback covers the loaded window).
pub const HISTORY_LIMIT: usize = 50;

/// Scrollback page step (lines per PgUp/PgDn). Fixed so headless tests and
/// the live loop agree without passing areas into the state machine.
pub const PAGE_LINES: usize = 10;

/// Pending ask-user question for the prompt overlay (chunk 89, D3):
/// which tool wants to run + the handler-supplied question. Binary v1 —
/// no free-text reason entry (the decision + tool name are the record).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PromptState {
    pub tool: String,
    pub question: String,
}

/// One CLI-parity slash command: name + args → reply text for the
/// transcript. Implemented by the CLI over its stores/config/catalog;
/// tests use fakes. Unknown names must Err naming the valid set
/// (strict-vocabulary precedent — a typo never silently becomes a model
/// turn). Session-local commands (/new, /rename, /delete, /extend) stay
/// in the loop; everything else rides here.
pub trait CommandHandler {
    fn handle(&self, cmd: &str, args: &str) -> impl Future<Output = anyhow::Result<String>> + Send;
}

/// No-op handler (headless/tests without CLI wiring): every slash
/// reports unknown instead of hitting a model turn by accident.
pub struct NoCommands;

impl CommandHandler for NoCommands {
    async fn handle(&self, cmd: &str, _args: &str) -> anyhow::Result<String> {
        anyhow::bail!("unknown command /{cmd} (no CLI wiring — try /help in the app)")
    }
}

/// Split a `/command args` line. `None` = not a slash line. The name is
/// lowercased (commands are case-insensitive); args keep their case.
/// Existing session-local commands match first in the loop, so this
/// never shadows them — it only routes what they decline.
pub fn split_slash(input: &str) -> Option<(String, String)> {
    let trimmed = input.trim();
    let rest = trimmed.strip_prefix('/')?;
    if rest.trim().is_empty() {
        return None;
    }
    let mut parts = rest.splitn(2, char::is_whitespace);
    let cmd = parts.next().unwrap_or("").to_ascii_lowercase();
    if cmd.is_empty() {
        return None;
    }
    let args = parts.next().unwrap_or("").trim().to_string();
    Some((cmd, args))
}

/// Chat application state: message log + input line + overlay panes
/// (picker/diff/tasks/prompt) + scrollback.
#[derive(Debug)]
pub struct App {
    messages: Vec<Message>,
    input: String,
    overlay: Overlay,
    picker: PickerState,
    diff: DiffSnapshot,
    plan: PlanSnapshot,
    scroll: usize,
    status: String,
    theme: Theme,
    bound: Option<String>,
    prompt: PromptState,
    pending_loop: Option<drive::DriveIntent>,
    /// Staged vision images for the next chat turn (consumed on submit).
    pending_images: Vec<std::path::PathBuf>,
    gutter: GutterSnapshot,
    circuit_log: Vec<String>,
    inspection_scroll: usize,
    /// Scroll offset for the open Diff/Tasks/Gutter content itself.
    /// Separate from chat scroll (left pane) and inspection scroll
    /// (trace): three panes, three offsets, zero ambiguity.
    overlay_scroll: usize,
    /// Live-follow: new content pins to the bottom while true; any
    /// manual scroll up drops it (scrolling back down re-engages).
    /// True by default — a fresh screen always follows.
    follow_inspection: bool,
    should_quit: bool,
}

impl Default for App {
    fn default() -> Self {
        Self {
            messages: Vec::new(),
            input: String::new(),
            overlay: Overlay::default(),
            picker: PickerState::default(),
            diff: DiffSnapshot::default(),
            plan: PlanSnapshot::default(),
            scroll: 0,
            status: String::new(),
            theme: Theme::default(),
            bound: None,
            prompt: PromptState::default(),
            pending_loop: None,
            pending_images: Vec::new(),
            gutter: GutterSnapshot::default(),
            circuit_log: Vec::new(),
            inspection_scroll: 0,
            overlay_scroll: 0,
            follow_inspection: true,
            should_quit: false,
        }
    }
}

impl App {
    /// Pure key routing (chunk 146): overlay + code + modifiers + kind →
    /// intent. Mirrors the `run()` precedence arm-for-arm: toggles first
    /// (any overlay), then scroll, then overlay-local, then chat submit,
    /// else type. Esc in chat clears non-empty input, quits on empty (R1).
    pub fn classify_key(&self, key: KeyEvent) -> KeyIntent {
        if !Self::is_press(&key) {
            return KeyIntent::Ignore;
        }
        if key.code == KeyCode::Tab {
            return KeyIntent::TogglePicker;
        }
        if key.code == KeyCode::F(1) {
            return KeyIntent::OverlayClose;
        }
        if key.code == KeyCode::F(2) {
            return KeyIntent::ToggleDiff;
        }
        if key.code == KeyCode::F(3) {
            return KeyIntent::ToggleTasks;
        }
        if key.code == KeyCode::F(4) {
            return KeyIntent::ToggleGutter;
        }
        if key.code == KeyCode::F(5) {
            return KeyIntent::ToggleCircuit;
        }
        if key.code == KeyCode::PageUp {
            // Shift pages scroll inspection everywhere (legacy cmd.exe
            // never delivers Alt+arrows — the 162334 screenshot class).
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                return KeyIntent::ScrollInspection(PAGE_LINES as i32);
            }
            return KeyIntent::ScrollPages(1);
        }
        if key.code == KeyCode::PageDown {
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                return KeyIntent::ScrollInspection(-(PAGE_LINES as i32));
            }
            return KeyIntent::ScrollPages(-1);
        }
        match self.overlay {
            Overlay::Picker => match key.code {
                KeyCode::Esc => KeyIntent::PickerClose,
                KeyCode::Up => KeyIntent::PickerMove(-1),
                KeyCode::Down => KeyIntent::PickerMove(1),
                KeyCode::Enter => KeyIntent::PickerSwitch,
                KeyCode::Char('c') | KeyCode::Char('n') => KeyIntent::PickerCreate,
                KeyCode::Char('d') | KeyCode::Delete => KeyIntent::PickerDelete,
                KeyCode::Char('r') => KeyIntent::PickerRename,
                _ => KeyIntent::Ignore,
            },
            Overlay::Prompt => match Self::prompt_key(key) {
                Some(allow) => KeyIntent::PromptAnswer(allow),
                None => KeyIntent::Ignore,
            },
            Overlay::Diff | Overlay::Tasks | Overlay::Gutter => {
                // Page keys reach the left transcript (global arm above);
                // plain arrows scroll this pane's own content (170346
                // class: an overflowing pane with no keys is a dead end).
                match key.code {
                    KeyCode::Esc => KeyIntent::OverlayClose,
                    KeyCode::Up => KeyIntent::ScrollOverlay(1),
                    KeyCode::Down => KeyIntent::ScrollOverlay(-1),
                    _ => KeyIntent::Ignore,
                }
            }
            Overlay::Circuit => match key.code {
                KeyCode::Esc => KeyIntent::OverlayClose,
                KeyCode::PageUp => KeyIntent::ScrollInspection(PAGE_LINES as i32),
                KeyCode::PageDown => KeyIntent::ScrollInspection(-(PAGE_LINES as i32)),
                KeyCode::Up => KeyIntent::ScrollInspection(1),
                KeyCode::Down => KeyIntent::ScrollInspection(-1),
                _ => KeyIntent::Ignore,
            },
            Overlay::Chat => {
                let shift = key.modifiers.contains(KeyModifiers::SHIFT);
                // Shift+arrows scroll the inspection pane (legacy cmd.exe
                // never delivers Alt+arrows — the 162334 screenshot class).
                // Plain arrows type/navigate; Alt variants stay for
                // terminals that deliver them (Windows Terminal does).
                if shift {
                    match key.code {
                        KeyCode::Up => return KeyIntent::ScrollInspection(1),
                        KeyCode::Down => return KeyIntent::ScrollInspection(-1),
                        _ => {}
                    }
                }
                if key.modifiers.contains(KeyModifiers::ALT) {
                    match key.code {
                        KeyCode::Up => return KeyIntent::ScrollInspection(1),
                        KeyCode::Down => return KeyIntent::ScrollInspection(-1),
                        KeyCode::PageUp => return KeyIntent::ScrollInspection(PAGE_LINES as i32),
                        KeyCode::PageDown => {
                            return KeyIntent::ScrollInspection(-(PAGE_LINES as i32))
                        }
                        _ => {}
                    }
                }
                if key.code == KeyCode::Enter {
                    KeyIntent::Submit
                } else if key.code == KeyCode::Esc {
                    if self.input.is_empty() {
                        KeyIntent::Quit
                    } else {
                        KeyIntent::ClearInput
                    }
                } else {
                    KeyIntent::TypeKey
                }
            }
        }
    }

    /// Clear the input line, keeping status and history (R1: Esc
    /// defocuses before it quits).
    pub fn clear_input(&mut self) {
        self.input.clear();
    }

    /// Quit flag (Esc on empty input; Ctrl-C path unchanged in
    /// `handle_key`).
    pub fn quit(&mut self) {
        self.should_quit = true;
    }

    /// Pure key handling — no terminal, fully testable. Esc / Ctrl-C quits;
    /// printable chars append (NONE- or SHIFT-modified; Ctrl/Alt-modified
    /// chars are ignored); Backspace pops; Enter submits (see
    /// `take_input`). Matches quit on (`code`, `modifiers`) exactly: a bare
    /// `c` types, Ctrl-C quits. Ignored wholesale while an overlay is open
    /// (overlay keystrokes are loop-owned: nav/switch/close). Key-event
    /// kind (Press/Release/Repeat) is filtered at the `run()` entry — this
    /// fn sees Press only by contract.
    pub fn handle_key(&mut self, key: KeyEvent) {
        if self.overlay != Overlay::Chat {
            return;
        }
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) => self.should_quit = true,
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => self.should_quit = true,
            (KeyCode::Char(c), m) if m == KeyModifiers::NONE || m == KeyModifiers::SHIFT => {
                self.input.push(c)
            }
            (KeyCode::Backspace, _) => {
                self.input.pop();
            }
            _ => {}
        }
    }

    /// Press-only entry guard (tent.txt Windows fix): Release/Repeat
    /// events are dropped by the `run()` loop before routing. Unit-covered
    /// here so the rule survives without an async harness.
    pub fn is_press(key: &KeyEvent) -> bool {
        key.kind == KeyEventKind::Press
    }

    /// Enter handling, separated for headless testing: blank input submits
    /// nothing (`None`); otherwise moves the line out and records the user
    /// message. The caller awaits the responder and records the reply via
    /// `push_model` / `push_error`.
    pub fn take_input(&mut self) -> Option<String> {
        if self.input.trim().is_empty() {
            return None;
        }
        let prompt = std::mem::take(&mut self.input);
        self.scroll = 0; // new content re-pins to the live bottom
        self.messages.push(Message {
            role: Role::User,
            text: prompt.clone(),
        });
        Some(prompt)
    }

    /// Record a model reply.
    pub fn push_model(&mut self, text: String) {
        self.scroll = 0; // new content re-pins to the live bottom
        self.messages.push(Message {
            role: Role::Model,
            text,
        });
    }

    /// Record a failed turn WITHOUT killing the session (fail-soft: a bad
    /// key or a 429 must not strand the user in a dead TUI). Returns the
    /// recorded text so the loop can persist the same line (c86: no format
    /// duplication between memory and store).
    pub fn push_error(&mut self, err: anyhow::Error) -> String {
        let text = format!("error: {err:#}");
        self.push_model(text.clone());
        text
    }

    /// Open the picker over a freshly listed backend snapshot. Selection
    /// starts on the current session (row 0 fallback); status clears.
    pub fn open_picker(&mut self, sessions: Vec<SessionSummary>) {
        let mut selected = sessions.iter().position(|s| s.current).unwrap_or(0);
        if !sessions.is_empty() {
            selected = selected.min(sessions.len() - 1);
        }
        self.picker = PickerState { sessions, selected };
        self.overlay = Overlay::Picker;
        self.status.clear();
    }

    /// Close whichever overlay is open, leaving chat state untouched.
    pub fn close_overlay(&mut self) {
        self.overlay = Overlay::Chat;
    }

    /// Open the ask-user prompt overlay (D3: exclusive, input preserved).
    /// Overlays never stack (c78 rule) — the prompt takes precedence.
    pub fn open_prompt(&mut self, tool: String, question: String) {
        self.prompt = PromptState { tool, question };
        self.overlay = Overlay::Prompt;
        self.status.clear();
    }

    /// Answer the pending prompt: records the decision in the status line
    /// and returns to chat. The caller owns the underlying tool verdict;
    /// this only closes the surface (D3: session-only, no persistence).
    pub fn answer_prompt(&mut self, allow: bool) {
        let tool = self.prompt.tool.clone();
        self.status = if allow {
            format!("allowed {tool}")
        } else {
            format!("denied {tool}")
        };
        self.overlay = Overlay::Chat;
    }

    /// Prompt key mapping, separated for headless testing: `y` allows,
    /// `n` / `Esc` denies, anything else is ignored (`None`). Uppercase
    /// `Y`/`N` arrive SHIFT-modified on most terminals, so NONE or SHIFT
    /// both count. The loop feeds `Some` into `answer_prompt`.
    pub fn prompt_key(key: KeyEvent) -> Option<bool> {
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) => Some(false),
            (KeyCode::Char('y') | KeyCode::Char('Y'), m)
                if m == KeyModifiers::NONE || m == KeyModifiers::SHIFT =>
            {
                Some(true)
            }
            (KeyCode::Char('n') | KeyCode::Char('N'), m)
                if m == KeyModifiers::NONE || m == KeyModifiers::SHIFT =>
            {
                Some(false)
            }
            _ => None,
        }
    }

    /// Move selection by `delta` rows, clamped (never wraps, empty-safe).
    pub fn move_selection(&mut self, delta: i32) {
        if self.picker.sessions.is_empty() {
            return;
        }
        let max = self.picker.sessions.len() - 1;
        let next = self.picker.selected as i32 + delta;
        self.picker.selected = next.clamp(0, max as i32) as usize;
    }

    /// Highlighted session id, `None` on an empty list.
    pub fn selected_id(&self) -> Option<String> {
        self.picker
            .sessions
            .get(self.picker.selected)
            .map(|s| s.id.clone())
    }

    /// Show a diff snapshot (closes other overlays by construction).
    pub fn open_diff(&mut self, snapshot: DiffSnapshot) {
        self.diff = snapshot;
        self.overlay = Overlay::Diff;
        self.overlay_scroll = 0;
        self.status.clear();
    }

    /// Show a plan snapshot (closes other overlays by construction).
    pub fn open_tasks(&mut self, snapshot: PlanSnapshot) {
        self.plan = snapshot;
        self.overlay = Overlay::Tasks;
        self.overlay_scroll = 0;
        self.status.clear();
    }

    /// Show a diagnostics snapshot (closes other overlays by construction).
    pub fn open_gutter(&mut self, snapshot: GutterSnapshot) {
        self.gutter = snapshot;
        self.overlay = Overlay::Gutter;
        self.overlay_scroll = 0;
        self.status.clear();
    }

    /// Show the full-screen circuit trace overlay (closes other overlays by construction).
    pub fn open_circuit(&mut self) {
        self.overlay = Overlay::Circuit;
        self.status.clear();
    }

    /// Read-only slice of the latest circuit execution log.
    pub fn circuit_log(&self) -> &[String] {
        &self.circuit_log
    }

    /// Set the circuit log (new content pins to the live bottom ONLY
    /// while following — a scrolled-up operator keeps their place
    /// instead of being yanked on every update).
    pub fn set_circuit_log(&mut self, log: Vec<String>) {
        self.circuit_log = log;
        self.pin_inspection();
    }

    /// Re-pin to the live bottom when following; no-op while the
    /// operator is scrolled up reading history.
    pub fn pin_inspection(&mut self) {
        if self.follow_inspection {
            self.inspection_scroll = 0;
        }
    }

    /// Stage a confirmed-spend loop intent (NL routing): the prompt
    /// overlay asks first; only `allow` launches. One pending at a time.
    pub fn stage_loop(&mut self, intent: drive::DriveIntent) {
        self.pending_loop = Some(intent);
    }

    /// Take staged vision images (submit consumes exactly once).
    pub fn take_images(&mut self) -> Vec<std::path::PathBuf> {
        std::mem::take(&mut self.pending_images)
    }

    /// Take the staged intent (confirm path consumes exactly once).
    pub fn take_pending_loop(&mut self) -> Option<drive::DriveIntent> {
        self.pending_loop.take()
    }

    /// Scroll the inspection pane by `delta` lines. Positive scrolls
    /// toward older content and drops the live-follow; scrolling back
    /// down to the bottom re-engages it.
    pub fn scroll_inspection(&mut self, delta: i32) {
        let next = self.inspection_scroll as i32 + delta;
        if next <= 0 {
            self.inspection_scroll = 0;
            self.follow_inspection = true;
        } else {
            self.inspection_scroll = next as usize;
            self.follow_inspection = false;
        }
    }

    /// Scroll the open Diff/Tasks/Gutter content by `delta` rendered
    /// rows (clamped at zero; content windows clamp the top).
    pub fn scroll_overlay(&mut self, delta: i32) {
        let next = self.overlay_scroll as i32 + delta;
        self.overlay_scroll = next.max(0) as usize;
    }

    /// Rendered rows one logical line occupies at `width` columns
    /// (wrapping-aware — counting array items instead is why scrolling
    /// felt dead on wrapped review prose).
    pub fn wrap_rows(line: &str, width: usize) -> usize {
        use ratatui::text::Line;
        let width = width.max(1);
        let w = Line::from(line).width().max(1);
        w.div_ceil(width).max(1)
    }

    /// Clamped offset for a window: banked-off capped at total-page.
    /// Single source — `inspection_window` and footers agree by construction.
    pub fn window_off(lines: &[String], page: usize, width: usize, off: usize) -> usize {
        let total: usize = lines.iter().map(|l| Self::wrap_rows(l, width)).sum();
        off.min(total.saturating_sub(page))
    }

    /// Bottom-pinned window over logical lines: `off` rendered rows up
    /// from the live bottom, `page` rows tall at `width` columns.
    /// Returns the logical lines covering the window (whole lines — a
    /// partially visible top line renders fully, same as before).
    pub fn inspection_window(
        lines: &[String],
        page: usize,
        width: usize,
        off: usize,
    ) -> Vec<String> {
        if lines.is_empty() || page == 0 {
            return Vec::new();
        }
        let rows: Vec<usize> = lines.iter().map(|l| Self::wrap_rows(l, width)).collect();
        let total: usize = rows.iter().sum();
        let off = Self::window_off(lines, page, width, off);
        // Rows to skip from the top = everything above the window.
        let mut skip = total.saturating_sub(page).saturating_sub(off);
        let mut out = Vec::new();
        for (line, &r) in lines.iter().zip(rows.iter()) {
            if skip >= r {
                skip -= r;
                continue;
            }
            out.push(line.clone());
        }
        out
    }

    /// Scroll the log by `delta` pages (PgUp/PgDn). Clamped at draw time
    /// against the visible height (the state machine never sees the area);
    /// any new content re-pins to the live bottom (see the push paths).
    pub fn scroll_by(&mut self, pages: i32) {
        let next = self.scroll as i32 + pages * PAGE_LINES as i32;
        self.scroll = next.max(0) as usize;
    }

    /// Switch the chrome theme (selection surface for a future `--theme`
    /// flag / config key — the API lands first, wiring follows).
    pub fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
    }

    /// One-line result note for the status line (fail-soft switch feedback
    /// lives here, not in a crash).
    pub fn set_status(&mut self, status: String) {
        self.status = status;
    }

    /// Clear the status line note.
    pub fn clear_status(&mut self) {
        self.status.clear();
    }

    /// Bind the log to a session's persisted turns (startup load +
    /// switch reload): replaces in-memory rows, re-pins scroll, tracks the
    /// bound id. Input line preserved (reloads must not eat typing).
    pub fn load_history(&mut self, session_id: String, records: Vec<ChatRecord>) {
        // Defensive cap (backends should page too): keep the last window.
        let tail: Vec<ChatRecord> = records
            .into_iter()
            .rev()
            .take(HISTORY_LIMIT)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        self.messages = tail
            .into_iter()
            .map(|r| Message {
                role: match r.role {
                    TurnRole::User => Role::User,
                    TurnRole::Model => Role::Model,
                },
                text: r.text,
            })
            .collect();
        self.scroll = 0;
        self.bound = Some(session_id);
    }

    /// Render content for whichever overlay is active.
    fn overlay_content(&self) -> Vec<String> {
        match self.overlay {
            Overlay::Picker => {
                if self.picker.sessions.is_empty() {
                    vec!["no sessions here — run `polymorph sessions create`".to_string()]
                } else {
                    self.picker
                        .sessions
                        .iter()
                        .enumerate()
                        .map(|(i, s)| {
                            let cursor = if i == self.picker.selected {
                                "> "
                            } else {
                                "  "
                            };
                            let mark = if s.current { "*" } else { " " };
                            format!("{cursor}{mark} {}  {}", s.id, s.title)
                        })
                        .collect()
                }
            }
            Overlay::Diff => {
                let mut v = vec!["worktree:".to_string()];
                v.extend(self.diff.status.lines().map(str::to_string));
                v.push(String::new());
                v.extend(self.diff.stat.lines().map(str::to_string));
                v
            }
            Overlay::Tasks => {
                if self.plan.rows.is_empty() {
                    vec!["no ./PLAN.md — run `review plan-show` to start one".to_string()]
                } else {
                    std::iter::once(format!("plan: {}", self.plan.title))
                        .chain(
                            self.plan
                                .rows
                                .iter()
                                .map(|r| format!("[{}] {}  {}", r.status, r.id, r.title)),
                        )
                        .collect()
                }
            }
            Overlay::Prompt => vec![
                format!("allow `{}`?", self.prompt.tool),
                self.prompt.question.clone(),
                String::new(),
            ],
            Overlay::Gutter => {
                if self.gutter.rows.is_empty() {
                    vec!["no diagnostics — none recorded yet".to_string()]
                } else {
                    self.gutter
                        .rows
                        .iter()
                        .map(|r| format!("{}  {}", r.location, r.message))
                        .collect()
                }
            }
            Overlay::Circuit => {
                if self.circuit_log.is_empty() {
                    vec![
                        "Circuit Execution Trace:".to_string(),
                        "  No circuit runs recorded yet in this session.".to_string(),
                        String::new(),
                        "  When an engineering task is submitted, the 4-stage loop".to_string(),
                        "  (Plan → Act → Review → Converge) traces each stage here.".to_string(),
                    ]
                } else {
                    self.circuit_log
                        .iter()
                        .flat_map(|entry| entry.lines())
                        .map(str::to_string)
                        .collect()
                }
            }
            Overlay::Chat => self.chat_lines(),
        }
    }

    /// Render formatted chat transcript lines.
    fn chat_lines(&self) -> Vec<String> {
        self.messages
            .iter()
            .map(|m| {
                let who = match m.role {
                    Role::User => "you",
                    Role::Model => "model",
                };
                format!("{who}: {}", m.text)
            })
            .collect()
    }

    /// Compute status bar string.
    fn status_line(&self, off: usize) -> String {
        let mut status = match self.overlay {
            Overlay::Picker => {
                "Arrows move · Enter switch · d delete · r rename · Esc close".to_string()
            }
            Overlay::Diff => "PgUp/PgDn transcript · Up/Dn here · Esc close".to_string(),
            Overlay::Tasks => "PgUp/PgDn transcript · Up/Dn here · Esc close".to_string(),
            Overlay::Gutter => "PgUp/PgDn transcript · Up/Dn here · Esc close".to_string(),
            Overlay::Circuit => "PgUp/PgDn transcript · Up/Dn trace · Esc close".to_string(),
            Overlay::Prompt => "y allow · n deny · Esc deny".to_string(),
            Overlay::Chat => {
                "Tab pick · F2 diff · F3 tasks · F4 diagnostics · F5 circuit · Enter send · PgUp transcript"
                    .to_string()
            }
        };
        if self.overlay == Overlay::Chat && self.bound.is_none() {
            status.push_str(" · unbound — Tab: pick");
        }
        if off > 0 {
            status.push_str(&format!(" · scroll {off}"));
        }
        if !self.status.is_empty() {
            status.push_str(" · ");
            status.push_str(&self.status);
        }
        status
    }

    /// Render: on standard terminal screens (height >= 12, width >= 50),
    /// draws a true multi-pane TUI layout with side-by-side inspection
    /// (when width >= 90), pinned bottom input box, and live terminal
    /// cursor positioning. On compact viewports (<12 rows), falls back
    /// to single-block rendering.
    pub fn draw(&self, frame: &mut Frame) {
        let area = frame.area();

        if area.height >= 12 && area.width >= 50 {
            // Standard multi-pane layout:
            // [0] Header (1 row)
            // [1] Main Body (Chat + optional Inspection side pane) (Min 6 rows)
            // [2] Input box (3 rows, bordered)
            // [3] Footer status line (1 row)
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(1),
                    Constraint::Min(6),
                    Constraint::Length(3),
                    Constraint::Length(1),
                ])
                .split(area);

            let header_area = chunks[0];
            let body_area = chunks[1];
            let input_area = chunks[2];
            let footer_area = chunks[3];

            // 1. Header
            let session_title = match &self.bound {
                Some(id) => format!("session: {}", &id[..id.len().min(8)]),
                None => "unbound".to_string(),
            };
            let overlay_tag = match self.overlay {
                Overlay::Chat => "Chat Mode",
                Overlay::Diff => "Diff Pane (F2)",
                Overlay::Tasks => "Tasks Pane (F3)",
                Overlay::Gutter => "Findings/Gutter (F4)",
                Overlay::Picker => "Session Picker (Tab)",
                Overlay::Prompt => "Action Permission (Prompt)",
                Overlay::Circuit => "Circuit Trace (F5)",
            };
            let header_text = format!(" polymorph · {session_title} · {overlay_tag} ");
            let header = Paragraph::new(header_text).style(self.theme.title_style());
            frame.render_widget(header, header_area);

            // 2. Main Body: Split or Full Pane
            let chat_content = self.chat_lines();
            let page = (body_area.height as usize).saturating_sub(2);
            // Wrap-aware like inspection (tent3 class repeated here:
            // long model replies wrap, so logical counts collapse the
            // scroll range to nothing). Width is estimated pre-split
            // (55/45); a 1-col rounding miss only nudges edge lines.
            let chat_width = if body_area.width >= 90 {
                (body_area.width as usize * 55 / 100).saturating_sub(2)
            } else {
                (body_area.width as usize).saturating_sub(2)
            };
            let visible_chat =
                Self::inspection_window(&chat_content, page, chat_width, self.scroll);
            // Honest footer: the clamped offset, not the raw banked one
            // (banked 30 clamped to 7 must read 7 — a number that stops
            // growing IS the top indicator).
            let chat_off = Self::window_off(&chat_content, page, chat_width, self.scroll);

            if body_area.width >= 90 {
                // Side-by-side split
                let main_chunks = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
                    .split(body_area);

                // Left: Chat / Transcript
                let chat_p = Paragraph::new(visible_chat.join("\n"))
                    .block(
                        Block::bordered()
                            .title(" Chat / Transcript ")
                            .border_style(self.theme.border_style())
                            .title_style(self.theme.title_style()),
                    )
                    .wrap(Wrap { trim: false });
                frame.render_widget(chat_p, main_chunks[0]);

                // Right: Inspection / Overlay
                let (right_title, right_lines) = match self.overlay {
                    Overlay::Chat => {
                        let overview = if !self.circuit_log.is_empty() {
                            let mut lines = vec![
                                "Circuit Trace (Shift+Up/Dn scroll):".to_string(),
                                "─────────────────────────────────".to_string(),
                            ];
                            let page = (body_area.height as usize).saturating_sub(4);
                            let width = (main_chunks[1].width as usize).saturating_sub(2);
                            let flat: Vec<String> = self
                                .circuit_log
                                .iter()
                                .flat_map(|entry| entry.lines())
                                .map(str::to_string)
                                .collect();
                            lines.extend(Self::inspection_window(
                                &flat,
                                page,
                                width,
                                self.inspection_scroll,
                            ));
                            lines
                        } else {
                            let mut ov = vec![
                                "Inspection Quickview:".to_string(),
                                "  [Tab] Sessions Switcher".to_string(),
                                "  [F2]  Git Worktree Diff".to_string(),
                                "  [F3]  Task Plan (PLAN.md)".to_string(),
                                "  [F4]  Findings & Diagnostics".to_string(),
                                "  [F5]  Circuit Execution Trace".to_string(),
                                String::new(),
                                "Circuit Loop:".to_string(),
                                "  Plan → Act → Review → Converge".to_string(),
                                String::new(),
                            ];
                            if !self.diff.status.is_empty() {
                                ov.push("Latest Git Status:".to_string());
                                ov.extend(self.diff.status.lines().take(4).map(str::to_string));
                            } else if !self.plan.rows.is_empty() {
                                ov.push(format!("Plan Tasks ({}):", self.plan.rows.len()));
                                ov.extend(
                                    self.plan
                                        .rows
                                        .iter()
                                        .take(4)
                                        .map(|r| format!("[{}] {}", r.status, r.title)),
                                );
                            }
                            ov
                        };
                        (" Inspection Quickview ", overview)
                    }
                    Overlay::Diff | Overlay::Tasks | Overlay::Gutter => {
                        let title = match self.overlay {
                            Overlay::Diff => " Git Diff / Worktree ",
                            Overlay::Tasks => " Task Plan / PLAN.md ",
                            _ => " Findings & Diagnostics ",
                        };
                        let page = (body_area.height as usize).saturating_sub(2);
                        let width = (main_chunks[1].width as usize).saturating_sub(2);
                        let content = self.overlay_content();
                        let visible =
                            Self::inspection_window(&content, page, width, self.overlay_scroll);
                        (title, visible)
                    }
                    Overlay::Picker => (" Session Switcher ", self.overlay_content()),
                    Overlay::Prompt => (" Tool Approval Prompt ", self.overlay_content()),
                    Overlay::Circuit => {
                        let page = (body_area.height as usize).saturating_sub(2);
                        let width = (main_chunks[1].width as usize).saturating_sub(2);
                        let content = self.overlay_content();
                        let visible =
                            Self::inspection_window(&content, page, width, self.inspection_scroll);
                        (" Circuit Execution Trace ", visible)
                    }
                };

                let right_p = Paragraph::new(right_lines.join("\n"))
                    .block(
                        Block::bordered()
                            .title(right_title)
                            .border_style(self.theme.border_style())
                            .title_style(self.theme.title_style()),
                    )
                    .wrap(Wrap { trim: false });
                frame.render_widget(right_p, main_chunks[1]);
            } else {
                // Narrow screen: active view takes 100% of body
                let (title, lines) = if self.overlay == Overlay::Chat {
                    (" Chat / Transcript ", visible_chat)
                } else {
                    (
                        match self.overlay {
                            Overlay::Diff => " Git Diff / Worktree ",
                            Overlay::Tasks => " Task Plan / PLAN.md ",
                            Overlay::Gutter => " Findings & Diagnostics ",
                            Overlay::Picker => " Session Switcher ",
                            Overlay::Prompt => " Tool Approval Prompt ",
                            Overlay::Circuit => " Circuit Execution Trace ",
                            Overlay::Chat => " Chat ",
                        },
                        self.overlay_content(),
                    )
                };

                let body_p = Paragraph::new(lines.join("\n"))
                    .block(
                        Block::bordered()
                            .title(title)
                            .border_style(self.theme.border_style())
                            .title_style(self.theme.title_style()),
                    )
                    .wrap(Wrap { trim: false });
                frame.render_widget(body_p, body_area);
            }

            // 3. Input Box (pinned to bottom, dedicated border)
            let input_widget = Paragraph::new(format!("> {}", self.input)).block(
                Block::bordered()
                    .title(" Prompt (Enter: submit · Esc: clear/quit) ")
                    .border_style(self.theme.border_style())
                    .title_style(self.theme.title_style()),
            );
            frame.render_widget(input_widget, input_area);

            // Set terminal cursor position inside the input box!
            let inner_input = input_area.inner(Margin {
                vertical: 1,
                horizontal: 1,
            });
            let char_offset = (self.input.chars().count() as u16) + 2; // account for "> "
            let cursor_x = inner_input.x + char_offset.min(inner_input.width.saturating_sub(1));
            let cursor_y = inner_input.y;
            frame.set_cursor_position(Position::new(cursor_x, cursor_y));

            // 4. Footer status line
            let status_widget =
                Paragraph::new(self.status_line(chat_off)).style(self.theme.status_style());
            frame.render_widget(status_widget, footer_area);
        } else {
            // Compact / Test-fixture fallback (< 12 rows or < 50 cols)
            let page = (area.height as usize).saturating_sub(4);
            let content = self.overlay_content();
            let off = self.scroll.min(content.len().saturating_sub(page));
            let mut lines: Vec<String> = if off > 0 {
                content
                    .into_iter()
                    .rev()
                    .skip(off)
                    .take(page)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect()
            } else {
                content
                    .into_iter()
                    .rev()
                    .take(page)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect()
            };
            lines.push(format!("> {}", self.input));
            lines.push(self.status_line(off));

            let paragraph = Paragraph::new(lines.join("\n"))
                .block(
                    Block::bordered()
                        .title("polymorph")
                        .border_style(self.theme.border_style())
                        .title_style(self.theme.title_style()),
                )
                .wrap(Wrap { trim: false });
            frame.render_widget(paragraph, area);

            // Set cursor for compact view as well
            if area.height >= 3 && area.width >= 4 {
                let cursor_y = area.y
                    + (lines.len().saturating_sub(2) as u16).min(area.height.saturating_sub(2));
                let cursor_x = area.x
                    + 1
                    + 2
                    + (self.input.chars().count() as u16).min(area.width.saturating_sub(4));
                frame.set_cursor_position(Position::new(cursor_x, cursor_y));
            }
        }
    }
}

/// Startup bind (chunk 86): derive the active session from the list snapshot
/// (D5 — no extra round-trip), load its history, fail-soft everything.
/// No current session = unbound with the draw hint (never auto-create, D4).
async fn bind_initial(app: &mut App, sessions: &impl SessionBackend) {
    let list = match sessions.list().await {
        Ok(list) => list,
        Err(err) => {
            app.set_status(format!("sessions: {err:#}"));
            return;
        }
    };
    let current = list
        .iter()
        .find(|s| s.current)
        .map(|s| s.id.clone())
        .or_else(|| list.first().map(|s| s.id.clone()));
    let Some(id) = current else { return };
    // Resume note (c144): silent binds read as "whose session is this".
    // Short id fits the single status line; full id stays in the picker.
    let short = id.chars().take(8).collect::<String>();
    match sessions.history(&id).await {
        Ok(records) => {
            app.load_history(id.clone(), records);
            let _ = sessions.switch(id.clone()).await;
            if let Ok(log) = sessions.latest_circuit_log(&id).await {
                if !log.is_empty() {
                    app.set_circuit_log(log);
                    app.pin_inspection();
                }
            }
            app.set_status(format!("resumed {short}"));
        }
        Err(err) => app.set_status(format!("history: {err:#}")),
    }
}

/// Persist one turn fail-soft: unbound sessions skip silently (in-memory
/// only, per D4); store errors become status notes, never loop breaks.
async fn persist_turn(
    app: &mut App,
    sessions: &impl SessionBackend,
    role: TurnRole,
    text: String,
    circuit_log: &[String],
) {
    let Some(id) = app.bound.clone() else {
        return;
    };
    if let Err(err) = sessions.append(&id, role, text, circuit_log).await {
        app.set_status(format!("persist: {err:#}"));
    }
}

/// Enter the interactive TUI. Blocks until quit. Restores the terminal on
/// EVERY exit (including `?` bails — the guard below runs before the
/// error propagates). Non-terminal stdout refuses with a named reason
/// instead of hanging in raw mode.
/// In-flight live-loop run (NL-routed drive/review/plan): events apply
/// on completion, Esc cancels mid-run (the turn shape, reused).
type LoopFuture<'a> = std::pin::Pin<
    Box<dyn std::future::Future<Output = anyhow::Result<Vec<drive::DriveEvent>>> + Send + 'a>,
>;

// 8 backends/seams (7-limit + commands + driver): one screen, one call
// site — bundling would churn every fake. Allowed with reason, like the
// bench ctor's too_many_arguments in review/binding.rs.
#[allow(clippy::too_many_arguments)]
pub async fn run<'a>(
    responder: &'a impl Responder,
    sessions: &impl SessionBackend,
    diff: &impl DiffBackend,
    tasks: &impl TaskBackend,
    gutter: &impl GutterBackend,
    commands: &impl CommandHandler,
    driver: &'a impl drive::Driver,
    theme: Theme,
) -> anyhow::Result<()> {
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() {
        anyhow::bail!("no terminal on stdout — run `polymorph` in an interactive terminal");
    }
    crossterm::terminal::enable_raw_mode()
        .map_err(|e| anyhow::anyhow!("cannot enable raw mode: {e}"))?;
    // Raw mode is on past this point: the guard MUST be constructed before
    // any fallible step, or a bail strands the user's terminal.
    let _restore = RestoreTerminal;
    let mut stdout = std::io::stdout();
    crossterm::execute!(stdout, crossterm::terminal::EnterAlternateScreen)
        .map_err(|e| anyhow::anyhow!("cannot enter alternate screen: {e}"))?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal =
        Terminal::new(backend).map_err(|e| anyhow::anyhow!("cannot open terminal: {e}"))?;

    tracing::info!("tui session start");
    let mut app = App::default();
    app.set_theme(theme);
    bind_initial(&mut app, sessions).await;
    let mut events = EventStream::new();
    let mut in_flight: Option<
        std::pin::Pin<
            Box<dyn std::future::Future<Output = anyhow::Result<TurnOutput>> + Send + 'a>,
        >,
    > = None;
    // Live-loop run (NL-routed drive/review/plan): same non-blocking
    // shape as turns — events apply on completion, Esc cancels mid-run.
    let mut in_flight_loop: Option<LoopFuture<'a>> = None;
    while !app.should_quit {
        terminal
            .draw(|frame| app.draw(frame))
            .map_err(|e| anyhow::anyhow!("tui draw failed: {e}"))?;
        let mut completed_turn = None;
        let mut completed_loop = None;

        tokio::select! {
                    turn_res = async {
                        if let Some(ref mut fut) = in_flight {
                            fut.as_mut().await
                        } else {
                            std::future::pending::<anyhow::Result<TurnOutput>>().await
                        }
                    }, if in_flight.is_some() => {
                        completed_turn = Some(turn_res);
                    }
                    loop_res = async {
                        if let Some(ref mut fut) = in_flight_loop {
                            fut.as_mut().await
                        } else {
                            std::future::pending::<anyhow::Result<Vec<drive::DriveEvent>>>().await
                        }
                    }, if in_flight_loop.is_some() => {
                        completed_loop = Some(loop_res);
                    }
                    event_opt = events.next() => {
                        match event_opt {
                            Some(Ok(Event::Key(key))) => {
                        match app.classify_key(key) {
                            KeyIntent::Ignore => {}
                            KeyIntent::TogglePicker => {
                                if app.overlay == Overlay::Picker {
                                    app.close_overlay();
                                } else {
                                    match sessions.list().await {
                                        Ok(list) => app.open_picker(list),
                                        Err(err) => app.set_status(format!("sessions: {err:#}")),
                                    }
                                }
                            }
                            KeyIntent::ToggleDiff => {
                                if app.overlay == Overlay::Diff {
                                    app.close_overlay();
                                } else {
                                    match diff.snapshot().await {
                                        Ok(snap) => app.open_diff(snap),
                                        Err(err) => app.set_status(format!("diff: {err:#}")),
                                    }
                                }
                            }
                            KeyIntent::ToggleTasks => {
                                if app.overlay == Overlay::Tasks {
                                    app.close_overlay();
                                } else {
                                    match tasks.load().await {
                                        Ok(snap) => app.open_tasks(snap),
                                        Err(err) => app.set_status(format!("tasks: {err:#}")),
                                    }
                                }
                            }
                            KeyIntent::ToggleGutter => {
                                // Gutter pane (methods tested; this arm is review-only,
                                // the c86 precedent).
                                if app.overlay == Overlay::Gutter {
                                    app.close_overlay();
                                } else {
                                    match gutter.load().await {
                                        Ok(snap) => app.open_gutter(snap),
                                        Err(err) => app.set_status(format!("gutter: {err:#}")),
                                    }
                                }
                            }
                            KeyIntent::ToggleCircuit => {
                                if app.overlay == Overlay::Circuit {
                                    app.close_overlay();
                                } else {
                                    app.open_circuit();
                                }
                            }
                            KeyIntent::ScrollInspection(delta) => {
                                app.scroll_inspection(delta);
                            }
                            KeyIntent::ScrollOverlay(delta) => {
                                app.scroll_overlay(delta);
                            }
                            KeyIntent::ScrollPages(pages) => {
                                app.scroll_by(pages);
                            }
                            KeyIntent::PickerMove(delta) => app.move_selection(delta),
                            KeyIntent::PickerClose | KeyIntent::OverlayClose => app.close_overlay(),
                            KeyIntent::PickerDelete => {
                                if let Some(id) = app.selected_id() {
                                    match sessions.delete(id.clone()).await {
                                        Ok(()) => {
                                            if app.bound.as_ref() == Some(&id) {
                                                app.bound = None;
                                                app.messages.clear();
                                            }
                                            match sessions.list().await {
                                                Ok(list) => {
                                                    app.open_picker(list);
                                                    app.set_status(format!("deleted {id}"));
                                                }
                                                Err(err) => app.set_status(format!("sessions: {err:#}")),
                                            }
                                        }
                                        Err(err) => app.set_status(format!("delete: {err:#}")),
                                    }
                                }
                            }
                            KeyIntent::PickerRename => {
                                if let Some(id) = app.selected_id() {
                                    app.close_overlay();
                                    app.input = format!("/rename {id} ");
                                    app.set_status("Type new title and press Enter".to_string());
                                }
                            }
                            KeyIntent::PickerSwitch => {
                                if in_flight.is_some() {
                                    in_flight = None;
                                    app.clear_status();
                                }
                                if let Some(id) = app.selected_id() {
                                    match sessions.switch(id.clone()).await {
                                        Ok(()) => {
                                            // Session context follows the picker (D2):
                                            // reload history into the log, fail-soft.
                                            // NOTE: picker opens FIRST, status LAST —
                                            // `open_picker` clears status, so confirming
                                            // before it erases the confirmation (c144).
                                            let done = match sessions.history(&id).await {
                                                Ok(records) => {
                                                    let msg = format!("switched to {id}");
                                                    app.load_history(id.clone(), records);
                                                    if let Ok(log) = sessions.latest_circuit_log(&id).await
                                                    {
                                                        app.set_circuit_log(log);
                                                        app.pin_inspection();
                                                    }
                                                    msg
                                                }
                                                Err(err) => format!("history: {err:#}"),
                                            };
                                            // Refresh so the `*` mark follows.
                                            match sessions.list().await {
                                                Ok(list) => {
                                                    app.open_picker(list);
                                                    app.set_status(done);
                                                }
                                                Err(err) => app.set_status(format!("sessions: {err:#}")),
                                            }
                                        }
                                        Err(err) => app.set_status(format!("switch: {err:#}")),
                                    }
                                }
                            }
                            KeyIntent::PickerCreate => {
                                if in_flight.is_some() {
                                    in_flight = None;
                                    app.clear_status();
                                }
                                match sessions.create("new session".to_string()).await {
                                    Ok(summary) => {
                                        let id = summary.id;
                                        let _ = sessions.switch(id.clone()).await;
                                        app.load_history(id.clone(), Vec::new());
                                        app.set_circuit_log(Vec::new());
                                        app.pin_inspection();
                                        match sessions.list().await {
                                            Ok(list) => {
                                                app.open_picker(list);
                                                app.set_status(format!("created {id}"));
                                            }
                                            Err(err) => app.set_status(format!("sessions: {err:#}")),
                                        }
                                    }
                                    Err(err) => app.set_status(format!("create: {err:#}")),
                                }
                            }
                            KeyIntent::PromptAnswer(allow) => {
                                // Staged loop first (NL-routed drive/review/plan
                                // confirms spend here — allow launches, deny
                                // spends nothing); otherwise the legacy
                                // permission prompt path.
                                if let Some(intent) = app.take_pending_loop() {
                                    if allow {
                                        let line = drive::intent_text(&intent);
                                        app.messages.push(Message {
                                            role: Role::User,
                                            text: line.clone(),
                                        });
                                        persist_turn(
                                            &mut app,
                                            sessions,
                                            TurnRole::User,
                                            line,
                                            &[],
                                        )
                                        .await;
                                        app.set_status("driving…".to_string());
                                        in_flight_loop =
                                            Some(Box::pin(driver.drive(intent)));
                                    } else {
                                        app.set_status(
                                            "loop cancelled — nothing spent".to_string(),
                                        );
                                    }
                                } else {
                                    app.answer_prompt(allow);
                                }
                            }
                            KeyIntent::ClearInput => app.clear_input(),
                            KeyIntent::Quit => {
                                if in_flight_loop.is_some() {
                                    in_flight_loop = None;
                                    app.clear_status();
                                    app.set_status("loop cancelled (Esc)".to_string());
                                    app.push_model("[loop cancelled by user]".to_string());
                                } else if in_flight.is_some() {
                                    in_flight = None;
                                    app.clear_status();
                                    app.set_status("turn cancelled (Esc)".to_string());
                                    app.push_model("[turn cancelled by user]".to_string());
                                } else {
                                    app.quit();
                                }
                            }
                            KeyIntent::Submit => {
                                if let Some(prompt) = app.take_input() {
                                    let trimmed = prompt.trim();
                                    if trimmed.starts_with("/rename") {
                                        let rest = trimmed.strip_prefix("/rename").unwrap().trim();
                                        let list = sessions.list().await.unwrap_or_default();
                                        let target = if let Some((first, rem)) = rest.split_once(' ') {
                                            if list.iter().any(|s| s.id == first) {
                                                Some((first.to_string(), rem.trim().to_string()))
                                            } else {
                                                app.bound.clone().map(|b| (b, rest.to_string()))
                                            }
                                        } else if !rest.is_empty() {
                                            app.bound.clone().map(|b| (b, rest.to_string()))
                                        } else {
                                            None
                                        };

                                        if let Some((id, title)) = target {
                                            if title.is_empty() {
                                                app.set_status(
                                                    "usage: /rename <title> or /rename <id> <title>"
                                                        .to_string(),
                                                );
                                            } else {
                                                match sessions.rename(id.clone(), title.clone()).await {
                                                    Ok(()) => {
                                                        app.set_status(format!("renamed {id} to {title}"));
                                                    }
                                                    Err(err) => app.set_status(format!("rename: {err:#}")),
                                                }
                                            }
                                        } else {
                                            app.set_status(
                                                "usage: /rename <title> or /rename <id> <title>"
                                                    .to_string(),
                                            );
                                        }
                                    } else if trimmed.starts_with("/delete") {
                                        let rest = trimmed.strip_prefix("/delete").unwrap().trim();
                                        let target_id = if !rest.is_empty() {
                                            Some(rest.to_string())
                                        } else {
                                            app.bound.clone()
                                        };

                                        if let Some(id) = target_id {
                                            match sessions.delete(id.clone()).await {
                                                Ok(()) => {
                                                    if app.bound.as_ref() == Some(&id) {
                                                        app.bound = None;
                                                        app.messages.clear();
                                                    }
                                                    app.set_status(format!("deleted {id}"));
                                                }
                                                Err(err) => app.set_status(format!("delete: {err:#}")),
                                            }
                                        } else {
                                            app.set_status("usage: /delete or /delete <id>".to_string());
                                        }
                                    } else if trimmed.starts_with("/new") || trimmed.starts_with("/create")
                                    {
                                        let rest = if let Some(r) = trimmed.strip_prefix("/new") {
                                            r.trim()
                                        } else if let Some(r) = trimmed.strip_prefix("/create") {
                                            r.trim()
                                        } else {
                                            ""
                                        };
                                        let title = if rest.is_empty() {
                                            "new session".to_string()
                                        } else {
                                            rest.to_string()
                                        };
                                        match sessions.create(title).await {
                                            Ok(summary) => {
                                                let id = summary.id;
                                                let _ = sessions.switch(id.clone()).await;
                                                app.load_history(id.clone(), Vec::new());
                                                app.set_status(format!("created and switched to {id}"));
                                            }
                                            Err(err) => app.set_status(format!("create: {err:#}")),
                                        }
                                    } else if trimmed.starts_with("/attach") {
                                        // Vision sidecar: stage an image for the next
                                        // chat turn (validated now, consumed on submit).
                                        let rest =
                                            trimmed.strip_prefix("/attach").unwrap().trim();
                                        match validate_image(rest) {
                                            Ok(path) => {
                                                app.pending_images.push(path.clone());
                                                app.set_status(format!(
                                                    "staged {} ({} image{}) — next turn sees it",
                                                    path.display(),
                                                    app.pending_images.len(),
                                                    if app.pending_images.len() == 1 {
                                                        ""
                                                    } else {
                                                        "s"
                                                    }
                                                ));
                                            }
                                            Err(err) => {
                                                app.set_status(format!("attach: {err:#}"))
                                            }
                                        }
                                    } else if trimmed == "/detach" {
                                        let n = app.pending_images.len();
                                        app.pending_images.clear();
                                        app.set_status(format!("detached {n} image(s)"));
                                    } else if trimmed == "/extend" || trimmed.starts_with("/extend ") {
                                        let ext_prompt = if let Some(rest) = trimmed.strip_prefix("/extend")
                                        {
                                            let r = rest.trim();
                                            if r.is_empty() {
                                                "Please continue the previous task using your tools, verifying and finalizing the results.".to_string()
                                            } else {
                                                format!("Continue the task using your tools: {r}")
                                            }
                                        } else {
                                            "Please continue the previous task using your tools, verifying and finalizing the results.".to_string()
                                        };
                                        if app.bound.is_none() {
                                            if let Ok(summary) =
                                                sessions.create("extension".to_string()).await
                                            {
                                                let _ = sessions.switch(summary.id.clone()).await;
                                                app.bound = Some(summary.id);
                                            }
                                        }
                                        persist_turn(
                                            &mut app,
                                            sessions,
                                            TurnRole::User,
                                            ext_prompt.clone(),
                                            &[],
                                        )
                                        .await;
                                        app.set_status("working…".to_string());
                                        let ext_images = app.take_images();
                                        in_flight = Some(Box::pin(
                                            responder.chat_turn(ext_prompt, ext_images),
                                        ));
                                    } else if let Some((cmd, args)) = split_slash(trimmed) {
                                        // CLI-parity commands: answered in place, both
                                        // sides persisted like a turn (user line +
                                        // command reply) so history stays complete.
                                        app.messages.push(Message {
                                            role: Role::User,
                                            text: prompt.clone(),
                                        });
                                        match commands.handle(&cmd, &args).await {
                                            Ok(reply) => {
                                                app.push_model(reply.clone());
                                                persist_turn(
                                                    &mut app,
                                                    sessions,
                                                    TurnRole::User,
                                                    prompt.clone(),
                                                    &[],
                                                )
                                                .await;
                                                persist_turn(
                                                    &mut app,
                                                    sessions,
                                                    TurnRole::Model,
                                                    reply,
                                                    &[],
                                                )
                                                .await;
                                            }
                                            Err(err) => {
                                                let text = app.push_error(err);
                                                persist_turn(
                                                    &mut app,
                                                    sessions,
                                                    TurnRole::User,
                                                    prompt.clone(),
                                                    &[],
                                                )
                                                .await;
                                                persist_turn(
                                                    &mut app,
                                                    sessions,
                                                    TurnRole::Model,
                                                    text,
                                                    &[],
                                                )
                                                .await;
                                            }
                                        }
                                    } else {
                                        // NL→loop routing: the smol discriminator gets
                                        // first say (one cheap word); keywords decide
                                        // when it abstains. Headless stays chat-only.
                                        let route = if driver.available() {
                                            match driver.discriminate(&prompt).await {
                                                Some(word) => drive::parse_discriminator(
                                                    &word,
                                                    &prompt,
                                                )
                                                .unwrap_or_else(|| {
                                                    drive::classify_loop(&prompt)
                                                }),
                                                None => drive::classify_loop(&prompt),
                                            }
                                        } else {
                                            drive::LoopIntent::Chat {
                                                prompt: prompt.clone(),
                                            }
                                        };
                                        match route {
                                            drive::LoopIntent::Chat { .. } => {
                                        if app.bound.is_none() {
                                            let title = if prompt.len() > 30 {
                                                format!("{}…", &prompt[..30])
                                            } else {
                                                prompt.clone()
                                            };
                                            let title = if title.trim().is_empty() {
                                                "new session".to_string()
                                            } else {
                                                title
                                            };
                                            if let Ok(summary) = sessions.create(title).await {
                                                let _ = sessions.switch(summary.id.clone()).await;
                                                app.bound = Some(summary.id);
                                            }
                                        }
                                        let images = app.take_images();
                                        let recorded = if images.is_empty() {
                                            prompt.clone()
                                        } else {
                                            let names: Vec<String> = images
                                                .iter()
                                                .map(|p| {
                                                    p.file_name()
                                                        .and_then(|n| n.to_str())
                                                        .unwrap_or("?")
                                                        .to_string()
                                                })
                                                .collect();
                                            format!("{} [image: {}]", prompt, names.join(", "))
                                        };
                                        persist_turn(
                                            &mut app,
                                            sessions,
                                            TurnRole::User,
                                            recorded,
                                            &[],
                                        )
                                        .await;
                                        app.set_status("working…".to_string());
                                        in_flight = Some(Box::pin(
                                            responder.chat_turn(prompt, images),
                                        ));
                                            }
                                            drive::LoopIntent::Drive { goal } => {
                                                app.stage_loop(drive::DriveIntent::DiveIn {
                                                    goal: goal.clone(),
                                                });
                                                app.open_prompt(
                                                    "drive".to_string(),
                                                    format!(
                                                        "Run drive for: {goal}? (y = spend turns, n = chat)"
                                                    ),
                                                );
                                            }
                                            drive::LoopIntent::Review { target } => {
                                                app.stage_loop(drive::DriveIntent::ReviewFile {
                                                    target: target.clone(),
                                                });
                                                app.open_prompt(
                                                    "review".to_string(),
                                                    format!(
                                                        "Run review loop on {target}? (y = spend turns, n = chat)"
                                                    ),
                                                );
                                            }
                                            drive::LoopIntent::PlanDrive { file } => {
                                                app.stage_loop(drive::DriveIntent::PlanFile {
                                                    file: file.clone(),
                                                });
                                                app.open_prompt(
                                                    "plan".to_string(),
                                                    format!(
                                                        "Drive plan file {file}? (y = spend turns, n = chat)"
                                                    ),
                                                );
                                            }
                                        }
                                    }
                                }
                            }
                            KeyIntent::TypeKey => {
                                app.handle_key(key);
                            }
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => anyhow::bail!("tui input failed: {e}"),
                    None => anyhow::bail!("tui input stream ended"),
                }
            }
        }

        if let Some(turn_res) = completed_turn {
            in_flight = None;
            app.clear_status();
            match turn_res {
                Ok(out) => {
                    app.circuit_log = out.circuit_log.clone();
                    app.pin_inspection();
                    app.push_model(out.text.clone());
                    persist_turn(
                        &mut app,
                        sessions,
                        TurnRole::Model,
                        out.text,
                        &out.circuit_log,
                    )
                    .await;
                }
                Err(err) => {
                    let text = app.push_error(err);
                    persist_turn(&mut app, sessions, TurnRole::Model, text, &[]).await;
                }
            }
        }

        if let Some(loop_res) = completed_loop {
            in_flight_loop = None;
            app.clear_status();
            match loop_res {
                Ok(events) => {
                    let n = events.len();
                    for ev in events {
                        app.apply_drive_event(ev);
                    }
                    let summary = app.status.clone();
                    app.push_model(format!("loop done ({n} events): {summary}"));
                }
                Err(err) => {
                    app.push_error(err);
                }
            }
        }
    }
    tracing::info!("tui session end");
    Ok(())
}

/// Restores cooked mode + main screen when dropped. Runs on every exit
/// path through `run` (normal quit AND `?` bails) — best-effort, never
/// panics, never masks the in-flight error.
struct RestoreTerminal;

impl Drop for RestoreTerminal {
    fn drop(&mut self) {
        let _ = crossterm::execute!(std::io::stdout(), crossterm::terminal::LeaveAlternateScreen);
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyModifiers;
    use ratatui::{backend::TestBackend, buffer::Buffer, layout::Rect, Terminal};

    use super::*;

    /// Expected buffer WITH theme chrome: border rows/cols + title span
    /// styled, content default. Mirrors `App::draw` styling exactly — a
    /// styling change fails these, which is the point (pixel-pinned).
    fn styled_expected(lines: &[&str], theme: Theme) -> Buffer {
        let mut buf = Buffer::with_lines(lines.iter().copied());
        let height = lines.len() as u16;
        buf.set_style(Rect::new(0, 0, 40, 1), theme.border_style());
        buf.set_style(Rect::new(0, height - 1, 40, 1), theme.border_style());
        buf.set_style(Rect::new(0, 1, 1, height - 2), theme.border_style());
        buf.set_style(Rect::new(39, 1, 1, height - 2), theme.border_style());
        buf.set_style(Rect::new(1, 0, 9, 1), theme.title_style());
        buf
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// Key-free stand-in: proves the submit cycle without a provider.
    struct FakeResponder {
        reply: String,
    }

    #[test]
    fn validate_image_accepts_and_refuses() {
        // Fail-closed: missing, wrong extension, and empty all name
        // the fault (a bad path never reaches a billed turn).
        assert!(validate_image("").is_err());
        assert!(validate_image("nope-missing.png").is_err());
        let dir = tempfile::tempdir().unwrap();
        let txt = dir.path().join("note.txt");
        std::fs::write(&txt, "hi").unwrap();
        assert!(validate_image(txt.to_str().unwrap()).is_err());
        let png = dir.path().join("shot.png");
        std::fs::write(&png, [137u8, 80, 78, 71]).unwrap();
        assert_eq!(validate_image(png.to_str().unwrap()).unwrap(), png);
    }

    #[test]
    fn split_slash_routes_commands_case_insensitively() {
        assert_eq!(
            split_slash("/sessions"),
            Some(("sessions".to_string(), String::new()))
        );
        assert_eq!(
            split_slash("/Config show"),
            Some(("config".to_string(), "show".to_string()))
        );
        assert_eq!(split_slash("just chatting"), None);
        assert_eq!(split_slash("/"), None);
        assert_eq!(split_slash("/   "), None);
    }

    #[tokio::test]
    async fn no_commands_handler_names_itself_unknown() {
        let err = NoCommands
            .handle("bogus", "")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("bogus"), "{err}");
    }

    impl Responder for FakeResponder {
        async fn respond(&self, _prompt: String) -> anyhow::Result<TurnOutput> {
            Ok(TurnOutput::from(self.reply.clone()))
        }
    }

    #[test]
    fn links() {
        assert_eq!(super::crate_name(), "polymorph-tui");
    }

    #[test]
    fn esc_quits_modifiers_ignored() {
        // Matches on code: Shift+Esc still quits (kind/modifiers never gate
        // the Esc decision — the c75 correction, carried over).
        let mut app = App::default();
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::SHIFT));
        assert!(app.should_quit);
    }

    #[test]
    fn ctrl_c_quits_bare_c_types() {
        let mut app = App::default();
        app.handle_key(key(KeyCode::Char('c')));
        assert!(!app.should_quit);
        assert_eq!(app.input, "c");
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.should_quit);
    }

    #[test]
    fn typing_backspace_q_types() {
        // c75 behavior evolves: `q` types now (Esc/Ctrl-C quit instead).
        let mut app = App::default();
        for c in ['h', 'i', 'q'] {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Backspace));
        assert_eq!(app.input, "hi");
        assert!(!app.should_quit);
    }

    #[test]
    fn shift_uppercase_types_ctrl_char_ignored() {
        // Chunk 143: uppercase `H` arrives SHIFT-modified on most
        // terminals — it must type, while Ctrl-modified chars must not.
        let mut app = App::default();
        app.handle_key(KeyEvent::new(KeyCode::Char('H'), KeyModifiers::SHIFT));
        assert_eq!(app.input, "H");
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
        assert_eq!(app.input, "H");
        assert!(!app.should_quit);
    }

    #[test]
    fn press_only_guard_drops_release_repeat() {
        use crossterm::event::KeyEventKind;
        use crossterm::event::KeyEventState;
        let kinded = |code, kind| KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind,
            state: KeyEventState::empty(),
        };
        assert!(App::is_press(&kinded(
            KeyCode::Char('a'),
            KeyEventKind::Press
        )));
        assert!(!App::is_press(&kinded(
            KeyCode::Char('a'),
            KeyEventKind::Release
        )));
        assert!(!App::is_press(&kinded(
            KeyCode::Char('a'),
            KeyEventKind::Repeat
        )));
    }

    #[test]
    fn classify_key_routes_every_arm() {
        // Chunk 146: the classifier reproduces the loop precedence.
        use crossterm::event::{KeyEventKind, KeyEventState};
        let press = |code| KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        };
        let release = |code| KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Release,
            state: KeyEventState::empty(),
        };
        let chat = App::default();
        assert_eq!(
            chat.classify_key(press(KeyCode::Tab)),
            KeyIntent::TogglePicker
        );
        assert_eq!(
            chat.classify_key(press(KeyCode::F(1))),
            KeyIntent::OverlayClose
        );
        assert_eq!(
            chat.classify_key(press(KeyCode::F(2))),
            KeyIntent::ToggleDiff
        );
        assert_eq!(
            chat.classify_key(press(KeyCode::F(3))),
            KeyIntent::ToggleTasks
        );
        assert_eq!(
            chat.classify_key(press(KeyCode::F(4))),
            KeyIntent::ToggleGutter
        );
        assert_eq!(
            chat.classify_key(press(KeyCode::F(5))),
            KeyIntent::ToggleCircuit
        );
        assert_eq!(
            chat.classify_key(press(KeyCode::PageUp)),
            KeyIntent::ScrollPages(1)
        );
        assert_eq!(chat.classify_key(press(KeyCode::Enter)), KeyIntent::Submit);
        assert_eq!(
            chat.classify_key(press(KeyCode::Char('a'))),
            KeyIntent::TypeKey
        );
        assert_eq!(
            chat.classify_key(release(KeyCode::Char('a'))),
            KeyIntent::Ignore
        );
        // Esc: empty quits, non-empty clears (R1).
        assert_eq!(chat.classify_key(press(KeyCode::Esc)), KeyIntent::Quit);
        let typing = App {
            input: "hi".to_string(),
            ..Default::default()
        };
        assert_eq!(
            typing.classify_key(press(KeyCode::Esc)),
            KeyIntent::ClearInput
        );
        // Overlay split (170346): page keys reach the left transcript
        // everywhere; plain arrows scroll the right pane's own content
        // (trace in F5, self content in F2/F3/F4).
        let mut diff = App::default();
        diff.open_diff(DiffSnapshot {
            status: "M a".to_string(),
            stat: String::new(),
        });
        assert_eq!(
            diff.classify_key(press(KeyCode::Up)),
            KeyIntent::ScrollOverlay(1)
        );
        assert_eq!(
            diff.classify_key(press(KeyCode::Down)),
            KeyIntent::ScrollOverlay(-1)
        );
        assert_eq!(
            diff.classify_key(press(KeyCode::PageUp)),
            KeyIntent::ScrollPages(1)
        );
        assert_eq!(
            diff.classify_key(press(KeyCode::Esc)),
            KeyIntent::OverlayClose
        );
        diff.scroll_overlay(3);
        assert_eq!(diff.overlay_scroll, 3);
        diff.scroll_overlay(-99);
        assert_eq!(diff.overlay_scroll, 0);
        // Picker-local.
        let mut picker = App::default();
        picker.open_picker(Vec::new());
        assert_eq!(
            picker.classify_key(press(KeyCode::Enter)),
            KeyIntent::PickerSwitch
        );
        assert_eq!(
            picker.classify_key(press(KeyCode::Esc)),
            KeyIntent::PickerClose
        );
        assert_eq!(
            picker.classify_key(press(KeyCode::Char('d'))),
            KeyIntent::PickerDelete
        );
        assert_eq!(
            picker.classify_key(press(KeyCode::Delete)),
            KeyIntent::PickerDelete
        );
        assert_eq!(
            picker.classify_key(press(KeyCode::Char('r'))),
            KeyIntent::PickerRename
        );
        assert_eq!(
            picker.classify_key(press(KeyCode::Char('c'))),
            KeyIntent::PickerCreate
        );
        assert_eq!(
            picker.classify_key(press(KeyCode::Char('n'))),
            KeyIntent::PickerCreate
        );
        assert_eq!(
            picker.classify_key(press(KeyCode::Tab)),
            KeyIntent::TogglePicker
        );
        // Prompt-local.
        let mut asked = App::default();
        asked.open_prompt("write".to_string(), "q?".to_string());
        assert_eq!(
            asked.classify_key(press(KeyCode::Char('y'))),
            KeyIntent::PromptAnswer(true)
        );
        assert_eq!(
            asked.classify_key(press(KeyCode::Char('q'))),
            KeyIntent::Ignore
        );
    }

    #[test]
    fn esc_clears_input_before_quit() {
        // Chunk 146 R1: Esc defocuses first, quits second.
        let mut app = App {
            input: "half".to_string(),
            ..Default::default()
        };
        app.clear_input();
        assert!(app.input.is_empty());
        assert!(!app.should_quit);
        app.quit();
        assert!(app.should_quit);
    }

    #[test]
    fn blank_enter_submits_nothing() {
        let mut app = App {
            input: "   ".to_string(),
            ..Default::default()
        };
        assert_eq!(app.take_input(), None);
        assert!(app.messages.is_empty());
    }

    #[test]
    fn submit_cycle_records_both_sides() {
        // Full Enter→response→record loop, key-free via the fake.
        let responder = FakeResponder {
            reply: "fake-turn".to_string(),
        };
        let mut app = App {
            input: "hello?".to_string(),
            ..Default::default()
        };
        let prompt = app.take_input().expect("non-blank submits");
        assert!(app.input.is_empty());
        let out = futures::executor::block_on(responder.respond(prompt)).unwrap();
        app.push_model(out.text);
        assert_eq!(
            app.messages,
            vec![
                Message {
                    role: Role::User,
                    text: "hello?".to_string(),
                },
                Message {
                    role: Role::Model,
                    text: "fake-turn".to_string(),
                },
            ]
        );
    }

    #[test]
    fn error_is_a_message_not_a_crash() {
        let mut app = App::default();
        app.push_error(anyhow::anyhow!("boom"));
        assert_eq!(app.messages.len(), 1);
        assert!(app.messages[0].text.starts_with("error: "));
    }

    #[test]
    fn prompt_open_answer_close() {
        // Exclusive overlay (c78 rule), input preserved, decision recorded.
        let mut app = App {
            input: "half-typed".to_string(),
            ..Default::default()
        };
        app.open_prompt("write".to_string(), "write x.txt?".to_string());
        assert_eq!(app.overlay, Overlay::Prompt);
        assert_eq!(app.input, "half-typed");
        app.answer_prompt(true);
        assert_eq!(app.overlay, Overlay::Chat);
        assert_eq!(app.status, "allowed write");
        assert_eq!(app.input, "half-typed");
        app.open_prompt("bash".to_string(), "run tests?".to_string());
        app.answer_prompt(false);
        assert_eq!(app.status, "denied bash");
    }

    #[test]
    fn prompt_key_mapping() {
        use crossterm::event::KeyEventKind;
        let press = |code| KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::empty(),
        };
        let shift_press = |code| KeyEvent {
            code,
            modifiers: KeyModifiers::SHIFT,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::empty(),
        };
        assert_eq!(App::prompt_key(press(KeyCode::Char('y'))), Some(true));
        assert_eq!(App::prompt_key(press(KeyCode::Char('Y'))), Some(true));
        assert_eq!(App::prompt_key(press(KeyCode::Char('n'))), Some(false));
        assert_eq!(App::prompt_key(press(KeyCode::Char('N'))), Some(false));
        assert_eq!(App::prompt_key(press(KeyCode::Esc)), Some(false));
        assert_eq!(App::prompt_key(press(KeyCode::Enter)), None);
        assert_eq!(App::prompt_key(press(KeyCode::Char('q'))), None);
        // Chunk 143: uppercase answers arrive SHIFT-modified — same verdict.
        assert_eq!(App::prompt_key(shift_press(KeyCode::Char('Y'))), Some(true));
        assert_eq!(
            App::prompt_key(shift_press(KeyCode::Char('N'))),
            Some(false)
        );
        assert_eq!(App::prompt_key(shift_press(KeyCode::Char('y'))), Some(true));
    }

    /// Key-free stand-in for the gutter pane.
    struct FakeGutter {
        snapshot: GutterSnapshot,
    }

    impl FakeGutter {
        fn load_now(&self) -> GutterSnapshot {
            self.snapshot.clone()
        }
    }

    #[test]
    fn gutter_open_close_preserves_input() {
        let mut app = App {
            input: "half-typed".to_string(),
            ..Default::default()
        };
        app.open_gutter(
            FakeGutter {
                snapshot: GutterSnapshot::default(),
            }
            .load_now(),
        );
        assert_eq!(app.overlay, Overlay::Gutter);
        assert_eq!(app.input, "half-typed");
        app.close_overlay();
        assert_eq!(app.overlay, Overlay::Chat);
        assert_eq!(app.input, "half-typed");
    }

    #[test]
    fn gutter_renders_exact() {
        // 40x6: two rows + input + status, all inside borders.
        let backend = TestBackend::new(40, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::default();
        app.open_gutter(
            FakeGutter {
                snapshot: GutterSnapshot {
                    rows: vec![
                        GutterRow {
                            location: "a.rs:1".to_string(),
                            message: "probe-diagnostic".to_string(),
                        },
                        GutterRow {
                            location: "b.py:12".to_string(),
                            message: "other".to_string(),
                        },
                    ],
                },
            }
            .load_now(),
        );
        terminal.draw(|frame| app.draw(frame)).unwrap();
        terminal.backend().assert_buffer(&styled_expected(
            &[
                "┌polymorph─────────────────────────────┐",
                "│a.rs:1  probe-diagnostic              │",
                "│b.py:12  other                        │",
                "│>                                     │",
                "│PgUp/PgDn transcript · Up/Dn here ·   │",
                "└──────────────────────────────────────┘",
            ],
            Theme::default(),
        ));
    }

    #[test]
    fn gutter_empty_note_renders_exact() {
        // 40x5: honest note + input + status, all inside borders.
        let backend = TestBackend::new(40, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::default();
        app.open_gutter(
            FakeGutter {
                snapshot: GutterSnapshot::default(),
            }
            .load_now(),
        );
        terminal.draw(|frame| app.draw(frame)).unwrap();
        terminal.backend().assert_buffer(&styled_expected(
            &[
                "┌polymorph─────────────────────────────┐",
                "│no diagnostics — none recorded yet    │",
                "│>                                     │",
                "│PgUp/PgDn transcript · Up/Dn here ·   │",
                "└──────────────────────────────────────┘",
            ],
            Theme::default(),
        ));
    }

    #[test]
    fn prompt_renders_exact() {
        // 40x7: three prompt rows + input + status, all inside borders.
        let backend = TestBackend::new(40, 7);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::default();
        app.open_prompt("write".to_string(), "changed 3 lines".to_string());
        terminal.draw(|frame| app.draw(frame)).unwrap();
        terminal.backend().assert_buffer(&styled_expected(
            &[
                "┌polymorph─────────────────────────────┐",
                "│allow `write`?                        │",
                "│changed 3 lines                       │",
                "│                                      │",
                "│>                                     │",
                "│y allow · n deny · Esc deny           │",
                "└──────────────────────────────────────┘",
            ],
            Theme::default(),
        ));
    }

    /// Store-free stand-in: proves picker flows without a database.
    struct FakeSessions {
        sessions: std::sync::Mutex<Vec<SessionSummary>>,
        switched: std::sync::Mutex<Vec<String>>,
        history: std::sync::Mutex<Vec<ChatRecord>>,
        appended: std::sync::Mutex<Vec<(String, TurnRole, String)>>,
        circuit_log: std::sync::Mutex<Vec<String>>,
        fail_list: bool,
        fail_switch: bool,
    }

    impl FakeSessions {
        fn two() -> Self {
            Self {
                sessions: std::sync::Mutex::new(vec![
                    SessionSummary {
                        id: "a".to_string(),
                        title: "first".to_string(),
                        current: true,
                    },
                    SessionSummary {
                        id: "b".to_string(),
                        title: "second".to_string(),
                        current: false,
                    },
                ]),
                switched: std::sync::Mutex::new(Vec::new()),
                history: std::sync::Mutex::new(vec![ChatRecord {
                    role: TurnRole::Model,
                    text: "seeded past".to_string(),
                }]),
                appended: std::sync::Mutex::new(Vec::new()),
                circuit_log: std::sync::Mutex::new(vec!["[Stage 1: Plan]".to_string()]),
                fail_list: false,
                fail_switch: false,
            }
        }

        fn list_now(&self) -> Vec<SessionSummary> {
            futures::executor::block_on(self.list()).unwrap()
        }
    }

    impl SessionBackend for FakeSessions {
        async fn list(&self) -> anyhow::Result<Vec<SessionSummary>> {
            if self.fail_list {
                anyhow::bail!("db down");
            }
            Ok(self.sessions.lock().unwrap().clone())
        }

        async fn switch(&self, id: String) -> anyhow::Result<()> {
            if self.fail_switch {
                anyhow::bail!("locked");
            }
            self.switched.lock().unwrap().push(id.clone());
            for s in self.sessions.lock().unwrap().iter_mut() {
                s.current = s.id == id;
            }
            Ok(())
        }

        async fn history(&self, _session_id: &str) -> anyhow::Result<Vec<ChatRecord>> {
            Ok(self.history.lock().unwrap().clone())
        }

        async fn latest_circuit_log(&self, _session_id: &str) -> anyhow::Result<Vec<String>> {
            Ok(self.circuit_log.lock().unwrap().clone())
        }

        async fn append(
            &self,
            session_id: &str,
            role: TurnRole,
            text: String,
            _circuit_log: &[String],
        ) -> anyhow::Result<()> {
            self.appended
                .lock()
                .unwrap()
                .push((session_id.to_string(), role, text));
            Ok(())
        }

        async fn rename(&self, id: String, title: String) -> anyhow::Result<()> {
            let mut guard = self.sessions.lock().unwrap();
            if let Some(s) = guard.iter_mut().find(|s| s.id == id) {
                s.title = title;
                Ok(())
            } else {
                anyhow::bail!("no session {id}");
            }
        }

        async fn delete(&self, id: String) -> anyhow::Result<()> {
            let mut guard = self.sessions.lock().unwrap();
            let len_before = guard.len();
            guard.retain(|s| s.id != id);
            if guard.len() == len_before {
                anyhow::bail!("no session {id}");
            }
            Ok(())
        }

        async fn create(&self, title: String) -> anyhow::Result<SessionSummary> {
            let mut guard = self.sessions.lock().unwrap();
            let id = format!("s-{}", guard.len() + 1);
            for s in guard.iter_mut() {
                s.current = false;
            }
            let summary = SessionSummary {
                id: id.clone(),
                title,
                current: true,
            };
            guard.push(summary.clone());
            self.switched.lock().unwrap().push(id);
            Ok(summary)
        }
    }

    #[test]
    fn picker_opens_on_current_and_clamps() {
        let backend = FakeSessions::two();
        let mut app = App::default();
        app.open_picker(backend.list_now());
        assert_eq!(app.overlay, Overlay::Picker);
        assert_eq!(app.selected_id().as_deref(), Some("a"));
        app.move_selection(1);
        assert_eq!(app.selected_id().as_deref(), Some("b"));
        app.move_selection(1);
        assert_eq!(app.selected_id().as_deref(), Some("b"));
        app.move_selection(-9);
        assert_eq!(app.selected_id().as_deref(), Some("a"));
        app.close_overlay();
        assert_eq!(app.overlay, Overlay::Chat);
    }

    #[test]
    fn picker_empty_list_safe() {
        let mut app = App::default();
        app.open_picker(Vec::new());
        assert_eq!(app.overlay, Overlay::Picker);
        assert_eq!(app.selected_id(), None);
        app.move_selection(1);
        app.close_overlay();
    }

    #[test]
    fn picker_empty_renders_guidance_not_bare_legend() {
        // Chunk 144: an empty per-cwd DB rendered only the legend line.
        let backend = TestBackend::new(40, 7);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::default();
        app.open_picker(Vec::new());
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(text.contains("no sessions here"), "{text}");
    }

    #[test]
    fn bind_initial_leaves_resumed_note() {
        // Chunk 144: silent binds read as "whose session is this".
        let backend = FakeSessions::two();
        let mut app = App::default();
        futures::executor::block_on(bind_initial(&mut app, &backend));
        assert_eq!(app.bound.as_deref(), Some("a"));
        assert!(app.status.contains("resumed"), "{}", app.status);
    }

    #[test]
    fn picker_switch_flow_refreshes_current() {
        // Switch records the id, refresh moves the `*` mark, pane stays open.
        let backend = FakeSessions::two();
        let mut app = App::default();
        app.open_picker(backend.list_now());
        app.move_selection(1);
        let id = app.selected_id().expect("row highlighted");
        futures::executor::block_on(backend.switch(id)).unwrap();
        app.open_picker(backend.list_now());
        assert_eq!(
            backend.switched.lock().unwrap().as_slice(),
            ["b".to_string()]
        );
        assert!(app.picker.sessions[1].current);
        assert_eq!(app.overlay, Overlay::Picker);
    }

    #[test]
    fn fake_sessions_rename_and_delete() {
        let backend = FakeSessions::two();
        futures::executor::block_on(backend.rename("a".to_string(), "brand new".to_string()))
            .unwrap();
        let list = backend.list_now();
        assert_eq!(list[0].title, "brand new");

        futures::executor::block_on(backend.delete("a".to_string())).unwrap();
        let list = backend.list_now();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "b");
    }

    #[test]
    fn fake_sessions_create() {
        let backend = FakeSessions::two();
        let summary = futures::executor::block_on(backend.create("third".to_string())).unwrap();
        assert_eq!(summary.title, "third");
        assert!(summary.current);
        let list = backend.list_now();
        assert_eq!(list.len(), 3);
        assert_eq!(list.iter().filter(|s| s.current).count(), 1);
        assert_eq!(list.last().unwrap().id, summary.id);
    }

    #[test]
    fn picker_keys_do_not_leak_into_chat() {
        let mut app = App::default();
        app.open_picker(FakeSessions::two().list_now());
        app.handle_key(key(KeyCode::Char('x')));
        app.handle_key(key(KeyCode::Backspace));
        assert!(app.input.is_empty());
    }

    #[test]
    fn picker_renders_exact() {
        // 40x6: two rows + input + status, all inside borders.
        let backend = TestBackend::new(40, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::default();
        app.open_picker(FakeSessions::two().list_now());
        terminal.draw(|frame| app.draw(frame)).unwrap();
        terminal.backend().assert_buffer(&styled_expected(
            &[
                "┌polymorph─────────────────────────────┐",
                "│> * a  first                          │",
                "│    b  second                         │",
                "│>                                     │",
                "│Arrows move · Enter switch · d delete │",
                "└──────────────────────────────────────┘",
            ],
            Theme::default(),
        ));
    }

    #[test]
    fn chat_renders_exact() {
        // 40x6: two messages + input + status, all inside borders.
        let backend = TestBackend::new(40, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        let app = App {
            messages: vec![
                Message {
                    role: Role::User,
                    text: "hi".to_string(),
                },
                Message {
                    role: Role::Model,
                    text: "hello".to_string(),
                },
            ],
            input: "ok".to_string(),
            bound: Some("s".to_string()),
            ..Default::default()
        };
        terminal.draw(|frame| app.draw(frame)).unwrap();
        terminal.backend().assert_buffer(&styled_expected(
            &[
                "┌polymorph─────────────────────────────┐",
                "│you: hi                               │",
                "│model: hello                          │",
                "│> ok                                  │",
                "│Tab pick · F2 diff · F3 tasks · F4    │",
                "└──────────────────────────────────────┘",
            ],
            Theme::default(),
        ));
    }

    /// Key-free stand-ins for the read-only panes.
    struct FakeDiff {
        snapshot: DiffSnapshot,
    }

    impl DiffBackend for FakeDiff {
        async fn snapshot(&self) -> anyhow::Result<DiffSnapshot> {
            Ok(self.snapshot.clone())
        }
    }

    impl FakeDiff {
        fn snap(&self) -> DiffSnapshot {
            futures::executor::block_on(self.snapshot()).unwrap()
        }
    }

    struct FakeTasks {
        snapshot: PlanSnapshot,
    }

    impl TaskBackend for FakeTasks {
        async fn load(&self) -> anyhow::Result<PlanSnapshot> {
            Ok(self.snapshot.clone())
        }
    }

    impl FakeTasks {
        fn load_now(&self) -> PlanSnapshot {
            futures::executor::block_on(self.load()).unwrap()
        }
    }

    #[test]
    fn overlays_are_exclusive_by_construction() {
        let mut app = App::default();
        app.open_picker(FakeSessions::two().list_now());
        assert_eq!(app.overlay, Overlay::Picker);
        app.open_diff(DiffSnapshot {
            status: String::new(),
            stat: String::new(),
        });
        assert_eq!(app.overlay, Overlay::Diff);
        app.open_tasks(PlanSnapshot::default());
        assert_eq!(app.overlay, Overlay::Tasks);
        app.close_overlay();
        assert_eq!(app.overlay, Overlay::Chat);
    }

    #[test]
    fn scrollback_shows_older_and_resets_on_push() {
        // 40x10 (page 6): 8 messages, scrolled one page shows m0 first;
        // a new message re-pins to the live bottom.
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App {
            bound: Some("s".to_string()),
            ..Default::default()
        };
        for i in 0..8 {
            app.push_model(format!("m{i}"));
        }
        app.scroll_by(1);
        terminal.draw(|frame| app.draw(frame)).unwrap();
        terminal.backend().assert_buffer(&styled_expected(
            &[
                "┌polymorph─────────────────────────────┐",
                "│model: m0                             │",
                "│model: m1                             │",
                "│model: m2                             │",
                "│model: m3                             │",
                "│model: m4                             │",
                "│model: m5                             │",
                "│>                                     │",
                "│Tab pick · F2 diff · F3 tasks · F4    │",
                "└──────────────────────────────────────┘",
            ],
            Theme::default(),
        ));
    }

    #[test]
    fn diff_renders_exact() {
        // 40x10: header + status + blank + stat + input + status.
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let diff = FakeDiff {
            snapshot: DiffSnapshot {
                status: " M a.txt\n?? b.txt".to_string(),
                stat: " a.txt | 2 +-\n 1 file changed".to_string(),
            },
        };
        let mut app = App::default();
        app.open_diff(diff.snap());
        terminal.draw(|frame| app.draw(frame)).unwrap();
        terminal.backend().assert_buffer(&styled_expected(
            &[
                "┌polymorph─────────────────────────────┐",
                "│worktree:                             │",
                "│ M a.txt                              │",
                "│?? b.txt                              │",
                "│                                      │",
                "│ a.txt | 2 +-                         │",
                "│ 1 file changed                       │",
                "│>                                     │",
                "│PgUp/PgDn transcript · Up/Dn here ·   │",
                "└──────────────────────────────────────┘",
            ],
            Theme::default(),
        ));
    }

    #[test]
    fn tasks_renders_exact() {
        // 40x8: title + two rows + input + status.
        let backend = TestBackend::new(40, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        let tasks = FakeTasks {
            snapshot: PlanSnapshot {
                title: "demo".to_string(),
                rows: vec![
                    TaskRow {
                        id: "t1".to_string(),
                        title: "first".to_string(),
                        status: "pending".to_string(),
                    },
                    TaskRow {
                        id: "t2".to_string(),
                        title: "second".to_string(),
                        status: "done".to_string(),
                    },
                ],
            },
        };
        let mut app = App::default();
        app.open_tasks(tasks.load_now());
        terminal.draw(|frame| app.draw(frame)).unwrap();
        terminal.backend().assert_buffer(&styled_expected(
            &[
                "┌polymorph─────────────────────────────┐",
                "│plan: demo                            │",
                "│[pending] t1  first                   │",
                "│[done] t2  second                     │",
                "│>                                     │",
                "│PgUp/PgDn transcript · Up/Dn here ·   │",
                "│Esc close                             │",
                "└──────────────────────────────────────┘",
            ],
            Theme::default(),
        ));
    }

    #[test]
    fn tasks_empty_note() {
        // No plan file = honest note, never an error (40x6).
        let backend = TestBackend::new(40, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        let tasks = FakeTasks {
            snapshot: PlanSnapshot::default(),
        };
        let mut app = App::default();
        app.open_tasks(tasks.load_now());
        terminal.draw(|frame| app.draw(frame)).unwrap();
        terminal.backend().assert_buffer(&styled_expected(
            &[
                "┌polymorph─────────────────────────────┐",
                "│no ./PLAN.md — run `review plan-show` │",
                "│to start one                          │",
                "│>                                     │",
                "│PgUp/PgDn transcript · Up/Dn here ·   │",
                "└──────────────────────────────────────┘",
            ],
            Theme::default(),
        ));
    }

    #[test]
    fn themed_renders_hold_area() {
        // Pixel-proof queued with the eyeball: assert_buffer_lines compares
        // cell styles, so themed renders pin mapping (theme.rs) + area here.
        for name in ["default", "high-contrast"] {
            let backend = TestBackend::new(40, 6);
            let mut terminal = Terminal::new(backend).unwrap();
            let mut app = App::default();
            app.set_theme(Theme::named(name).unwrap());
            app.push_model("hi".to_string());
            terminal.draw(|frame| app.draw(frame)).unwrap();
            assert_eq!(terminal.backend().buffer().area.width, 40);
        }
    }

    #[test]
    fn history_load_replaces_log_and_binds() {
        let backend = FakeSessions::two();
        let mut app = App::default();
        app.push_model("stale in-memory".to_string());
        let records = futures::executor::block_on(backend.history("a")).expect("fake history");
        app.load_history("a".to_string(), records);
        assert_eq!(app.messages.len(), 1);
        assert_eq!(app.messages[0].text, "seeded past");
        assert_eq!(app.bound.as_deref(), Some("a"));
    }

    #[test]
    fn submit_reply_append_sequence() {
        // The loop sequence, driven manually (loop arms review-only): the
        // user line persists at submit, the model line at reply.
        let backend = FakeSessions::two();
        let mut app = App {
            input: "q?".to_string(),
            ..Default::default()
        };
        app.load_history("a".to_string(), Vec::new());
        let prompt = app.take_input().expect("non-blank submits");
        futures::executor::block_on(backend.append("a", TurnRole::User, prompt.clone(), &[]))
            .expect("fake append");
        app.push_model("r!".to_string());
        futures::executor::block_on(backend.append("a", TurnRole::Model, "r!".to_string(), &[]))
            .expect("fake append");
        assert_eq!(
            *backend.appended.lock().unwrap(),
            vec![
                ("a".to_string(), TurnRole::User, "q?".to_string()),
                ("a".to_string(), TurnRole::Model, "r!".to_string()),
            ]
        );
        assert_eq!(app.messages.len(), 2);
    }

    #[test]
    fn bind_initial_loads_current_and_unbound_without() {
        // Startup bind through fakes: current session loads its history;
        // no current session = unbound, in-memory only.
        let backend = FakeSessions::two();
        let mut app = App::default();
        futures::executor::block_on(bind_initial(&mut app, &backend));
        assert_eq!(app.bound.as_deref(), Some("a"));
        assert_eq!(app.messages.len(), 1);
        let empty = FakeSessions {
            sessions: std::sync::Mutex::new(Vec::new()),
            switched: std::sync::Mutex::new(Vec::new()),
            history: std::sync::Mutex::new(Vec::new()),
            appended: std::sync::Mutex::new(Vec::new()),
            circuit_log: std::sync::Mutex::new(Vec::new()),
            fail_list: false,
            fail_switch: false,
        };
        let mut bare = App::default();
        futures::executor::block_on(bind_initial(&mut bare, &empty));
        assert_eq!(bare.bound, None);
        assert!(bare.messages.is_empty());
    }

    #[test]
    fn bind_initial_resumes_first_if_no_current() {
        let backend = FakeSessions {
            sessions: std::sync::Mutex::new(vec![
                SessionSummary {
                    id: "first".to_string(),
                    title: "first session".to_string(),
                    current: false,
                },
                SessionSummary {
                    id: "second".to_string(),
                    title: "second session".to_string(),
                    current: false,
                },
            ]),
            switched: std::sync::Mutex::new(Vec::new()),
            history: std::sync::Mutex::new(vec![ChatRecord {
                role: TurnRole::Model,
                text: "restored".to_string(),
            }]),
            appended: std::sync::Mutex::new(Vec::new()),
            circuit_log: std::sync::Mutex::new(Vec::new()),
            fail_list: false,
            fail_switch: false,
        };
        let mut app = App::default();
        futures::executor::block_on(bind_initial(&mut app, &backend));
        assert_eq!(app.bound.as_deref(), Some("first"));
        assert_eq!(app.messages.len(), 1);
        assert_eq!(backend.switched.lock().unwrap().as_slice(), &["first"]);
    }

    #[test]
    fn unbound_hint_renders() {
        // 40x7, bound None: wrapped status carries the remedy; input
        // usable throughout (unbound sessions stay functional, D4).
        let backend = TestBackend::new(40, 7);
        let mut terminal = Terminal::new(backend).unwrap();
        let app = App::default();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        terminal.backend().assert_buffer(&styled_expected(
            &[
                "┌polymorph─────────────────────────────┐",
                "│>                                     │",
                "│Tab pick · F2 diff · F3 tasks · F4    │",
                "│diagnostics · F5 circuit · Enter send │",
                "│· PgUp transcript · unbound — Tab:    │",
                "│pick                                  │",
                "└──────────────────────────────────────┘",
            ],
            Theme::default(),
        ));
    }

    #[test]
    fn standard_terminal_renders_side_by_side_panes() {
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App {
            bound: Some("session123".to_string()),
            input: "build project".to_string(),
            ..Default::default()
        };
        app.push_model("ready to assist".to_string());
        terminal.draw(|frame| app.draw(frame)).unwrap();

        let buffer = terminal.backend().buffer();
        // Check header exists
        let header_line = (0..100)
            .map(|x| buffer[(x, 0)].symbol())
            .collect::<String>();
        assert!(header_line.contains("polymorph"));
        assert!(header_line.contains("session: session1"));

        // Check side-by-side titles exist
        let mut found_chat = false;
        let mut found_inspection = false;
        let mut found_prompt = false;
        for y in 0..20 {
            let row = (0..100)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>();
            if row.contains("Chat / Transcript") {
                found_chat = true;
            }
            if row.contains("Inspection Quickview") {
                found_inspection = true;
            }
            if row.contains("Prompt") {
                found_prompt = true;
            }
        }
        assert!(found_chat, "Left pane title should be visible");
        assert!(found_inspection, "Right pane title should be visible");
        assert!(found_prompt, "Pinned prompt box should be visible");

        // Verify cursor position is inside the input box (row 17, inside box at height 20 - 4 = 16)
        let cursor = terminal.get_cursor_position().unwrap();
        assert!(
            cursor.y >= 16 && cursor.y <= 19,
            "Cursor should be positioned inside input box, was at ({}, {})",
            cursor.x,
            cursor.y
        );
        assert!(
            cursor.x > 0 && cursor.x < 100,
            "Cursor x should be inside bounds"
        );
    }

    #[test]
    fn circuit_overlay_and_inspection_scroll() {
        use crossterm::event::{KeyEventKind, KeyEventState};
        let mut app = App::default();
        app.set_circuit_log(vec![
            "[Stage 1: Plan] Plan steps".to_string(),
            "[Stage 2: Act] Tool call 1".to_string(),
            "[Stage 3: Review] Verdict: PASS".to_string(),
            "[Stage 4: Converge] All criteria met".to_string(),
        ]);
        assert_eq!(app.circuit_log().len(), 4);
        assert_eq!(app.inspection_scroll, 0);

        app.scroll_inspection(2);
        assert_eq!(app.inspection_scroll, 2);
        app.scroll_inspection(-5);
        assert_eq!(app.inspection_scroll, 0);

        app.open_circuit();
        assert_eq!(app.overlay, Overlay::Circuit);
        let content = app.overlay_content();
        assert_eq!(content.len(), 4);

        let press = |code, modifiers| KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        };

        assert_eq!(
            app.classify_key(press(KeyCode::Esc, KeyModifiers::NONE)),
            KeyIntent::OverlayClose
        );
        assert_eq!(
            app.classify_key(press(KeyCode::Down, KeyModifiers::NONE)),
            KeyIntent::ScrollInspection(-1)
        );
        assert_eq!(
            app.classify_key(press(KeyCode::Up, KeyModifiers::NONE)),
            KeyIntent::ScrollInspection(1)
        );

        app.close_overlay();
        assert_eq!(app.overlay, Overlay::Chat);

        // In chat with Alt, arrows scroll inspection
        assert_eq!(
            app.classify_key(press(KeyCode::Up, KeyModifiers::ALT)),
            KeyIntent::ScrollInspection(1)
        );
        assert_eq!(
            app.classify_key(press(KeyCode::Down, KeyModifiers::ALT)),
            KeyIntent::ScrollInspection(-1)
        );
        // In chat with Shift (legacy cmd.exe never delivers Alt+arrows),
        // arrows and pages scroll inspection too.
        assert_eq!(
            app.classify_key(press(KeyCode::Up, KeyModifiers::SHIFT)),
            KeyIntent::ScrollInspection(1)
        );
        assert_eq!(
            app.classify_key(press(KeyCode::Down, KeyModifiers::SHIFT)),
            KeyIntent::ScrollInspection(-1)
        );
        assert_eq!(
            app.classify_key(press(KeyCode::PageUp, KeyModifiers::SHIFT)),
            KeyIntent::ScrollInspection(PAGE_LINES as i32)
        );
        assert_eq!(
            app.classify_key(press(KeyCode::PageDown, KeyModifiers::SHIFT)),
            KeyIntent::ScrollInspection(-(PAGE_LINES as i32))
        );
        // Plain arrows in chat still type/navigate (no scroll steal).
        assert_eq!(
            app.classify_key(press(KeyCode::Up, KeyModifiers::NONE)),
            KeyIntent::TypeKey
        );
    }

    #[tokio::test]
    async fn default_chat_turn_matches_respond() {
        // Trait default: implementers without chat_turn keep working —
        // FakeResponder answers identically through both methods.
        let responder = FakeResponder {
            reply: "fake-turn".to_string(),
        };
        let out = responder
            .chat_turn("hi".to_string(), Vec::new())
            .await
            .unwrap();
        assert_eq!(out.text, "fake-turn");
        assert!(out.circuit_log.is_empty());
    }

    #[test]
    fn inspection_window_counts_wrapped_rows_not_items() {
        // Two 100-char lines at width 40 = 3 rows each (the tent3 class:
        // counting items claimed 2 rows and scrolling felt dead).
        assert_eq!(App::wrap_rows(&"x".repeat(100), 40), 3);
        assert_eq!(App::wrap_rows("short", 40), 1);
        assert_eq!(App::wrap_rows("", 40), 1);
        let lines = vec!["x".repeat(100), "y".repeat(100), "z".to_string()];
        // 7 rows total, page 4: bottom shows y-tail + z.
        let win = App::inspection_window(&lines, 4, 40, 0);
        assert_eq!(win, vec!["y".repeat(100), "z".to_string()]);
        // Scroll up 3 rows: everything visible (window covers to top).
        let win = App::inspection_window(&lines, 4, 40, 3);
        assert_eq!(win, lines);
        // Over-scroll clamps to the top (never empty, never panics).
        let win = App::inspection_window(&lines, 4, 40, 999);
        assert!(!win.is_empty());
        assert!(win.contains(&"x".repeat(100)));
    }

    #[test]
    fn follow_drops_on_scroll_up_and_reengages_at_bottom() {
        let mut app = App::default();
        assert!(app.follow_inspection);
        app.scroll_inspection(5);
        assert_eq!(app.inspection_scroll, 5);
        assert!(!app.follow_inspection);
        // New content while scrolled up keeps your place.
        app.set_circuit_log(vec!["new".to_string()]);
        assert_eq!(app.inspection_scroll, 5);
        // Scrolling back to the bottom re-engages follow.
        app.scroll_inspection(-99);
        assert_eq!(app.inspection_scroll, 0);
        assert!(app.follow_inspection);
        app.set_circuit_log(vec!["newer".to_string()]);
        assert_eq!(app.inspection_scroll, 0);
    }

    #[test]
    fn chat_scroll_moves_wrapped_transcript() {
        // Tent3 on-glass class, headless: 30 wrapped reply lines at
        // 100x20 must actually move when paging up (old logical-count
        // math clamped the range to ~nothing on wrapped prose).
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App {
            bound: Some("session123".to_string()),
            ..Default::default()
        };
        for i in 0..30 {
            app.messages.push(Message {
                role: Role::Model,
                text: format!(
                    "reply {i:02} with enough trailing words to wrap the fifty-five column pane"
                ),
            });
        }
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let bottom: Vec<String> = (0..20)
            .map(|y| {
                (0..100)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect()
            })
            .collect();
        app.scroll_by(1);
        assert!(app.scroll > 0, "paging up must bank scroll offset");
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let scrolled: Vec<String> = (0..20)
            .map(|y| {
                (0..100)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect()
            })
            .collect();
        assert_ne!(bottom, scrolled, "paged view must differ");
        // Five pages reach the clamped top: oldest replies surface.
        app.scroll_by(4);
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let top: Vec<String> = (0..20)
            .map(|y| {
                (0..100)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect()
            })
            .collect();
        let joined = top.join("\n");
        assert!(
            joined.contains("reply 00") || joined.contains("reply 01"),
            "older content must surface after paging up"
        );
    }

    #[test]
    fn circuit_log_restored_on_bind_and_switch() {
        futures::executor::block_on(async {
            let fake = FakeSessions::two();
            let mut app = App::default();
            assert!(app.circuit_log().is_empty());

            bind_initial(&mut app, &fake).await;
            assert_eq!(app.bound, Some("a".to_string()));
            assert_eq!(app.circuit_log(), &["[Stage 1: Plan]".to_string()]);

            // Clear circuit log and switch to b
            app.set_circuit_log(Vec::new());
            *fake.circuit_log.lock().unwrap() = vec!["[Stage 2: Act (b)]".to_string()];
            app.open_picker(fake.list().await.unwrap());
            app.picker.selected = 1; // select "b"

            // Execute switch
            if let Some(id) = app.selected_id() {
                fake.switch(id.clone()).await.unwrap();
                let records = fake.history(&id).await.unwrap();
                app.load_history(id.clone(), records);
                let log = fake.latest_circuit_log(&id).await.unwrap();
                app.set_circuit_log(log);
            }
            assert_eq!(app.circuit_log(), &["[Stage 2: Act (b)]".to_string()]);
        });
    }

    #[test]
    fn circuit_overlay_flattens_multiline_entries() {
        let mut app = App::default();
        app.set_circuit_log(vec![
            "[Stage 1: Plan]\nLine 1\nLine 2".to_string(),
            "[Stage 2: Act]\nCall 1\nCall 2\nCall 3".to_string(),
        ]);
        app.open_circuit();
        let content = app.overlay_content();
        assert_eq!(content.len(), 7); // 3 lines + 4 lines
        assert_eq!(content[0], "[Stage 1: Plan]");
        assert_eq!(content[1], "Line 1");
        assert_eq!(content[2], "Line 2");
        assert_eq!(content[3], "[Stage 2: Act]");
    }
}
