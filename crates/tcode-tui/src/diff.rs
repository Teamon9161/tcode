//! Syntax-coloured file-change previews. Diff polarity is expressed by a
//! subtle background, leaving the foreground free for the code's own syntax.

use std::sync::OnceLock;

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use similar::ChangeTag;
use std::ops::Range;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;

use crate::theme;

const MAX_DIFF_LINES: usize = 80;
/// Word-level emphasis earns its brightness only when the two sides are similar
/// enough that the change is a minority of the line. Past this share it is a
/// rewrite, not a correction, and the whole line is filled calmly instead.
const MAX_INLINE_EMPHASIS_FRACTION_DENOMINATOR: usize = 2;

struct Highlighter {
    syntaxes: SyntaxSet,
    theme: Theme,
}

fn highlighter() -> &'static Highlighter {
    static HIGHLIGHTER: OnceLock<Highlighter> = OnceLock::new();
    HIGHLIGHTER.get_or_init(|| {
        let themes = ThemeSet::load_defaults();
        Highlighter {
            syntaxes: SyntaxSet::load_defaults_newlines(),
            theme: themes.themes["base16-eighties.dark"].clone(),
        }
    })
}

/// Single-line commands up to this width stay inline in the call header;
/// longer or multi-line ones collapse the header and render as a block.
const COMMAND_HEADER_MAX: usize = 72;
/// Cap on command-block lines so a giant heredoc can't flood the transcript.
const MAX_COMMAND_LINES: usize = 40;

/// True when a command is long or multi-line enough that cramming it into
/// the one-line call header would truncate or corrupt the display.
pub fn command_is_block(cmd: &str) -> bool {
    cmd.contains('\n') || cmd.chars().count() > COMMAND_HEADER_MAX
}

/// Render a command as an indented block, shown under a terse header. Empty
/// for short single-line commands, so callers can extend unconditionally.
pub fn command_block(cmd: &str) -> Vec<Line<'static>> {
    if !command_is_block(cmd) {
        return Vec::new();
    }
    let total = cmd.lines().count();
    let mut lines: Vec<Line<'static>> = cmd
        .lines()
        .take(MAX_COMMAND_LINES)
        .map(|line| Line::styled(format!("    {line}"), theme::dim()))
        .collect();
    if total > MAX_COMMAND_LINES {
        lines.push(Line::styled(
            format!("    … (+{} more lines)", total - MAX_COMMAND_LINES),
            theme::dim(),
        ));
    }
    lines
}

/// Render a numbered `read` result with syntax foregrounds but no change
/// background. Lines which are not part of the tool's numbered file payload
/// remain subdued metadata, such as freshness and pagination notices.
pub fn read_preview(path: &str, content: &str) -> Vec<Line<'static>> {
    content
        .lines()
        .map(|line| match line.split_once('\t') {
            Some((number, text)) => match number.trim().parse::<usize>() {
                Ok(number) => code_line(path, "  ", Some(number), text, None),
                Err(_) => Line::styled(format!("  {line}"), theme::dim()),
            },
            None => Line::styled(format!("  {line}"), theme::dim()),
        })
        .collect()
}

/// Full new-content preview for a `write` call: every line is an addition.
pub fn write_preview(path: &str, content: &str) -> Vec<Line<'static>> {
    content
        .lines()
        .enumerate()
        .map(|(index, line)| {
            code_line(
                path,
                "+ ",
                Some(index + 1),
                line,
                Some(theme::diff_add_bg()),
            )
        })
        .collect()
}

