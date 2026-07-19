//! Composing a prompt: prompt-box selection and copy, clipboard and
//! attachments, and `@`/slash completion.
//!
//! The layout and matching arithmetic is in `crate::composer`, which is pure.
//! This module owns the state those functions are applied to and the
//! decisions that need the session — which paths exist, what a paste becomes.
//!
//! Touches: editor, attachments, next_attachment_id, clipboard,
//! reference_index, input_hitbox, input_mouse_active, input_dragged,
//! drag_scroll, popup_index, dismissed_reference, overlay.

use super::*;

pub(super) enum Attachment {
    Image {
        id: u32,
        bytes: Vec<u8>,
        media_type: &'static str,
        label: String,
    },
    Text {
        id: u32,
        content: String,
        label: String,
    },
}

impl Attachment {
    /// The inline token shown in the editor. Stable per attachment (the id
    /// never renumbers within a draft) so it can be matched back for deletion.
    pub(super) fn placeholder(&self) -> String {
        match self {
            Attachment::Image { id, .. } => format!("[Image #{id}]"),
            Attachment::Text { id, .. } => format!("[Pasted text #{id}]"),
        }
    }

    pub(super) fn label(&self) -> &str {
        match self {
            Attachment::Image { label, .. } | Attachment::Text { label, .. } => label,
        }
    }
}

#[derive(Clone)]
pub(super) enum CompletionKind {
    Slash,
    Reference { start: Position, end: Position },
}

#[derive(Clone)]
pub(super) struct CompletionMatch {
    pub(super) label: String,
    pub(super) description: String,
    pub(super) replacement: String,
    pub(super) kind: CompletionKind,
}

#[derive(Clone, Copy)]
pub(super) struct InputHitbox {
    pub(super) rect: Rect,
    pub(super) editor_start: usize,
}

/// How a prompt appears in the transcript. The single renderer for both paths:
/// a prompt sent immediately and one that waited in the queue must be
/// indistinguishable once they land. A `/name` skill invocation is echoed
/// folded, exactly as `SessionView`'s ledger replay folds the same sentinel
/// text back down (`crate::view::skill_echo_lines`) — one shared detector so
/// live and replay cannot draw it differently.
pub(super) fn prompt_echo(text: &str, attachments: &[String]) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = vec![Line::default()];
    match tcode_tools::parse_skill_echo(text) {
        Some(skill_echo) => lines.extend(crate::view::skill_echo_lines(&skill_echo)),
        None => lines.extend(quote_lines(None, text)),
    }
    lines.extend(attachments.iter().map(|label| quote_attachment_line(label)));
    lines.push(Line::default());
    lines
}

impl App {
    /// Freeze the draft into the message it will stay: the blocks that go on
    /// the wire, plus what the transcript renders it from. Attachments are
    /// consumed here, so a queued prompt keeps the image that was pasted into
    /// it — and one whose inline token the user deleted drops it, exactly as
    /// when sending immediately.
    pub(super) fn compose_draft(&mut self, input: String) -> PendingMessage {
        let mut attachments: Vec<String> = Vec::new();
        let mut blocks: Vec<ContentBlock> = Vec::new();
        for att in self.attachments.drain(..) {
            let placeholder = att.placeholder();
            if !input.contains(&placeholder) {
                continue;
            }
            match att {
                Attachment::Image {
                    id,
                    bytes,
                    media_type,
                    label,
                } => {
                    attachments.push(label);
                    if self.agent.model.snapshot().provider.supports_vision() {
                        use base64::Engine as _;
                        blocks.push(ContentBlock::Image {
                            media_type: media_type.into(),
                            data: base64::engine::general_purpose::STANDARD.encode(bytes),
                        });
                    } else {
                        let dir = self.scratch_dir.join("pasted");
                        let ext = match media_type {
                            "image/jpeg" => "jpg",
                            "image/png" => "png",
                            _ => "img",
                        };
                        let stamp = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis();
                        let path = dir.join(format!("paste-{stamp}-{id}.{ext}"));
                        match std::fs::create_dir_all(&dir)
                            .and_then(|()| std::fs::write(&path, bytes))
                        {
                            Ok(()) => blocks.push(ContentBlock::Text {
                                text: format!("[image saved to {}]", path.display()),
                            }),
                            Err(error) => {
                                self.notice = Some((
                                    format!("could not save pasted image: {error}"),
                                    Instant::now(),
                                ));
                                blocks.push(ContentBlock::Text {
                                    text: "[pasted image could not be saved; the current model cannot view it]".into(),
                                });
                            }
                        }
                    }
                }
                Attachment::Text { content, .. } => {
                    blocks.push(ContentBlock::Text {
                        text: format!("{placeholder}:\n{content}"),
                    });
                }
            }
        }
        self.next_attachment_id = 1;
        blocks.push(ContentBlock::Text {
            text: input.clone(),
        });
        PendingMessage {
            text: input,
            attachments,
            blocks,
        }
    }

