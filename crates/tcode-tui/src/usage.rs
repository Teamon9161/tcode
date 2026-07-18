//! Token accounting for the status row and the turn receipt.
//!
//! The two figures here are different quantities and must not be mixed (see
//! `plan.md`): `context` is what a single request's full prompt occupies in
//! the model window — cached prefix included, because cached input still
//! takes up room. `turn` is the receipt for one turn, and reports the
//! *uncached* input actually paid for plus a cache-hit share. Summing
//! `total_input()` across a multi-step turn would recount the cached prefix
//! once per request.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ratatui::text::{Line, Span};
use tcode_core::{RateLimits, Usage};

use crate::theme;

/// Everything the UI knows about what the running turn has cost.
///
/// These fields move together — a turn reset touches all of them, a usage
/// event replaces several at once — so they live behind one type rather than
/// as ten loose `App` fields that any method could half-update.
pub struct TurnMeter {
    /// Streamed output tokens, for the live `↓ ~N tok` readout.
    pub out_tokens: usize,
    /// Usage reported by sub-agents. Tracked apart from `turn` so it can be
    /// re-added after the session's authoritative tally replaces the estimate.
    delegated: Usage,
    pub turn: Usage,
    pub rate_limits: Option<RateLimits>,
    /// Best available estimate of the conversation currently occupying the
    /// model window. A completed provider usage event replaces estimates;
    /// streamed output and tool results keep it moving between those events.
    pub context_tokens: u64,
    /// Start of the current model request, so a retry can discard the
    /// speculative streamed-token estimate from the failed attempt.
    step_start: u64,
    /// Session JSONL stores messages, not provider token counters. A resumed
    /// conversation starts from a local estimate until its next response
    /// supplies an authoritative usage event.
    pub context_estimated: bool,
    /// Time the turn was deliberately paused for a human decision. Receipts
    /// report active execution time, not time spent deciding.
    wait_started: Option<Instant>,
    wait_total: Duration,
    /// Cache-read share of the previous turn; the regression sentinel compares
    /// against it so cache decay is visible immediately.
    prev_cache_ratio: Option<f64>,
}

impl TurnMeter {
    pub fn new(context_tokens: u64, context_estimated: bool) -> Self {
        Self {
            out_tokens: 0,
            delegated: Usage::default(),
            turn: Usage::default(),
            rate_limits: None,
            context_tokens,
            step_start: context_tokens,
            context_estimated,
            wait_started: None,
            wait_total: Duration::ZERO,
            prev_cache_ratio: None,
        }
    }

    /// Zero the per-turn tallies. Context figures survive: they describe the
    /// conversation, not the turn.
    pub fn start_turn(&mut self) {
        self.turn = Usage::default();
        self.delegated = Usage::default();
        self.wait_started = None;
        self.wait_total = Duration::ZERO;
        self.out_tokens = 0;
    }

    /// Compaction legitimately rewrites the prefix, so the next turn's low
    /// cache share is expected rather than a regression to warn about.
    pub fn forget_cache_baseline(&mut self) {
        self.prev_cache_ratio = None;
    }

    /// Streamed text, thinking or tool-input deltas: they are in the window
    /// before any usage event confirms them.
    pub fn on_streamed_tokens(&mut self, tokens: usize) {
        self.out_tokens += tokens;
        self.context_tokens = self.context_tokens.saturating_add(tokens as u64);
    }

    pub fn add_context(&mut self, tokens: u64) {
        self.context_tokens = self.context_tokens.saturating_add(tokens);
    }

    /// An authoritative provider tally: it replaces the speculative estimate
    /// rather than adding to it.
    pub fn on_usage(&mut self, u: Usage) {
        self.turn.input_tokens += u.input_tokens;
        self.turn.output_tokens += u.output_tokens;
        self.turn.cache_read_tokens += u.cache_read_tokens;
        self.turn.cache_write_tokens += u.cache_write_tokens;
        self.context_tokens = u.total_input().saturating_add(u.output_tokens);
        self.step_start = self.context_tokens;
        self.context_estimated = false;
    }

    pub fn on_delegated_usage(&mut self, u: Usage) {
        self.delegated = add_usage(self.delegated, u);
        self.turn = add_usage(self.turn, u);
        self.out_tokens = self.out_tokens.saturating_add(u.output_tokens as usize);
    }

    /// A new request begins: the current estimate becomes the point a retry
    /// rewinds to.
    pub fn begin_step(&mut self) {
        self.step_start = self.context_tokens;
    }

