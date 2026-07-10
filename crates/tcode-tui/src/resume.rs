//! Current-project and imported-session picker used by `/resume`.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::text::Line;
use tcode_core::{ExternalSessionInfo, ExternalSource, SessionInfo};

use crate::theme;

enum Item {
    Current(SessionInfo),
    Source(ExternalSource),
    External(ExternalSessionInfo),
}

pub struct Picker {
    title: String,
    items: Vec<Item>,
    selected: usize,
}

pub enum PickResult {
    Pending,
    Cancelled,
    Current(String),
    Source(ExternalSource),
    External(ExternalSessionInfo),
}

impl Picker {
    pub fn new(sessions: Vec<SessionInfo>) -> Self {
        let mut items = vec![
            Item::Source(ExternalSource::Codex),
            Item::Source(ExternalSource::Claude),
        ];
        items.extend(sessions.into_iter().map(Item::Current));
        Self {
            title: "resume conversation".into(),
            items,
            selected: 0,
        }
    }

    pub fn external(source: ExternalSource, sessions: Vec<ExternalSessionInfo>) -> Option<Self> {
        (!sessions.is_empty()).then_some(Self {
            title: format!("import {} conversation", source.label()),
            items: sessions.into_iter().map(Item::External).collect(),
            selected: 0,
        })
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> PickResult {
        match key.code {
            KeyCode::Esc => PickResult::Cancelled,
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                PickResult::Pending
            }
            KeyCode::Down => {
                self.selected = (self.selected + 1).min(self.items.len() - 1);
                PickResult::Pending
            }
            KeyCode::Enter => match &self.items[self.selected] {
                Item::Current(session) => PickResult::Current(session.id.clone()),
                Item::Source(source) => PickResult::Source(*source),
                Item::External(session) => PickResult::External(session.clone()),
            },
            _ => PickResult::Pending,
        }
    }

    pub fn render(&self) -> Vec<Line<'static>> {
        let mut lines = vec![Line::styled(self.title.clone(), theme::bold().fg(theme::ACCENT))];
        for (index, item) in self.items.iter().enumerate().take(8) {
            let marker = if index == self.selected { "▸ " } else { "  " };
            let style = if index == self.selected { theme::accent() } else { theme::dim() };
            let text = match item {
                Item::Current(session) => format!(
                    "{} · {}",
                    session.id,
                    truncate(&session.last_user_preview, 54)
                ),
                Item::Source(source) => format!("import from {}…", source.label()),
                Item::External(session) => format!(
                    "› {}  [{}]",
                    truncate(&session.last_user_preview, 68),
                    short_id(&session.id),
                ),
            };
            lines.push(Line::styled(format!("  {marker}{text}"), style));
        }
        lines.push(Line::styled("  ↑↓ choose · enter select · esc cancel", theme::dim()));
        lines
    }

    pub fn height(&self) -> u16 {
        (self.items.len().min(8) + 2) as u16
    }
}

/// Codex stores sessions in verbose `rollout-…` filenames.  In a picker the
/// conversation is the useful identifier; retain only a small stable suffix
/// for disambiguation instead of letting the filename hide the preview.
fn short_id(id: &str) -> String {
    let tail: String = id.chars().rev().take(8).collect::<Vec<_>>().into_iter().rev().collect();
    if id.chars().count() > 8 {
        format!("…{tail}")
    } else {
        tail
    }
}

fn truncate(text: &str, limit: usize) -> String {
    let mut chars = text.chars();
    let prefix: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}
