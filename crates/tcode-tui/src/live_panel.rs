//! Persistent agent tree above the editor. It unifies `update_progress` with
//! sub-agent status and navigation, and is visible from both the main
//! conversation and task traces. Everything here is pure line construction so
//! interaction state and presentation stay unit-testable.

use std::path::Path;
use std::time::Instant;

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use tcode_core::{AgentEvent, TaskRunStatus, Usage};

use crate::render::RenderRegistry;
use crate::theme;
use crate::transcript::wrap_lines;

/// Progress rows stay small and predictable; long phase lists render a focused
/// window instead of stealing scroll focus from the transcript.
const VISIBLE_PHASES: usize = 5;
const ACTIVITY_CHARS: usize = 56;
const STEP_HISTORY: usize = 3;

pub struct ProgressPhase {
    pub phase: String,
    pub status: String,
}

impl ProgressPhase {
    pub fn is_completed(&self) -> bool {
        self.status == "completed"
    }
}

/// An actionable tree row. The app maps it to the appropriate single- or
/// double-click behavior, while this module only decides which rendered row
/// belongs to which target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PanelTarget {
    Main,
    Task(String),
}

/// The root agent's compact live status.
pub struct MainAgent<'a> {
    pub running: bool,
    pub activity: &'a str,
    pub elapsed_secs: u64,
    pub output_tokens: usize,
}

/// One `task` sub-agent run as the UI tracks it, fed by `TaskRun*` events.
pub struct UiTaskRun {
    pub id: String,
    pub parent_call: String,
    pub kind: String,
    pub model: String,
    pub prompt: String,
    /// Parent-authored one-line objective. It appears in both the tree and
    /// the parent task card while the run is active.
    pub summary: String,
    pub status: TaskRunStatus,
    pub tools: usize,
    pub usage: Usage,
    pub started: Instant,
    /// What the run is doing right now, one short line.
    pub activity: String,
    /// Latest meaningful tool headers, rendered on the parent task card rather
    /// than expanded in the tree.
    pub steps: Vec<String>,
    /// Ordered, coalesced sub-agent events. These are view-only data for a
    /// live trace; durable replay uses the trace JSONL instead.
    pub events: Vec<AgentEvent>,
    /// The main transcript's parent task-call header, when it has a simple
    /// header block to attach the task trace link to.
    pub block: Option<usize>,
}

impl UiTaskRun {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: String,
        parent_call: String,
        kind: String,
        model: String,
        prompt: String,
        summary: String,
        block: Option<usize>,
    ) -> Self {
        Self {
            id,
            parent_call,
            kind,
            model,
            prompt,
            summary,
            status: TaskRunStatus::Running,
            tools: 0,
            usage: Usage::default(),
            started: Instant::now(),
            activity: "starting…".into(),
            steps: Vec::new(),
            events: Vec::new(),
            block,
        }
    }

    /// Advance the activity line from one forwarded sub-agent event. Tool
    /// summaries go through the same renderer entry point as transcript
    /// headers, so the tree never disagrees with the trace about a call name.
    pub fn note_event(&mut self, ev: &AgentEvent, renderers: &RenderRegistry, cwd: &Path) {
        match ev {
            AgentEvent::ToolBatchStart { label, calls } => {
                self.tools += calls.len();
                self.activity = cap_activity(label);
                self.note_step(self.activity.clone());
            }
            AgentEvent::ToolStart { name, input, .. } => {
                self.tools += 1;
                let summary = renderers.get(name).header(name, input, Some(cwd));
                self.activity = cap_activity(&summary);
                self.note_step(self.activity.clone());
            }
            AgentEvent::TextDelta(_) => self.activity = "writing…".into(),
            AgentEvent::ThinkingDelta(_) => self.activity = "thinking…".into(),
            AgentEvent::Retrying { attempt, max, .. } => {
                self.activity = format!("retrying ({attempt}/{max})");
            }
            AgentEvent::Usage(usage) | AgentEvent::DelegatedUsage(usage) => {
                self.usage = add_usage(self.usage, *usage);
            }
            _ => {}
        }
    }

    fn note_step(&mut self, step: String) {
        if self.steps.last() == Some(&step) {
            return;
        }
        self.steps.push(step);
        if self.steps.len() > STEP_HISTORY {
            self.steps.remove(0);
        }
    }
}

