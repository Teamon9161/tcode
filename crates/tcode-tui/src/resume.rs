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

const VISIBLE_ROWS: usize = 8;

pub enum PickResult {
    Pending,
    Cancelled,
    Current(String),
    Source(ExternalSource),
    External(ExternalSessionInfo),
}

impl Picker {
    pub fn new(sessions: Vec<SessionInfo>) -> Self {
        // Resuming one's own latest session is the common case; keep it on
        // the default selection and park the import entry points below.
        let mut items: Vec<Item> = sessions.into_iter().map(Item::Current).collect();
        items.push(Item::Source(ExternalSource::Codex));
        items.push(Item::Source(ExternalSource::Claude));
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
        let mut lines = vec![Line::styled(
            self.title.clone(),
            theme::bold().fg(theme::ACCENT),
        )];
        // A window that follows the selection: the fixed `take(8)` would make
        // items beyond the eighth unreachable even though ↓ selects them.
        let start = self.selected.saturating_sub(VISIBLE_ROWS - 1);
        for (index, item) in self.items.iter().enumerate().skip(start).take(VISIBLE_ROWS) {
            let selected = index == self.selected;
            let marker = if selected { "▸ " } else { "  " };
            let style = if selected {
                theme::accent()
            } else {
                theme::dim()
            };
            let text = match item {
                Item::Current(session) => format!(
                    "{}{} · {}",
                    session.id,
                    age_suffix(session.modified),
                    truncate(&session.last_user_preview, 54)
                ),
                Item::Source(source) => format!("⇣ import from {}…", source.label()),
                Item::External(session) => format!(
                    "› {}  [{}]{}",
                    truncate(&session.last_user_preview, 60),
                    short_id(&session.id),
                    age_suffix(session.modified),
                ),
            };
            lines.push(Line::styled(format!("  {marker}{text}"), style));
        }
        let position = if self.items.len() > VISIBLE_ROWS {
            format!("{}/{} · ", self.selected + 1, self.items.len())
        } else {
            String::new()
        };
        lines.push(Line::styled(
            format!("  {position}↑↓ choose · enter select · esc cancel"),
            theme::dim(),
        ));
        lines
    }
}

/// Codex stores sessions in verbose `rollout-…` filenames.  In a picker the
/// conversation is the useful identifier; retain only a small stable suffix
/// for disambiguation instead of letting the filename hide the preview.
fn short_id(id: &str) -> String {
    let tail: String = id
        .chars()
        .rev()
        .take(8)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if id.chars().count() > 8 {
        format!("…{tail}")
    } else {
        tail
    }
}

/// Compact "how long ago" marker for picker rows, e.g. " · 3h ago".
fn age_suffix(modified: Option<std::time::SystemTime>) -> String {
    let Some(modified) = modified else {
        return String::new();
    };
    let Ok(elapsed) = std::time::SystemTime::now().duration_since(modified) else {
        return String::new();
    };
    let secs = elapsed.as_secs();
    let age = match secs {
        0..=59 => "now".to_string(),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86_399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    };
    format!(" · {age}")
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
