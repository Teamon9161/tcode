use ratatui::style::{Color, Modifier, Style};

pub const ACCENT: Color = Color::Cyan;
pub const DIM: Color = Color::DarkGray;
pub const ERROR: Color = Color::Red;
pub const WARN: Color = Color::Yellow;
pub const OK: Color = Color::Green;

pub fn dim() -> Style {
    Style::default().fg(DIM)
}

/// Rounded-box borders around the input area and popups.
pub fn border() -> Style {
    Style::default().fg(DIM)
}

pub fn border_active() -> Style {
    Style::default().fg(ACCENT)
}

pub fn accent() -> Style {
    Style::default().fg(ACCENT)
}

pub fn bold() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

pub fn user_prompt() -> Style {
    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
}

/// A quiet cue in scrollback so human messages do not get lost among
/// assistant prose and tool output.
pub fn user_message() -> Style {
    Style::default().fg(Color::White).bg(Color::Rgb(52, 52, 70))
}

pub fn user_prompt_message() -> Style {
    user_prompt().bg(Color::Rgb(52, 52, 70))
}

pub fn thinking() -> Style {
    Style::default().fg(DIM).add_modifier(Modifier::ITALIC)
}

pub fn diff_add_bg() -> Color {
    Color::Rgb(20, 62, 38)
}

pub fn diff_del_bg() -> Color {
    Color::Rgb(78, 30, 34)
}

pub fn inline_code() -> Style {
    Style::default().fg(Color::LightCyan)
}