/// The tree lines plus, per line, its action target. `None` means an inert
/// phase/detail/filler row. Empty only when there is neither progress nor a
/// task run to show. `current` is the view the user is looking at; its row
/// carries a `▸` gutter marker so the tree always shows where you are.
pub fn lines(
    progress: &[ProgressPhase],
    runs: &[&UiTaskRun],
    main: MainAgent<'_>,
    width: u16,
    hovered: Option<&PanelTarget>,
    current: &PanelTarget,
) -> (Vec<Line<'static>>, Vec<Option<PanelTarget>>) {
    let mut lines = Vec::new();
    let mut targets = Vec::new();

    if !runs.is_empty() {
        let main_hovered = hovered == Some(&PanelTarget::Main);
        let mut main_spans = vec![
            gutter(current == &PanelTarget::Main),
            Span::styled("● ", row_style(theme::accent(), main_hovered)),
            Span::styled("main", row_style(theme::bold(), main_hovered)),
        ];
        if main.running {
            main_spans.push(Span::styled(
                format!(" · {}", main.activity),
                row_style(Style::default(), main_hovered),
            ));
            main_spans.push(Span::styled(
                format!(
                    " · {} · ↓ {} tok",
                    fmt_elapsed(main.elapsed_secs),
                    token_count(main.output_tokens as u64)
                ),
                row_style(theme::dim(), main_hovered),
            ));
        }
        lines.push(Line::from(main_spans));
        targets.push(Some(PanelTarget::Main));

        for (i, run) in runs.iter().enumerate() {
            let target = PanelTarget::Task(run.id.clone());
            let highlighted = hovered == Some(&target);
            let connector = if i + 1 == runs.len() { "└ " } else { "├ " };
            let task_lines = task_lines(run, width, highlighted, connector, current == &target);
            targets.extend(std::iter::repeat_n(Some(target), task_lines.len()));
            lines.extend(task_lines);
        }
    }

    let phases = progress_lines(progress);
    if !lines.is_empty() && !phases.is_empty() {
        lines.push(Line::default());
        targets.push(None);
    }
    targets.extend(std::iter::repeat_n(None, phases.len()));
    lines.extend(phases);
    (lines, targets)
}

fn task_lines(
    run: &UiTaskRun,
    width: u16,
    highlighted: bool,
    connector: &str,
    current: bool,
) -> Vec<Line<'static>> {
    let marker = match run.status {
        TaskRunStatus::Running => "●",
        TaskRunStatus::Done => "✓",
        TaskRunStatus::Failed => "!",
        TaskRunStatus::Cancelled => "⨯",
        TaskRunStatus::Interrupted => "⊘",
    };
    let spans = vec![
        gutter(current),
        Span::styled(connector.to_string(), row_style(theme::dim(), highlighted)),
        Span::styled(
            format!("{marker} "),
            row_style(status_style(run.status), highlighted),
        ),
        Span::styled(
            title_case(&run.kind),
            row_style(Style::default(), highlighted),
        ),
        Span::styled(" · ", row_style(theme::dim(), highlighted)),
        Span::styled(
            run.summary.clone(),
            row_style(Style::default(), highlighted),
        ),
        Span::styled(
            format!(
                " · {} · ↓ {} tok",
                fmt_elapsed(run.started.elapsed().as_secs()),
                token_count(run.usage.output_tokens)
            ),
            row_style(theme::dim(), highlighted),
        ),
    ];
    wrap_lines(
        vec![Line::from(spans)],
        usize::from(width.saturating_sub(2).max(1)),
    )
}

/// Two-column gutter: `▸` on the row whose view is on screen, blank elsewhere.
/// The same glyph the view picker uses, so "where am I" reads identically.
fn gutter(current: bool) -> Span<'static> {
    if current {
        Span::styled("▸ ", theme::accent())
    } else {
        Span::raw("  ")
    }
}

fn status_style(status: TaskRunStatus) -> Style {
    match status {
        TaskRunStatus::Running => theme::dim(),
        TaskRunStatus::Done => theme::ok(),
        TaskRunStatus::Failed | TaskRunStatus::Cancelled => theme::warn(),
        TaskRunStatus::Interrupted => theme::dim(),
    }
}

fn row_style(base: Style, highlighted: bool) -> Style {
    if highlighted {
        theme::hover_highlight()
            .fg(theme::ACCENT)
            .add_modifier(ratatui::style::Modifier::BOLD)
    } else {
        base
    }
}

