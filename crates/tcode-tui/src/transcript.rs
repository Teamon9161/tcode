//! In-memory transcript: the single source of truth for finalized
//! conversation content. The screen is only a viewport into it. Unlike
//! native terminal scrollback, blocks can still be truncated (rewind),
//! collapsed, or restyled after they were first shown.
//!
//! Performance discipline (do not regress):
//! - wrapping is computed once per block per width; only a resize
//!   invalidates every block, streaming appends touch one block;
//! - rendering slices the visible window via a running height prefix
//!   sum, so a frame costs O(viewport height), not O(transcript).

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

/// Mouse selection in transcript coordinates: (visual row, display column).
/// Rows are global wrapped-row indexes, stable under append.
#[derive(Clone, Copy)]
struct Selection {
    anchor: (usize, usize),
    head: (usize, usize),
}

impl Selection {
    fn ordered(&self) -> ((usize, usize), (usize, usize)) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }
}

pub struct Transcript {
    blocks: Vec<Block>,
    /// `cum[i]` = visible height of `blocks[..=i]` at `width`.
    cum: Vec<usize>,
    width: u16,
    /// Rows between the transcript bottom and the view bottom;
    /// 0 = pinned to the bottom, following new output.
    scroll: usize,
    /// Height of the most recent render, for page-sized scrolling.
    view_height: usize,
    /// Geometry of the most recent render, for mouse hit-testing.
    view_area: Rect,
    view_top: usize,
    selection: Option<Selection>,
    /// Block emphasized by rewind navigation.
    highlight: Option<usize>,
}

/// Lines wrapped at a width. `starts` is aligned with `lines`: true where
/// a row starts a logical line (false = soft-wrap continuation). Selection
/// extraction joins continuations without a newline.
#[derive(Default)]
struct Wrapped {
    lines: Vec<Line<'static>>,
    starts: Vec<bool>,
}

impl Wrapped {
    fn of(lines: &[Line<'static>], width: u16) -> Self {
        let flagged = wrap_lines_flagged(lines.to_vec(), width as usize);
        Self {
            starts: flagged.iter().map(|(start, _)| *start).collect(),
            lines: flagged.into_iter().map(|(_, line)| line).collect(),
        }
    }

    fn len(&self) -> usize {
        self.lines.len()
    }
}

/// A block's collapsible body: tool output (starts closed) or an edit
/// diff (starts open). When open it occupies a fixed number of rows and
/// scrolls internally under the mouse wheel; a click on the block toggles.
struct Detail {
    lines: Vec<Line<'static>>,
    wrapped: Wrapped,
    open: bool,
    scroll: usize,
    view_rows: usize,
}

impl Detail {
    fn overflows(&self) -> bool {
        self.wrapped.len() > self.view_rows
    }

    fn visible(&self) -> usize {
        if !self.open {
            0
        } else if self.overflows() {
            self.view_rows + 1 // + footer row
        } else {
            self.wrapped.len()
        }
    }

    fn max_scroll(&self) -> usize {
        self.wrapped.len().saturating_sub(self.view_rows)
    }

    fn footer(&self) -> Line<'static> {
        Line::styled(
            format!(
                "    ↕ {}-{} / {} · wheel scrolls · click folds",
                self.scroll + 1,
                (self.scroll + self.view_rows).min(self.wrapped.len()),
                self.wrapped.len()
            ),
            crate::theme::dim(),
        )
    }
}

struct Block {
    head: Vec<Line<'static>>,
    head_wrapped: Wrapped,
    detail: Option<Detail>,
    /// Ledger entry this block echoes (user inputs). Rewind uses it to
    /// jump-highlight and to truncate the view together with the ledger.
    entry: Option<usize>,
}

impl Block {
    fn rewrap(&mut self, width: u16) {
        self.head_wrapped = Wrapped::of(&self.head, width);
        if let Some(detail) = &mut self.detail {
            detail.wrapped = Wrapped::of(&detail.lines, width);
            detail.scroll = detail.scroll.min(detail.max_scroll());
        }
    }

