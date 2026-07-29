import { describe, expect, it } from "vitest";

import { nearestPane, type Box } from "./focus";

/** `at(x, y, w, h)` — panes as they would be laid out on screen. */
const at = (left: number, top: number, width: number, height: number): Box => ({
  left,
  top,
  right: left + width,
  bottom: top + height,
});

/**
 *   ┌──────────┬──────────┐
 *   │          │    b     │   a is full height on the left,
 *   │    a     ├──────────┤   b and c are stacked on the right
 *   │          │    c     │
 *   └──────────┴──────────┘
 */
const STACKED = new Map<string, Box>([
  ["a", at(0, 0, 400, 800)],
  ["b", at(400, 0, 400, 400)],
  ["c", at(400, 400, 400, 400)],
]);

describe("nearestPane", () => {
  it("finds the neighbour in the direction asked for", () => {
    expect(nearestPane(STACKED, "b", "left")).toBe("a");
    expect(nearestPane(STACKED, "c", "left")).toBe("a");
    expect(nearestPane(STACKED, "b", "down")).toBe("c");
    expect(nearestPane(STACKED, "c", "up")).toBe("b");
  });

  it("never lands on something behind you, however close", () => {
    expect(nearestPane(STACKED, "a", "left")).toBeNull();
    expect(nearestPane(STACKED, "b", "right")).toBeNull();
    expect(nearestPane(STACKED, "b", "up")).toBeNull();
  });

  it("prefers straight ahead over merely near", () => {
    // From a's centre (200, 400): b is 200px above the axis, c is 200px below,
    // and both are the same distance to the right. The tie is broken by which
    // is level with the eye — here neither, so the first one wins and the point
    // is only that it is deterministic.
    const eye = new Map<string, Box>([
      ["a", at(0, 300, 400, 200)], // centre y = 400
      ["far", at(500, 380, 300, 40)], // level, further right
      ["near", at(420, 0, 300, 40)], // closer, but 380px off-axis
    ]);
    expect(nearestPane(eye, "a", "right")).toBe("far");
  });

  it("returns null when the pane is not on screen", () => {
    expect(nearestPane(STACKED, "gone", "left")).toBeNull();
  });

  it("has nothing to go to in a single-pane window", () => {
    const alone = new Map<string, Box>([["a", at(0, 0, 800, 800)]]);
    for (const dir of ["left", "right", "up", "down"] as const) {
      expect(nearestPane(alone, "a", dir)).toBeNull();
    }
  });
});