fn progress_lines(progress: &[ProgressPhase]) -> Vec<Line<'static>> {
    if progress.is_empty() {
        return Vec::new();
    }
    let complete = progress.iter().filter(|item| item.is_completed()).count();
    let (start, end) = visible_phase_range(progress, VISIBLE_PHASES);
    let hidden_before = start;
    let hidden_after = progress.len().saturating_sub(end);
    let mut lines = vec![Line::from(vec![
        Span::styled("  progress ", theme::bold().fg(theme::ACCENT)),
        Span::styled(
            format!("{complete}/{} phases complete", progress.len()),
            theme::dim(),
        ),
        if hidden_before + hidden_after > 0 {
            Span::styled(format!(" · showing {}-{}", start + 1, end), theme::dim())
        } else {
            Span::raw("")
        },
    ])];
    if hidden_before > 0 {
        lines.push(Line::styled(
            format!("    … {hidden_before} earlier"),
            theme::dim(),
        ));
    }
    lines.extend(progress[start..end].iter().map(|item| {
        let (marker, style) = match item.status.as_str() {
            "completed" => ("✓ ", Style::default().fg(theme::OK)),
            "in_progress" => ("● ", theme::accent()),
            _ => ("○ ", theme::dim()),
        };
        Line::from(vec![
            Span::styled(format!("    {marker}"), style),
            Span::styled(
                item.phase.clone(),
                if item.status == "completed" {
                    Style::default()
                        .fg(theme::OK)
                        .add_modifier(ratatui::style::Modifier::CROSSED_OUT)
                } else if item.status == "pending" {
                    theme::dim()
                } else {
                    Style::default()
                },
            ),
        ])
    }));
    if hidden_after > 0 {
        lines.push(Line::styled(
            format!("    … {hidden_after} later"),
            theme::dim(),
        ));
    }
    lines
}

fn visible_phase_range(phases: &[ProgressPhase], max_visible: usize) -> (usize, usize) {
    if phases.len() <= max_visible || max_visible == 0 {
        return (0, phases.len());
    }
    let focus = phases
        .iter()
        .position(|item| item.status == "in_progress")
        .or_else(|| phases.iter().position(|item| item.status == "pending"))
        .unwrap_or(phases.len() - 1);
    let mut start = focus.saturating_sub(max_visible / 2);
    start = start.min(phases.len().saturating_sub(max_visible));
    (start, start + max_visible)
}

/// First line only, capped: a long shell command or path must not reflow the
/// persistent bottom tree.
fn cap_activity(summary: &str) -> String {
    let first = summary.lines().next().unwrap_or_default().trim();
    if first.chars().count() <= ACTIVITY_CHARS {
        return first.to_string();
    }
    let capped: String = first.chars().take(ACTIVITY_CHARS - 1).collect();
    format!("{capped}…")
}

fn fmt_elapsed(secs: u64) -> String {
    if secs < 100 {
        format!("{secs}s")
    } else {
        format!("{}m{:02}s", secs / 60, secs % 60)
    }
}

