import { describe, expect, it } from "vitest";

import { wrapsLines } from "./diff";

describe("wrapsLines", () => {
  // The reported failure: a long line in a markdown diff was clipped at the box
  // edge and needed a horizontal drag to read the tail. Prose folds.
  it("wraps markdown and other prose languages", () => {
    expect(wrapsLines("md")).toBe(true);
    expect(wrapsLines("markdown")).toBe(true);
    expect(wrapsLines("txt")).toBe(true);
    expect(wrapsLines("rst")).toBe(true);
  });

  // A code line's shape is information: wrapping one added line into two visual
  // rows could read as two added lines, so code keeps `pre` + horizontal scroll.
  it("keeps code lines un-wrapped", () => {
    expect(wrapsLines("rust")).toBe(false);
    expect(wrapsLines("ts")).toBe(false);
    expect(wrapsLines("py")).toBe(false);
    expect(wrapsLines("toml")).toBe(false);
  });

  // A plan edit, a ```diff fence, a file with no extension: no language is
  // known, and the tail-hidden failure outweighs the shape argument.
  it("wraps when the language is unknown", () => {
    expect(wrapsLines("")).toBe(true);
  });
});
