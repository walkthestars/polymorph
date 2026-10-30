//! Read-only terminal viewer for a Polymorph drive event ledger.
//!
//! The ledger written by `polymorph review drive --events-json` is the whole
//! input: rounds, verdicts, findings, budget, and the terminal verdict. This
//! viewer never calls a model, never edits a file, and never opens a session;
//! it renders the contract the substrate already emits.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoundState {
    Passed,
    Failed,
    Invalid,
    Unknown,
}

impl RoundState {
    fn label(self) -> &'static str {
        match self {
            RoundState::Passed => "PASS",
            RoundState::Failed => "FAIL",
            RoundState::Invalid => "INVALID",
            RoundState::Unknown => "?",
        }
    }

    fn color(self) -> Color {
        match self {
            RoundState::Passed => Color::Green,
            RoundState::Failed => Color::Red,
            RoundState::Invalid => Color::Yellow,
            RoundState::Unknown => Color::DarkGray,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoundView {
    pub round: u64,
    pub state: RoundState,
    pub summary: String,
    pub synthesized: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetView {
    pub rounds_used: u64,
    pub rounds_max: u64,
    pub tools_used: u64,
    pub tools_max: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ViewState {
    pub rounds: Vec<RoundView>,
    pub findings: Vec<String>,
    pub budget: Option<BudgetView>,
    pub verdict: Option<String>,
    pub halted: Option<String>,
    pub event_count: usize,
}

impl ViewState {
    pub fn apply(&mut self, event: &Value) {
        let Some(object) = event.as_object() else {
            return;
        };
        self.event_count += 1;
        match object.get("type").and_then(Value::as_str).unwrap_or("") {
            "round_started" => {
                let round = object.get("round").and_then(Value::as_u64).unwrap_or(0);
                let max = object
                    .get("max_rounds")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                self.rounds.push(RoundView {
                    round,
                    state: RoundState::Unknown,
                    summary: format!("round {round}/{max}"),
                    synthesized: None,
                });
            }
            "verdict" => {
                let round = object.get("round").and_then(Value::as_u64).unwrap_or(0);
                let invalid = object.get("invalid").and_then(Value::as_bool) == Some(true);
                let passed = object.get("passed").and_then(Value::as_bool) == Some(true);
                let state = if invalid {
                    RoundState::Invalid
                } else if passed {
                    RoundState::Passed
                } else {
                    RoundState::Failed
                };
                let summary = object
                    .get("summary")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let synthesized = object
                    .get("synthesized")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                match self.rounds.iter_mut().find(|row| row.round == round) {
                    Some(row) => {
                        row.state = state;
                        row.summary = summary;
                        row.synthesized = synthesized;
                    }
                    None => self.rounds.push(RoundView {
                        round,
                        state,
                        summary,
                        synthesized,
                    }),
                }
            }
            "finding" => {
                let location = object
                    .get("location")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let message = object
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                self.findings.push(if location.is_empty() {
                    message.to_string()
                } else {
                    format!("{location}: {message}")
                });
            }
            "budget" => {
                let field = |key: &str| object.get(key).and_then(Value::as_u64).unwrap_or(0);
                self.budget = Some(BudgetView {
                    rounds_used: field("rounds_used"),
                    rounds_max: field("rounds_max"),
                    tools_used: field("tools_used"),
                    tools_max: field("tools_max"),
                });
            }
            "halted" => {
                self.halted = Some(
                    object
                        .get("reason")
                        .and_then(Value::as_str)
                        .unwrap_or("halted")
                        .to_string(),
                );
            }
            "finished" => {
                self.verdict = Some(
                    object
                        .get("verdict")
                        .and_then(Value::as_str)
                        .unwrap_or("UNKNOWN")
                        .to_string(),
                );
            }
            _ => {}
        }
    }

    pub fn apply_all(&mut self, ledger: &Value) -> anyhow::Result<()> {
        let events = ledger
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("ledger is not a JSON array"))?;
        for event in events {
            self.apply(event);
        }
        Ok(())
    }

    pub fn status(&self) -> String {
        match (&self.halted, &self.verdict) {
            (Some(reason), _) => format!("HALTED: {reason}"),
            (None, Some(verdict)) => verdict.clone(),
            (None, None) => "running".to_string(),
        }
    }

    pub fn from_text(text: &str) -> anyhow::Result<Self> {
        let mut state = Self::default();
        state.apply_all(&parse_events(text)?)?;
        Ok(state)
    }
}

pub fn parse_events(text: &str) -> anyhow::Result<Value> {
    let value: Value = serde_json::from_str(text)?;
    if !value.is_array() {
        anyhow::bail!("ledger is not a JSON array");
    }
    Ok(value)
}

pub fn draw(frame: &mut Frame, state: &ViewState, path: &str, following: bool) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(8),
        ])
        .split(frame.area());

    let mut header = vec![Line::from(vec![
        Span::styled(
            "polymorph watch",
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!("  {path}")),
        Span::styled(
            if following { "  [following]" } else { "" },
            Style::default().fg(Color::Cyan),
        ),
        Span::raw(format!("  events: {}", state.event_count)),
    ])];
    let status = state.status();
    let color = if state.halted.is_some() {
        Color::Red
    } else if status == "PASSED" {
        Color::Green
    } else {
        Color::Yellow
    };
    header.push(Line::from(vec![
        Span::raw("status: "),
        Span::styled(status, Style::default().fg(color)),
    ]));
    frame.render_widget(
        Paragraph::new(header).block(Block::default().borders(Borders::BOTTOM)),
        chunks[0],
    );

    let rounds: Vec<Line> = state
        .rounds
        .iter()
        .map(|row| {
            let mut spans = vec![
                Span::raw(format!("  #{:<3} ", row.round)),
                Span::styled(
                    format!("{:<7}", row.state.label()),
                    Style::default().fg(row.state.color()),
                ),
                Span::raw(row.summary.clone()),
            ];
            if let Some(kind) = &row.synthesized {
                spans.push(Span::styled(
                    format!("  [{kind} seed]"),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            Line::from(spans)
        })
        .collect();
    frame.render_widget(
        Paragraph::new(rounds).block(Block::default().borders(Borders::ALL).title("rounds")),
        chunks[1],
    );

    let mut footer = Vec::new();
    if let Some(budget) = state.budget {
        footer.push(Line::from(format!(
            "rounds {}/{}  tools {}/{}",
            budget.rounds_used, budget.rounds_max, budget.tools_used, budget.tools_max
        )));
    }
    let shown = state.findings.len().min(5);
    for finding in &state.findings[..shown] {
        footer.push(Line::from(Span::styled(
            finding.clone(),
            Style::default().fg(Color::Red),
        )));
    }
    footer.push(Line::from(Span::styled(
        "q quit",
        Style::default().fg(Color::DarkGray),
    )));
    frame.render_widget(
        Paragraph::new(footer)
            .block(Block::default().borders(Borders::ALL).title("findings"))
            .wrap(ratatui::widgets::Wrap { trim: true }),
        chunks[2],
    );
}

fn read_state(path: &Path) -> anyhow::Result<ViewState> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", path.display()))?;
    ViewState::from_text(&text)
}

fn file_stamp(path: &Path) -> Option<(u64, SystemTime)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.len(), meta.modified().ok()?))
}

