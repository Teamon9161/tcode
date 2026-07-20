//! `/provider` in-session: the same `setup::Setup` the first-run wizard
//! drives, painted as an overlay in the app's own theme.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::setup::{Mark, Row, Tone, View};
use crate::theme;

/// One rendered `View`, in the same shape every other picker returns.
pub fn render(view: &View) -> Vec<Line<'static>> {
    let mut lines = vec![Line::styled(view.title.clone(), theme::bold())];
    for row in &view.rows {
        lines.push(row_line(row, view.caret));
    }
    lines.push(Line::styled(format!("  {}", view.hint), theme::dim()));
    lines
}

fn row_line(row: &Row, caret: bool) -> Line<'static> {
    let mut spans = vec![Span::styled(
        if row.active { " ▸ " } else { "   " }.to_string(),
        theme::accent(),
    )];
    match row.mark {
        Mark::Checked => spans.push(Span::styled("[x] ".to_string(), theme::accent())),
        Mark::Unchecked => spans.push(Span::raw("[ ] ".to_string())),
        Mark::None => {}
    }
    spans.push(Span::styled(
        row.label.clone(),
        match (row.active, row.tone) {
            // A menu row highlights on the cursor; a placeholder stays dim
            // even though the field it names is the active one.
            (_, Tone::Dim) if row.mark == Mark::None && !row.active => theme::dim(),
            (_, Tone::Dim) if row.mark == Mark::None && caret => theme::dim(),
            (true, _) if row.mark == Mark::None => theme::accent(),
            _ => Style::default(),
        },
    ));
    if caret {
        spans.push(Span::raw("▏".to_string()));
    }
    if !row.status.is_empty() {
        spans.push(Span::styled(
            format!("  {}", row.status),
            match row.tone {
                Tone::Ok => theme::ok(),
                Tone::Dim => theme::dim(),
            },
        ));
    }
    Line::from(spans)
}