    fn height(&self) -> usize {
        self.head_wrapped.len() + self.detail.as_ref().map_or(0, Detail::visible)
    }

    /// The i-th visible row of this block.
    fn row(&self, i: usize) -> (Line<'static>, bool) {
        if i < self.head_wrapped.len() {
            let mut line = self.head_wrapped.lines[i].clone();
            // A foldable block advertises itself on its last head row: a
            // closed body shows "▸ N lines", an open one a "▾" that a click
            // collapses. The accent colour marks it as interactive.
            if i + 1 == self.head_wrapped.len() {
                if let Some(detail) = &self.detail {
                    line.spans.push(if detail.open {
                        Span::styled("  ▾", crate::theme::accent())
                    } else {
                        Span::styled(
                            format!("  ▸ {} lines", detail.lines.len()),
                            crate::theme::accent(),
                        )
                    });
                }
            }
            return (line, self.head_wrapped.starts[i]);
        }
        let Some(detail) = &self.detail else {
            return (Line::default(), true);
        };
        let j = i - self.head_wrapped.len();
        if detail.overflows() && j == detail.view_rows {
            return (detail.footer(), true);
        }
        let k = detail.scroll + j;
        (detail.wrapped.lines[k].clone(), detail.wrapped.starts[k])
    }
}

impl Transcript {
    pub fn new(width: u16) -> Self {
        Self {
            blocks: Vec::new(),
            cum: Vec::new(),
            width: width.max(1),
            scroll: 0,
            view_height: 0,
            view_area: Rect::default(),
            view_top: 0,
            selection: None,
            highlight: None,
        }
    }

    pub fn push(&mut self, lines: Vec<Line<'static>>) {
        if lines.is_empty() {
            return;
        }
        self.push_block(Block {
            head: lines,
            head_wrapped: Wrapped::default(),
            detail: None,
            entry: None,
        });
    }

    /// Append a user-input echo tied to its ledger entry index.
    pub fn push_tagged(&mut self, lines: Vec<Line<'static>>, entry: usize) {
        if lines.is_empty() {
            return;
        }
        self.push_block(Block {
            head: lines,
            head_wrapped: Wrapped::default(),
            detail: None,
            entry: Some(entry),
        });
    }

    /// Append a block with a collapsible body. `open` bodies (diffs) show
    /// immediately, capped to `view_rows`; closed bodies (tool output)
    /// expand on click.
    pub fn push_with_detail(
        &mut self,
        head: Vec<Line<'static>>,
        detail: Vec<Line<'static>>,
        open: bool,
        view_rows: usize,
    ) {
        if detail.is_empty() {
            return self.push(head);
        }
        self.push_block(Block {
            head,
            head_wrapped: Wrapped::default(),
            detail: Some(Detail {
                lines: detail,
                wrapped: Wrapped::default(),
                open,
                scroll: 0,
                view_rows: view_rows.max(1),
            }),
            entry: None,
        });
    }

    /// Replace a finalized-looking head block in place. Used by the TUI's
    /// still-streaming assistant message: each delta re-renders the Markdown
    /// for the whole message and swaps the block without creating duplicates.
    pub fn replace_block(&mut self, index: usize, lines: Vec<Line<'static>>) {
        if lines.is_empty() || index >= self.blocks.len() {
            return;
        }
        let old_height = self.blocks[index].height();
        let block = &mut self.blocks[index];
        block.head = lines;
        block.detail = None;
        block.entry = None;
        block.rewrap(self.width);
        let new_height = block.height();
        if self.scroll > 0 {
            if new_height >= old_height {
                self.scroll += new_height - old_height;
            } else {
                self.scroll = self.scroll.saturating_sub(old_height - new_height);
            }
        }
        self.rebuild_cum();
    }

