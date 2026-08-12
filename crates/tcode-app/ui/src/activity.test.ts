import { describe, expect, it } from "vitest";

import {
  elapsedLabel,
  phaseOf,
  retryFrom,
  secondsLeft,
  statusLabel,
} from "./activity";
import type { AgentEvent } from "./types";

/**
 * The live line said `working` for every second of every turn. These pin the
 * two properties that replaced it: the stream is read for *where* the turn is,
 * and an event that says nothing about that leaves the previous answer alone.
 */

const phase = (event: AgentEvent) => phaseOf(event);

describe("phaseOf", () => {
  it("distinguishes the places a turn actually spends its time", () => {
    expect(phase({ type: "Started" })).toBe("responding");
    expect(phase({ type: "ThinkingDelta", data: "…" })).toBe("thinking");
    expect(phase({ type: "TextDelta", data: "…" })).toBe("writing");
    expect(phase({ type: "ToolInputDelta", data: "…" })).toBe("calling a tool");
    expect(phase({ type: "Compacting" })).toBe("compacting history");
  });

  it("keeps a tool call at the shared phase instead of repeating its target", () => {
    const event: AgentEvent = {
      type: "ToolStart",
      data: { call_id: "c1", name: "read", summary: "read", input: { file_path: "src/main.rs" } },
    };

    expect(phase(event)).toBe("calling a tool");
  });

  it("takes a batch's label from core, which is what its row is headed with", () => {
    expect(
      phase({ type: "ToolBatchStart", data: { label: "Read 3 files", calls: [] } }),
    ).toBe("Read 3 files");
  });

  it("counts retries, because the number is the whole news", () => {
    expect(
      phase({ type: "Retrying", data: { attempt: 2, max: 5, error: "reset", delay_ms: 400 } }),
    ).toBe("retrying (2/5)");
  });

  it("reports the delegating turn's own state, not the sub-agent's", () => {
    // Nested events arrive by the hundred and belong to another conversation.
    // Letting them through would make this line flicker with a turn nobody is
    // looking at, while the one on screen is doing exactly one thing.
    expect(
      phase({
        type: "TaskRunStarted",
        data: { run: "r1", parent_call: "c3", kind: "explore", model: "m", prompt: "p", summary: "s" },
      }),
    ).toBe("sub-agent working");
    expect(
      phase({ type: "TaskRunEvent", data: { run: "r1", event: { type: "ThinkingDelta", data: "…" } } }),
    ).toBeNull();
  });

  it("uses a leading capital when a phase is rendered as status copy", () => {
    expect(statusLabel("calling a tool")).toBe("Calling a tool");
    expect(statusLabel("Read 3 files")).toBe("Read 3 files");
    expect(statusLabel("  ")).toBe("Working");
  });

  it("leaves the phase standing for events that are not about it", () => {
    // Null means "unchanged". A fallback here would flicker the line back to a
    // generic word between every two interesting events, because bookkeeping
    // outnumbers them.
    expect(phase({ type: "Usage", data: {} })).toBeNull();
    expect(phase({ type: "Note", data: "…" })).toBeNull();
    expect(phase({ type: "ToolEnd", data: {} })).toBeNull();
    // A variant core added after this file was written, which the open end of
    // the wire union lets through.
    expect(phase({ type: "SomethingNew", data: {} })).toBeNull();
  });
});

/**
 * The readings beside the phase. They exist because a wait produces exactly two
 * questions the phase cannot answer — how long, and how much has come back —
 * and the terminal has printed both since its first version.
 */
describe("retryFrom", () => {
  it("turns the event's delay into a deadline, so the countdown cannot drift", () => {
    // Decrementing a stored remainder on a timer drifts against the backend
    // that is actually waiting, and drifts differently in a pane nobody is
    // looking at. A deadline computed once is the same answer from anywhere.
    expect(
      retryFrom(
        { type: "Retrying", data: { attempt: 2, max: 5, error: "reset", delay_ms: 4_000 } },
        1_000,
      ),
    ).toEqual({ attempt: 2, max: 5, until: 5_000 });
  });

  it("clears on the request that ends the wait rather than on the clock", () => {
    // A provider answering early leaves a countdown ticking under a turn that
    // is already streaming, unless the next request itself is the signal.
    expect(retryFrom({ type: "Started" }, 1_000)).toBe("clear");
  });

  it("says nothing about the events that are not about retrying", () => {
    expect(retryFrom({ type: "TextDelta", data: "…" }, 1_000)).toBeNull();
    expect(retryFrom({ type: "Usage", data: {} }, 1_000)).toBeNull();
  });
});

describe("elapsedLabel", () => {
  it("counts in seconds while somebody is plausibly watching", () => {
    expect(elapsedLabel(0)).toBe("0s");
    expect(elapsedLabel(12_400)).toBe("12s");
    expect(elapsedLabel(59_999)).toBe("59s");
  });

  it("switches to minutes rather than printing a number nobody divides", () => {
    // The TUI prints raw seconds forever because a status line has one column
    // to spend. This row has the width, and `421s` is not read as seven
    // minutes without doing the arithmetic.
    expect(elapsedLabel(60_000)).toBe("1m 00s");
    expect(elapsedLabel(421_000)).toBe("7m 01s");
    expect(elapsedLabel(3_600_000)).toBe("1h 00m");
    expect(elapsedLabel(7_845_000)).toBe("2h 10m");
  });

  it("never counts backwards from a clock that moved", () => {
    expect(elapsedLabel(-5_000)).toBe("0s");
  });
});

describe("secondsLeft", () => {
  it("rounds up, so the last second is shown rather than skipped", () => {
    expect(secondsLeft(5_000, 1_000)).toBe(4);
    expect(secondsLeft(5_000, 4_200)).toBe(1);
  });

  it("runs out at zero instead of sitting at one", () => {
    // The retry itself takes a moment; a countdown frozen at `1s` is a worse
    // lie than one that admits it is out of numbers.
    expect(secondsLeft(5_000, 5_000)).toBe(0);
    expect(secondsLeft(5_000, 9_000)).toBe(0);
  });
});