    /// A failed attempt streamed tokens that never became part of a prompt.
    pub fn rewind_step(&mut self) {
        self.context_tokens = self.step_start;
    }

    pub fn set_context(&mut self, tokens: u64, estimated: bool) {
        self.context_tokens = tokens;
        self.step_start = tokens;
        self.context_estimated = estimated;
    }

    pub fn pause_for_user(&mut self) {
        if self.wait_started.is_none() {
            self.wait_started = Some(Instant::now());
        }
    }

    pub fn resume_from_user(&mut self) {
        if let Some(started) = self.wait_started.take() {
            self.wait_total += started.elapsed();
        }
    }

    /// Wall time minus every stretch spent waiting on a human decision.
    pub fn active_elapsed(&self, started: Instant) -> f32 {
        started
            .elapsed()
            .saturating_sub(self.wait_total)
            .saturating_sub(
                self.wait_started
                    .map(|wait| wait.elapsed())
                    .unwrap_or_default(),
            )
            .as_secs_f32()
    }

    /// Adopt the session's authoritative per-turn tally, which also covers
    /// compaction (that streams no Usage events to the UI).
    pub fn finish_turn(&mut self, session_usage: Usage) {
        self.turn = add_usage(session_usage, self.delegated);
    }

    /// Cache regression sentinel: an append-only ledger should keep the hit
    /// share high, so a sharp drop means something rewrote the prefix and
    /// deserves attention now, not on the monthly bill. Returns the previous
    /// and current share when the drop is worth reporting.
    pub fn take_cache_regression(&mut self) -> Option<(f64, f64)> {
        if self.turn.total_input() == 0 {
            return None;
        }
        let ratio = self.turn.cache_read_tokens as f64 / self.turn.total_input() as f64;
        let regression = self
            .prev_cache_ratio
            .filter(|prev| *prev >= 0.5 && ratio < prev * 0.5)
            .map(|prev| (prev, ratio));
        self.prev_cache_ratio = Some(ratio);
        regression
    }
}

/// One compact row below the editor. The meter intentionally reports the
/// current conversation, rather than cumulative billable tokens: cached input
/// still occupies context and must count toward the model window.
pub fn context_progress_line(
    used: u64,
    window: u64,
    terminal_width: u16,
    estimated: bool,
) -> Line<'static> {
    let window = window.max(1);
    let pct = used.saturating_mul(100).saturating_div(window).min(100);
    let estimate_mark = if estimated { "≈" } else { "" };
    let color = if pct >= 95 {
        theme::ERROR
    } else if pct >= 85 {
        theme::WARN
    } else {
        theme::OK
    };
    let (label, bar_width) = if terminal_width < 42 {
        ("  ctx ", 8usize)
    } else {
        ("  context ", 12usize)
    };
    let filled = if used == 0 {
        0
    } else {
        ((bar_width as u64 * pct).div_ceil(100) as usize).min(bar_width)
    };
    let mut spans = vec![Span::styled(label, theme::dim())];
    spans.extend(slim_bar(filled, bar_width, color));
    spans.push(Span::styled(
        format!(" {estimate_mark}{pct}%"),
        ratatui::style::Style::default().fg(color),
    ));
    if terminal_width >= 42 {
        spans.push(Span::styled(
            format!(" · {}", token_count(window)),
            theme::dim(),
        ));
    }
    Line::from(spans)
}

/// The slim gauge shared by the context meter and the rate-limit row:
/// a heavy coloured run over a dim dashed track.
fn slim_bar(filled: usize, width: usize, color: ratatui::style::Color) -> [Span<'static>; 2] {
    [
        Span::styled(
            "━".repeat(filled),
            ratatui::style::Style::default().fg(color),
        ),
        Span::styled("╌".repeat(width.saturating_sub(filled)), theme::dim()),
    ]
}

pub fn token_count(tokens: u64) -> String {
    if tokens < 1_000 {
        tokens.to_string()
    } else if tokens < 10_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        format!("{}k", tokens.div_ceil(1_000))
    }
}

pub fn rate_limit_line(limits: RateLimits) -> Line<'static> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    rate_limit_line_at(limits, now)
}