    fn push_block(&mut self, mut block: Block) {
        block.rewrap(self.width);
        let height = block.height();
        self.cum.push(self.total() + height);
        self.blocks.push(block);
        // A reader who scrolled up keeps their place while output grows
        // below; only scroll 0 follows the tail.
        if self.scroll > 0 {
            self.scroll += height;
        }
    }

    pub fn clear(&mut self) {
        self.blocks.clear();
        self.cum.clear();
        self.scroll = 0;
        self.selection = None;
        self.highlight = None;
    }

    /// Number of blocks — a stable mark to `truncate_blocks` back to.
    pub fn block_count(&self) -> usize {
        self.blocks.len()
    }

    /// Drop blocks past `n`. Used to retract a change diff that was baked
    /// while its approval dialog was open once the decision comes in.
    pub fn truncate_blocks(&mut self, n: usize) {
        if n >= self.blocks.len() {
            return;
        }
        self.blocks.truncate(n);
        self.selection = None;
        self.highlight = None;
        self.scroll = 0;
        self.rebuild_cum();
    }

    // ---------------------------------------------------------- rewind

    fn block_of_entry(&self, entry: usize) -> Option<usize> {
        self.blocks.iter().rposition(|b| b.entry == Some(entry))
    }

    /// Emphasize the echo of a ledger entry and scroll it into view.
    /// False when the entry has no echo (e.g. compacted history).
    pub fn highlight_entry(&mut self, entry: usize) -> bool {
        let Some(index) = self.block_of_entry(entry) else {
            self.highlight = None;
            return false;
        };
        self.highlight = Some(index);
        let block_top = if index == 0 { 0 } else { self.cum[index - 1] };
        self.scroll = self
            .total()
            .saturating_sub(block_top + self.view_height.max(1))
            .min(self.max_scroll());
        true
    }

    pub fn clear_highlight(&mut self) {
        self.highlight = None;
    }

    /// Drop the entry's echo and everything after it — the visual
    /// counterpart of `Ledger::truncate_tail`.
    pub fn truncate_from_entry(&mut self, entry: usize) -> bool {
        let Some(index) = self.block_of_entry(entry) else {
            return false;
        };
        self.blocks.truncate(index);
        self.selection = None;
        self.highlight = None;
        self.scroll = 0;
        self.rebuild_cum();
        true
    }

    fn ensure_width(&mut self, width: u16) {
        let width = width.max(1);
        if width == self.width {
            return;
        }
        self.width = width;
        // Row coordinates shift when everything rewraps.
        self.selection = None;
        for block in &mut self.blocks {
            block.rewrap(width);
        }
        self.rebuild_cum();
    }

    fn rebuild_cum(&mut self) {
        self.cum.clear();
        let mut total = 0;
        for block in &self.blocks {
            total += block.height();
            self.cum.push(total);
        }
    }

    fn total(&self) -> usize {
        self.cum.last().copied().unwrap_or(0)
    }

    fn max_scroll(&self) -> usize {
        self.total().saturating_sub(self.view_height.max(1))
    }

    pub fn scroll_up(&mut self, n: usize) {
        self.scroll = (self.scroll + n).min(self.max_scroll());
    }

    pub fn scroll_down(&mut self, n: usize) {
        self.scroll = self.scroll.saturating_sub(n);
    }

    pub fn page_up(&mut self) {
        self.scroll_up(self.view_height.saturating_sub(2).max(1));
    }

    pub fn page_down(&mut self) {
        self.scroll_down(self.view_height.saturating_sub(2).max(1));
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll = 0;
    }

    /// True when the view is pinned to the newest output.
    pub fn is_following(&self) -> bool {
        self.scroll == 0
    }

