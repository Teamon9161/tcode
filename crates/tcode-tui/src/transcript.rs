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
use ratatui::style::{Color, Modifier};
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
    /// Collapsible block currently under the pointer. Its header uses the
    /// terminal's regular foreground while leaving expanded output alone.
    hovered: Option<usize>,
    /// Tick frame used only when painting a live task status. Keeping it out of
    /// the wrapped block data preserves the transcript's resize-only reflow.
    task_activity_frame: usize,
    /// Block whose head rows shimmer because its call (or batch) is still in
    /// flight. Paint-only, like the frame: content and wrapping never change.
    live_head: Option<usize>,
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
    content: Content,
    /// Materialized source lines at the current transcript width. The wrapped
    /// cache remains the only per-width cache used during frame rendering.
    lines: Vec<Line<'static>>,
    wrapped: Wrapped,
    open: bool,
    scroll: usize,
    view_rows: usize,
}

enum Content {
    Lines(Vec<Line<'static>>),
    Markdown {
        document: crate::markdown::Document,
        prefix: Vec<Span<'static>>,
    },
}

impl Content {
    fn lines_at(&self, width: u16) -> Vec<Line<'static>> {
        match self {
            Self::Lines(lines) => lines.clone(),
            Self::Markdown { document, prefix } => {
                use unicode_width::UnicodeWidthStr;

                // `wrap_lines_flagged` reserves one terminal cell, so the
                // table chooser must make the same reservation before it
                // decides whether a grid fits.
                let available = (width as usize)
                    .saturating_sub(1)
                    .max(1)
                    .saturating_sub(prefix.iter().map(|span| span.content.width()).sum());
                document
                    .lines_at(available)
                    .into_iter()
                    .map(|line| {
                        let mut spans = prefix.clone();
                        spans.extend(line.spans);
                        Line::from(spans)
                    })
                    .collect()
            }
        }
    }

    fn is_empty(&self) -> bool {
        match self {
            Self::Lines(lines) => lines.is_empty(),
            Self::Markdown { document, .. } => document.is_empty(),
        }
    }
}

impl Detail {
    fn rewrap(&mut self, width: u16) {
        self.lines = self.content.lines_at(width);
        self.wrapped = Wrapped::of(&self.lines, width);
        self.scroll = self.scroll.min(self.max_scroll());
    }

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
    head: Content,
    head_wrapped: Wrapped,
    detail: Option<Detail>,
    /// A short mutable status rendered immediately below the header. Task
    /// cards use it for live activity without forcing the summary foldout open.
    status: Option<Vec<Line<'static>>>,
    status_wrapped: Wrapped,
    /// Ledger entry this block echoes (user inputs). Rewind uses it to
    /// jump-highlight and to truncate the view together with the ledger.
    entry: Option<usize>,
    /// A task trace this record opens. This is view-only metadata: it never
    /// reaches the provider ledger and coexists with ordinary detail folding.
    task_run: Option<String>,
}

impl Block {
    fn rewrap(&mut self, width: u16, hovered: bool) {
        if let Some(detail) = &mut self.detail {
            detail.rewrap(width);
        }
        self.status_wrapped = self
            .status
            .as_ref()
            .map_or_else(Wrapped::default, |lines| Wrapped::of(lines, width));
        self.head_wrapped = Wrapped::of(&self.display_head(width, hovered), width);
    }

