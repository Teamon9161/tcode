import { describe, expect, it } from "vitest";

import type { Block, RunMeta } from "./blocks";
import {
  agentKind,
  changeSetLabel,
  groupTranscriptBlocks,
  runInspect,
  runState,
} from "./Transcript";

const tool = (name: string, callId: string, input: unknown = {}): Extract<Block, { kind: "tool" }> => ({
  kind: "tool",
  name,
  callId,
  summary: name,
  input,
});

describe("groupTranscriptBlocks", () => {
  it("collapses contiguous reads and searches into one exploration step", () => {
    const items = groupTranscriptBlocks([tool("read", "r1"), tool("grep", "g1"), tool("glob", "g2")]);

    expect(items).toHaveLength(1);
    expect(items[0]).toMatchObject({ kind: "exploration", blocks: [{ callId: "r1" }, { callId: "g1" }, { callId: "g2" }] });
  });

  it("collapses consecutive changes, but keeps a single change and execution boundaries direct", () => {
    const grouped = groupTranscriptBlocks([tool("edit", "e1"), tool("write", "w1"), tool("multi_edit", "m1")]);
    expect(grouped).toHaveLength(1);
    expect(grouped[0]).toMatchObject({
      kind: "changes",
      blocks: [{ callId: "e1" }, { callId: "w1" }, { callId: "m1" }],
    });

    const separated = groupTranscriptBlocks([
      tool("edit", "e2"),
      { kind: "assistant", text: "The first change is complete." },
      tool("edit", "e3"),
    ]);
    expect(separated.map((item) => item.kind)).toEqual(["block", "block", "block"]);

    expect(
      changeSetLabel([
        tool("edit", "e4", { file_path: "src/Transcript.tsx", old_string: "a", new_string: "b" }),
        tool("write", "w2", { file_path: "src/toolViews.tsx", content: "next" }),
        tool("edit", "e5", { file_path: "src/Transcript.tsx", old_string: "b", new_string: "c" }),
      ]),
    ).toBe("Edit 2 files");
  });

  it("collapses consecutive shell and bash calls into a command run", () => {
    const grouped = groupTranscriptBlocks([tool("shell", "s1"), tool("bash", "b1"), tool("shell", "s2")]);
    expect(grouped).toHaveLength(1);
    expect(grouped[0]).toMatchObject({
      kind: "commands",
      blocks: [{ callId: "s1" }, { callId: "b1" }, { callId: "s2" }],
    });

    const separated = groupTranscriptBlocks([tool("shell", "s3"), tool("read", "r1"), tool("bash", "b2")]);
    expect(separated.map((item) => item.kind)).toEqual(["block", "exploration", "block"]);
  });

  // Reasoning used to be swept into the surrounding exploration group, from when
  // it was a folded row that looked like a step. It is prose now, so it is a
  // boundary like any other prose — and a group must not be able to swallow it:
  // folded shut, "show me the reasoning" would answer by hiding it.
  it("treats reasoning as a boundary rather than part of an exploration step", () => {
    const items = groupTranscriptBlocks([
      tool("read", "r1"),
      { kind: "thinking", text: "The next target is likely nearby." },
      tool("grep", "g1"),
    ]);

    expect(items.map((item) => item.kind)).toEqual(["exploration", "block", "exploration"]);
  });
});

describe("runState", () => {
  // `TaskRunStatus` on the wire, and the reason every finished sub-agent used to
  // wear the failure cross: the comparison was against "ok", which no status has
  // ever been.
  it("reads the statuses core actually sends", () => {
    expect(runState(undefined)).toBe("running");
    expect(runState("running")).toBe("running");
    expect(runState("done")).toBe("idle");
    expect(runState("failed")).toBe("failed");
  });

  it("does not call a stopped run a failure", () => {
    expect(runState("cancelled")).toBe("idle");
    expect(runState("interrupted")).toBe("idle");
  });
});

describe("runInspect", () => {
  const meta = (over: Partial<RunMeta> = {}): RunMeta => ({
    kind: "explore",
    model: "claude-opus-5",
    prompt: "Find every place that sleeps inline.\nReport file and line.",
    summary: "Find the inline sleeps",
    parentCall: "a1",
    ...over,
  });

  it("names the pane after the run rather than after its species", () => {
    expect(runInspect("r1", meta(), "Explore").label).toBe("Explore · Find the inline sleeps");
  });

  it("falls back to the prompt's first line when no summary was written", () => {
    expect(runInspect("r1", meta({ summary: "" }), "Explore").label).toBe(
      "Explore · Find every place that sleeps inline.",
    );
  });
});

describe("agentKind", () => {
  it("capitalizes the config key, and names an unnamed one", () => {
    expect(agentKind("explore")).toBe("Explore");
    expect(agentKind("")).toBe("Agent");
  });

  it("keeps a model response and execution call as grouping boundaries", () => {
    const items = groupTranscriptBlocks([
      tool("read", "r1"),
      { kind: "assistant", text: "I found the first file." },
      tool("grep", "g1"),
      tool("shell", "s1"),
      tool("glob", "g2"),
    ]);

    expect(items.map((item) => item.kind)).toEqual(["exploration", "block", "exploration", "block", "exploration"]);
  });
});