    pub fn render(&mut self, buf: &mut Buffer, area: Rect) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        // Transcript rows vary a lot while scrolling and while live panels
        // resize. A Line widget only paints its own cells, so stale glyphs from
        // the previous frame otherwise remain as "floating" letters.
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                buf[(x, y)].reset();
            }
        }
        self.ensure_width(area.width);
        self.view_height = area.height as usize;
        self.view_area = area;
        self.scroll = self.scroll.min(self.max_scroll());
        let height = area.height as usize;
        // First visible transcript row. Content shorter than the view is
        // anchored to the top.
        let top = self.total().saturating_sub(height + self.scroll);
        self.view_top = top;
        let mut index = self.cum.partition_point(|&c| c <= top);
        let mut row = top - if index == 0 { 0 } else { self.cum[index - 1] };
        let mut y = area.y;
        'blocks: while index < self.blocks.len() {
            let block = &self.blocks[index];
            for i in row..block.height() {
                if y >= area.bottom() {
                    break 'blocks;
                }
                block.row(i).0.render(
                    Rect {
                        x: area.x,
                        y,
                        width: area.width,
                        height: 1,
                    },
                    buf,
                );
                if self.highlight == Some(index) {
                    for x in area.left()..area.right() {
                        buf[(x, y)].set_style(
                            ratatui::style::Style::default()
                                .bg(crate::theme::rewind_highlight_bg()),
                        );
                    }
                }
                y += 1;
            }
            row = 0;
            index += 1;
        }
        self.highlight_selection(buf, area, top);
    }

    fn highlight_selection(&self, buf: &mut Buffer, area: Rect, top: usize) {
        let Some(selection) = &self.selection else {
            return;
        };
        let (start, end) = selection.ordered();
        for screen_row in 0..area.height {
            let row = top + screen_row as usize;
            if row < start.0 || row > end.0 {
                continue;
            }
            let from = if row == start.0 { start.1 } else { 0 };
            let to = if row == end.0 {
                end.1
            } else {
                area.width as usize
            };
            for col in from..=to.min(area.width.saturating_sub(1) as usize) {
                buf[(area.x + col as u16, area.y + screen_row)]
                    .modifier
                    .insert(Modifier::REVERSED);
            }
        }
    }

    // ----------------------------------------------------------- mouse

    /// (block index, row within block) at a transcript row.
    fn block_at(&self, row: usize) -> Option<(usize, usize)> {
        let index = self.cum.partition_point(|&c| c <= row);
        if index >= self.blocks.len() {
            return None;
        }
        Some((
            index,
            row - if index == 0 { 0 } else { self.cum[index - 1] },
        ))
    }

    /// Wheel input: an open, overflowing detail region under the cursor
    /// scrolls internally; everywhere else scrolls the transcript.
    pub fn wheel(&mut self, x: u16, y: u16, up: bool, step: usize) {
        if let Some((index, in_block)) = self.pos_at(x, y).and_then(|(row, _)| self.block_at(row)) {
            let head = self.blocks[index].head_wrapped.len();
            if let Some(detail) = self.blocks[index].detail.as_mut() {
                if detail.open && detail.overflows() && in_block >= head {
                    detail.scroll = if up {
                        detail.scroll.saturating_sub(step)
                    } else {
                        (detail.scroll + step).min(detail.max_scroll())
                    };
                    return;
                }
            }
        }
        if up {
            self.scroll_up(step);
        } else {
            self.scroll_down(step);
        }
    }

    /// A plain click folds/unfolds the detail region of the block under it.
    fn toggle_at(&mut self, row: usize) {
        let Some((index, _)) = self.block_at(row) else {
            return;
        };
        let Some(detail) = self.blocks[index].detail.as_mut() else {
            return;
        };
        detail.open = !detail.open;
        self.rebuild_cum();
        self.scroll = self.scroll.min(self.max_scroll());
    }

    /// Screen position → transcript coordinates; None outside the view.
    fn pos_at(&self, x: u16, y: u16) -> Option<(usize, usize)> {
        let area = self.view_area;
        if self.total() == 0 || x < area.x || x >= area.right() || y < area.y || y >= area.bottom()
        {
            return None;
        }
        let row = (self.view_top + (y - area.y) as usize).min(self.total() - 1);
        Some((row, (x - area.x) as usize))
    }

    pub fn mouse_down(&mut self, x: u16, y: u16) {
        self.selection = self.pos_at(x, y).map(|pos| Selection {
            anchor: pos,
            head: pos,
        });
    }

    pub fn mouse_drag(&mut self, x: u16, y: u16) {
        // Clamp into the view so a drag that wanders outside still selects
        // up to the nearest edge.
        let area = self.view_area;
        if area.width == 0 || area.height == 0 || self.selection.is_none() {
            return;
        }
        let x = x.clamp(area.x, area.right().saturating_sub(1));
        let y = y.clamp(area.y, area.bottom().saturating_sub(1));
        if let (Some(pos), Some(selection)) = (self.pos_at(x, y), self.selection.as_mut()) {
            selection.head = pos;
        }
    }

    /// Finish a selection drag; returns the selected text. A plain click
    /// is not a selection: it toggles the block's detail region instead.
    /// The highlight stays until the next click.
    pub fn mouse_up(&mut self) -> Option<String> {
        let selection = self.selection?;
        if selection.anchor == selection.head {
            self.selection = None;
            self.toggle_at(selection.anchor.0);
            return None;
        }
        let (start, end) = selection.ordered();
        let mut out = String::new();
        for row in start.0..=end.0.min(self.total().saturating_sub(1)) {
            let (index, in_block) = self.block_at(row)?;
            let (line, starts_line) = self.blocks[index].row(in_block);
            if row > start.0 && starts_line {
                // Trailing padding/whitespace belongs to the display, not
                // to the copied text.
                while out.ends_with(' ') {
                    out.pop();
                }
                out.push('\n');
            }
            let from = if row == start.0 { start.1 } else { 0 };
            let to = if row == end.0 { end.1 } else { usize::MAX };
            out.push_str(&row_slice(&line, from, to));
        }
        while out.ends_with(' ') {
            out.pop();
        }
        Some(out)
    }
}