    pub(super) fn editor_visual_up(&mut self) -> bool {
        let layout = editor_layout(&self.editor, area_width(&self.terminal));
        move_editor_visual(&mut self.editor, &layout, VisualMove::Up)
    }

    pub(super) fn editor_visual_down(&mut self) -> bool {
        let layout = editor_layout(&self.editor, area_width(&self.terminal));
        move_editor_visual(&mut self.editor, &layout, VisualMove::Down)
    }

    pub(super) fn submit(&mut self, running: bool) {
        let text = self.editor.text();
        let trimmed = text.trim().to_string();
        if trimmed.is_empty() && self.attachments.is_empty() {
            return;
        }
        if trimmed.starts_with('/') {
            let cmd = self
                .popup_selection()
                .filter(|completion| matches!(&completion.kind, CompletionKind::Slash))
                .map(|completion| completion.replacement)
                .unwrap_or_else(|| trimmed.clone());
            self.editor.take();
            self.run_slash(&cmd);
            return;
        }
        if running {
            // The turn owns the Session, and a user entry cannot be spliced
            // between a tool call and its result anyway. Queue the finished
            // message — attachments and all — for the loop's next boundary;
            // ctrl+c sends it right away.
            self.editor.take();
            let message = self.compose_draft(trimmed);
            self.pending.push(message);
            return;
        }
        let input = self.editor.take();
        // Sending a message means the user is done reading history.
        self.transcript.scroll_to_bottom();
        let message = self.compose_draft(input);
        self.start_turn(message);
    }

    /// Is the mouse row inside the input box? Used to route the wheel to the
    /// prompt instead of the transcript. None hitbox (a dialog/picker owns the
    /// panel) means the wheel keeps scrolling the transcript.
    pub(super) fn wheel_over_input(&self, y: u16) -> bool {
        self.input_hitbox
            .is_some_and(|hit| y >= hit.rect.y && y < hit.rect.bottom())
    }

    pub(super) fn input_mouse_down(&mut self, x: u16, y: u16) -> bool {
        let Some((row, col)) = self.input_position_at(x, y) else {
            self.input_mouse_active = false;
            return false;
        };
        self.input_mouse_active = true;
        self.input_dragged = false;
        self.editor.start_selection_by_display_col(row, col);
        self.popup_index = 0;
        true
    }

    pub(super) fn input_mouse_drag(&mut self, x: u16, y: u16) {
        if let Some((row, col)) = self.input_position_at(x, y) {
            self.input_dragged = true;
            self.editor.extend_selection_by_display_col(row, col);
        }
    }

    /// End a selection drag: stop any auto-scroll, then copy like a normal
    /// mouse-up. A plain click immediately folds or unfolds the block under it.
    pub(super) fn finish_drag(&mut self, x: u16, y: u16) {
        self.drag_scroll = None;
        if self.input_mouse_active {
            self.input_mouse_up(x, y);
        } else if let Some(text) = self.active_transcript_mut().mouse_up() {
            self.copy_selection(text);
        }
    }

