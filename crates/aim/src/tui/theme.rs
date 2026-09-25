//! Colours and text attributes, kept in one place (docs/adr/0017: M7's declarative UI protocol
//! and DTCG themes resolve to these semantic roles).
//!
//! Palettes use the 16 named ANSI colours so they follow the terminal's own palette, plus text
//! attributes (bold, dim, italic, reverse) that survive `NO_COLOR`.

use ratatui::style::{Color, Modifier, Style};

/// A terminal background's tone.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tone {
    Dark,
    Light,
}

/// The TUI's semantic styles.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Theme {
    /// Ordinary text.
    pub text: Style,
    /// Secondary text (hints, separators, metadata).
    pub muted: Style,
    /// Emphasised chrome (the model in the status line, selected markers).
    pub accent: Style,
    /// The user's echoed prompts.
    pub user: Style,
    /// The `›` before a prompt and in the composer.
    pub prompt_mark: Style,
    /// Reasoning summaries.
    pub reasoning: Style,
    /// `⏺` of a tool call that is running.
    pub tool_running: Style,
    /// `⏺` of a tool call that succeeded.
    pub tool_ok: Style,
    /// A failed tool call's marker and output.
    pub tool_error: Style,
    /// A tool's name.
    pub tool_name: Style,
    /// A tool's arguments and output.
    pub tool_detail: Style,
    /// Markdown headings.
    pub heading: Style,
    /// Inline code.
    pub code: Style,
    /// Code block lines.
    pub code_block: Style,
    /// A code block's language label.
    pub code_label: Style,
    /// Block quote bars and text.
    pub quote: Style,
    /// Links.
    pub link: Style,
    /// List bullets and numbers.
    pub list_marker: Style,
    /// Informational notices.
    pub notice: Style,
    /// Warnings.
    pub warning: Style,
    /// Errors.
    pub error: Style,
    /// The status line.
    pub status: Style,
    /// A running session in the status line.
    pub busy: Style,
    /// A queued steering chip.
    pub chip_queued: Style,
    /// A delivered steering chip.
    pub chip_delivered: Style,
    /// A collapsed paste in the composer.
    pub paste_chip: Style,
    /// The composer's placeholder.
    pub placeholder: Style,
    /// A popup's rows.
    pub popup: Style,
    /// A popup's selected row.
    pub popup_selected: Style,
    /// A popup row's detail column.
    pub popup_detail: Style,
}

fn fg(color: Color) -> Style {
    Style::new().fg(color)
}

fn attr(modifier: Modifier) -> Style {
    Style::new().add_modifier(modifier)
}

impl Theme {
    /// The default for dark backgrounds.
    #[must_use]
    pub fn dark() -> Self {
        Self {
            text: Style::new(),
            muted: fg(Color::DarkGray),
            accent: fg(Color::Cyan),
            user: attr(Modifier::BOLD),
            prompt_mark: fg(Color::Cyan).add_modifier(Modifier::BOLD),
            reasoning: attr(Modifier::DIM | Modifier::ITALIC),
            tool_running: fg(Color::Yellow),
            tool_ok: fg(Color::Green),
            tool_error: fg(Color::Red),
            tool_name: attr(Modifier::BOLD),
            tool_detail: fg(Color::Gray),
            heading: fg(Color::Magenta).add_modifier(Modifier::BOLD),
            code: fg(Color::Cyan),
            code_block: fg(Color::LightYellow),
            code_label: fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
            quote: fg(Color::Green),
            link: fg(Color::Blue).add_modifier(Modifier::UNDERLINED),
            list_marker: fg(Color::Cyan),
            notice: fg(Color::DarkGray),
            warning: fg(Color::Yellow),
            error: fg(Color::Red).add_modifier(Modifier::BOLD),
            status: fg(Color::DarkGray),
            busy: fg(Color::Yellow),
            chip_queued: fg(Color::Yellow),
            chip_delivered: fg(Color::Green),
            paste_chip: fg(Color::Magenta).add_modifier(Modifier::BOLD),
            placeholder: fg(Color::DarkGray),
            popup: Style::new(),
            popup_selected: attr(Modifier::REVERSED),
            popup_detail: fg(Color::DarkGray),
        }
    }

