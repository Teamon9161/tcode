/**
 * What a turn costs and what is left of the budget, as pure reads.
 *
 * Two different quantities live here and must not be mixed (the same split
 * `tcode-tui/src/usage.rs` documents): **context** is what the conversation
 * occupies in the model's window right now — cached prefix included, because
 * cached input still takes up room — and **turn** is the receipt for one turn,
 * which reports the uncached input actually paid for. Summing `total_input()`
 * across a multi-step turn would recount the cached prefix once per request.
 *
 * The reducer is here rather than in `App.tsx` for this codebase's usual
 * reason: "an authoritative usage event replaces the estimate instead of adding
 * to it" is a rule with a right and a wrong answer, and answers are testable.
 * `UsagePanel.tsx` draws what these return.
 */

import type { AgentEvent } from "./types";

/** One provider budget window, as `tcode_core::RateLimit` sends it. */
export type Limit = {
  used_percent: number;
  /** 0 when the provider did not say. */
  window_minutes: number;
  /** Unix seconds; 0 when the provider did not say. */
  resets_at: number;
};

export type Limits = { primary: Limit; secondary: Limit | null };

/** Normalized token counts for one model request. Mirrors `tcode_core::Usage`:
 *  `input_tokens` is the NON-cached input only. */
export type Usage = {
  input_tokens: number;
  output_tokens: number;
  cache_read_tokens: number;
  cache_write_tokens: number;
};

export type Meter = {
  /** Best available estimate of what this conversation occupies in the window. */
  context: number;
  /**
   * True while that figure has never been confirmed by a provider tally.
   *
   * A resumed conversation starts here: the session log stores messages, not
   * token counters, so until the next response arrives the meter is showing an
   * estimate and has to say so rather than quietly rounding it to a fact.
   */
  estimated: boolean;
  /** The turn in flight, or the last one that finished. */
  turn: Usage;
};

export const NO_USAGE: Usage = {
  input_tokens: 0,
  output_tokens: 0,
  cache_read_tokens: 0,
  cache_write_tokens: 0,
};

export const NO_METER: Meter = {
  context: 0,
  estimated: false,
  turn: NO_USAGE,
};

/**
 * The subscription budget an event reports, or `null` when it reports none.
 *
 * Deliberately *not* part of `Meter`: a 5-hour window belongs to the account,
 * not to a conversation, and every session in the window spends the same one.
 * Keeping it per session would mean a folder opened five minutes ago claims to
 * know nothing about a budget the pane next to it is already showing at 42%.
 */
export function limitsFrom(event: AgentEvent): Limits | null {
  return event.type === "RateLimits" ? (event.data as Limits) : null;
}

/**
 * Fold one event into the meter.
 *
 * `Usage` is a per-step tally and is authoritative: the whole prompt for that
 * request is `total_input`, so the context figure is *replaced* by it rather
 * than accumulated. The turn receipt is the one thing that adds up, and
 * `DelegatedUsage` (a sub-agent's spend) joins it — a sub-agent costs money but
 * occupies its own window, not the parent's.
 *
 * Nothing here zeroes the receipt. `AgentEvent::Started` looks like the moment
 * to do it and is not: core emits it for every model *request*, so a turn that
 * takes six steps would arrive with only the sixth one's cost. The receipt is
 * reset where the turn is submitted (`App.tsx`), which is also where the TUI
 * does it.
 */
export function applyUsage(meter: Meter, event: AgentEvent): Meter {
  switch (event.type) {
    case "Usage": {
      const usage = event.data as Usage;
      return {
        ...meter,
        context: totalInput(usage) + usage.output_tokens,
        estimated: false,
        turn: addUsage(meter.turn, usage),
      };
    }
    case "DelegatedUsage":
      return { ...meter, turn: addUsage(meter.turn, event.data as Usage) };
    // Reference context is expanded into the prompt *before* the turn runs, so
    // it is already in the window while the meter is still showing whatever the
    // last response reported. Counting it keeps the ring honest for the one
    // case where a single message moves the needle several percent.
    case "ReferencesExpanded": {
      const added = (event.data as { added_tokens: number }).added_tokens;
      return { ...meter, context: meter.context + added, estimated: true };
    }
    // A compaction replaced the history with a summary. Nothing here knows how
    // big that summary is; the next response does, and until then saying so is
    // better than showing the pre-compaction figure as if it still held.
    case "Compacted":
      return { ...meter, estimated: true };
    default:
      return meter;
  }
}

