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

/// Parsed Markdown that keeps table cells structured until the transcript
/// knows how much terminal width is available.
pub struct Document {
    parts: Vec<Part>,
}

enum Part {
    Lines(Vec<Line<'static>>),
    Table(Table),
}

impl Document {
    pub fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }

    pub fn with_trailing_blank(mut self) -> Self {
        self.parts.push(Part::Lines(vec![Line::default()]));
        self
    }

    pub fn lines_at(&self, width: usize) -> Vec<Line<'static>> {
        self.parts
            .iter()
            .flat_map(|part| match part {
                Part::Lines(lines) => lines.clone(),
                Part::Table(table) => table.render_at(width),
            })
            .collect()
    }
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
    /// Compatibility path for callers that have no layout context. The
    /// transcript uses `parse` so tables can reflow on resize. This renders
    /// without premature wrapping (the transcript re-wraps at display width),
    /// so it suits pre-baked blocks like a plan body.
    pub fn render(&self, text: &str) -> Vec<Line<'static>> {
        self.parse(text).lines_at(usize::MAX)
    }

    pub fn parse(&self, text: &str) -> Document {
        let mut out: Vec<Line<'static>> = Vec::new();
        let mut parts = Vec::new();
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
                        if !out.is_empty() {
                            parts.push(Part::Lines(std::mem::take(&mut out)));
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
                            parts.push(Part::Table(table));
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
        if !out.is_empty() {
            parts.push(Part::Lines(out));
        }
        Document { parts }
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

impl Table {
    fn columns(&self) -> usize {
        self.alignments
            .len()
            .max(self.header.len())
            .max(self.rows.iter().map(Vec::len).max().unwrap_or(0))
    }

    fn widths(&self, columns: usize) -> Vec<usize> {
        let mut widths = vec![1; columns];
        for row in std::iter::once(&self.header).chain(self.rows.iter()) {
            for (index, cell) in row.iter().enumerate() {
                widths[index] = widths[index].max(cell_width(cell));
            }
        }
        widths
    }

    fn render_at(&self, width: usize) -> Vec<Line<'static>> {
        let columns = self.columns();
        if columns == 0 {
            return Vec::new();
        }
        let widths = self.widths(columns);
        let grid_width = widths.iter().sum::<usize>() + columns * 3 + 1;
        if grid_width <= width {
            self.render_grid(&widths)
        } else {
            self.render_cards(columns)
        }
    }

    fn render_grid(&self, widths: &[usize]) -> Vec<Line<'static>> {
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
            for (index, width) in widths.iter().enumerate() {
                let cell = cells.get(index).map(Vec::as_slice).unwrap_or(&[]);
                let padding = width.saturating_sub(cell_width(cell));
                let (left, right) = match self
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
                spans.extend(styled_cell(cell, header));
                spans.push(Span::raw(format!("{} ", " ".repeat(right))));
                spans.push(Span::styled("│", theme::border()));
            }
            Line::from(spans)
        };

        let mut out = vec![border("┌", "┬", "┐")];
        if !self.header.is_empty() {
            out.push(row_line(&self.header, true));
            out.push(border("├", "┼", "┤"));
        }
        out.extend(self.rows.iter().map(|row| row_line(row, false)));
        out.push(border("└", "┴", "┘"));
        out
    }

    fn render_cards(&self, columns: usize) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        for (row_index, row) in self.rows.iter().enumerate() {
            if row_index > 0 {
                out.push(Line::default());
            }
            let titled = self.header.first().is_some_and(|cell| !cell.is_empty());
            let first_field = if titled { 1 } else { 0 };
            if titled {
                let title = row.first().map(Vec::as_slice).unwrap_or(&[]);
                out.push(Line::from(styled_cell(title, true)));
            }
            for column in first_field..columns {
                let label = self.header.get(column).map(Vec::as_slice).unwrap_or(&[]);
                let value = row.get(column).map(Vec::as_slice).unwrap_or(&[]);
                let mut spans = vec![Span::styled("  ", theme::dim())];
                if label.is_empty() {
                    spans.push(Span::styled(
                        format!("Column {}", column + 1),
                        theme::bold(),
                    ));
                } else {
                    spans.extend(styled_cell(label, true));
                }
                spans.push(Span::raw(": "));
                if value.is_empty() {
                    spans.push(Span::styled("—", theme::dim()));
                } else {
                    spans.extend(styled_cell(value, false));
                }
                out.push(Line::from(spans));
            }
        }
        out
    }
}

