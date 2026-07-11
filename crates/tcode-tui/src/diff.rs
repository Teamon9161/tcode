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
            .take(MAX_DIFF_LINES)
            .map(|line| code_line(path, "+ ", line, Some(theme::diff_add_bg())))
            .chain(
                if input["content"].as_str().unwrap_or("").lines().count() > MAX_DIFF_LINES {
                    Some(Line::styled("    … (preview truncated)", theme::dim()))
                } else {
                    None
                },
            )
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
        lines.push(code_line(&path, marker, text, background));
    }
    if patch.lines().count() > MAX_DIFF_LINES {
        lines.push(Line::styled("    … (preview truncated)", theme::dim()));
    }
    lines
}

/// Edit diffs render in full: the user is approving this exact change and
/// must be able to see all of it. Only `write` previews are capped.
fn diff_lines(path: &str, old: &str, new: &str) -> Vec<Line<'static>> {
    let diff = similar::TextDiff::from_lines(old, new);
    diff.iter_all_changes()
        .map(|change| {
            let text = change.value().trim_end_matches('\n');
            match change.tag() {
                ChangeTag::Delete => code_line(path, "- ", text, Some(theme::diff_del_bg())),
                ChangeTag::Insert => code_line(path, "+ ", text, Some(theme::diff_add_bg())),
                ChangeTag::Equal => code_line(path, "  ", text, None),
            }
        })
        .collect()
}

fn code_line(path: &str, marker: &str, text: &str, background: Option<Color>) -> Line<'static> {
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
    let mut spans = vec![Span::styled(format!("  {marker}"), marker_style)];
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
}