fn rate_limit_line_at(limits: RateLimits, now: u64) -> Line<'static> {
    let primary_used = limits.primary.used_percent.clamp(0.0, 100.0);
    let filled = ((primary_used / 100.0) * 12.0).round() as usize;
    let color = usage_color(primary_used);
    let mut spans = vec![Span::styled("  Codex 5h ", theme::dim())];
    spans.extend(slim_bar(filled, 12, color));
    spans.push(Span::styled(
        format!(" {primary_used:.0}%"),
        ratatui::style::Style::default().fg(color),
    ));
    append_reset_countdown(&mut spans, limits.primary.resets_at, now);

    if let Some(weekly) = limits.secondary.filter(|limit| limit.used_percent >= 80.0) {
        let weekly_used = weekly.used_percent.clamp(0.0, 100.0);
        let weekly_filled = ((weekly_used / 100.0) * 12.0).round() as usize;
        let weekly_color = usage_color(weekly_used);
        spans.push(Span::styled(" · week ", theme::dim()));
        spans.extend(slim_bar(weekly_filled, 12, weekly_color));
        spans.push(Span::styled(
            format!(" {weekly_used:.0}%"),
            ratatui::style::Style::default().fg(weekly_color),
        ));
        append_reset_countdown(&mut spans, weekly.resets_at, now);
    }
    Line::from(spans)
}

fn usage_color(used_percent: f64) -> ratatui::style::Color {
    if used_percent >= 90.0 {
        theme::ERROR
    } else if used_percent >= 75.0 {
        theme::WARN
    } else {
        theme::OK
    }
}

fn append_reset_countdown(spans: &mut Vec<Span<'static>>, resets_at: u64, now: u64) {
    let Some(remaining) = resets_at.checked_sub(now).filter(|&seconds| seconds > 0) else {
        return;
    };
    spans.push(Span::styled(
        format!(" ↻ {}", brief_duration(remaining)),
        theme::dim(),
    ));
}

/// Compact countdown for the status line: enough precision for a human to
/// decide whether to wait, without turning the meter into a timestamp.
fn brief_duration(seconds: u64) -> String {
    if seconds < 60 {
        "<1m".into()
    } else if seconds < 3_600 {
        format!("{}m", seconds.div_ceil(60))
    } else if seconds < 86_400 {
        format!("{}h{}m", seconds / 3_600, (seconds % 3_600).div_ceil(60))
    } else {
        format!("{}d", seconds.div_ceil(86_400))
    }
}

pub fn add_usage(left: Usage, right: Usage) -> Usage {
    Usage {
        input_tokens: left.input_tokens.saturating_add(right.input_tokens),
        output_tokens: left.output_tokens.saturating_add(right.output_tokens),
        cache_read_tokens: left
            .cache_read_tokens
            .saturating_add(right.cache_read_tokens),
        cache_write_tokens: left
            .cache_write_tokens
            .saturating_add(right.cache_write_tokens),
    }
}

