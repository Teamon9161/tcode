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

/// Green — used for tool names and successful status indicators.
pub fn ok() -> Style {
    Style::default().fg(OK)
}

pub fn user_prompt() -> Style {
    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
}

/// The rail down the left of a human turn. It is a display gutter, not
/// text: `transcript::wrap_lines_flagged` re-emits it on every soft-wrap
/// continuation so a long message stays one quoted block.
pub const USER_GUTTER: &str = "▌ ";

pub fn user_gutter() -> Style {
    Style::default()
        .fg(Color::LightCyan)
        .add_modifier(Modifier::BOLD)
}

/// A note is not a turn: an aside the human slipped to the model mid-turn
/// (approval comment or `/note`). It keeps the human rail so it remains a
/// quoted aside, but a coloured `Note:` label distinguishes it from a full
/// user prompt. The label carries the colour; the note's own text stays plain.
pub fn note_label() -> Style {
    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
}

/// A human turn must outweigh the assistant prose it sits among, which is
/// drawn in the terminal's default foreground. Weight, not hue, does that
/// work: colour already carries meaning here (cyan = interactive, green =
/// tool, dim = aside), and washing a long paste in a fifth colour shouts.
/// A filled background is out for the same reason — it would compete with
/// the diff blocks.
pub fn user_message() -> Style {
    Style::default()
        .fg(Color::White)
        .add_modifier(Modifier::BOLD)
}

pub fn thinking() -> Style {
    Style::default().fg(DIM).add_modifier(Modifier::ITALIC)
}

/// Text selection in the input box — matches the transcript's reversed
/// selection so the two read as one selection model.
pub fn selection() -> Style {
    Style::default().add_modifier(Modifier::REVERSED)
}

/// Amber-tinted row background marking the rewind-navigation target.
pub fn rewind_highlight_bg() -> Color {
    Color::Rgb(82, 62, 24)
}

/// Red highlight for API/tool errors so a failure is unmissable in
/// scrollback — a bold light foreground on a deep-red background.
pub fn error_highlight() -> Style {
    Style::default()
        .fg(Color::White)
        .bg(Color::Rgb(90, 24, 28))
        .add_modifier(Modifier::BOLD)
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
