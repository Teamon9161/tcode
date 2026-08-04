import { describe, expect, it } from "vitest";

import { phaseOf, statusLabel } from "./activity";
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