/// Characters of a wrapped row whose display cells intersect [from, to].
fn row_slice(line: &Line<'_>, from: usize, to: usize) -> String {
    use unicode_width::UnicodeWidthChar;

    let mut cell = 0usize;
    let mut out = String::new();
    for span in &line.spans {
        for c in span.content.chars() {
            let width = c.width().unwrap_or(0);
            if width == 0 {
                // Zero-width marks travel with the preceding character.
                if cell > from && cell <= to.saturating_add(1) {
                    out.push(c);
                }
                continue;
            }
            if cell + width > from && cell <= to {
                out.push(c);
            }
            cell += width;
            if cell > to {
                return out;
            }
        }
    }
    out
}

/// Pre-wrap lines at the target width instead of leaving soft wrapping to
/// a Paragraph: only pre-wrapped lines can be sliced for the viewport and
/// mapped back for selection.
#[cfg(test)]
pub fn wrap_lines(lines: Vec<Line<'static>>, width: usize) -> Vec<Line<'static>> {
    wrap_lines_flagged(lines, width)
        .into_iter()
        .map(|(_, line)| line)
        .collect()
}

/// The flag marks rows that start a logical line; soft-wrap continuations
/// carry `false` so copied text joins them without a newline.
fn wrap_lines_flagged(lines: Vec<Line<'static>>, width: usize) -> Vec<(bool, Line<'static>)> {
    use unicode_width::UnicodeWidthChar;

    let width = width.saturating_sub(1).max(1);
    let mut out = Vec::new();
    for line in lines {
        let mut first = true;
        let mut current: Vec<Span<'static>> = Vec::new();
        let mut current_width = 0usize;
        for span in line.spans {
            for raw in span.content.chars() {
                // A tab has zero measured width but still occupies a real cell
                // in ratatui's buffer diff, which leaves stray glyphs floating
                // between columns (visible between line numbers and content, or
                // stranded in blank areas while scrolling). Expand tabs to
                // spaces at 8-column stops so every display cell is accounted
                // for. Copy then yields spaces, matching what is on screen.
                let mut buf = [' '; 8];
                let expanded: &[char] = if raw == '\t' {
                    let stop = 8 - (current_width % 8);
                    &buf[..stop]
                } else {
                    buf[0] = raw;
                    &buf[..1]
                };
                for &c in expanded {
                    let char_width = c.width().unwrap_or(0);
                    if !current.is_empty() && current_width + char_width > width {
                        out.push((
                            std::mem::replace(&mut first, false),
                            pad_background_line(std::mem::take(&mut current), current_width, width),
                        ));
                        current_width = 0;
                    }
                    current.push(Span::styled(c.to_string(), span.style));
                    current_width += char_width;
                }
            }
        }
        out.push((first, pad_background_line(current, current_width, width)));
    }
    out
}

/// Ratatui backgrounds otherwise stop at the final code character. Extend
/// diff lines to the terminal edge, including every wrapped chunk.
fn pad_background_line(mut spans: Vec<Span<'static>>, used: usize, width: usize) -> Line<'static> {
    if let Some(background) = spans.iter().find_map(|span| span.style.bg) {
        spans.push(Span::styled(
            " ".repeat(width.saturating_sub(used)),
            ratatui::style::Style::default().bg(background),
        ));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn wrapped_lines_split_at_width() {
        let lines = wrap_lines(vec![Line::raw("abcdefghi")], 7);
        assert_eq!(text_of(&lines), ["abcdef", "ghi"]);
    }

    #[test]
    fn tabs_expand_to_spaces_at_eight_column_stops() {
        // "     1\tcontent" is the numbered() shape: 6 columns then a tab that
        // must land on column 8, i.e. two spaces — never a stray tab cell.
        let lines = wrap_lines(vec![Line::raw("     1\tx")], 40);
        assert_eq!(text_of(&lines), ["     1  x"]);
        // A leading tab expands to a full eight-column stop.
        let lines = wrap_lines(vec![Line::raw("\ty")], 40);
        assert_eq!(text_of(&lines), ["        y"]);
    }

    #[test]
    fn render_shows_tail_and_scroll_reveals_history() {
        let mut t = Transcript::new(20);
        for i in 0..10 {
            t.push(vec![Line::raw(format!("line {i}"))]);
        }
        let area = Rect::new(0, 0, 20, 4);
        let mut buf = Buffer::empty(area);
        t.render(&mut buf, area);
        let visible: String = (0..4)
            .map(|y| {
                (0..20)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect();
        assert!(visible.contains("line 9"));
        assert!(!visible.contains("line 0"));

        t.scroll_up(100); // clamped to max
        let mut buf = Buffer::empty(area);
        t.render(&mut buf, area);
        let visible: String = (0..4)
            .map(|y| {
                (0..20)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect();
        assert!(visible.contains("line 0"));
        assert!(!visible.contains("line 9"));
    }

    #[test]
    fn scrolled_reader_keeps_position_while_output_grows() {
        let mut t = Transcript::new(20);
        for i in 0..10 {
            t.push(vec![Line::raw(format!("line {i}"))]);
        }
        let area = Rect::new(0, 0, 20, 4);
        let mut buf = Buffer::empty(area);
        t.render(&mut buf, area);
        t.scroll_up(3);
        assert!(!t.is_following());
        t.push(vec![Line::raw("new output")]);
        let mut buf = Buffer::empty(area);
        t.render(&mut buf, area);
        let visible: String = (0..4)
            .map(|y| {
                (0..20)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect();
        // Still looking at the same slice of history, not the new tail.
        assert!(visible.contains("line 6"));
        assert!(!visible.contains("new output"));
    }

    #[test]
    fn short_transcript_is_anchored_to_the_top() {
        let mut t = Transcript::new(20);
        t.push(vec![Line::raw("only line")]);
        let area = Rect::new(0, 0, 20, 6);
        let mut buf = Buffer::empty(area);
        t.render(&mut buf, area);
        let first_row: String = (0..20).map(|x| buf[(x, 0)].symbol().to_string()).collect();
        assert!(first_row.contains("only line"));
    }

    #[test]
    fn selection_copies_text_and_joins_soft_wraps() {
        let mut t = Transcript::new(7); // usable width 6
        t.push(vec![Line::raw("abcdefghi")]); // rows: "abcdef" + "ghi"
        t.push(vec![Line::raw("next")]);
        let area = Rect::new(0, 0, 7, 4);
        let mut buf = Buffer::empty(area);
        t.render(&mut buf, area);
        t.mouse_down(0, 0);
        t.mouse_drag(3, 2);
        // Soft-wrapped rows join without a newline; block boundary keeps one.
        assert_eq!(t.mouse_up().as_deref(), Some("abcdefghi\nnext"));
    }

    #[test]
    fn plain_click_is_not_a_selection() {
        let mut t = Transcript::new(20);
        t.push(vec![Line::raw("hello")]);
        let area = Rect::new(0, 0, 20, 4);
        let mut buf = Buffer::empty(area);
        t.render(&mut buf, area);
        t.mouse_down(2, 0);
        assert_eq!(t.mouse_up(), None);
    }

    #[test]
    fn wide_characters_copy_whole_glyphs() {
        let mut t = Transcript::new(20);
        t.push(vec![Line::raw("你好世界")]);
        let area = Rect::new(0, 0, 20, 4);
        let mut buf = Buffer::empty(area);
        t.render(&mut buf, area);
        t.mouse_down(0, 0);
        t.mouse_drag(3, 0); // covers 你(0-1) and 好(2-3)
        assert_eq!(t.mouse_up().as_deref(), Some("你好"));
    }

    fn buffer_text(buf: &Buffer, area: Rect) -> String {
        (0..area.height)
            .flat_map(|y| {
                (0..area.width)
                    .map(move |x| buf[(x, y)].symbol().to_string())
                    .chain(["\n".to_string()])
            })
            .collect()
    }

    #[test]
    fn render_clears_stale_cells_from_previous_frame() {
        let mut t = Transcript::new(20);
        let area = Rect::new(0, 0, 20, 3);
        let mut buf = Buffer::empty(area);

        t.push(vec![Line::raw("long stale text")]);
        t.render(&mut buf, area);
        assert!(buffer_text(&buf, area).contains("long stale text"));

        t.clear();
        t.push(vec![Line::raw("new")]);
        t.render(&mut buf, area);
        let text = buffer_text(&buf, area);
        assert!(text.contains("new"));
        assert!(!text.contains("stale"));
    }

    #[test]
    fn closed_detail_expands_on_click_and_scrolls_under_the_wheel() {
        let mut t = Transcript::new(30);
        t.push_with_detail(
            vec![Line::raw("preview row")],
            (0..30).map(|i| Line::raw(format!("out {i}"))).collect(),
            false,
            5,
        );
        let area = Rect::new(0, 0, 30, 20);
        let mut buf = Buffer::empty(area);
        t.render(&mut buf, area);
        assert_eq!(t.total(), 1, "closed detail shows only the head");

        // A plain click on the head unfolds: 5 detail rows + footer.
        t.mouse_down(0, 0);
        assert_eq!(t.mouse_up(), None);
        let mut buf = Buffer::empty(area);
        t.render(&mut buf, area);
        assert_eq!(t.total(), 1 + 5 + 1);
        assert!(buffer_text(&buf, area).contains("out 0"));

        // Wheel over the detail region scrolls inside it, not the view.
        t.wheel(2, 3, false, 3);
        let mut buf = Buffer::empty(area);
        t.render(&mut buf, area);
        let text = buffer_text(&buf, area);
        assert!(text.contains("out 3"));
        assert!(!text.contains("out 0 "));
        assert!(t.is_following(), "transcript scroll must be untouched");

        // A second click folds it away again.
        t.mouse_down(0, 0);
        t.mouse_up();
        let mut buf = Buffer::empty(area);
        t.render(&mut buf, area);
        assert_eq!(t.total(), 1);
    }

    #[test]
    fn fold_indicator_flips_and_counts() {
        let mut t = Transcript::new(40);
        t.push_with_detail(
            vec![Line::raw("⎿ preview")],
            (0..3).map(|i| Line::raw(format!("out {i}"))).collect(),
            false,
            5,
        );
        let area = Rect::new(0, 0, 40, 10);
        let mut buf = Buffer::empty(area);
        t.render(&mut buf, area);
        // Closed: affordance advertises how much is hidden.
        assert!(buffer_text(&buf, area).contains("▸ 3 lines"));

        t.mouse_down(0, 0);
        t.mouse_up();
        let mut buf = Buffer::empty(area);
        t.render(&mut buf, area);
        let text = buffer_text(&buf, area);
        assert!(text.contains('▾'));
        assert!(!text.contains('▸'));
    }

    #[test]
    fn truncate_blocks_retracts_the_tail() {
        let mut t = Transcript::new(30);
        t.push(vec![Line::raw("kept")]);
        let mark = t.block_count();
        t.push_with_detail(
            vec![Line::raw("proposed")],
            vec![Line::raw("+ new")],
            true,
            14,
        );
        assert_eq!(t.block_count(), mark + 1);
        t.truncate_blocks(mark);
        assert_eq!(t.block_count(), mark);
        let area = Rect::new(0, 0, 30, 6);
        let mut buf = Buffer::empty(area);
        t.render(&mut buf, area);
        let text = buffer_text(&buf, area);
        assert!(text.contains("kept"));
        assert!(!text.contains("proposed"));
    }

    #[test]
    fn short_open_detail_shows_fully_without_footer() {
        let mut t = Transcript::new(30);
        t.push_with_detail(
            vec![Line::raw("edit(src/x.rs)")],
            vec![Line::raw("+ new"), Line::raw("- old")],
            true,
            14,
        );
        let area = Rect::new(0, 0, 30, 10);
        let mut buf = Buffer::empty(area);
        t.render(&mut buf, area);
        assert_eq!(t.total(), 3);
    }

    #[test]
    fn rewind_truncates_the_view_from_the_tagged_entry() {
        let mut t = Transcript::new(30);
        t.push(vec![Line::raw("banner")]);
        t.push_tagged(vec![Line::raw("› first")], 0);
        t.push(vec![Line::raw("answer 1")]);
        t.push_tagged(vec![Line::raw("› second")], 2);
        t.push(vec![Line::raw("answer 2")]);
        assert!(t.highlight_entry(2));
        assert!(t.truncate_from_entry(2));
        let area = Rect::new(0, 0, 30, 10);
        let mut buf = Buffer::empty(area);
        t.render(&mut buf, area);
        let text = buffer_text(&buf, area);
        assert!(text.contains("answer 1"));
        assert!(!text.contains("second"));
        assert!(!text.contains("answer 2"));
        // Entries without an echo (compacted history) report false.
        assert!(!t.truncate_from_entry(9));
    }

    #[test]
    fn highlight_scrolls_the_entry_into_view() {
        let mut t = Transcript::new(30);
        t.push_tagged(vec![Line::raw("› early input")], 0);
        for i in 0..40 {
            t.push(vec![Line::raw(format!("later {i}"))]);
        }
        let area = Rect::new(0, 0, 30, 5);
        let mut buf = Buffer::empty(area);
        t.render(&mut buf, area);
        assert!(t.highlight_entry(0));
        let mut buf = Buffer::empty(area);
        t.render(&mut buf, area);
        assert!(buffer_text(&buf, area).contains("early input"));
    }

    #[test]
    fn resize_rewraps_every_block() {
        let mut t = Transcript::new(40);
        t.push(vec![Line::raw("abcdefghij")]);
        // Narrow enough (usable width 6) that the block needs two rows.
        let area = Rect::new(0, 0, 7, 4);
        let mut buf = Buffer::empty(area);
        t.render(&mut buf, area);
        assert_eq!(t.total(), 2);
    }
}