/**
 * A resumed conversation's window occupancy, from the log itself.
 *
 * A session log stores messages, not token counters, so there is no true answer
 * to give until the next response arrives — and a resumed conversation with
 * 90k of history showing `0%` would be the one moment the meter is read and the
 * one moment it is wrong. Same rough divisor as core's `approx_tokens`, and the
 * result is always flagged `estimated`: it under-counts (the harness prefix and
 * project instructions never reach this side) and the first real tally replaces
 * it.
 */
export function estimateContext(history: unknown[]): number {
  if (history.length === 0) return 0;
  return Math.ceil(JSON.stringify(history).length / 3);
}

export function totalInput(usage: Usage): number {
  return usage.input_tokens + usage.cache_read_tokens + usage.cache_write_tokens;
}

function addUsage(left: Usage, right: Usage): Usage {
  return {
    input_tokens: left.input_tokens + right.input_tokens,
    output_tokens: left.output_tokens + right.output_tokens,
    cache_read_tokens: left.cache_read_tokens + right.cache_read_tokens,
    cache_write_tokens: left.cache_write_tokens + right.cache_write_tokens,
  };
}

/** Share of the last turn's prompt that was served from cache. `null` when the
 *  turn read nothing — 0% and "no input yet" are different facts. */
export function cacheShare(usage: Usage): number | null {
  const total = totalInput(usage);
  return total > 0 ? usage.cache_read_tokens / total : null;
}

/** 0–100, clamped. A window of 0 (no model, or a model with none declared)
 *  reads as empty rather than dividing by zero. */
export function percent(used: number, of: number): number {
  if (of <= 0) return 0;
  return Math.min(100, Math.max(0, (used / of) * 100));
}

/**
 * How loud a meter is allowed to be.
 *
 * Achromatic until it matters, which is this app's palette rule rather than a
 * shade of the terminal's: chroma means state, and "34% of a window used" is not
 * a state — it is the number being fine. The thresholds are the TUI's, so the
 * two frontends warn at the same moment.
 */
export type Level = "calm" | "high" | "full";

export function contextLevel(pct: number): Level {
  if (pct >= 95) return "full";
  if (pct >= 85) return "high";
  return "calm";
}

export function limitLevel(pct: number): Level {
  if (pct >= 90) return "full";
  if (pct >= 75) return "high";
  return "calm";
}

/** Compact token counts, as the TUI writes them: `980`, `1.2k`, `68k`. */
export function tokens(count: number): string {
  if (count < 1_000) return String(Math.round(count));
  if (count < 10_000) return `${(count / 1_000).toFixed(1)}k`;
  return `${Math.ceil(count / 1_000)}k`;
}

/**
 * What to call a budget window, from the minutes the provider reported.
 *
 * Derived rather than hard-coded to "5h" and "weekly": those are today's Codex
 * plan, and a meter that labels a 3-hour window "5h" because the string was
 * baked in is worse than one that says "3h".
 */
export function windowLabel(minutes: number): string {
  if (minutes <= 0) return "window";
  if (minutes >= 10_080 && minutes % 10_080 === 0) {
    const weeks = minutes / 10_080;
    return weeks === 1 ? "weekly" : `${weeks} weeks`;
  }
  if (minutes >= 1_440 && minutes % 1_440 === 0) {
    const days = minutes / 1_440;
    return days === 1 ? "daily" : `${days}d`;
  }
  if (minutes >= 60 && minutes % 60 === 0) return `${minutes / 60}h`;
  return `${minutes}m`;
}

/**
 * How long until a window resets, or `null` when it already has (or the
 * provider never said). `null` is the signal to draw nothing: a countdown that
 * has run out is noise, and "resets in 0m" reads as a fault.
 */
export function resetIn(resetsAt: number, now: number): string | null {
  const left = resetsAt - now;
  if (resetsAt <= 0 || left <= 0) return null;
  if (left < 60) return "under a minute";
  if (left < 3_600) return `${Math.ceil(left / 60)}m`;
  if (left < 86_400) {
    const hours = Math.floor(left / 3_600);
    const minutes = Math.ceil((left % 3_600) / 60);
    return minutes > 0 ? `${hours}h ${minutes}m` : `${hours}h`;
  }
  return `${Math.ceil(left / 86_400)}d`;
}