fn token_count(tokens: u64) -> String {
    if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

fn add_usage(mut total: Usage, usage: Usage) -> Usage {
    total.input_tokens += usage.input_tokens;
    total.output_tokens += usage.output_tokens;
    total.cache_read_tokens += usage.cache_read_tokens;
    total.cache_write_tokens += usage.cache_write_tokens;
    total
}

fn title_case(name: &str) -> String {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn phases(statuses: &[&str]) -> Vec<ProgressPhase> {
        statuses
            .iter()
            .enumerate()
            .map(|(i, status)| ProgressPhase {
                phase: format!("Phase {i}"),
                status: status.to_string(),
            })
            .collect()
    }

    fn run(id: &str) -> UiTaskRun {
        UiTaskRun::new(
            id.into(),
            "call".into(),
            "explore".into(),
            "test".into(),
            "find it".into(),
            "inspect the implementation".into(),
            None,
        )
    }

    fn text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn main() -> MainAgent<'static> {
        MainAgent {
            running: true,
            activity: "thinking…",
            elapsed_secs: 15,
            output_tokens: 2_200,
        }
    }

    #[test]
    fn visible_phase_range_focuses_in_progress_item() {
        let mut items = phases(&["pending"; 8]);
        items[5].status = "in_progress".into();
        assert_eq!(visible_phase_range(&items, 5), (3, 8));
    }

    #[test]
    fn tree_is_absent_when_no_sub_agent_is_running() {
        let (lines, targets) = lines(&[], &[], main(), 80, None, &PanelTarget::Main);
        assert!(lines.is_empty());
        assert!(targets.is_empty());
    }

    #[test]
    fn tree_keeps_only_status_and_navigation_rows() {
        let mut active = run("t1");
        active.note_event(
            &AgentEvent::ToolStart {
                call_id: "c1".into(),
                name: "grep".into(),
                summary: String::new(),
                input: serde_json::json!({"pattern": "needle"}),
            },
            &RenderRegistry::from_tools(&tcode_tools::builtin_tools(Path::new("."))),
            Path::new("."),
        );
        let (lines, targets) = lines(
            &phases(&["in_progress"]),
            &[&active],
            main(),
            80,
            None,
            &PanelTarget::Main,
        );
        assert_eq!(
            lines.len(),
            5,
            "root, task, separator, and progress rows only"
        );
        assert_eq!(targets[1], Some(PanelTarget::Task("t1".into())));
        assert!(text(&lines[1]).contains("inspect the implementation"));
        assert!(
            !text(&lines[1]).contains("needle"),
            "current activity belongs to the visible parent task status"
        );
    }

    #[test]
    fn long_summary_wraps_without_losing_its_task_target() {
        let mut active = run("t1");
        active.summary = "用中文完整概括这个足够长的委派任务，不能被树裁掉".into();
        let (lines, targets) = lines(&[], &[&active], main(), 20, None, &PanelTarget::Main);
        assert!(lines.len() > 2, "the task summary wraps on a narrow panel");
        let summary = lines
            .iter()
            .skip(1)
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(summary.contains(&active.summary));
        assert!(targets[1..]
            .iter()
            .all(|target| target == &Some(PanelTarget::Task("t1".into()))));
    }

    #[test]
    fn task_tree_keeps_navigation_static_and_places_summary_before_stats() {
        let active = run("t1");
        let (lines, _) = lines(&[], &[&active], main(), 120, None, &PanelTarget::Main);
        let row = text(&lines[1]);
        assert!(row.starts_with("  └ ● Explore · inspect the implementation"));
        assert!(
            row.find("inspect the implementation").unwrap() < row.find(" · 0s").unwrap(),
            "the summary matches main's activity-before-stats order"
        );
    }

    #[test]
    fn current_view_row_carries_the_gutter_marker() {
        let first = run("t1");
        let second = run("t2");
        let current = PanelTarget::Task("t1".into());
        let (in_task, _) = lines(&[], &[&first, &second], main(), 120, None, &current);
        assert!(text(&in_task[0]).starts_with("  ● main"));
        assert!(text(&in_task[1]).starts_with("▸ ├ ● Explore"));
        assert!(text(&in_task[2]).starts_with("  └ ● Explore"));

        let (in_main, _) = lines(
            &[],
            &[&first, &second],
            main(),
            120,
            None,
            &PanelTarget::Main,
        );
        assert!(text(&in_main[0]).starts_with("▸ ● main"));
        assert!(text(&in_main[1]).starts_with("  ├ ● Explore"));
    }

    #[test]
    fn elapsed_switches_to_minutes_past_the_readable_seconds_range() {
        assert_eq!(fmt_elapsed(99), "99s");
        assert_eq!(fmt_elapsed(100), "1m40s");
        assert_eq!(fmt_elapsed(134), "2m14s");
    }

    #[test]
    fn activity_tracks_usage_and_the_latest_run_event() {
        let registry = RenderRegistry::from_tools(&tcode_tools::builtin_tools(Path::new(".")));
        let mut task = run("t1");
        task.note_event(
            &AgentEvent::ToolBatchStart {
                label: "Read 3 files".into(),
                calls: vec![("c1".into(), "read".into(), serde_json::json!({}))],
            },
            &registry,
            Path::new("."),
        );
        task.note_event(
            &AgentEvent::Usage(Usage {
                input_tokens: 2_000,
                ..Default::default()
            }),
            &registry,
            Path::new("."),
        );
        assert_eq!(task.activity, "Read 3 files");
        assert_eq!(task.usage.input_tokens, 2_000);
    }
}