/// Render a unified patch imported from another agent's transcript.  It uses
/// the exact same syntax foreground and add/delete backgrounds as a live
/// tcode edit, while treating patch headers as metadata rather than code.
pub fn render_unified_patch(patch: &str) -> Vec<Line<'static>> {
    let mut path = String::new();
    let mut lines = Vec::new();
    for line in patch.lines().take(MAX_DIFF_LINES) {
        if let Some(updated) = line.strip_prefix("*** Update File: ") {
            path = updated.trim().to_owned();
            lines.push(Line::styled(format!("  {line}"), theme::bold()));
            continue;
        }
        if line.starts_with("*** ") || line.starts_with("@@") {
            lines.push(Line::styled(format!("  {line}"), theme::dim()));
            continue;
        }
        let (marker, text, background) = match line.as_bytes().first() {
            Some(b'+') if !line.starts_with("+++") => {
                ("+ ", &line[1..], Some(theme::diff_add_bg()))
            }
            Some(b'-') if !line.starts_with("---") => {
                ("- ", &line[1..], Some(theme::diff_del_bg()))
            }
            Some(b' ') => ("  ", &line[1..], None),
            _ => ("  ", line, None),
        };
        lines.push(code_line(&path, marker, None, text, background));
    }
    if patch.lines().count() > MAX_DIFF_LINES {
        lines.push(Line::styled("    … (preview truncated)", theme::dim()));
    }
    lines
}

/// Live edit diffs render in full: the user is approving this exact change
/// and must be able to see all of it. Imported historical patches stay
/// capped separately because they are only transcript context.
pub fn edit_diff(path: &str, old: &str, new: &str) -> Vec<Line<'static>> {
    let diff = similar::TextDiff::from_lines(old, new);
    let mut old_line = edit_start_line(path, old);
    let mut new_line = old_line;
    let mut lines = Vec::new();
    for op in diff.ops() {
        for change in diff.iter_inline_changes(op) {
            let (marker, number, base_bg, emphasis_bg) = match change.tag() {
                ChangeTag::Delete => {
                    let line = old_line;
                    old_line += 1;
                    ("- ", line, theme::diff_del_bg(), theme::diff_del_emph_bg())
                }
                ChangeTag::Insert => {
                    let line = new_line;
                    new_line += 1;
                    ("+ ", line, theme::diff_add_bg(), theme::diff_add_emph_bg())
                }
                ChangeTag::Equal => {
                    let line = old_line;
                    old_line += 1;
                    new_line += 1;
                    ("  ", line, Color::Reset, Color::Reset)
                }
            };
            let (text, ranges) = inline_emphasis(&change);
            // Changed lines always retain their polarity. A local correction
            // overlays only its changed words; a rewrite keeps the calm base
            // fill without pretending it has useful token-level matches.
            let background = (change.tag() != ChangeTag::Equal).then_some(base_bg);
            let emphasis = (!ranges.is_empty()).then_some(Emphasis {
                ranges,
                bg: emphasis_bg,
            });
            lines.push(code_line_emphasized(
                path,
                marker,
                Some(number),
                &text,
                background,
                emphasis.as_ref(),
            ));
        }
    }
    lines
}

/// Flatten one inline change into its text plus the byte ranges that actually
/// differ from the other side. A wholly-rewritten line reports no ranges: the
/// line background already says "all of this changed", and lifting every byte
/// would only make the emphasis meaningless everywhere else.
fn inline_emphasis(change: &similar::InlineChange<'_, str>) -> (String, Vec<Range<usize>>) {
    let mut text = String::new();
    let mut ranges: Vec<Range<usize>> = Vec::new();
    for (emphasized, value) in change.iter_strings_lossy() {
        let start = text.len();
        text.push_str(&value);
        if emphasized {
            ranges.push(start..text.len());
        }
    }
    let trimmed = text.trim_end_matches('\n').len();
    text.truncate(trimmed);
    ranges.retain_mut(|range| {
        range.end = range.end.min(trimmed);
        range.start < range.end
    });
    if inline_emphasis_is_local(&text, &ranges) {
        ranges = trimmed_emphasis_ranges(&text, ranges);
    } else {
        ranges.clear();
    }
    (text, ranges)
}

