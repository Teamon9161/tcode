import { describe, expect, it } from "vitest";

import { applyEvent, reportOf, runPairs, runSteps, type Block } from "./blocks";
import type { AgentEvent } from "./types";

const started = (run: string, parentCall: string): AgentEvent => ({
  type: "TaskRunStarted",
  data: {
    run,
    parent_call: parentCall,
    kind: "explore",
    model: "claude-opus-5",
    prompt: "Find the inline sleeps.",
    summary: "Find the inline sleeps",
  },
});

const call = (callId: string): AgentEvent => ({
  type: "ToolStart",
  data: { call_id: callId, name: "agent", summary: "agent(explore)", input: { agent: "explore" } },
});

const ended = (callId: string, content: string): AgentEvent => ({
  type: "ToolEnd",
  data: { call_id: callId, name: "agent", preview: content, content, is_error: false },
});

const build = (events: AgentEvent[]): Block[] => events.reduce(applyEvent, [] as Block[]);

describe("tool UI metadata", () => {
  it("preserves structured Browser tab metadata outside result prose", () => {
    const blocks = build([
      {
        type: "ToolStart",
        data: {
          call_id: "b1",
          name: "browser",
          summary: "open",
          input: { action: "open" },
        },
      },
      {
        type: "ToolEnd",
        data: {
          call_id: "b1",
          name: "browser",
          preview: "opened browser tab prose-is-not-an-id",
          content: "opened browser tab prose-is-not-an-id",
          is_error: false,
          ui_metadata: { kind: "browser_tab", id: "tab-structured" },
        },
      },
    ]);

    expect(blocks[0]).toMatchObject({
      kind: "tool",
      result: { uiMetadata: { kind: "browser_tab", id: "tab-structured" } },
    });
  });
});

describe("queued input", () => {
  it("records a delivered prompt as a normal user message", () => {
    const blocks = build([
      {
        type: "QueuedInput",
        data: {
          text: "also add a test for the cap at 30s",
          attachments: ["data:image/png;base64,queued-image"],
          entry_index: 12,
        },
      },
    ]);

    expect(blocks).toEqual([
      {
        kind: "user",
        text: "also add a test for the cap at 30s",
        images: ["data:image/png;base64,queued-image"],
        entryIndex: 12,
      },
    ]);
  });
});

describe("permission mode records", () => {
  it("renders the Core boundary event as a visible transcript record", () => {
    expect(build([{ type: "ModeChanged", data: "accept-edits" }])).toEqual([
      { kind: "note", text: "permission mode → accept-edits" },
    ]);
  });
});

describe("runPairs", () => {
  // The delegating call and its run are two records of one step; drawn as two
  // rows the step took two lines, the first of them `agent · agent(explore)`.
  it("pairs a run with the call that started it", () => {
    const blocks = build([call("a1"), started("r1", "a1"), ended("a1", "found two")]);
    const pairs = runPairs(blocks);

    expect([...pairs.superseded]).toEqual(["a1"]);
    expect(pairs.report.get("r1")?.callId).toBe("a1");
  });

  // Matched on `parent_call`, so nothing in the transcript needs to know what the
  // delegating tool is called.
  it("does not touch a call that started nothing", () => {
    const blocks = build([
      { type: "ToolStart", data: { call_id: "t1", name: "read", summary: "read", input: {} } },
      call("a1"),
      started("r1", "a1"),
    ]);
    const pairs = runPairs(blocks);

    expect(pairs.superseded.has("t1")).toBe(false);
    expect(pairs.superseded.has("a1")).toBe(true);
  });

  // A log recorded before runs carried their parent call pairs with nothing, and
  // both rows draw. Two rows beats a row that vanished.
  it("pairs nothing when the run does not name a parent", () => {
    const blocks = build([call("a1"), started("r1", "")]);
    const pairs = runPairs(blocks);

    expect(pairs.superseded.size).toBe(0);
    expect(pairs.report.size).toBe(0);
  });
});

describe("runSteps", () => {
  const step: Block = { kind: "tool", callId: "t1", name: "grep", summary: "grep", input: {} };
  const said = (text: string): Block => ({ kind: "assistant", text });

  // Core builds the report out of the run's final assistant entry, so drawing
  // both puts the same paragraphs on screen twice.
  it("drops the last message when it is the report", () => {
    expect(runSteps([step, said("two hits, both in retry")], "two hits, both in retry")).toEqual([
      step,
    ]);
  });

  // A resumable run's result is prefixed with a header naming the run, and the
  // report is what follows it.
  it("recognizes the report behind the header a resumable run carries", () => {
    const report = "[explore sub-agent r1 on opus: found it]\ntwo hits, both in retry";
    expect(runSteps([step, said("two hits, both in retry")], report)).toEqual([step]);
  });

  // Cancelled, failed, or an older log with no `parent_call`: there is no
  // report, so every step stays where it is.
  it("keeps every step when nothing came back", () => {
    const blocks = [step, said("two hits, both in retry")];
    expect(runSteps(blocks, undefined)).toBe(blocks);
    expect(runSteps(blocks, "   ")).toBe(blocks);
  });

  it("keeps a last message the report does not account for", () => {
    const blocks = [step, said("still looking")];
    expect(runSteps(blocks, "cancelled before it answered")).toBe(blocks);
  });

  it("leaves a run whose last block is a step of its own", () => {
    const blocks = [said("here is what I found"), step];
    expect(runSteps(blocks, "here is what I found")).toBe(blocks);
  });
});

describe("reportOf", () => {
  it("finds what a run came back with", () => {
    const blocks = build([call("a1"), started("r1", "a1"), ended("a1", "two hits, both in retry")]);
    expect(reportOf(blocks, "r1")).toBe("two hits, both in retry");
  });

  it("has nothing while the run is still going", () => {
    expect(reportOf(build([call("a1"), started("r1", "a1")]), "r1")).toBeNull();
  });

  // The walkers descend, so a run nested inside a batch or another run is found
  // the same way — which is the whole reason this uses them instead of scanning
  // one list.
  it("reaches a run nested inside another one", () => {
    const inner: AgentEvent[] = [call("b1"), started("r2", "b1"), ended("b1", "inner report")];
    const blocks = build([
      call("a1"),
      started("r1", "a1"),
      ...inner.map((event): AgentEvent => ({ type: "TaskRunEvent", data: { run: "r1", event } })),
    ]);

    expect(reportOf(blocks, "r2")).toBe("inner report");
  });
});