struct TerminalGuard {
    terminal: ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
}

impl TerminalGuard {
    fn enter() -> anyhow::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        let mut stdout = std::io::stdout();
        crossterm::execute!(stdout, crossterm::terminal::EnterAlternateScreen)?;
        let terminal = ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(stdout))?;
        Ok(Self { terminal })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = crossterm::execute!(
            self.terminal.backend_mut(),
            crossterm::terminal::LeaveAlternateScreen
        );
        let _ = self.terminal.show_cursor();
    }
}

pub fn run(path: PathBuf, follow: bool, interval_ms: u64) -> anyhow::Result<()> {
    let mut state = read_state(&path)?;
    let mut stamp = file_stamp(&path);
    let mut guard = TerminalGuard::enter()?;
    let label = path.display().to_string();
    loop {
        if follow {
            let current = file_stamp(&path);
            if current != stamp {
                state = read_state(&path)?;
                stamp = current;
            }
        }
        guard
            .terminal
            .draw(|frame| draw(frame, &state, &label, follow))?;
        if crossterm::event::poll(Duration::from_millis(interval_ms))? {
            if let crossterm::event::Event::Key(key) = crossterm::event::read()? {
                use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};
                if key.kind == KeyEventKind::Press
                    && (matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
                        || (key.code == KeyCode::Char('c')
                            && key.modifiers.contains(KeyModifiers::CONTROL)))
                {
                    break;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ledger() -> Value {
        serde_json::json!([
            {"type":"round_started","round":1,"max_rounds":3},
            {"type":"verdict","round":1,"passed":false,"summary":"findings=1"},
            {"type":"finding","location":"src/a.rs:4","message":"missing guard"},
            {"type":"round_started","round":2,"max_rounds":3},
            {"type":"verdict","round":2,"passed":false,"summary":"unparseable","invalid":true,"parse_error":"not json"},
            {"type":"round_started","round":3,"max_rounds":3},
            {"type":"verdict","round":3,"passed":true,"summary":"findings=0","synthesized":"create"},
            {"type":"budget","rounds_used":3,"rounds_max":3,"tools_used":9,"tools_max":30},
            {"type":"finished","verdict":"PASSED"},
            {"type":"unknown_kind"}
        ])
    }

    #[test]
    fn applies_every_event_type() {
        let mut state = ViewState::default();
        state.apply_all(&ledger()).unwrap();
        assert_eq!(state.event_count, 10);
        assert_eq!(state.rounds.len(), 3);
        assert_eq!(state.rounds[0].state, RoundState::Failed);
        assert_eq!(state.rounds[1].state, RoundState::Invalid);
        assert_eq!(state.rounds[2].state, RoundState::Passed);
        assert_eq!(state.rounds[2].synthesized.as_deref(), Some("create"));
        assert_eq!(state.findings, vec!["src/a.rs:4: missing guard"]);
        assert_eq!(
            state.budget,
            Some(BudgetView {
                rounds_used: 3,
                rounds_max: 3,
                tools_used: 9,
                tools_max: 30
            })
        );
        assert_eq!(state.status(), "PASSED");
    }

    #[test]
    fn halted_wins_over_verdict() {
        let mut state = ViewState::default();
        state
            .apply_all(&serde_json::json!([
                {"type":"halted","reason":"MaxIterations"},
                {"type":"finished","verdict":"PASSED"}
            ]))
            .unwrap();
        assert_eq!(state.status(), "HALTED: MaxIterations");
    }

    #[test]
    fn finding_without_location_uses_message_alone() {
        let mut state = ViewState::default();
        state.apply(&serde_json::json!({"type":"finding","location":"","message":"bare"}));
        assert_eq!(state.findings, vec!["bare"]);
    }

    #[test]
    fn parse_rejects_a_non_array() {
        assert!(parse_events("{}").is_err());
        assert!(parse_events("not json").is_err());
        assert!(parse_events("[]").is_ok());
    }

    #[test]
    fn draw_renders_the_ledger() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let state = ViewState::from_text(&ledger().to_string()).unwrap();
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw(frame, &state, "events.json", false))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let text: String = buffer.content().iter().map(|cell| cell.symbol()).collect();
        assert!(text.contains("polymorph watch"), "{text}");
        assert!(text.contains("PASSED"), "{text}");
        assert!(text.contains("missing guard"), "{text}");
        assert!(text.contains("PASS"), "{text}");
    }
}