/// Emphasis is useful only when the change is a minority of the line: the two
/// sides are similar and the eye should land on the few bytes that differ.
/// `similar` can emit a large changed run after a shared prefix; once the
/// changed (non-whitespace) characters reach half the visible line, that is a
/// rewrite, and the caller fills the line calmly instead of lighting it up.
fn inline_emphasis_is_local(text: &str, ranges: &[Range<usize>]) -> bool {
    let mut visible = 0usize;
    let mut changed = 0usize;
    for (start, ch) in text.char_indices() {
        if ch.is_whitespace() {
            continue;
        }
        visible += 1;
        let end = start + ch.len_utf8();
        if ranges
            .iter()
            .any(|range| range.start <= start && end <= range.end)
        {
            changed += 1;
        }
    }
    changed > 0 && changed * MAX_INLINE_EMPHASIS_FRACTION_DENOMINATOR <= visible
}

/// Preserve the whitespace inside a changed phrase so its bright background is
/// continuous. Only its leading and trailing layout whitespace stays at the
/// line's ordinary diff colour.
fn trimmed_emphasis_ranges(text: &str, ranges: Vec<Range<usize>>) -> Vec<Range<usize>> {
    ranges
        .into_iter()
        .filter_map(|range| {
            let changed = &text[range.clone()];
            let leading = changed.len() - changed.trim_start().len();
            let trailing = changed.len() - changed.trim_end().len();
            let start = range.start + leading;
            let end = range.end - trailing;
            (start < end).then_some(start..end)
        })
        .collect()
}

/// The parts of a changed line that differ, and the background that lifts them
/// out of the line's own.
struct Emphasis {
    ranges: Vec<Range<usize>>,
    bg: Color,
}

/// Split one syntax-highlighted token at the emphasis boundaries that fall
/// inside it, so a changed word keeps its syntax colour and only swaps
/// background. `start` is the token's byte offset within the line.
fn emphasis_pieces<'a>(
    token: &'a str,
    start: usize,
    emphasis: Option<&Emphasis>,
) -> Vec<(&'a str, bool)> {
    let Some(emphasis) = emphasis else {
        return vec![(token, false)];
    };
    let end = start + token.len();
    let mut pieces = Vec::new();
    let mut cursor = start;
    for range in &emphasis.ranges {
        if range.end <= cursor || range.start >= end {
            continue;
        }
        let hit = range.start.max(cursor)..range.end.min(end);
        if hit.start > cursor {
            pieces.push((&token[cursor - start..hit.start - start], false));
        }
        pieces.push((&token[hit.start - start..hit.end - start], true));
        cursor = hit.end;
    }
    if cursor < end {
        pieces.push((&token[cursor - start..], false));
    }
    pieces
}

fn edit_start_line(path: &str, old: &str) -> usize {
    if old.is_empty() {
        return 1;
    }
    let Ok(content) = std::fs::read_to_string(path) else {
        return 1;
    };
    let Some(byte_pos) = content.find(old) else {
        return 1;
    };
    content[..byte_pos].bytes().filter(|b| *b == b'\n').count() + 1
}

fn code_line(
    path: &str,
    marker: &str,
    line_number: Option<usize>,
    text: &str,
    background: Option<Color>,
) -> Line<'static> {
    code_line_emphasized(path, marker, line_number, text, background, None)
}

