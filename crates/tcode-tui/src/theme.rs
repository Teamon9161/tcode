use ratatui::style::{Color, Modifier, Style};

pub const ACCENT: Color = Color::Cyan;
pub const DIM: Color = Color::DarkGray;
pub const ERROR: Color = Color::Red;
pub const WARN: Color = Color::Yellow;
pub const OK: Color = Color::Green;

pub fn dim() -> Style {
    Style::default().fg(DIM)
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

pub fn thinking() -> Style {
    Style::default().fg(DIM).add_modifier(Modifier::ITALIC)
}

pub fn diff_add() -> Style {
    Style::default().fg(OK)
}

pub fn diff_del() -> Style {
    Style::default().fg(ERROR)
}

pub fn inline_code() -> Style {
    Style::default().fg(Color::LightCyan)
}
