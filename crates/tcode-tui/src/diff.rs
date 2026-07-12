//! Syntax-coloured file-change previews. Diff polarity is expressed by a
//! subtle background, leaving the foreground free for the code's own syntax.

use std::sync::OnceLock;

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;
use similar::ChangeTag;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;

use crate::theme;

const MAX_DIFF_LINES: usize = 80;

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

/// True when a shell/bash command is long or multi-line enough that cramming
/// it into the one-line call header would truncate or corrupt the display.
pub fn command_is_block(tool: &str, input: &Value) -> bool {
    if tool != "shell" && tool != "bash" {
        return false;
    }
    let cmd = input["command"].as_str().unwrap_or("");
    cmd.contains('\n') || cmd.chars().count() > COMMAND_HEADER_MAX
}

/// Render a shell/bash command as an indented block, shown under a terse
/// header. Empty for short single-line commands or non-shell tools, so callers
/// can extend unconditionally.
pub fn render_command(tool: &str, input: &Value) -> Vec<Line<'static>> {
    if !command_is_block(tool, input) {
        return Vec::new();
    }
    let cmd = input["command"].as_str().unwrap_or("");
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

/// Render the file-change preview for an edit/write call, if applicable.
pub fn render_change(tool: &str, input: &Value) -> Vec<Line<'static>> {
    let path = input["path"].as_str().unwrap_or("");
    match tool {
        "edit" => diff_lines(
            path,
            input["old_string"].as_str().unwrap_or(""),
            input["new_string"].as_str().unwrap_or(""),
        ),
        "write" => input["content"]
            .as_str()
            .unwrap_or("")
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
            .collect(),
        _ => Vec::new(),
    }
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

/// Live edit/write diffs render in full: the user is approving this exact
/// change and must be able to see all of it. Imported historical patches stay
/// capped separately because they are only transcript context.
fn diff_lines(path: &str, old: &str, new: &str) -> Vec<Line<'static>> {
    let diff = similar::TextDiff::from_lines(old, new);
    let mut old_line = edit_start_line(path, old);
    let mut new_line = old_line;
    diff.iter_all_changes()
        .map(|change| {
            let text = change.value().trim_end_matches('\n');
            match change.tag() {
                ChangeTag::Delete => {
                    let line = old_line;
                    old_line += 1;
                    code_line(path, "- ", Some(line), text, Some(theme::diff_del_bg()))
                }
                ChangeTag::Insert => {
                    let line = new_line;
                    new_line += 1;
                    code_line(path, "+ ", Some(line), text, Some(theme::diff_add_bg()))
                }
                ChangeTag::Equal => {
                    let line = old_line;
                    old_line += 1;
                    new_line += 1;
                    code_line(path, "  ", Some(line), text, None)
                }
            }
        })
        .collect()
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
        Ok(ranges) => spans.extend(ranges.into_iter().map(|(style, token)| {
            let foreground = style.foreground;
            let mut style =
                Style::default().fg(Color::Rgb(foreground.r, foreground.g, foreground.b));
            if let Some(background) = background {
                style = style.bg(background);
            }
            Span::styled(token.to_string(), style)
        })),
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
    fn diff_uses_background_without_losing_syntax_foreground() {
        let lines = render_change(
            "edit",
            &serde_json::json!({
                "path": "src/main.rs", "old_string": "let x = 1;", "new_string": "let x = 2;"
            }),
        );
        let added = lines
            .iter()
            .find(|line| line.spans[0].content.contains('+'))
            .unwrap();
        assert_eq!(added.spans[0].style.bg, Some(theme::diff_add_bg()));
        assert!(added
            .spans
            .iter()
            .skip(1)
            .any(|span| span.style.fg.is_some()));
    }

    #[test]
    fn imported_patch_uses_the_same_add_delete_backgrounds() {
        let lines = render_unified_patch("*** Update File: src/main.rs\n@@\n-old\n+new");
        assert_eq!(lines[2].spans[0].style.bg, Some(theme::diff_del_bg()));
        assert_eq!(lines[3].spans[0].style.bg, Some(theme::diff_add_bg()));
    }

    #[test]
    fn live_diff_shows_left_gutter_line_numbers() {
        let lines = render_change(
            "edit",
            &serde_json::json!({
                "path": "missing.rs",
                "old_string": "alpha\nbeta\n",
                "new_string": "alpha\ngamma\n",
            }),
        );
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
        let lines = render_change(
            "edit",
            &serde_json::json!({
                "path": "src/main.rs",
                "old_string": old,
                "new_string": new,
            }),
        );

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
        let lines = render_change(
            "write",
            &serde_json::json!({
                "path": "src/main.rs",
                "content": content,
            }),
        );

        assert!(lines.len() > MAX_DIFF_LINES);
        assert!(!lines.iter().any(|line| line
            .spans
            .iter()
            .any(|span| span.content.contains("preview truncated"))));
    }

    #[test]
    fn short_single_line_command_stays_in_header() {
        let input = serde_json::json!({ "command": "git status" });
        assert!(!command_is_block("shell", &input));
        assert!(render_command("shell", &input).is_empty());
    }

    #[test]
    fn multiline_command_renders_as_a_block() {
        let input = serde_json::json!({
            "command": "python - <<'PY'\nimport sys\nprint(sys.version)\nPY",
        });
        assert!(command_is_block("shell", &input));
        let lines = render_command("shell", &input);
        assert_eq!(lines.len(), 4);
        // Each block row is indented.
        assert!(lines[0].spans[0].content.starts_with("    python"));
    }

    #[test]
    fn long_single_line_command_becomes_a_block() {
        let cmd = format!("echo {}", "x".repeat(100));
        let input = serde_json::json!({ "command": cmd });
        assert!(command_is_block("bash", &input));
        assert_eq!(render_command("bash", &input).len(), 1);
    }

    #[test]
    fn non_shell_tools_never_block() {
        let input = serde_json::json!({ "path": "src/main.rs" });
        assert!(!command_is_block("read", &input));
        assert!(render_command("read", &input).is_empty());
    }
}