fn styled_cell(cell: &[Span<'_>], bold: bool) -> Vec<Span<'static>> {
    cell.iter()
        .map(|span| {
            let style = if bold {
                span.style.add_modifier(Modifier::BOLD)
            } else {
                span.style
            };
            Span::styled(span.content.to_string(), style)
        })
        .collect()
}

fn cell_width(cell: &[Span<'_>]) -> usize {
    cell.iter().map(|span| span.content.width()).sum()
}

/// Split plan markdown into top-level block source strings: a heading, a
/// paragraph, a code block, a block quote, or a table each becomes one block,
/// while a list is split into its items so a comment can anchor to a single
/// bullet. The returned slices are verbatim source (indentation and fences
/// preserved) so each renders on its own and `$EDITOR` round-trips losslessly.
/// Inter-block whitespace is dropped — it belongs to no block.
pub fn split_blocks(text: &str) -> Vec<String> {
    let parser = Parser::new_ext(
        text,
        Options::ENABLE_STRIKETHROUGH
            | Options::ENABLE_TABLES
            | Options::ENABLE_TASKLISTS
            | Options::ENABLE_MATH,
    )
    .into_offset_iter();

    let mut ranges: Vec<(usize, usize)> = Vec::new();
    // Depth of open block-level containers; inline tags never count, so a
    // top-level block opens at depth 0→1 and closes at 1→0.
    let mut depth = 0usize;
    let mut block_start: Option<usize> = None;
    // Inside a top-level list, items — not the list — are the blocks.
    let mut top_list = false;

    for (ev, range) in parser {
        match ev {
            Event::Start(tag) => {
                if !is_block_container(&tag) {
                    continue;
                }
                if depth == 0 {
                    if matches!(tag, Tag::List(_)) {
                        top_list = true;
                    } else {
                        block_start = Some(range.start);
                    }
                } else if depth == 1 && top_list && matches!(tag, Tag::Item) {
                    block_start = Some(range.start);
                }
                depth += 1;
            }
            Event::End(tag) => {
                if !is_block_container_end(&tag) {
                    continue;
                }
                depth = depth.saturating_sub(1);
                if matches!(tag, TagEnd::List(_)) {
                    // Only the top-level list ending leaves item mode; a nested
                    // list closing at depth > 0 must not clear the flag.
                    if depth == 0 {
                        top_list = false;
                    }
                } else {
                    let closes_block =
                        depth == 0 || (depth == 1 && top_list && matches!(tag, TagEnd::Item));
                    if closes_block {
                        if let Some(start) = block_start.take() {
                            ranges.push((start, range.end));
                        }
                    }
                }
            }
            _ => {}
        }
    }

    ranges
        .into_iter()
        .map(|(s, e)| text[s..e].trim_end().to_string())
        .filter(|b| !b.is_empty())
        .collect()
}

/// Block-level container start tags (everything that nests block content);
/// inline tags (emphasis, links, images, …) are excluded so they never move
/// the container depth.
fn is_block_container(tag: &Tag) -> bool {
    matches!(
        tag,
        Tag::Paragraph
            | Tag::Heading { .. }
            | Tag::BlockQuote(_)
            | Tag::CodeBlock(_)
            | Tag::List(_)
            | Tag::Item
            | Tag::FootnoteDefinition(_)
            | Tag::Table(_)
            | Tag::TableHead
            | Tag::TableRow
            | Tag::TableCell
            | Tag::HtmlBlock
            | Tag::MetadataBlock(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
    )
}

fn is_block_container_end(tag: &TagEnd) -> bool {
    matches!(
        tag,
        TagEnd::Paragraph
            | TagEnd::Heading(_)
            | TagEnd::BlockQuote(_)
            | TagEnd::CodeBlock
            | TagEnd::List(_)
            | TagEnd::Item
            | TagEnd::FootnoteDefinition
            | TagEnd::Table
            | TagEnd::TableHead
            | TagEnd::TableRow
            | TagEnd::TableCell
            | TagEnd::HtmlBlock
            | TagEnd::MetadataBlock(_)
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_blocks_separates_heading_paragraph_and_code() {
        let blocks = split_blocks("# Title\n\nA paragraph.\n\n```rust\nfn main() {}\n```");
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0], "# Title");
        assert_eq!(blocks[1], "A paragraph.");
        assert_eq!(blocks[2], "```rust\nfn main() {}\n```");
    }

    #[test]
    fn split_blocks_breaks_a_list_into_items() {
        let blocks = split_blocks("Intro\n\n- first\n- second\n- third");
        // Intro paragraph plus three separately-anchorable items.
        assert_eq!(blocks.len(), 4);
        assert_eq!(blocks[0], "Intro");
        assert_eq!(blocks[1], "- first");
        assert_eq!(blocks[2], "- second");
        assert_eq!(blocks[3], "- third");
    }

    #[test]
    fn split_blocks_keeps_a_nested_list_with_its_parent_item() {
        let blocks = split_blocks("- parent\n  - child\n- sibling");
        assert_eq!(blocks.len(), 2);
        assert!(blocks[0].contains("parent"));
        assert!(
            blocks[0].contains("child"),
            "nested list stays with its item"
        );
        assert_eq!(blocks[1], "- sibling");
    }

    #[test]
    fn split_blocks_keeps_a_table_as_one_block() {
        let src = "Before\n\n| a | b |\n| --- | --- |\n| 1 | 2 |\n\nAfter";
        let blocks = split_blocks(src);
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0], "Before");
        assert!(blocks[1].contains("| a | b |"));
        assert!(blocks[1].contains("| 1 | 2 |"));
        assert_eq!(blocks[2], "After");
    }

    #[test]
    fn split_blocks_ignores_inline_markup() {
        // Emphasis/link tags must not open a block.
        let blocks = split_blocks("Some *emph* and [a](b) link in one paragraph.");
        assert_eq!(blocks.len(), 1);
    }

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
    fn responsive_table_uses_cards_when_grid_exceeds_width() {
        let document = Renderer::default().parse(
            "| 组合 | Sharpe | 年化收益 | 最大回撤 |\n| --- | ---: | ---: | ---: |\n| baseline_swcta | 2.106 | 2.54% | -1.71% |",
        );
        let wide: Vec<String> = document
            .lines_at(80)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect();
        let narrow: Vec<String> = document
            .lines_at(20)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect();

        assert!(wide.iter().any(|line| line.starts_with("┌")));
        assert!(wide.iter().any(|line| line.contains("│ baseline_swcta")));
        assert!(!narrow.iter().any(|line| line.starts_with("┌")));
        assert!(narrow.iter().any(|line| line == "baseline_swcta"));
        assert!(narrow.iter().any(|line| line.contains("Sharpe: 2.106")));
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
