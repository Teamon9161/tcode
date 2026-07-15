//! Compact markdown → styled ratatui lines. Used both for finalized assistant
//! messages and for the still-streaming transcript block that is replaced in
//! place as deltas arrive.

use pulldown_cmark::{Alignment, CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::ThemeSet;
use syntect::parsing::SyntaxSet;
use unicode_width::UnicodeWidthStr;

use crate::theme;

pub struct Renderer {
    syntaxes: SyntaxSet,
    theme: syntect::highlighting::Theme,
}

struct Table {
    alignments: Vec<Alignment>,
    header: Vec<Vec<Span<'static>>>,
    rows: Vec<Vec<Vec<Span<'static>>>>,
    row: Vec<Vec<Span<'static>>>,
}

impl Table {
    fn new(alignments: Vec<Alignment>) -> Self {
        Self {
            alignments,
            header: Vec::new(),
            rows: Vec::new(),
            row: Vec::new(),
        }
    }
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
        let mut table: Option<Table> = None;
        let mut in_table_head = false;
        let mut display_math = false;

        let flush = |spans: &mut Vec<Span<'static>>, out: &mut Vec<Line<'static>>, quote: usize| {
            let mut line_spans = Vec::new();
            if quote > 0 {
                line_spans.push(Span::styled("▎ ".repeat(quote), theme::dim()));
            }
            line_spans.append(spans);
            out.push(Line::from(line_spans));
        };

        let parser = Parser::new_ext(
            text,
            Options::ENABLE_STRIKETHROUGH
                | Options::ENABLE_TABLES
                | Options::ENABLE_TASKLISTS
                | Options::ENABLE_MATH,
        );
        for ev in parser {
            match ev {
                Event::Start(tag) => match tag {
                    Tag::Table(alignments) => {
                        if !spans.is_empty() {
                            flush(&mut spans, &mut out, quote_depth);
                        }
                        table = Some(Table::new(alignments));
                    }
                    Tag::TableHead => in_table_head = true,
                    Tag::TableRow => {
                        if let Some(table) = &mut table {
                            table.row.clear();
                        }
                    }
                    Tag::TableCell => spans.clear(),
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
                    TagEnd::TableCell => {
                        if let Some(table) = &mut table {
                            table.row.push(std::mem::take(&mut spans));
                        }
                    }
                    TagEnd::TableHead => {
                        if let Some(table) = &mut table {
                            table.header = std::mem::take(&mut table.row);
                        }
                        in_table_head = false;
                    }
                    TagEnd::TableRow => {
                        if let Some(table) = &mut table {
                            if in_table_head {
                                table.header = std::mem::take(&mut table.row);
                            } else {
                                table.rows.push(std::mem::take(&mut table.row));
                            }
                        }
                    }
                    TagEnd::Table => {
                        if let Some(table) = table.take() {
                            out.extend(self.render_table(table));
                            out.push(Line::default());
                        }
                    }
                    TagEnd::Heading(_) => {
                        style_stack.pop();
                        flush(&mut spans, &mut out, quote_depth);
                        out.push(Line::default());
                    }
                    TagEnd::Paragraph => {
                        if !spans.is_empty() || !display_math {
                            flush(&mut spans, &mut out, quote_depth);
                        }
                        out.push(Line::default());
                        display_math = false;
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
                    // An empty item has nothing to flush, and flushing it would
                    // emit a blank line.
                    TagEnd::Item if !spans.is_empty() => {
                        flush(&mut spans, &mut out, quote_depth);
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
                Event::InlineMath(t) => {
                    spans.push(Span::styled(format!("${t}$"), theme::math_inline()));
                }
                Event::DisplayMath(t) => {
                    if !spans.is_empty() {
                        flush(&mut spans, &mut out, quote_depth);
                    }
                    let mut math = Vec::new();
                    if quote_depth > 0 {
                        math.push(Span::styled("▎ ".repeat(quote_depth), theme::dim()));
                    }
                    math.push(Span::styled("∑ ", theme::accent()));
                    math.push(Span::styled(format!("$${t}$$"), theme::math_block()));
                    out.push(Line::from(math));
                    display_math = true;
                }
                Event::SoftBreak | Event::HardBreak => {
                    if table.is_some() {
                        spans.push(Span::raw(" "));
                    } else {
                        flush(&mut spans, &mut out, quote_depth);
                    }
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

    fn render_table(&self, table: Table) -> Vec<Line<'static>> {
        let columns = table
            .alignments
            .len()
            .max(table.header.len())
            .max(table.rows.iter().map(Vec::len).max().unwrap_or(0));
        if columns == 0 {
            return Vec::new();
        }

        let mut widths = vec![1; columns];
        for row in std::iter::once(&table.header).chain(table.rows.iter()) {
            for (index, cell) in row.iter().enumerate() {
                widths[index] = widths[index].max(cell_width(cell));
            }
        }

        let border = |left, join, right| {
            let mut text = String::from(left);
            for (index, width) in widths.iter().enumerate() {
                if index > 0 {
                    text.push_str(join);
                }
                text.push_str(&"─".repeat(width + 2));
            }
            text.push_str(right);
            Line::styled(text, theme::border())
        };
        let row_line = |cells: &[Vec<Span<'static>>], header: bool| {
            let mut spans = vec![Span::styled("│", theme::border())];
            for index in 0..columns {
                let cell = cells.get(index).map(Vec::as_slice).unwrap_or(&[]);
                let cell_width = cell_width(cell);
                let padding = widths[index].saturating_sub(cell_width);
                let (left, right) = match table
                    .alignments
                    .get(index)
                    .copied()
                    .unwrap_or(Alignment::None)
                {
                    Alignment::Right => (padding, 0),
                    Alignment::Center => (padding / 2, padding - padding / 2),
                    Alignment::None | Alignment::Left => (0, padding),
                };
                spans.push(Span::raw(format!(" {}", " ".repeat(left))));
                spans.extend(cell.iter().map(|span| {
                    let style = if header {
                        span.style.add_modifier(Modifier::BOLD)
                    } else {
                        span.style
                    };
                    Span::styled(span.content.to_string(), style)
                }));
                spans.push(Span::raw(format!("{} ", " ".repeat(right))));
                spans.push(Span::styled("│", theme::border()));
            }
            Line::from(spans)
        };

        let mut out = vec![border("┌", "┬", "┐")];
        if !table.header.is_empty() {
            out.push(row_line(&table.header, true));
            out.push(border("├", "┼", "┤"));
        }
        out.extend(table.rows.iter().map(|row| row_line(row, false)));
        out.push(border("└", "┴", "┘"));
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

fn cell_width(cell: &[Span<'_>]) -> usize {
    cell.iter().map(|span| span.content.width()).sum()
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
    fn renders_tables_as_distinct_rows_and_cells() {
        let lines = Renderer::default().render(
            "| 组合 | Sharpe |\n| --- | ---: |\n| baseline_swcta | 2.106 |\n| fincta_soft_short_nogold | 2.330 |",
        );
        let text: Vec<String> = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect();

        assert!(text.iter().any(|line| line.contains("│ 组合")));
        assert!(text.iter().any(|line| line.contains("│ baseline_swcta")));
        assert!(text
            .iter()
            .any(|line| line.contains("│ fincta_soft_short_nogold")));
        assert!(text.iter().any(|line| line.starts_with("├")));
        assert!(!text.iter().any(|line| line.contains("组合Sharpe")));
    }

    #[test]
    fn renders_task_list_markers() {
        let lines = Renderer::default().render("- [ ] todo\n- [x] done");
        let text: Vec<String> = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect();

        assert!(text.iter().any(|line| line.contains("• [ ] todo")));
        assert!(text.iter().any(|line| line.contains("• [x] done")));
    }

    #[test]
    fn renders_inline_and_display_math_as_distinct_formulae() {
        let lines = Renderer::default().render("Energy is $E = mc^2$.\n\n$$\\frac{a}{b} = c$$");
        let text: Vec<String> = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect();

        assert!(text
            .iter()
            .any(|line| line.contains("Energy is $E = mc^2$.")));
        assert!(text.iter().any(|line| line == "∑ $$\\frac{a}{b} = c$$"));
    }

    #[test]
    fn renders_unclosed_fenced_code_for_streaming() {
        let lines = Renderer::default().render("```rust\nlet answer = 42;");
        let text: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert!(text.iter().any(|l| l.contains("let answer")));
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
