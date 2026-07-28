import { describe, expect, it } from "vitest";

import { replayLedger } from "./replay";

describe("replayLedger", () => {
  it("rebuilds the transcript and file index from a resumed ledger", () => {
    const resumed = replayLedger([
      {
        kind: "user",
        data: [
          { type: "text", text: "Please inspect this" },
          { type: "image", media_type: "image/png", data: "aW1hZ2U=" },
        ],
      },
      {
        kind: "assistant",
        data: [
          { type: "thinking", thinking: "I should read the file." },
          { type: "tool_use", id: "read-1", name: "read", input: { file_path: "src/main.rs" } },
        ],
      },
      {
        kind: "tool_results",
        data: [{ type: "tool_result", tool_use_id: "read-1", content: "fn main() {}", is_error: false }],
      },
      { kind: "note", data: "resumed after restart" },
    ]);

    expect(resumed.blocks).toMatchObject([
      { kind: "user", text: "Please inspect this", images: ["data:image/png;base64,aW1hZ2U="] },
      { kind: "thinking", text: "I should read the file." },
      { kind: "tool", callId: "read-1", name: "read", result: { content: "fn main() {}" } },
      { kind: "note", text: "resumed after restart" },
    ]);
    expect(resumed.files).toMatchObject([
      { path: "src/main.rs", action: "read", pending: false, failed: false },
    ]);
  });

  it("keeps project instructions out of the human transcript", () => {
    const resumed = replayLedger([
      { kind: "instruction", data: "never show this" },
      { kind: "assistant", data: [{ type: "text", text: "visible answer" }] },
    ]);

    expect(resumed.blocks).toEqual([{ kind: "assistant", text: "visible answer" }]);
  });
});
