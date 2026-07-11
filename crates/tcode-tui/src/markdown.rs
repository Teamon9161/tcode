//! Compact markdown → styled ratatui lines. Baked once per completed
//! assistant message; streaming text is shown raw until then.

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::ThemeSet;
use syntect::parsing::SyntaxSet;

use crate::theme;

pub struct Renderer {
    syntaxes: SyntaxSet,
    theme: syntect::highlighting::Theme,
}

impl Default for Renderer {
    fn default() -> Self {
        let themes = ThemeSet::load_defaults();
        Self {
            syntaxes: SyntaxSet::load_defaults_newlines(),
            theme: themes.themes["base16-eighties.dark"].clone(),
        }
    }
}

impl Renderer {
    pub fn render(&self, text: &str) -> Vec<Line<'static>> {
        let mut out: Vec<Line<'static>> = Vec::new();
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut style_stack: Vec<Style> = vec![Style::default()];
        let mut list_depth: usize = 0;
        let mut ordered_index: Vec<Option<u64>> = Vec::new();
        let mut in_code = false;
        let mut code_lang = String::new();
        let mut code_buf = String::new();
        let mut quote_depth = 0usize;

        let flush = |spans: &mut Vec<Span<'static>>, out: &mut Vec<Line<'static>>, quote: usize| {
            let mut line_spans = Vec::new();
            if quote > 0 {
                line_spans.push(Span::styled("▎ ".repeat(quote), theme::dim()));
            }
            line_spans.append(spans);
            out.push(Line::from(line_spans));
        };