    /// One timer step of edge auto-scroll: scroll the transcript a line in the
    /// armed direction, then re-extend the selection to the (now different)
    /// edge row. Self-terminates once the view reaches the top or bottom of the
    /// content — nothing more to reveal — so a release the terminal never
    /// reported (button let go outside the window) cannot scroll forever. A
    /// dialog opening mid-drag takes over the mouse, so disarm.
    pub(super) fn drag_autoscroll_step(&mut self) {
        let Some((toward_older, x, y)) = self.drag_scroll else {
            return;
        };
        if self.overlay.is_some() {
            self.drag_scroll = None;
            return;
        }
        let before = self.active_transcript().scroll_offset();
        if toward_older {
            self.active_transcript_mut().scroll_up(1);
        } else {
            self.active_transcript_mut().scroll_down(1);
        }
        if self.active_transcript().scroll_offset() == before {
            // Hit the content edge: stop and copy what is now selected, so the
            // gesture completes even if its release was lost outside the window.
            self.drag_scroll = None;
            if let Some(text) = self.active_transcript_mut().mouse_up() {
                self.copy_selection(text);
            }
            return;
        }
        self.active_transcript_mut().mouse_drag(x, y);
    }

    pub(super) fn input_mouse_up(&mut self, x: u16, y: u16) {
        self.input_mouse_active = false;
        // A plain click (no drag) only repositions the cursor — never copies,
        // even if the release cell rounds to a neighbour of the press cell.
        if !self.input_dragged {
            return;
        }
        self.input_mouse_drag(x, y);
        if let Some(text) = self.editor.selected_text() {
            self.copy_input_text(text);
        }
    }

    pub(super) fn input_position_at(&self, x: u16, y: u16) -> Option<(usize, usize)> {
        let hit = self.input_hitbox?;
        if x <= hit.rect.x
            || x >= hit.rect.right().saturating_sub(1)
            || y <= hit.rect.y
            || y >= hit.rect.bottom().saturating_sub(1)
        {
            return None;
        }
        let layout = editor_layout(&self.editor, hit.rect.width);
        if layout.lines.is_empty() {
            return Some((0, 0));
        }
        let visual_row = hit.editor_start + (y - hit.rect.y - 1) as usize;
        let visual_row = visual_row.min(layout.lines.len() - 1);
        let line = &layout.lines[visual_row];
        let content_x = x.saturating_sub(hit.rect.x + 3) as usize;
        let display_col =
            line.start_col + content_x.min(line.end_col.saturating_sub(line.start_col));
        Some((line.logical_row, display_col))
    }

    pub(super) fn copy_input_text(&mut self, text: String) {
        let lines = text.lines().count().max(1);
        let what = if lines <= 1 {
            "input".to_string()
        } else {
            format!("input {lines} lines")
        };
        self.copy_text(text, what);
    }

    pub(super) fn copy_editor_selection_or_prompt(&mut self) {
        if let Some(text) = self.editor.selected_text() {
            self.copy_input_text(text);
        } else if !self.editor.is_empty() {
            self.copy_input_text(self.editor.text());
        }
    }

    pub(super) fn cut_editor_selection_or_prompt(&mut self) {
        self.dismissed_reference = None;
        if let Some(text) = self.editor.selected_text() {
            self.copy_input_text(text);
            self.editor.delete_selection();
        } else if !self.editor.is_empty() {
            self.copy_input_text(self.editor.text());
            self.editor.clear();
        }
    }

    /// Mouse-selection copy: system clipboard first (arboard), OSC 52 as
    /// the remote/SSH fallback where no local clipboard exists.
    pub(super) fn copy_selection(&mut self, text: String) {
        let lines = text.lines().count();
        let what = if lines <= 1 {
            "selection".to_string()
        } else {
            format!("{lines} lines")
        };
        self.copy_text(text, what);
    }

    pub(super) fn copy_text(&mut self, text: String, what: String) {
        let copied = self
            .clipboard
            .as_mut()
            .is_some_and(|clipboard| clipboard.set_text(text.clone()).is_ok());
        if !copied {
            use base64::Engine as _;
            use std::io::Write as _;
            let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
            let mut out = std::io::stdout();
            let _ = write!(out, "\x1b]52;c;{encoded}\x07");
            let _ = out.flush();
        }
        self.notice = Some((format!("copied {what}"), Instant::now()));
    }

    /// Schedule a fresh ignored-aware index without blocking terminal input.
    /// Results carry their cwd, so a slow scan that finishes after `/cd` is
    /// harmlessly discarded.
    pub(super) fn refresh_reference_index(&self) {
        let cwd = self.cwd.clone();
        let tx = self.reference_tx.clone();
        tokio::spawn(async move {
            let scan_cwd = cwd.clone();
            let index = tokio::task::spawn_blocking(move || tcode_core::index_project(&scan_cwd))
                .await
                .unwrap_or_default();
            let _ = tx.send((cwd, index)).await;
        });
    }

