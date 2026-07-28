import { describe, expect, it } from "vitest";

import type { Block } from "./blocks";
import { changeSetLabel, groupTranscriptBlocks } from "./Transcript";

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
    ).toBe("edit 2 files");
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

  it("keeps thinking inside an exploration step until another transcript block arrives", () => {
    const items = groupTranscriptBlocks([
      tool("read", "r1"),
      { kind: "thinking", text: "The next target is likely nearby." },
      tool("grep", "g1"),
    ]);

    expect(items).toHaveLength(1);
    expect(items[0]).toMatchObject({ kind: "exploration", blocks: [{ callId: "r1" }, { kind: "thinking" }, { callId: "g1" }] });
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