        let parser = Parser::new_ext(text, Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES);
        for ev in parser {
            match ev {
                Event::Start(tag) => match tag {
                    Tag::Heading { level, .. } => {
                        if !spans.is_empty() {
                            flush(&mut spans, &mut out, quote_depth);
                        }
                        let style = theme::bold().fg(theme::ACCENT);
                        style_stack.push(style);
                        spans.push(Span::styled(
                            format!("{} ", "#".repeat(level as usize)),
                            style,
                        ));
                    }
                    Tag::Emphasis => {
                        let s = style_stack.last().copied().unwrap_or_default();
                        style_stack.push(s.add_modifier(Modifier::ITALIC));
                    }
                    Tag::Strong => {
                        let s = style_stack.last().copied().unwrap_or_default();
                        style_stack.push(s.add_modifier(Modifier::BOLD));
                    }
                    Tag::Strikethrough => {
                        let s = style_stack.last().copied().unwrap_or_default();
                        style_stack.push(s.add_modifier(Modifier::CROSSED_OUT));
                    }
                    Tag::BlockQuote(_) => {
                        if !spans.is_empty() {
                            flush(&mut spans, &mut out, quote_depth);
                        }
                        quote_depth += 1;
                    }
                    Tag::CodeBlock(kind) => {
                        if !spans.is_empty() {
                            flush(&mut spans, &mut out, quote_depth);
                        }
                        in_code = true;
                        code_lang = match kind {
                            CodeBlockKind::Fenced(lang) => lang.to_string(),
                            CodeBlockKind::Indented => String::new(),
                        };
                        code_buf.clear();
                    }
                    Tag::List(start) => {
                        if !spans.is_empty() {
                            flush(&mut spans, &mut out, quote_depth);
                        }
                        list_depth += 1;
                        ordered_index.push(start);
                    }
                    Tag::Item => {
                        let indent = "  ".repeat(list_depth.saturating_sub(1));
                        let marker = match ordered_index.last_mut() {
                            Some(Some(n)) => {
                                let m = format!("{indent}{n}. ");
                                *n += 1;
                                m
                            }
                            _ => format!("{indent}• "),
                        };
                        spans.push(Span::styled(marker, theme::accent()));
                    }
                    Tag::Link { .. } => {
                        let s = style_stack.last().copied().unwrap_or_default();
                        style_stack.push(s.add_modifier(Modifier::UNDERLINED));
                    }
                    Tag::Paragraph => {}
                    _ => {}
                },
                Event::End(tag) => match tag {
                    TagEnd::Heading(_) => {
                        style_stack.pop();
                        flush(&mut spans, &mut out, quote_depth);
                        out.push(Line::default());
                    }
                    TagEnd::Paragraph => {
                        flush(&mut spans, &mut out, quote_depth);
                        out.push(Line::default());
                    }
                    TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Link => {
                        style_stack.pop();
                    }
                    TagEnd::BlockQuote(_) => {
                        if !spans.is_empty() {
                            flush(&mut spans, &mut out, quote_depth);
                        }
                        quote_depth = quote_depth.saturating_sub(1);
                    }
                    TagEnd::CodeBlock => {
                        in_code = false;
                        out.extend(self.highlight(&code_lang, &code_buf));
                        out.push(Line::default());
                    }
                    TagEnd::List(_) => {
                        list_depth = list_depth.saturating_sub(1);
                        ordered_index.pop();
                        if list_depth == 0 {
                            out.push(Line::default());
                        }
                    }
                    TagEnd::Item => {
                        if !spans.is_empty() {
                            flush(&mut spans, &mut out, quote_depth);
                        }
                    }
                    _ => {}
                },
                Event::Text(t) => {
                    if in_code {
                        code_buf.push_str(&t);
                    } else {
                        let style = style_stack.last().copied().unwrap_or_default();
                        // pulldown merges newlines into text rarely; split defensively.
                        let mut first = true;
                        for part in t.split('\n') {
                            if !first {
                                flush(&mut spans, &mut out, quote_depth);
                            }
                            first = false;
                            if !part.is_empty() {
                                spans.push(Span::styled(part.to_string(), style));
                            }
                        }
                    }
                }
                Event::Code(t) => {
                    spans.push(Span::styled(t.to_string(), theme::inline_code()));
                }
                Event::SoftBreak | Event::HardBreak => {
                    flush(&mut spans, &mut out, quote_depth);
                }
                Event::Rule => {
                    out.push(Line::styled("─".repeat(40), theme::dim()));
                }
                Event::TaskListMarker(done) => {
                    spans.push(Span::styled(
                        if done { "[x] " } else { "[ ] " },
                        theme::accent(),
                    ));
                }
                _ => {}
            }
        }
        if !spans.is_empty() {
            flush(&mut spans, &mut out, quote_depth);
        }
        while out.last().is_some_and(|l| l.spans.is_empty()) {
            out.pop();
        }
        out
    }

    fn highlight(&self, lang: &str, code: &str) -> Vec<Line<'static>> {
        let syntax = self
            .syntaxes
            .find_syntax_by_token(lang)
            .unwrap_or_else(|| self.syntaxes.find_syntax_plain_text());
        let mut hl = HighlightLines::new(syntax, &self.theme);
        let mut out = Vec::new();
        for line in code.lines() {
            let mut spans = vec![Span::styled("  ", theme::dim())];
            match hl.highlight_line(line, &self.syntaxes) {
                Ok(ranges) => {
                    for (style, text) in ranges {
                        let fg = style.foreground;
                        spans.push(Span::styled(
                            text.to_string(),
                            Style::default().fg(ratatui::style::Color::Rgb(fg.r, fg.g, fg.b)),
                        ));
                    }
                }
                Err(_) => spans.push(Span::raw(line.to_string())),
            }
            out.push(Line::from(spans));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_basic_markdown() {
        let r = Renderer::default();
        let lines = r.render(
            "# Title\n\nsome **bold** and `code`\n\n```rust\nfn main() {}\n```\n\n- a\n- b",
        );
        let text: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert!(text.iter().any(|l| l.contains("Title")));
        assert!(text.iter().any(|l| l.contains("fn main")));
        assert!(text.iter().any(|l| l.contains("• a")));
    }

    #[test]
    fn fenced_code_uses_syntax_colors_when_language_is_known() {
        let lines = Renderer::default().render("```rust\nlet answer = 42;\n```");
        let code = lines
            .iter()
            .find(|line| {
                line.spans
                    .iter()
                    .any(|span| span.content.contains("answer"))
            })
            .expect("code line");
        // The first span is indentation; at least one code token must carry
        // the colour emitted by syntect rather than the plain terminal style.
        assert!(code
            .spans
            .iter()
            .skip(1)
            .any(|span| span.style.fg.is_some()));
    }
}