    pub(super) fn paste_from_clipboard(&mut self) {
        let Some(clipboard) = self.clipboard.as_mut() else {
            return;
        };
        // Pull owned data out while the clipboard borrow is held, then release
        // it before touching other `self` fields.
        let image = clipboard.get_image().ok().and_then(|img| {
            let width = u32::try_from(img.width).ok()?;
            let height = u32::try_from(img.height).ok()?;
            tcode_core::images::normalize_rgba(width, height, img.bytes.into_owned()).ok()
        });
        if let Some(image) = image {
            let kb = image.bytes.len() / 1024;
            self.add_attachment(|id| Attachment::Image {
                id,
                bytes: image.bytes,
                media_type: image.media_type,
                label: format!("Image #{id} ({}x{}, {kb}KB)", image.width, image.height),
            });
            return;
        }
        if let Some(text) = self.clipboard.as_mut().and_then(|c| c.get_text().ok()) {
            self.on_paste_text(text);
        }
    }

    /// Discard the whole draft: editor text, attachments, and the token
    /// numbering that ties them together.
    pub(super) fn clear_draft(&mut self) {
        self.dismissed_reference = None;
        self.editor.clear();
        self.attachments.clear();
        self.next_attachment_id = 1;
    }

    /// Register an attachment and drop its inline token into the editor at the
    /// cursor. The token is how the user sees, moves, and deletes it — pressing
    /// backspace right after it removes the whole thing (see `on_key`).
    pub(super) fn add_attachment(&mut self, make: impl FnOnce(u32) -> Attachment) {
        self.dismissed_reference = None;
        let id = self.next_attachment_id;
        self.next_attachment_id += 1;
        let att = make(id);
        let placeholder = att.placeholder();
        self.attachments.push(att);
        self.editor.insert_str(&placeholder);
    }

    /// If the cursor sits immediately after an attachment's inline token,
    /// delete the whole token and drop that attachment in one keystroke.
    pub(super) fn backspace_attachment_token(&mut self) -> bool {
        let pos = self.editor.position();
        let before: String = self.editor.lines()[pos.row].chars().take(pos.col).collect();
        let Some(idx) = self
            .attachments
            .iter()
            .position(|a| before.ends_with(&a.placeholder()))
        else {
            return false;
        };
        let token_len = self.attachments[idx].placeholder().chars().count();
        for _ in 0..token_len {
            self.editor.backspace();
        }
        let label = self.attachments.remove(idx).label().to_string();
        self.notice = Some((format!("removed {label}"), Instant::now()));
        true
    }

    pub(super) fn on_paste_text(&mut self, text: String) {
        self.dismissed_reference = None;
        let lines = text.lines().count().max(1);
        let chars = text.chars().count();
        if paste_should_fold(chars, lines) {
            self.add_attachment(|id| Attachment::Text {
                id,
                content: text,
                label: format!("Pasted text #{id} ({chars} chars · {lines} lines)"),
            });
        } else {
            self.editor.insert_str(&text);
        }
    }

    pub(super) fn popup_active(&self) -> bool {
        self.overlay.is_none() && !self.completion_matches().is_empty()
    }