fn code_line_emphasized(
    path: &str,
    marker: &str,
    line_number: Option<usize>,
    text: &str,
    background: Option<Color>,
    emphasis: Option<&Emphasis>,
) -> Line<'static> {
    let highlighter = highlighter();
    let extension = std::path::Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .or_else(|| {
            path.split_whitespace()
                .filter_map(|part| {
                    std::path::Path::new(
                        part.trim_matches(|c: char| !c.is_alphanumeric() && c != '.'),
                    )
                    .extension()
                    .and_then(|extension| extension.to_str())
                })
                .next_back()
        });
    let syntax = extension
        .and_then(|extension| highlighter.syntaxes.find_syntax_by_extension(extension))
        .unwrap_or_else(|| highlighter.syntaxes.find_syntax_plain_text());
    let mut line_highlighter = HighlightLines::new(syntax, &highlighter.theme);
    let marker_style = Style::default()
        .fg(match marker {
            "+ " => theme::OK,
            "- " => theme::ERROR,
            _ => theme::DIM,
        })
        .bg(background.unwrap_or(Color::Reset));
    let line_no = line_number
        .map(|n| format!("{n:>4}"))
        .unwrap_or_else(|| "    ".to_string());
    let mut spans = vec![
        Span::styled(format!("  {marker}"), marker_style),
        Span::styled(line_no, marker_style),
        Span::styled(" │ ", marker_style),
    ];
    match line_highlighter.highlight_line(text, &highlighter.syntaxes) {
        Ok(ranges) => {
            let mut offset = 0;
            for (style, token) in ranges {
                let foreground = style.foreground;
                for (piece, emphasized) in emphasis_pieces(token, offset, emphasis) {
                    let mut style =
                        Style::default().fg(Color::Rgb(foreground.r, foreground.g, foreground.b));
                    match (emphasized, emphasis, background) {
                        (true, Some(emphasis), _) => style = style.bg(emphasis.bg),
                        (_, _, Some(background)) => style = style.bg(background),
                        _ => {}
                    }
                    spans.push(Span::styled(piece.to_string(), style));
                }
                offset += token.len();
            }
        }
        Err(_) => spans.push(Span::styled(
            text.to_string(),
            Style::default().bg(background.unwrap_or(Color::Reset)),
        )),
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_preview_keeps_source_line_numbers_and_syntax_foregrounds() {
        let lines = read_preview(
            "src/main.rs",
            "note: this file changed on disk since you last read it.\n     7\tlet answer = 42;\n",
        );
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].style.fg, Some(theme::DIM));
        let rendered: String = lines[1]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(rendered.contains("7 │ let answer = 42;"));
        assert!(lines[1]
            .spans
            .iter()
            .skip(3)
            .any(|span| span.style.fg.is_some()));
    }

    #[test]
    fn diff_uses_background_without_losing_syntax_foreground() {
        let lines = edit_diff("src/main.rs", "let x = 1;", "let x = 2;");
        let added = lines
            .iter()
            .find(|line| line.spans[0].content.contains('+'))
            .unwrap();
        // The single changed byte carries a diff background...
        assert!(added
            .spans
            .iter()
            .any(|span| span.style.bg == Some(theme::diff_add_emph_bg())));
        // ...while syntax colouring still drives the foreground.
        assert!(added
            .spans
            .iter()
            .skip(1)
            .any(|span| span.style.fg.is_some()));
    }

    /// The whole point: in a mostly-unchanged line the eye should land on the
    /// words that differ, not have to diff the sentence itself.
    #[test]
    fn changed_words_inside_a_line_get_the_emphasis_background() {
        let lines = edit_diff(
            "notes.md",
            "the quick brown fox jumps over the lazy dog\n",
            "the quick red wolf fox jumps over the lazy dog\n",
        );
        let added = lines
            .iter()
            .find(|line| line.spans[0].content.contains('+'))
            .unwrap();
        let emphasized: String = added
            .spans
            .iter()
            .filter(|span| span.style.bg == Some(theme::diff_add_emph_bg()))
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(emphasized.trim(), "red wolf");
        assert!(
            emphasized.contains(' '),
            "the highlight must remain continuous across a changed phrase"
        );
        let shared: String = added
            .spans
            .iter()
            .skip(3)
            .filter(|span| span.style.bg == Some(theme::diff_add_bg()))
            .map(|span| span.content.as_ref())
            .collect();
        assert!(
            shared.contains("quick"),
            "shared words keep the add base background"
        );
        assert!(shared.contains("jumps over the lazy dog"));
        assert_eq!(added.spans[0].style.bg, Some(theme::diff_add_bg()));
        assert_eq!(added.spans[1].style.bg, Some(theme::diff_add_bg()));
    }

    /// A line with nothing in common can't pinpoint a difference, so it gets a
    /// calm full-line fill rather than a scatter of bright emphasis.
    #[test]
    fn a_wholly_rewritten_line_gets_no_word_emphasis() {
        let lines = edit_diff("notes.md", "alpha alpha alpha\n", "zulu victor tango\n");
        assert!(lines.iter().flat_map(|line| line.spans.iter()).all(|span| {
            span.style.bg != Some(theme::diff_add_emph_bg())
                && span.style.bg != Some(theme::diff_del_emph_bg())
        }));
        // It still carries the base fill so the change stays visible.
        assert!(lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .any(|span| span.style.bg == Some(theme::diff_del_bg())));
    }

    #[test]
    fn inline_emphasis_requires_a_small_local_change() {
        let local = "the quick red fox jumps over the lazy dog";
        assert!(inline_emphasis_is_local(local, &[10..13]));

        let rewrite = "stable prefix followed by an extensively rewritten paragraph";
        assert!(!inline_emphasis_is_local(rewrite, &[14..rewrite.len()]));
        assert!(!inline_emphasis_is_local(
            "delete this line",
            &[0..6, 7..11, 12..16]
        ));
    }

    #[test]
    fn inline_emphasis_keeps_spaces_inside_a_changed_phrase() {
        let text = "prefix  changed words  suffix";
        let phrase = "  changed words  ";
        let start = text.find(phrase).unwrap();
        assert_eq!(
            trimmed_emphasis_ranges(text, vec![start..start + phrase.len()]),
            vec![8..21]
        );
    }

    #[test]
    fn imported_patch_uses_the_same_add_delete_backgrounds() {
        let lines = render_unified_patch("*** Update File: src/main.rs\n@@\n-old\n+new");
        assert_eq!(lines[2].spans[0].style.bg, Some(theme::diff_del_bg()));
        assert_eq!(lines[3].spans[0].style.bg, Some(theme::diff_add_bg()));
    }

    #[test]
    fn live_diff_shows_left_gutter_line_numbers() {
        let lines = edit_diff("missing.rs", "alpha\nbeta\n", "alpha\ngamma\n");
        let changed = lines
            .iter()
            .find(|line| line.spans[0].content.contains('-'))
            .unwrap();
        assert_eq!(changed.spans[1].content.as_ref(), "   2");
        assert_eq!(changed.spans[2].content.as_ref(), " │ ");
    }

    #[test]
    fn approved_edit_diff_is_not_truncated() {
        let old = (0..81)
            .map(|line| format!("let value_{line} = 1;\n"))
            .collect::<String>();
        let new = (0..81)
            .map(|line| format!("let value_{line} = 2;\n"))
            .collect::<String>();
        let lines = edit_diff("src/main.rs", &old, &new);

        assert!(lines.len() > MAX_DIFF_LINES);
        assert!(!lines.iter().any(|line| line
            .spans
            .iter()
            .any(|span| span.content.contains("preview truncated"))));
    }

    #[test]
    fn approved_write_diff_is_not_truncated() {
        let content = (0..81)
            .map(|line| format!("let value_{line} = 1;\n"))
            .collect::<String>();
        let lines = write_preview("src/main.rs", &content);

        assert!(lines.len() > MAX_DIFF_LINES);
        assert!(!lines.iter().any(|line| line
            .spans
            .iter()
            .any(|span| span.content.contains("preview truncated"))));
    }

    #[test]
    fn short_single_line_command_stays_in_header() {
        assert!(!command_is_block("git status"));
        assert!(command_block("git status").is_empty());
    }

    #[test]
    fn multiline_command_renders_as_a_block() {
        let cmd = "python - <<'PY'\nimport sys\nprint(sys.version)\nPY";
        assert!(command_is_block(cmd));
        let lines = command_block(cmd);
        assert_eq!(lines.len(), 4);
        // Each block row is indented.
        assert!(lines[0].spans[0].content.starts_with("    python"));
    }

    #[test]
    fn long_single_line_command_becomes_a_block() {
        let cmd = format!("echo {}", "x".repeat(100));
        assert!(command_is_block(&cmd));
        assert_eq!(command_block(&cmd).len(), 1);
    }
}