    fn display_head(&self, width: u16, hovered: bool) -> Vec<Line<'static>> {
        let mut head = self.head.lines_at(width);
        if let Some(last) = head.last_mut() {
            if let Some(detail) = &self.detail {
                // Keep the fold affordance in the same logical line as the preview,
                // so wrapping is computed once for the combined row. Appending it
                // after wrapping lets the terminal/ratatui push it onto a stray
                // extra row on narrow panes. A task trace link coexists with this
                // ordinary foldout: click folds its summary/report, double-click
                // opens the isolated sub-agent trace.
                last.spans.push(if detail.open {
                    Span::styled("  ▾", crate::theme::dim())
                } else if hovered {
                    Span::styled(
                        format!("  ▸ {} lines", detail.lines.len()),
                        ratatui::style::Style::default().fg(Color::Reset),
                    )
                } else {
                    Span::raw("")
                });
            }
        }
        head
    }

    fn height(&self) -> usize {
        self.head_wrapped.len()
            + self.status_wrapped.len()
            + self.detail.as_ref().map_or(0, Detail::visible)
    }

    /// The i-th visible row of this block.
    fn row(&self, i: usize) -> (Line<'static>, bool) {
        if i < self.head_wrapped.len() {
            return (
                self.head_wrapped.lines[i].clone(),
                self.head_wrapped.starts[i],
            );
        }
        let status_start = self.head_wrapped.len();
        let status_end = status_start + self.status_wrapped.len();
        if i < status_end {
            return (
                self.status_wrapped.lines[i - status_start].clone(),
                self.status_wrapped.starts[i - status_start],
            );
        }
        let Some(detail) = &self.detail else {
            return (Line::default(), true);
        };
        let j = i - status_end;
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
            hovered: None,
            task_activity_frame: 0,
            live_head: None,
        }
    }

    pub fn push(&mut self, lines: Vec<Line<'static>>) {
        self.push_content(Content::Lines(lines));
    }

    pub fn push_markdown(&mut self, document: crate::markdown::Document) {
        self.push_content(Content::Markdown {
            document,
            prefix: Vec::new(),
        });
    }

    fn push_content(&mut self, head: Content) {
        if head.is_empty() {
            return;
        }
        self.push_block(Block {
            head,
            head_wrapped: Wrapped::default(),
            detail: None,
            status: None,
            status_wrapped: Wrapped::default(),
            entry: None,
            task_run: None,
        });
    }

    /// Append a user-input echo tied to its ledger entry index.
    pub fn push_tagged(&mut self, lines: Vec<Line<'static>>, entry: usize) {
        if lines.is_empty() {
            return;
        }
        self.push_block(Block {
            head: Content::Lines(lines),
            head_wrapped: Wrapped::default(),
            detail: None,
            status: None,
            status_wrapped: Wrapped::default(),
            entry: Some(entry),
            task_run: None,
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
        self.push_with_detail_content(head, Content::Lines(detail), open, view_rows);
    }

    pub fn push_with_markdown_detail(
        &mut self,
        head: Vec<Line<'static>>,
        document: crate::markdown::Document,
        prefix: Vec<Span<'static>>,
        open: bool,
        view_rows: usize,
    ) {
        self.push_with_detail_content(
            head,
            Content::Markdown { document, prefix },
            open,
            view_rows,
        );
    }

    fn push_with_detail_content(
        &mut self,
        head: Vec<Line<'static>>,
        detail: Content,
        open: bool,
        view_rows: usize,
    ) {
        if detail.is_empty() {
            return self.push(head);
        }
        self.push_block(Block {
            head: Content::Lines(head),
            head_wrapped: Wrapped::default(),
            detail: Some(Detail {
                content: detail,
                lines: Vec::new(),
                wrapped: Wrapped::default(),
                open,
                scroll: 0,
                view_rows: view_rows.max(1),
            }),
            status: None,
            status_wrapped: Wrapped::default(),
            entry: None,
            task_run: None,
        });
    }

    pub fn replace_markdown_block(&mut self, index: usize, document: crate::markdown::Document) {
        self.replace_content(
            index,
            Content::Markdown {
                document,
                prefix: Vec::new(),
            },
        );
    }

    fn replace_content(&mut self, index: usize, head: Content) {
        if head.is_empty() || index >= self.blocks.len() {
            return;
        }
        let old_height = self.blocks[index].height();
        let block = &mut self.blocks[index];
        block.head = head;
        block.detail = None;
        block.status = None;
        block.status_wrapped = Wrapped::default();
        block.entry = None;
        block.task_run = None;
        self.remeasure(index, old_height);
    }

    /// Index of the most recently appended block, useful for attaching
    /// view-only metadata immediately after a batch item bakes.
    pub fn last_block_index(&self) -> Option<usize> {
        self.blocks.len().checked_sub(1)
    }

    /// Associate an existing transcript block with a task trace. Calling this
    /// after the header was baked lets `TaskRunStarted` arrive naturally after
    /// its parent `ToolStart`, without special event ordering.
    pub fn link_task_run(&mut self, index: usize, run: impl Into<String>) {
        if let Some(block) = self.blocks.get_mut(index) {
            block.task_run = Some(run.into());
        }
    }

    /// The task trace link under a pending click, if any. The caller uses it
    /// after `mouse_up()` has performed the ordinary foldout toggle, so one
    /// click expands the task card and a second quick click can open its trace.
    pub fn selected_task_run(&self) -> Option<String> {
        let selection = self.selection?;
        (selection.anchor == selection.head)
            .then_some(selection.anchor.0)
            .and_then(|row| self.block_at(row).map(|(index, _)| index))
            .and_then(|index| self.blocks[index].task_run.clone())
    }

    /// Extend the last head line of an existing block — a tool-result
    /// preview landing on the call's own header row at `ToolEnd`.
    pub fn extend_head(&mut self, index: usize, spans: Vec<Span<'static>>) {
        if spans.is_empty() || index >= self.blocks.len() {
            return;
        }
        let old_height = self.blocks[index].height();
        if let Content::Lines(lines) = &mut self.blocks[index].head {
            if let Some(last) = lines.last_mut() {
                last.spans.extend(spans);
            }
        }
        self.remeasure(index, old_height);
    }

    /// Replace or clear a live status line below an existing block's header.
    /// It is transient UI state, intended for a running task's current action.
    pub fn set_live_status(&mut self, index: usize, status: Option<Vec<Line<'static>>>) {
        if index >= self.blocks.len() {
            return;
        }
        let old_height = self.blocks[index].height();
        self.blocks[index].status = status.filter(|lines| !lines.is_empty());
        self.blocks[index].status_wrapped = Wrapped::default();
        self.remeasure(index, old_height);
    }

    /// Retro-fit a collapsed body onto an existing block: a live tool
    /// record pushes its header at `ToolStart` and only learns its output
    /// at `ToolEnd`. The hover and fold affordances land on the header
    /// row itself — no separate result row.
    pub fn attach_detail(&mut self, index: usize, detail: Vec<Line<'static>>, view_rows: usize) {
        self.attach_detail_content(index, Content::Lines(detail), view_rows);
    }

    pub fn attach_markdown_detail(
        &mut self,
        index: usize,
        document: crate::markdown::Document,
        prefix: Vec<Span<'static>>,
        view_rows: usize,
    ) {
        self.attach_detail_content(index, Content::Markdown { document, prefix }, view_rows);
    }

    /// Replace a live detail while preserving the reader's fold choice. Task
    /// cards use this for their evolving summary/tool activity; ordinary tool
    /// results deliberately continue to attach as closed details.
    pub fn replace_detail_preserving_open(
        &mut self,
        index: usize,
        detail: Vec<Line<'static>>,
        view_rows: usize,
    ) {
        if detail.is_empty() || index >= self.blocks.len() {
            return;
        }
        let old_height = self.blocks[index].height();
        let open = self.blocks[index]
            .detail
            .as_ref()
            .is_some_and(|detail| detail.open);
        self.blocks[index].detail = Some(Detail {
            content: Content::Lines(detail),
            lines: Vec::new(),
            wrapped: Wrapped::default(),
            open,
            scroll: 0,
            view_rows: view_rows.max(1),
        });
        self.remeasure(index, old_height);
    }

    fn attach_detail_content(&mut self, index: usize, detail: Content, view_rows: usize) {
        if detail.is_empty() || index >= self.blocks.len() {
            return;
        }
        let old_height = self.blocks[index].height();
        self.blocks[index].detail = Some(Detail {
            content: detail,
            lines: Vec::new(),
            wrapped: Wrapped::default(),
            open: false,
            scroll: 0,
            view_rows: view_rows.max(1),
        });
        self.remeasure(index, old_height);
    }

    /// Extend a plain foldout without changing `attach_detail`'s replacement
    /// semantics. A long shell command uses this when its result arrives.
    pub fn append_detail(&mut self, index: usize, mut lines: Vec<Line<'static>>, view_rows: usize) {
        if lines.is_empty() || index >= self.blocks.len() {
            return;
        }
        let appends_to_plain = self.blocks[index]
            .detail
            .as_ref()
            .is_some_and(|detail| matches!(&detail.content, Content::Lines(_)));
        if !appends_to_plain {
            return self.attach_detail(index, lines, view_rows);
        }
        let old_height = self.blocks[index].height();
        let detail = self.blocks[index]
            .detail
            .as_mut()
            .expect("plain detail checked");
        let Content::Lines(existing_lines) = &mut detail.content else {
            unreachable!("plain detail checked");
        };
        if !existing_lines.is_empty() {
            existing_lines.push(Line::default());
        }
        existing_lines.append(&mut lines);
        detail.lines.clear();
        detail.wrapped = Wrapped::default();
        detail.scroll = 0;
        detail.view_rows = detail.view_rows.max(view_rows.max(1));
        self.remeasure(index, old_height);
    }

    /// Recompute layout after a block mutated in place, preserving a
    /// scrolled-up reader's position exactly like `push_block` does.
    fn remeasure(&mut self, index: usize, old_height: usize) {
        let hovered = self.hovered == Some(index);
        let block = &mut self.blocks[index];
        block.rewrap(self.width, hovered);
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
        block.rewrap(self.width, false);
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
        self.hovered = None;
        self.live_head = None;
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
        self.hovered = None;
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
        self.hovered = None;
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
        self.hovered = None;
        for block in &mut self.blocks {
            block.rewrap(width, false);
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

    /// Distance from the tail. Auto-scroll compares this before/after a step to
    /// detect that it has reached the top or bottom and should stop.
    pub fn scroll_offset(&self) -> usize {
        self.scroll
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

    /// Advance the paint-only animation for a live task status. This does not
    /// change line content or wrapping, so a tick remains O(viewport height).
    pub fn set_task_activity_frame(&mut self, frame: usize) {
        self.task_activity_frame = frame;
    }

    /// Mark (or clear) the block whose head rows shimmer while its tool call
    /// or batch is in flight. Paint-only; see `set_task_activity_frame`.
    pub fn set_live_head(&mut self, block: Option<usize>) {
        self.live_head = block;
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
                let (line, _) = block.row(i);
                let is_hovered_head = self.hovered == Some(index) && i < block.head_wrapped.len();
                let is_live_task_status = i >= block.head_wrapped.len()
                    && i < block.head_wrapped.len() + block.status_wrapped.len();
                let is_live_head = self.live_head == Some(index) && i < block.head_wrapped.len();
                let content_width = line_display_width(&line).min(area.width as usize);
                line.render(
                    Rect {
                        x: area.x,
                        y,
                        width: area.width,
                        height: 1,
                    },
                    buf,
                );
                if is_live_task_status || is_live_head {
                    for x in 0..content_width {
                        let cell = &mut buf[(area.x + x as u16, y)];
                        let base = cell.style().fg.unwrap_or(ratatui::style::Color::Reset);
                        cell.set_fg(crate::theme::shimmer_color(
                            self.task_activity_frame,
                            x,
                            content_width,
                            base,
                        ));
                    }
                }
                if is_hovered_head {
                    for x in 0..content_width {
                        buf[(area.x + x as u16, y)]
                            .set_style(ratatui::style::Style::default().fg(Color::Reset));
                    }
                }
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

    /// Update hover state from a terminal mouse-move event. Closed details keep
    /// their line count quiet until the pointer makes the block actionable.
    pub fn mouse_moved(&mut self, x: u16, y: u16) {
        let hovered = self
            .pos_at(x, y)
            .and_then(|(row, _)| self.block_at(row))
            .map(|(index, _)| index)
            .filter(|&index| {
                self.blocks[index].detail.is_some() || self.blocks[index].task_run.is_some()
            });
        if hovered == self.hovered {
            return;
        }
        let previous = std::mem::replace(&mut self.hovered, hovered);
        for index in [previous, hovered].into_iter().flatten() {
            self.blocks[index].rewrap(self.width, self.hovered == Some(index));
        }
        self.rebuild_cum();
        self.scroll = self.scroll.min(self.max_scroll());
    }

    pub fn clear_hover(&mut self) {
        let Some(index) = self.hovered.take() else {
            return;
        };
        self.blocks[index].rewrap(self.width, false);
        self.rebuild_cum();
        self.scroll = self.scroll.min(self.max_scroll());
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
        let hovered = self.hovered == Some(index);
        let block = &mut self.blocks[index];
        let Some(detail) = block.detail.as_mut() else {
            return;
        };
        detail.open = !detail.open;
        block.rewrap(self.width, hovered);
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
        let row = self.view_top + (y - area.y) as usize;
        if row >= self.total() {
            return None;
        }
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

    /// While a selection drag rests at the top or bottom edge of the view, the
    /// selection should keep growing in that direction — but a pointer held
    /// still sends no events, so the frontend drives it from a timer instead.
    /// `Some(true)` = scroll toward older content (top edge), `Some(false)` =
    /// toward newer (bottom edge), `None` = inside the view (no auto-scroll).
    /// Only armed while a selection is active.
    pub fn drag_edge(&self, y: u16) -> Option<bool> {
        let area = self.view_area;
        if area.height == 0 || self.selection.is_none() {
            return None;
        }
        if y <= area.y {
            Some(true)
        } else if y >= area.bottom().saturating_sub(1) {
            Some(false)
        } else {
            None
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
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

    let mut cell = 0usize;
    let mut out = String::new();
    // Wrapping materializes one span per character, so the rail is no longer
    // one `USER_GUTTER` span here. Its marker and dedicated style still identify
    // it; skip its fixed display width while retaining cells for hit testing.
    let gutter_width = line
        .spans
        .first()
        .filter(|span| {
            span.content
                .starts_with(crate::theme::USER_GUTTER.trim_end())
                && span.style == crate::theme::user_gutter()
        })
        .map(|_| crate::theme::USER_GUTTER.width())
        .unwrap_or(0);
    for span in &line.spans {
        for c in span.content.chars() {
            let width = c.width().unwrap_or(0);
            if width == 0 {
                // Zero-width marks travel with the preceding character.
                if cell >= gutter_width && cell > from && cell <= to.saturating_add(1) {
                    out.push(c);
                }
                continue;
            }
            if cell >= gutter_width && cell + width > from && cell <= to {
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

fn line_display_width(line: &Line<'_>) -> usize {
    use unicode_width::UnicodeWidthStr;

    let text: String = line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    text.trim_end().width()
}

/// Pre-wrap lines at the target width instead of leaving soft wrapping to
/// a Paragraph: only pre-wrapped lines can be sliced for the viewport and
/// mapped back for selection. Also used by the plan-review pane, which slices
/// its own viewport out of the wrapped plan.
pub fn wrap_lines(lines: Vec<Line<'static>>, width: usize) -> Vec<Line<'static>> {
    wrap_lines_flagged(lines, width)
        .into_iter()
        .map(|(_, line)| line)
        .collect()
}

/// The flag marks rows that start a logical line; soft-wrap continuations
/// carry `false` so copied text joins them without a newline.
fn wrap_lines_flagged(lines: Vec<Line<'static>>, width: usize) -> Vec<(bool, Line<'static>)> {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

    let width = width.saturating_sub(1).max(1);
    let gutter_width = crate::theme::USER_GUTTER.width();
    let mut out = Vec::new();
    for line in lines {
        // A human turn's left rail is display furniture: carry it onto every
        // continuation row so a wrapped message reads as one quoted block
        // instead of spilling flush-left into the surrounding prose.
        let gutter = line
            .spans
            .first()
            .filter(|span| span.content == crate::theme::USER_GUTTER)
            .cloned();
        // The inline-emphasis span can be the first thing on a wrapped
        // continuation. Padding must retain the logical diff line's base
        // background, not make the remainder of that continuation emphatic.
        let padding_background = line
            .spans
            .iter()
            .find_map(|span| span.style.bg)
            .map(base_diff_background);
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
                            pad_background_line(
                                std::mem::take(&mut current),
                                current_width,
                                width,
                                padding_background,
                            ),
                        ));
                        current_width = 0;
                        if let Some(gutter) = &gutter {
                            current.push(gutter.clone());
                            current_width = gutter_width;
                        }
                    }
                    current.push(Span::styled(c.to_string(), span.style));
                    current_width += char_width;
                }
            }
        }
        out.push((
            first,
            pad_background_line(current, current_width, width, padding_background),
        ));
    }
    out
}

/// Return a diff line's ordinary background when given one of its brighter
/// inline-emphasis colors; all other backgrounds are already their own base.
fn base_diff_background(background: ratatui::style::Color) -> ratatui::style::Color {
    if background == crate::theme::diff_add_emph_bg() {
        crate::theme::diff_add_bg()
    } else if background == crate::theme::diff_del_emph_bg() {
        crate::theme::diff_del_bg()
    } else {
        background
    }
}

/// Ratatui backgrounds otherwise stop at the final code character. Extend
/// diff lines to the terminal edge, including every wrapped chunk.
fn pad_background_line(
    mut spans: Vec<Span<'static>>,
    used: usize,
    width: usize,
    padding_background: Option<ratatui::style::Color>,
) -> Line<'static> {
    if let Some(background) =
        padding_background.or_else(|| spans.iter().find_map(|span| span.style.bg))
    {
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
    fn wrapped_diff_emphasis_pads_with_the_base_background() {
        let line = Line::from(vec![
            Span::styled(
                "base ",
                ratatui::style::Style::default().bg(crate::theme::diff_add_bg()),
            ),
            Span::styled(
                "changed",
                ratatui::style::Style::default().bg(crate::theme::diff_add_emph_bg()),
            ),
        ]);
        // Usable width is one less than the supplied width. The continuation
        // therefore begins inside the emphatic word and exposes the old bug.
        let rows = wrap_lines(vec![line], 8);
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[1].spans[0].style.bg,
            Some(crate::theme::diff_add_emph_bg())
        );
        assert_eq!(
            rows[1].spans.last().unwrap().style.bg,
            Some(crate::theme::diff_add_bg()),
            "trailing cells must stay at the ordinary diff color"
        );
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
    fn drag_edge_reports_direction_only_at_the_view_edges() {
        let mut t = Transcript::new(20);
        for i in 0..10 {
            t.push(vec![Line::raw(format!("line {i}"))]);
        }
        let area = Rect::new(0, 0, 20, 4);
        let mut buf = Buffer::empty(area);
        t.render(&mut buf, area);
        // No selection yet: nothing to auto-scroll.
        assert_eq!(t.drag_edge(0), None);
        t.mouse_down(0, 1);
        assert_eq!(t.drag_edge(0), Some(true), "top row scrolls toward history");
        assert_eq!(
            t.drag_edge(3),
            Some(false),
            "bottom row scrolls toward tail"
        );
        assert_eq!(t.drag_edge(2), None, "inside the view does not scroll");
    }

    #[test]
    fn edge_autoscroll_extends_the_selection_through_revealed_rows() {
        let mut t = Transcript::new(20);
        for i in 0..10 {
            t.push(vec![Line::raw(format!("line {i}"))]);
        }
        let area = Rect::new(0, 0, 20, 4);
        let mut buf = Buffer::empty(area);
        // Look at history (rows 2..5), then start selecting at the top and drag
        // to the bottom edge.
        t.scroll_up(4);
        t.render(&mut buf, area);
        t.mouse_down(0, 0);
        // Drag along the right edge so whole rows fall inside the selection.
        t.mouse_drag(19, 3);
        assert_eq!(t.drag_edge(3), Some(false));
        // Each timer step scrolls a line toward the tail; the redraw that
        // follows updates the view, and the next extend reaches the new bottom.
        for _ in 0..4 {
            t.scroll_down(1);
            t.render(&mut buf, area);
            t.mouse_drag(19, 3);
        }
        let text = t.mouse_up().expect("a multi-row selection");
        assert!(text.contains("line 2"), "anchor retained: {text:?}");
        assert!(
            text.contains("line 9"),
            "auto-scroll grew the selection to the tail: {text:?}"
        );
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
    fn selection_omits_the_user_rail_from_wrapped_prompts() {
        let mut t = Transcript::new(10); // usable width 9, including the rail
        t.push(vec![Line::from(vec![
            Span::styled(crate::theme::USER_GUTTER, crate::theme::user_gutter()),
            Span::raw("abcdefghi"),
        ])]);
        let area = Rect::new(0, 0, 10, 2);
        let mut buf = Buffer::empty(area);
        t.render(&mut buf, area);
        t.mouse_down(0, 0);
        t.mouse_drag(9, 1);

        assert_eq!(t.mouse_up().as_deref(), Some("abcdefghi"));
    }

    #[test]
    fn selection_preserves_ordinary_text_that_starts_with_the_rail_marker() {
        let mut t = Transcript::new(20);
        t.push(vec![Line::raw("▌ literal text")]);
        let area = Rect::new(0, 0, 20, 1);
        let mut buf = Buffer::empty(area);
        t.render(&mut buf, area);
        t.mouse_down(0, 0);
        t.mouse_drag(19, 0);

        assert_eq!(t.mouse_up().as_deref(), Some("▌ literal text"));
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
    fn hovered_tool_header_uses_plain_foreground_without_touching_detail_or_padding() {
        let mut t = Transcript::new(40);
        t.push_with_detail(
            vec![Line::styled("preview", crate::theme::dim())],
            vec![Line::raw("expanded output")],
            false,
            5,
        );
        let area = Rect::new(0, 0, 40, 4);
        let mut buf = Buffer::empty(area);
        t.render(&mut buf, area);
        t.mouse_moved(0, 0);
        t.render(&mut buf, area);
        assert_eq!(buf[(0, 0)].bg, ratatui::style::Color::Reset);
        assert_eq!(buf[(0, 0)].fg, Color::Reset);
        assert_eq!(buf[(30, 0)].bg, ratatui::style::Color::Reset);
        assert!(!buf[(0, 0)].modifier.contains(Modifier::UNDERLINED));

        t.mouse_down(0, 0);
        assert_eq!(t.mouse_up(), None);
        let mut buf = Buffer::empty(area);
        t.render(&mut buf, area);
        assert_eq!(buf[(0, 1)].bg, ratatui::style::Color::Reset);
        assert!(!buf[(0, 1)].modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn moving_into_blank_tail_clears_final_tool_hover() {
        let mut t = Transcript::new(40);
        t.push_with_detail(
            vec![Line::raw("preview")],
            (0..3).map(|i| Line::raw(format!("out {i}"))).collect(),
            false,
            5,
        );
        let area = Rect::new(0, 0, 40, 4);
        let mut buf = Buffer::empty(area);
        t.render(&mut buf, area);

        t.mouse_moved(0, 0);
        assert_eq!(t.hovered, Some(0));

        t.mouse_moved(0, 1);
        assert_eq!(t.hovered, None);
        t.render(&mut buf, area);
        assert!(!buffer_text(&buf, area).contains("▸ 3 lines"));
        assert_eq!(buf[(0, 0)].bg, ratatui::style::Color::Reset);
    }

    #[test]
    fn live_status_stays_visible_when_the_detail_is_folded_and_clears_cleanly() {
        let mut t = Transcript::new(40);
        t.push(vec![Line::raw("● Task")]);
        t.attach_detail(0, vec![Line::raw("summary")], 5);
        t.set_live_status(0, Some(vec![Line::raw("  ⠋ working · Search")]));
        let area = Rect::new(0, 0, 40, 5);
        let mut buf = Buffer::empty(area);
        t.set_task_activity_frame(0);
        t.render(&mut buf, area);
        assert_eq!(t.total(), 2, "status is visible outside the closed detail");
        assert!(buffer_text(&buf, area).contains("working · Search"));
        let first_frame = buf[(3, 1)].fg;

        t.set_task_activity_frame(5);
        t.render(&mut buf, area);
        assert_ne!(
            buf[(3, 1)].fg,
            first_frame,
            "the live status colour animates"
        );

        t.set_live_status(0, None);
        let mut buf = Buffer::empty(area);
        t.render(&mut buf, area);
        assert_eq!(t.total(), 1);
        assert!(!buffer_text(&buf, area).contains("working · Search"));
    }

    #[test]
    fn results_attach_to_the_already_baked_call_row() {
        let mut t = Transcript::new(40);
        t.push(vec![Line::raw("● Read app.rs")]);
        // ToolEnd lands: preview rides on the header row, output folds
        // under it — no separate `⎿` row appears.
        t.extend_head(0, vec![Span::raw(" — 3 lines")]);
        t.attach_detail(
            0,
            (0..3).map(|i| Line::raw(format!("out {i}"))).collect(),
            5,
        );
        let area = Rect::new(0, 0, 40, 10);
        let mut buf = Buffer::empty(area);
        t.render(&mut buf, area);
        assert_eq!(t.total(), 1, "closed detail keeps the record one row");
        assert!(buffer_text(&buf, area).contains("● Read app.rs — 3 lines"));

        // Hover advertises the fold on the call row itself; click opens it.
        t.mouse_moved(0, 0);
        let mut buf = Buffer::empty(area);
        t.render(&mut buf, area);
        assert!(buffer_text(&buf, area).contains("▸ 3 lines"));
        t.mouse_down(0, 0);
        t.mouse_up();
        let mut buf = Buffer::empty(area);
        t.render(&mut buf, area);
        let text = buffer_text(&buf, area);
        assert!(text.contains("out 0") && text.contains("out 2"));
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
        // Closed details stay quiet until the block is actionable under the pointer.
        assert!(!buffer_text(&buf, area).contains("▸ 3 lines"));
        t.mouse_moved(0, 0);
        let mut buf = Buffer::empty(area);
        t.render(&mut buf, area);
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
    fn attaching_a_result_preserves_an_existing_plain_detail() {
        let mut t = Transcript::new(30);
        t.push(vec![Line::raw("run")]);
        t.attach_detail(
            0,
            vec![Line::raw("command one"), Line::raw("command two")],
            14,
        );
        t.append_detail(
            0,
            vec![Line::raw("result one"), Line::raw("result two")],
            14,
        );

        let Some(detail) = &t.blocks[0].detail else {
            panic!("detail attached");
        };
        let Content::Lines(lines) = &detail.content else {
            panic!("plain detail retained");
        };
        let text: Vec<String> = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect();
        assert_eq!(
            text,
            ["command one", "command two", "", "result one", "result two"]
        );
    }

    #[test]
    fn the_user_rail_continues_onto_wrapped_rows() {
        let line = Line::from(vec![
            Span::styled(crate::theme::USER_GUTTER, crate::theme::user_gutter()),
            Span::raw("aaaa bbbb cccc"),
        ]);
        let rows = wrap_lines_flagged(vec![line], 10);
        assert!(rows.len() > 1);
        for (index, (starts, row)) in rows.iter().enumerate() {
            assert_eq!(*starts, index == 0);
            let text: String = row.spans.iter().map(|span| span.content.as_ref()).collect();
            assert!(text.starts_with(crate::theme::USER_GUTTER), "row {index}");
        }
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
    fn responsive_markdown_table_reflows_between_grid_and_cards_on_resize() {
        let document = crate::markdown::Renderer::default().parse(
            "| 组合 | Sharpe | 年化收益 | 最大回撤 |\n| --- | ---: | ---: | ---: |\n| baseline_swcta | 2.106 | 2.54% | -1.71% |",
        );
        let mut transcript = Transcript::new(80);
        transcript.push_markdown(document);

        let wide = Rect::new(0, 0, 80, 12);
        let mut wide_buffer = Buffer::empty(wide);
        transcript.render(&mut wide_buffer, wide);
        assert!(buffer_text(&wide_buffer, wide).contains("┌"));

        let narrow = Rect::new(0, 0, 20, 12);
        let mut narrow_buffer = Buffer::empty(narrow);
        transcript.render(&mut narrow_buffer, narrow);
        let narrow_text = buffer_text(&narrow_buffer, narrow);
        assert!(!narrow_text.contains("┌"));
        assert!(narrow_text.contains("baseline_swcta"));
        assert!(narrow_text.contains("Sharpe: 2.106"));

        let mut restored_buffer = Buffer::empty(wide);
        transcript.render(&mut restored_buffer, wide);
        assert!(buffer_text(&restored_buffer, wide).contains("┌"));
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
