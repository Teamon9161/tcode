import type { AgentEvent } from "./types";

/**
 * What a turn is doing right now, in one line.
 *
 * The window used to answer this with the word `working` for every second of
 * every turn, which is the least it could have said: a turn spends its time in
 * genuinely different places — waiting on a model, streaming a reply, running a
 * command, sitting behind a sub-agent — and "which of those" is exactly the
 * question somebody glances over to ask. The rail already carried a partial
 * answer from three event types; the rest of the stream was on the wire and
 * unread.
 *
 * The TUI's state words (`responding`, `thinking`, `writing`) are kept intact.
 * A tool execution stays at `calling a tool`: the trace immediately above the
 * composer already names the tool and its target, so repeating a file path in
 * this persistent status adds noise rather than useful progress.
 *
 * `null` means "this event says nothing about the phase", which leaves the
 * previous answer standing. That is the common case: `Usage`, `Note`, most of
 * the ledger bookkeeping. Returning a fallback instead would make the line
 * flicker back to a generic word between every two interesting events.
 */
export function phaseOf(event: AgentEvent): string | null {
  switch (event.type) {
    // One model request opened. The reply has not begun, so this is the wait.
    case "Started":
      return "responding";
    case "TextDelta":
      return "writing";
    case "ThinkingDelta":
      return "thinking";
    // Arguments are still streaming, so there is no call to name yet.
    case "ToolInputDelta":
      return "calling a tool";
    // Once the call starts, the transcript owns its target. Keep this fixed
    // status to the TUI's phase vocabulary rather than duplicating a file path.
    case "ToolStart":
      return "calling a tool";
    // Core's own label for the group ("Read 3 files"), which is the same string
    // the batch's row in the transcript is headed with.
    case "ToolBatchStart":
      return (event.data as { label: string }).label || null;
    case "Compacting":
      return "compacting history";
    case "Retrying": {
      const data = event.data as { attempt: number; max: number };
      return `retrying (${data.attempt}/${data.max})`;
    }
    // Nested events are the sub-agent's phase, not this turn's, and they arrive
    // by the hundred. The delegating turn is doing one thing while they run,
    // and it is this.
    case "TaskRunStarted":
      return "sub-agent working";
    case "StepLimitReached":
      return `step limit reached (${(event.data as { max: number }).max})`;
    case "AwaitingUserInput":
      return "waiting for your instruction";
    case "Interrupted":
      return "interrupted";
    case "ModeChanged":
      return `permission mode: ${String(event.data)}`;
    case "AutoModePaused":
      return "manual approvals required";
    default:
      return null;
  }
}

/** Render a phase as status copy without changing the canonical event vocabulary. */
export function statusLabel(phase: string): string {
  const trimmed = phase.trim();
  return trimmed ? trimmed.charAt(0).toUpperCase() + trimmed.slice(1) : "Working";
}

/**
 * A backoff the turn is sitting out, with the wall-clock instant it ends.
 *
 * The event carries a *duration* and the line has to draw a *countdown*, so the
 * deadline is computed once where the event lands rather than on every tick —
 * a remaining-time state decremented by a timer drifts against the backend that
 * is actually waiting, and drifts differently in a pane nobody is looking at.
 */
export type Retry = { attempt: number; max: number; until: number };

/**
 * The retry an event announces, `null` for events that announce none.
 *
 * `Started` clears it: the next attempt has actually opened a request, so the
 * wait is over regardless of what the arithmetic on the deadline says. Without
 * that, a provider answering early would leave a countdown ticking under a turn
 * that is already streaming.
 */
export function retryFrom(event: AgentEvent, now: number): Retry | null | "clear" {
  if (event.type === "Started") return "clear";
  if (event.type !== "Retrying") return null;
  const data = event.data as { attempt: number; max: number; delay_ms: number };
  return { attempt: data.attempt, max: data.max, until: now + data.delay_ms };
}

/**
 * How long a turn has been running, at a glance.
 *
 * Seconds while it is plausible to be watching, minutes once it is not. The TUI
 * prints raw seconds forever because a status *line* has one column to spend
 * and `421s` costs four characters; here the row has room, and nobody reads
 * `421s` as seven minutes without doing the division.
 */
export function elapsedLabel(ms: number): string {
  const total = Math.max(0, Math.floor(ms / 1000));
  if (total < 60) return `${total}s`;
  const minutes = Math.floor(total / 60);
  const seconds = total % 60;
  if (minutes < 60) return `${minutes}m ${String(seconds).padStart(2, "0")}s`;
  return `${Math.floor(minutes / 60)}h ${String(minutes % 60).padStart(2, "0")}m`;
}

/**
 * Seconds left of a backoff, rounded up so the last one is shown rather than
 * skipped, and `0` once it has run out. The TUI reads `now…` at zero for the
 * same reason: a countdown that sits at `1s` because the retry itself takes a
 * moment is a worse lie than one that admits it is out of numbers.
 */
export function secondsLeft(until: number, now: number): number {
  return Math.max(0, Math.ceil((until - now) / 1000));
}