    /// The default for light backgrounds: no light-on-white colours.
    #[must_use]
    pub fn light() -> Self {
        Self {
            accent: fg(Color::Blue),
            prompt_mark: fg(Color::Blue).add_modifier(Modifier::BOLD),
            tool_running: fg(Color::Magenta),
            tool_detail: fg(Color::DarkGray),
            code: fg(Color::Blue),
            code_block: fg(Color::Black),
            list_marker: fg(Color::Blue),
            busy: fg(Color::Magenta),
            chip_queued: fg(Color::Magenta),
            ..Self::dark()
        }
    }

    /// Attributes only, for `NO_COLOR`.
    #[must_use]
    pub fn plain() -> Self {
        let dim = attr(Modifier::DIM);
        Self {
            text: Style::new(),
            muted: dim,
            accent: attr(Modifier::BOLD),
            user: attr(Modifier::BOLD),
            prompt_mark: attr(Modifier::BOLD),
            reasoning: attr(Modifier::DIM | Modifier::ITALIC),
            tool_running: dim,
            tool_ok: Style::new(),
            tool_error: attr(Modifier::BOLD),
            tool_name: attr(Modifier::BOLD),
            tool_detail: Style::new(),
            heading: attr(Modifier::BOLD | Modifier::UNDERLINED),
            code: attr(Modifier::BOLD),
            code_block: Style::new(),
            code_label: dim,
            quote: attr(Modifier::ITALIC),
            link: attr(Modifier::UNDERLINED),
            list_marker: Style::new(),
            notice: dim,
            warning: attr(Modifier::BOLD),
            error: attr(Modifier::BOLD),
            status: dim,
            busy: attr(Modifier::BOLD),
            chip_queued: attr(Modifier::BOLD),
            chip_delivered: dim,
            paste_chip: attr(Modifier::BOLD),
            placeholder: dim,
            popup: Style::new(),
            popup_selected: attr(Modifier::REVERSED),
            popup_detail: dim,
        }
    }

    /// Chooses a theme from the environment: `AIM_THEME` (`dark`, `light`, `plain`) wins, then
    /// `NO_COLOR`, then `COLORFGBG`'s background; dark otherwise. `get` reads a variable.
    #[must_use]
    pub fn detect(get: impl Fn(&str) -> Option<String>) -> Self {
        match get("AIM_THEME").as_deref().map(str::trim) {
            Some("light") => return Self::light(),
            Some("dark") => return Self::dark(),
            Some("plain" | "none") => return Self::plain(),
            _ => {}
        }
        if get("NO_COLOR").is_some_and(|v| !v.is_empty()) {
            return Self::plain();
        }
        match get("COLORFGBG").as_deref().and_then(background_of_colorfgbg) {
            Some(Tone::Light) => Self::light(),
            _ => Self::dark(),
        }
    }
}

/// The background tone of a `COLORFGBG` value (`fg;bg` or `fg;default;bg`): ANSI 7 and 9–15 are
/// light backgrounds, 0–6 and 8 dark.
fn background_of_colorfgbg(value: &str) -> Option<Tone> {
    let bg: u8 = value.rsplit(';').next()?.trim().parse().ok()?;
    Some(if bg == 7 || (9..=15).contains(&bg) { Tone::Light } else { Tone::Dark })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let pairs: Vec<(String, String)> = pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
        move |key| pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    }

    #[test]
    fn detection_prefers_the_override_then_no_color_then_colorfgbg() {
        assert_eq!(Theme::detect(env(&[])), Theme::dark());
        assert_eq!(Theme::detect(env(&[("COLORFGBG", "0;15")])), Theme::light());
        assert_eq!(Theme::detect(env(&[("COLORFGBG", "15;default;0")])), Theme::dark());
        assert_eq!(Theme::detect(env(&[("COLORFGBG", "0;15"), ("NO_COLOR", "1")])), Theme::plain());
        assert_eq!(Theme::detect(env(&[("NO_COLOR", "1"), ("AIM_THEME", "light")])), Theme::light());
        assert_eq!(Theme::detect(env(&[("COLORFGBG", "garbage")])), Theme::dark());
        assert_ne!(Theme::dark(), Theme::light());
    }
}
