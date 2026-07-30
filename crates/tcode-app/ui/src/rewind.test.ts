import { describe, expect, it } from "vitest";

import type { Block } from "./blocks";
import { rewindPoints, type RewindTarget } from "./rewind";

const user = (text: string): Block => ({ kind: "user", text });
const target = (index: number, text: string, dirty = false): RewindTarget => ({
  index,
  text,
  dirty,
});

describe("rewindPoints", () => {
  it("pairs each target with the prompt it names, in order", () => {
    const first = user("make the retry path testable");
    const second = user("also cover the cap");
    const blocks: Block[] = [
      first,
      { kind: "assistant", text: "reading the loop" },
      second,
    ];

    const points = rewindPoints(blocks, [
      target(1, "make the retry path testable"),
      target(7, "also cover the cap", true),
    ]);

    expect(points.get(first)?.index).toBe(1);
    expect(points.get(second)?.index).toBe(7);
    expect(points.get(second)?.dirty).toBe(true);
  });

  // The transcript is replayed from the whole display history, which includes
  // the compacted-away era; core says outright that it holds no valid truncation
  // index. Those prompts are on screen and must not get a control.
  it("leaves a prompt the targets do not mention unmarked", () => {
    const archived = user("something from before the compaction");
    const live = user("the current ask");

    const points = rewindPoints([archived, live], [target(1, "the current ask")]);

    expect(points.has(archived)).toBe(false);
    expect(points.get(live)?.index).toBe(1);
  });

  it("tells two identical prompts apart by their order", () => {
    const first = user("try again");
    const second = user("try again");

    const points = rewindPoints(
      [first, { kind: "assistant", text: "no luck" }, second],
      [target(2, "try again"), target(9, "try again")],
    );

    expect(points.get(first)?.index).toBe(2);
    expect(points.get(second)?.index).toBe(9);
  });

  // The whole safety property: an unmatched prompt loses its button, and nothing
  // ever acquires the wrong index. The backend re-checks the index anyway.
  it("stops rather than guessing when the lists disagree", () => {
    const only = user("what is on screen");
    expect(rewindPoints([only], [target(3, "something else")]).size).toBe(0);
    expect(rewindPoints([only], []).size).toBe(0);
  });

  it("does not offer to go back to something the model said", () => {
    const spoken: Block = { kind: "assistant", text: "the current ask" };
    expect(rewindPoints([spoken], [target(1, "the current ask")]).size).toBe(0);
  });
});
