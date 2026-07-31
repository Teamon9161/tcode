import { displayToolSummary } from "./toolViews";
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
 * The vocabulary is the TUI's `state_label` (`app/turn.rs`), deliberately word
 * for word. It is not a model-facing contract, so nothing breaks if the two
 * drift — but a person who runs both should not have to learn that `writing`
 * here is the state they know as something else there, and every one of these
 * words was already chosen once.
 *
 * `null` means "this event says nothing about the phase", which leaves the
 * previous answer standing. That is the common case: `Usage`, `Note`, most of
 * the ledger bookkeeping. Returning a fallback instead would make the line
 * flicker back to a generic word between every two interesting events.
 */
export function phaseOf(
  event: AgentEvent,
  toolName: (name: string) => string,
): string | null {
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
    // Each payload is cast at its own branch rather than narrowed: the wire
    // union ends in an open `{ type: string; data?: unknown }` so a variant
    // added to core lands here instead of failing to compile, which means no
    // branch narrows on its own. `blocks.ts` reads the same events the same way.
    case "ToolStart": {
      const data = event.data as { name: string; summary: string; input: unknown };
      const about = displayToolSummary(data.name, data.summary, data.input);
      const name = toolName(data.name);
      return about && about !== data.name && about !== name ? `${name} · ${about}` : name;
    }
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
