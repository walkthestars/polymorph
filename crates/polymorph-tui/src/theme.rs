//! TUI themes (Epic J, chunk 80): named color sets over pane chrome.
//!
//! Minimal by design: border + accent colors applied to block titles and
//! borders in `App::draw`. Selection defaults to `default`; unknown names
//! are refused naming the valid set (strict-vocabulary precedent).
//! Pixel-exact styled asserts are queued with the operator eyeball —
//! `TestBackend::assert_buffer_lines` compares cell styles, so themed
//! renders are smoke-tested (no panic, area intact), not pixel-pinned.

use ratatui::style::{Color, Style};

/// Named color set for pane chrome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub name: &'static str,
    pub border: Color,
    pub accent: Color,
}

impl Theme {
    /// Look up a theme by name. Valid set is closed and named on refusal.
    pub fn named(name: &str) -> anyhow::Result<Self> {
        match name {
            "default" => Ok(Self {
                name: "default",
                border: Color::Gray,
                accent: Color::Cyan,
            }),
            "high-contrast" => Ok(Self {
                name: "high-contrast",
                border: Color::White,
                accent: Color::Yellow,
            }),
            _ => anyhow::bail!("unknown theme {name:?} (available: default, high-contrast)"),
        }
    }

    /// Style for block borders under this theme.
    pub fn border_style(self) -> Style {
        Style::default().fg(self.border)
    }

    /// Style for block titles under this theme.
    pub fn title_style(self) -> Style {
        Style::default().fg(self.accent)
    }

    /// Style for status lines under this theme.
    pub fn status_style(self) -> Style {
        Style::default().fg(Color::DarkGray)
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            name: "default",
            border: Color::Gray,
            accent: Color::Cyan,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_themes_pin_colors() {
        let default = Theme::named("default").unwrap();
        assert_eq!(default.border, Color::Gray);
        assert_eq!(default.accent, Color::Cyan);
        let hc = Theme::named("high-contrast").unwrap();
        assert_eq!(hc.border, Color::White);
        assert_eq!(hc.accent, Color::Yellow);
        assert_eq!(Theme::default(), default);
    }

    #[test]
    fn unknown_theme_names_the_set() {
        let err = Theme::named("midnight").unwrap_err().to_string();
        assert!(err.contains("default"), "{err}");
        assert!(err.contains("high-contrast"), "{err}");
    }
}
