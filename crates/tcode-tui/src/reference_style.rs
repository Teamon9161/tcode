use ratatui::text::Span;

use crate::theme;

/// Render user-authored `@path` markers with the same accent used by the input
/// editor. This is syntax-level rather than index-level so restored transcripts
/// do not depend on the asynchronous completion index being ready.
pub(crate) fn user_text_spans(text: &str) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    let ranges = reference_marker_ranges(&chars);
    let mut spans = Vec::new();
    let mut start = 0;
    let mut accented = ranges.iter().any(|&(from, to)| from == 0 && 0 < to);

    for index in 1..chars.len() {
        let next_accented = ranges.iter().any(|&(from, to)| from <= index && index < to);
        if next_accented == accented {
            continue;
        }
        let segment: String = chars[start..index].iter().collect();
        spans.push(if accented {
            Span::styled(segment, theme::accent())
        } else {
            Span::styled(segment, theme::user_message())
        });
        start = index;
        accented = next_accented;
    }
    if start < chars.len() {
        let segment: String = chars[start..].iter().collect();
        spans.push(if accented {
            Span::styled(segment, theme::accent())
        } else {
            Span::styled(segment, theme::user_message())
        });
    }
    spans
}

fn reference_marker_ranges(chars: &[char]) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        if chars[index] != '@' || !reference_boundary(chars, index) {
            index += 1;
            continue;
        }
        let mut end = index + 1;
        if chars.get(end) == Some(&'"') {
            end += 1;
            while end < chars.len() && chars[end] != '"' {
                end += 1;
            }
            if end < chars.len() {
                end += 1;
            }
        } else {
            while end < chars.len() && reference_token_char(chars[end]) {
                end += 1;
            }
        }
        if end > index + 1 {
            ranges.push((index, end));
        }
        index = end;
    }
    ranges
}

fn reference_boundary(chars: &[char], at: usize) -> bool {
    at == 0 || (!chars[at - 1].is_alphanumeric() && chars[at - 1] != '_')
}

fn reference_token_char(c: char) -> bool {
    !c.is_whitespace()
        && !matches!(
            c,
            '@' | '`' | '"' | '\'' | ')' | '(' | '[' | ']' | '{' | '}' | ',' | ';' | ':'
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accents_reference_markers_but_not_email_addresses() {
        let spans = user_text_spans("see @src/app.rs and @\"docs/my plan.md\"; me@example.com");
        let accented: Vec<_> = spans
            .iter()
            .filter(|span| span.style.fg == Some(theme::ACCENT))
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(accented, ["@src/app.rs", "@\"docs/my plan.md\""]);
    }
}
