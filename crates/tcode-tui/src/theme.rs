use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

// A pinned dark palette. Named ANSI colours resolve against whatever
// palette the emulator ships — warm themes render "DarkGray" as pale
// brown and "White" as cream, which used to tint the whole transcript.
// Fixed RGB values keep the UI identical in every terminal, matching
// the diff backgrounds and syntect output that were always RGB.
// Dark-first by design; a light variant can arrive as `[ui] theme`.
//
// Hue semantics: cyan = interactive, green = tool, dim = aside.
pub const ACCENT: Color = Color::Rgb(79, 195, 217);
pub const DIM: Color = Color::Rgb(124, 132, 144);
pub const ERROR: Color = Color::Rgb(224, 108, 117);
pub const WARN: Color = Color::Rgb(217, 163, 87);
pub const OK: Color = Color::Rgb(87, 199, 135);
/// A lighter accent for the user rail and inline code: same hue as
/// ACCENT, lifted for small glyphs that need to read at a glance.
const ACCENT_LIGHT: Color = Color::Rgb(122, 220, 232);
/// One notch above the terminal's default foreground, for the user's
/// own words and error text on filled backgrounds.
const FG_BRIGHT: Color = Color::Rgb(232, 236, 240);
/// Math is rare enough to afford its own hue, distinct from the
/// cyan/green working colours.
const MATH: Color = Color::Rgb(199, 146, 234);

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
    Style::default().fg(ACCENT_LIGHT)
}

/// A note is not a turn: an aside the human slipped to the model mid-turn
/// (approval comment or `/note`). It keeps the human rail so it remains a
/// quoted aside, but a coloured `Note:` label distinguishes it from a full
/// user prompt. The label carries the colour; the note's own text stays plain.
pub fn note_label() -> Style {
    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
}

/// A human turn must stand out from the assistant prose it sits among, which
/// is drawn in the terminal's default foreground. The rail does most of that
/// work; the text only lifts one notch of brightness above default. Weight is
/// deliberately not used: bold reads as a larger font on a long paste and
/// shouts over the whole screen. Hue is out too — colour already carries
/// meaning here (cyan = interactive, green = tool, dim = aside) — and a filled
/// background would compete with the diff blocks.
pub fn user_message() -> Style {
    Style::default().fg(FG_BRIGHT)
}

/// Text selection in the input box — matches the transcript's reversed
/// selection so the two read as one selection model.
pub fn selection() -> Style {
    Style::default().add_modifier(Modifier::REVERSED)
}

/// A low-contrast background for the actionable header of a hovered tool
/// record. It is intentionally not reverse-video: diffs retain their own
/// polarity, and detail rows stay visually untouched.
pub fn hover_highlight() -> Style {
    Style::default().bg(Color::Rgb(46, 56, 68))
}

/// Amber-tinted row background marking the rewind-navigation target.
pub fn rewind_highlight_bg() -> Color {
    Color::Rgb(82, 62, 24)
}

/// Red highlight for API/tool errors so a failure is unmissable in
/// scrollback — a bold light foreground on a deep-red background.
pub fn error_highlight() -> Style {
    Style::default()
        .fg(FG_BRIGHT)
        .bg(Color::Rgb(90, 24, 28))
        .add_modifier(Modifier::BOLD)
}

pub fn diff_add_bg() -> Color {
    Color::Rgb(20, 62, 38)
}

pub fn diff_del_bg() -> Color {
    Color::Rgb(78, 30, 34)
}

/// The words that actually differ inside a changed line. Same hue as the
/// line's background, lifted enough to be found at a glance — a replaced
/// paragraph is mostly unchanged text, and the eye should not have to diff it.
pub fn diff_add_emph_bg() -> Color {
    Color::Rgb(34, 110, 66)
}

pub fn diff_del_emph_bg() -> Color {
    Color::Rgb(132, 46, 52)
}

pub fn inline_code() -> Style {
    Style::default().fg(ACCENT_LIGHT)
}

/// Math renders as a best-effort Unicode linearization (`mathfmt`), so the
/// colour alone must keep it visibly distinct from prose and code.
pub fn math_inline() -> Style {
    Style::default().fg(MATH).add_modifier(Modifier::ITALIC)
}

pub fn math_block() -> Style {
    Style::default().fg(MATH)
}

/// Anchor colours for the startup logo: the tool green sliding into the
/// interactive cyan — the wordmark spans the palette's two working hues
/// instead of introducing a third. Anchors sit slightly outside OK and
/// ACCENT so the sweep stays visible across only twenty columns.
const LOGO_FROM: (u8, u8, u8) = (98, 205, 125);
const LOGO_TO: (u8, u8, u8) = (72, 188, 228);

/// One span per character, horizontally interpolated between the logo
/// anchors. Rows of equal length share column colours, so a multi-row
/// logo reads as a single gradient surface.
pub fn logo_gradient(text: &str) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    let last = chars.len().saturating_sub(1).max(1) as f32;
    chars
        .iter()
        .enumerate()
        .map(|(i, &c)| {
            let t = i as f32 / last;
            let lerp = |a: u8, b: u8| (f32::from(a) + (f32::from(b) - f32::from(a)) * t) as u8;
            Span::styled(
                c.to_string(),
                Style::default().fg(Color::Rgb(
                    lerp(LOGO_FROM.0, LOGO_TO.0),
                    lerp(LOGO_FROM.1, LOGO_TO.1),
                    lerp(LOGO_FROM.2, LOGO_TO.2),
                )),
            )
        })
        .collect()
}