    pub(super) fn completion_matches(&self) -> Vec<CompletionMatch> {
        if self.overlay.is_some() {
            return Vec::new();
        }
        if self.editor.line_count() == 1 && self.editor.text().starts_with('/') {
            let prefix = self.editor.text();
            let mut matches: Vec<CompletionMatch> = UI_COMMANDS
                .iter()
                .copied()
                .chain(self.registry.entries())
                .filter(|(command, _)| command.starts_with(&prefix))
                .map(|(command, description)| CompletionMatch {
                    label: command.to_string(),
                    description: description.to_string(),
                    replacement: command.to_string(),
                    kind: CompletionKind::Slash,
                })
                .collect();
            matches.extend(self.skills.iter().filter_map(|skill| {
                let command = format!("/{}", skill.name);
                command.starts_with(&prefix).then(|| CompletionMatch {
                    label: command.clone(),
                    description: clip_description(&skill.description, 100),
                    replacement: command,
                    kind: CompletionKind::Slash,
                })
            }));
            return matches;
        }
        let Some((start, end, query)) = self.active_reference() else {
            return Vec::new();
        };
        if self.dismissed_reference == Some(start) {
            return Vec::new();
        }
        let mut matches: Vec<(usize, &ReferenceCandidate)> = self
            .reference_index
            .iter()
            .filter_map(|candidate| {
                reference_score(&candidate.path, &query).map(|score| (score, candidate))
            })
            .collect();
        matches.sort_by(|(left_score, left), (right_score, right)| {
            reference_match_order(*left_score, &left.path, *right_score, &right.path)
        });
        let mut basename_counts = HashMap::new();
        for (_, candidate) in &matches {
            *basename_counts
                .entry(reference_basename(&candidate.path))
                .or_insert(0usize) += 1;
        }
        matches
            .into_iter()
            .take(8)
            .map(|(_, candidate)| {
                let path = reference_candidate_path(candidate);
                let label_path = if basename_counts
                    .get(reference_basename(&candidate.path))
                    .copied()
                    .unwrap_or_default()
                    > 1
                {
                    path.clone()
                } else {
                    reference_basename_path(candidate)
                };
                let replacement = reference_marker(&path);
                let size = candidate
                    .bytes
                    .map(format_bytes)
                    .map(|size| format!(" · {size}"))
                    .unwrap_or_default();
                CompletionMatch {
                    label: reference_marker(&label_path),
                    description: format!("{}{}", candidate.display_kind(), size),
                    replacement,
                    kind: CompletionKind::Reference { start, end },
                }
            })
            .collect()
    }

    pub(super) fn active_reference(&self) -> Option<(Position, Position, String)> {
        let cursor = self.editor.position();
        let line = self.editor.lines().get(cursor.row)?;
        let chars: Vec<char> = line.chars().collect();
        let at = (0..cursor.col)
            .rev()
            .find(|&index| chars[index] == '@' && reference_boundary(&chars, index))?;
        let quoted = chars.get(at + 1) == Some(&'"');
        let content_start = at + 1 + usize::from(quoted);
        let mut token_end = content_start;
        if quoted {
            while token_end < chars.len() && chars[token_end] != '"' {
                token_end += 1;
            }
            if token_end < chars.len() {
                token_end += 1;
            }
        } else {
            while token_end < chars.len() && reference_token_char(chars[token_end]) {
                token_end += 1;
            }
        }
        if cursor.col < content_start || cursor.col > token_end {
            return None;
        }
        let query: String = chars[content_start..cursor.col.min(token_end)]
            .iter()
            .collect();
        Some((
            Position {
                row: cursor.row,
                col: at,
            },
            Position {
                row: cursor.row,
                col: token_end,
            },
            query,
        ))
    }

    pub(super) fn popup_selection(&self) -> Option<CompletionMatch> {
        let matches = self.completion_matches();
        matches
            .get(self.popup_index.min(matches.len().saturating_sub(1)))
            .cloned()
    }

    pub(super) fn accept_reference_completion(&mut self) -> bool {
        let Some(completion) = self.popup_selection() else {
            return false;
        };
        if !matches!(completion.kind, CompletionKind::Reference { .. }) {
            return false;
        }
        self.accept_completion(completion);
        true
    }

    pub(super) fn apply_reference_completion(
        editor: &mut Editor,
        start: Position,
        end: Position,
        replacement: &str,
    ) {
        editor.replace_range(start, end, replacement);
        // A completed reference is an atomic prompt token. Leave the cursor
        // ready for prose rather than making the next character part of the
        // path marker.
        editor.insert_char(' ');
    }

    pub(super) fn accept_completion(&mut self, completion: CompletionMatch) {
        match completion.kind {
            CompletionKind::Slash => {
                self.dismissed_reference = None;
                self.editor.clear();
                self.editor.insert_str(&completion.replacement);
            }
            CompletionKind::Reference { start, end } => {
                Self::apply_reference_completion(
                    &mut self.editor,
                    start,
                    end,
                    &completion.replacement,
                );
                self.dismissed_reference = Some(start);
            }
        }
        self.popup_index = 0;
    }
}