pub fn turn_summary_line(elapsed: f32, usage: Usage) -> Line<'static> {
    let cache_pct = if usage.total_input() > 0 {
        (usage.cache_read_tokens as f64 / usage.total_input() as f64 * 100.0).round()
    } else {
        0.0
    };
    let cache_style = if cache_pct > 0.0 {
        theme::accent()
    } else {
        theme::dim()
    };
    Line::from(vec![
        Span::styled("✓ completed ", theme::ok()),
        Span::styled(format!("{elapsed:.1}s"), theme::bold()),
        Span::styled(" · ↑", theme::dim()),
        // Uncached input only: the tokens this turn actually paid full price
        // for. Summing total_input() across a multi-step turn would recount
        // the cached prefix on every request; the cache figure below shows
        // how much of the full prompt was reused. This is a turn receipt, not
        // the window-occupancy figure the context meter reports.
        Span::styled(token_count(usage.input_tokens), theme::accent()),
        Span::styled(" · ↓", theme::dim()),
        Span::styled(
            token_count(usage.output_tokens),
            ratatui::style::Style::default().fg(theme::OK),
        ),
        Span::styled(" · cache ", theme::dim()),
        Span::styled(format!("{cache_pct:.0}%"), cache_style),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_usage_meters_use_the_tool_green() {
        let context = context_progress_line(20_000, 200_000, 80, false);
        assert!(context
            .spans
            .iter()
            .any(|span| span.style.fg == Some(theme::OK)));

        let limits = tcode_core::RateLimits {
            primary: tcode_core::RateLimit {
                used_percent: 30.0,
                window_minutes: 300,
                resets_at: 14_800,
            },
            secondary: None,
        };
        let rate_limit = rate_limit_line_at(limits, 10_000);
        assert!(rate_limit
            .spans
            .iter()
            .any(|span| span.style.fg == Some(theme::OK)));
    }

    #[test]
    fn context_meter_reports_percent_and_warning_color() {
        let line = context_progress_line(170_000, 200_000, 80, false);
        let text = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(text.contains("context"));
        assert!(text.contains("85% · 200k"));
        assert!(!text.contains("170k/200k"));
        assert!(line
            .spans
            .iter()
            .any(|span| span.style.fg == Some(theme::WARN)));
    }

    #[test]
    fn codex_rate_limit_line_shows_used_percent_and_reset_countdowns() {
        let limits = tcode_core::RateLimits {
            primary: tcode_core::RateLimit {
                used_percent: 30.0,
                window_minutes: 300,
                resets_at: 14_800,
            },
            secondary: Some(tcode_core::RateLimit {
                used_percent: 80.0,
                window_minutes: 10_080,
                resets_at: 269_200,
            }),
        };
        let text = rate_limit_line_at(limits, 10_000)
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(text.contains("Codex 5h"));
        assert!(text.contains(" 30% ↻ 1h20m"));
        assert!(text.contains("week "));
        assert!(text.contains(" 80% ↻ 3d"));
    }

    #[test]
    fn brief_duration_stays_compact_at_unit_boundaries() {
        assert_eq!(brief_duration(59), "<1m");
        assert_eq!(brief_duration(60), "1m");
        assert_eq!(brief_duration(3_601), "1h1m");
        assert_eq!(brief_duration(86_401), "2d");
    }

    #[test]
    fn codex_rate_limit_line_hides_week_below_80_percent() {
        let limits = tcode_core::RateLimits {
            primary: tcode_core::RateLimit {
                used_percent: 30.0,
                window_minutes: 300,
                resets_at: 0,
            },
            secondary: Some(tcode_core::RateLimit {
                used_percent: 79.9,
                window_minutes: 10_080,
                resets_at: 0,
            }),
        };
        let text = rate_limit_line(limits)
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(!text.contains("week"));
    }

    #[test]
    fn turn_summary_is_a_scannable_receipt() {
        let line = turn_summary_line(
            2.5,
            Usage {
                input_tokens: 1_178,
                output_tokens: 23,
                ..Usage::default()
            },
        );
        let text = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(text, "✓ completed 2.5s · ↑1.2k · ↓23 · cache 0%");
    }

    #[test]
    fn delegated_usage_is_added_without_losing_cache_fields() {
        let total = add_usage(
            Usage {
                input_tokens: 10,
                output_tokens: 2,
                cache_read_tokens: 3,
                cache_write_tokens: 4,
            },
            Usage {
                input_tokens: 20,
                output_tokens: 5,
                cache_read_tokens: 6,
                cache_write_tokens: 7,
            },
        );
        assert_eq!(total.input_tokens, 30);
        assert_eq!(total.output_tokens, 7);
        assert_eq!(total.cache_read_tokens, 9);
        assert_eq!(total.cache_write_tokens, 11);
    }

    /// A retry must not leave the failed attempt's speculative streamed
    /// tokens counted against the window.
    #[test]
    fn a_rewound_step_discards_speculative_streamed_tokens() {
        let mut meter = TurnMeter::new(1_000, false);
        meter.begin_step();
        meter.on_streamed_tokens(250);
        assert_eq!(meter.context_tokens, 1_250);
        meter.rewind_step();
        assert_eq!(meter.context_tokens, 1_000);
    }

    /// The sentinel fires once per drop and then re-baselines, so a genuinely
    /// cold prefix does not warn on every following turn.
    #[test]
    fn cache_regression_reports_a_sharp_drop_then_rebaselines() {
        let mut meter = TurnMeter::new(0, false);
        meter.turn = Usage {
            input_tokens: 100,
            cache_read_tokens: 900,
            ..Usage::default()
        };
        assert!(
            meter.take_cache_regression().is_none(),
            "first turn sets a baseline"
        );

        meter.turn = Usage {
            input_tokens: 900,
            cache_read_tokens: 100,
            ..Usage::default()
        };
        let (prev, now) = meter.take_cache_regression().expect("drop is reported");
        assert!(prev > 0.5 && now < 0.5);

        // Same low share again: already the baseline, so no second warning.
        assert!(meter.take_cache_regression().is_none());
    }

    /// Receipts report execution time, not time the human spent deciding.
    #[test]
    fn time_spent_awaiting_a_decision_is_excluded_from_the_receipt() {
        let mut meter = TurnMeter::new(0, false);
        let started = Instant::now();
        meter.pause_for_user();
        std::thread::sleep(Duration::from_millis(30));
        meter.resume_from_user();
        assert!(meter.active_elapsed(started) < 0.03);
    }
}
