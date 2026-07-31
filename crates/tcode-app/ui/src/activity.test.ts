import { describe, expect, it } from "vitest";

import { phaseOf } from "./activity";
import type { AgentEvent } from "./types";

/**
 * The live line said `working` for every second of every turn. These pin the
 * two properties that replaced it: the stream is read for *where* the turn is,
 * and an event that says nothing about that leaves the previous answer alone.
 */

const NAMES: Record<string, string> = { shell: "Run", read: "Read", edit: "Edit" };
const toolName = (name: string) => NAMES[name] ?? name;
const phase = (event: AgentEvent) => phaseOf(event, toolName);

describe("phaseOf", () => {
  it("distinguishes the places a turn actually spends its time", () => {
    expect(phase({ type: "Started" })).toBe("responding");
    expect(phase({ type: "ThinkingDelta", data: "…" })).toBe("thinking");
    expect(phase({ type: "TextDelta", data: "…" })).toBe("writing");
    expect(phase({ type: "ToolInputDelta", data: "…" })).toBe("calling a tool");
    expect(phase({ type: "Compacting" })).toBe("compacting history");
  });

  it("names a running call by tool and target", () => {
    const event: AgentEvent = {
      type: "ToolStart",
      data: { call_id: "c1", name: "shell", summary: "shell", input: { command: "cargo test" } },
    };
    // `Run`, from core's `display_name()` — never the wire name. Two casings for
    // one tool in one column is the drift `display_name` travels to prevent.
    expect(phase(event)).toBe("Run · cargo test");
  });

  it("says the tool once when the call is about nothing else", () => {
    const event: AgentEvent = {
      type: "ToolStart",
      data: { call_id: "c2", name: "read", summary: "read", input: {} },
    };
    expect(phase(event)).toBe("Read");
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
