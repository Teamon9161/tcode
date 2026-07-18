//! Rewind navigation: walking back to an earlier user input, truncating the
//! ledger there and optionally restoring the files that turn touched.
//!
//! Rewind is the only sanctioned way to shorten history — `Ledger` is
//! otherwise append-only — so the truncation point is always an `Entry::User`
//! index the transcript recorded at bake time, never a recomputed guess.
//!
//! Touches: rewind_nav, session, transcript, editor.

use super::*;

pub(super) struct RewindCandidate {
    /// Ledger index of the user entry (truncate target).
    pub(super) index: usize,
    /// Full original input, prefilled into the editor.
    pub(super) text: String,
    /// Files changed at/after this point → offer to restore them.
    pub(super) dirty: bool,
}

/// Double-Esc rewind navigation: the transcript itself jumps to and
/// highlights the chosen user input — no picker dialog.
pub(super) struct RewindNav {
    pub(super) candidates: Vec<RewindCandidate>,
    pub(super) pos: usize,
    /// Editor content before navigation began, restored on exit.
    pub(super) saved_input: String,
}

impl App {
    pub(super) fn open_rewind(&mut self) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let candidates: Vec<RewindCandidate> = session
            .ledger
            .entries()
            .iter()
            .enumerate()
            .filter_map(|(i, e)| match e {
                tcode_core::Entry::User(blocks) => {
                    let text = blocks
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::Text { text }
                                if !text.starts_with("<tcode-status>")
                                    && !text.starts_with("[pasted content]") =>
                            {
                                Some(text.as_str())
                            }
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    (!text.is_empty()).then(|| RewindCandidate {
                        index: i,
                        text,
                        dirty: session.checkpoints.dirty_since(i),
                    })
                }
                _ => None,
            })
            .collect();
        if candidates.is_empty() {
            self.bake(vec![Line::styled("nothing to rewind to", theme::dim())]);
            return;
        }
        self.rewind_nav = Some(RewindNav {
            pos: candidates.len() - 1,
            candidates,
            saved_input: self.editor.text(),
        });
        self.apply_rewind_nav();
    }

    /// Show the current navigation target: highlight + scroll its echo
    /// into view, prefill the editor with the original input.
    pub(super) fn apply_rewind_nav(&mut self) {
        let Some(nav) = &self.rewind_nav else {
            return;
        };
        let candidate = &nav.candidates[nav.pos];
        let text = candidate.text.clone();
        self.transcript.highlight_entry(candidate.index);
        self.editor.clear();
        self.editor.insert_str(&text);
    }

    pub(super) fn exit_rewind_nav(&mut self) {
        if let Some(nav) = self.rewind_nav.take() {
            self.transcript.clear_highlight();
            self.transcript.scroll_to_bottom();
            self.editor.clear();
            self.editor.insert_str(&nav.saved_input);
        }
    }

    pub(super) fn confirm_rewind_nav(&mut self, restore_files: bool) {
        let Some(nav) = self.rewind_nav.take() else {
            return;
        };
        self.transcript.clear_highlight();
        let candidate = &nav.candidates[nav.pos];
        self.do_rewind(candidate.index, restore_files, candidate.text.clone());
    }

    pub(super) fn do_rewind(&mut self, index: usize, restore_files: bool, text: String) {
        // The turn it was predicting from is about to stop existing.
        self.drop_suggestion();
        let Some(session) = self.session.as_mut() else {
            return;
        };
        // Visual truncation first: the transcript forgets the rewound tail
        // exactly like the ledger does. (False only for history without an
        // echo, e.g. compacted or imported conversations.)
        self.transcript.truncate_from_entry(index);
        session.ledger.truncate_tail(index);
        session.last_prompt_tokens = self.agent.estimate_context_tokens(session);
        self.meter
            .set_context(session.last_prompt_tokens, !session.ledger.is_empty());
        // Earlier reads are gone from the model's context: freshness
        // stubs would point at nothing. Reset it wholesale.
        session
            .tool_ctx
            .freshness
            .lock()
            .expect("freshness lock")
            .clear();
        let mut lines = vec![Line::styled(
            format!("↺ rewound conversation to entry {index}"),
            ratatui::style::Style::default().fg(theme::WARN),
        )];
        if restore_files {
            for (path, outcome) in session.checkpoints.restore_to(index) {
                use tcode_core::checkpoint::Restore;
                let what = match outcome {
                    Restore::Restored => "restored".to_string(),
                    Restore::Deleted => "deleted (did not exist yet)".to_string(),
                    Restore::Failed(e) => format!("FAILED: {e}"),
                };
                lines.push(Line::styled(
                    format!("  ⎿ {} — {what}", path.display()),
                    theme::dim(),
                ));
            }
        } else if session.checkpoints.dirty_since(index) {
            lines.push(Line::styled(
                "  ⎿ files keep their current content (not rolled back)",
                theme::dim(),
            ));
        }
        self.bake(lines);
        // The original input comes back for editing and resending.
        self.editor.clear();
        self.editor.insert_str(&text);
    }
}
