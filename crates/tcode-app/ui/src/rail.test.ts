import { describe, expect, it } from "vitest";

import type { Block } from "./blocks";
import type { SessionInfo } from "./types";
import { moveProject, railGroups, sessionTitle } from "./rail";

const session = (id: string, cwd: string, name: string): SessionInfo => ({
  id,
  cwd,
  name,
  home: "/home/me",
});

const SESSIONS = [
  session("s1", "/code/tcode", "tcode"),
  session("s2", "/code/short_term_rs", "short_term_rs"),
  session("s3", "/code/tcode", "tcode"),
];

describe("railGroups", () => {
  it("gathers a folder's conversations under one heading", () => {
    const groups = railGroups(SESSIONS, []);

    expect(groups.map((group) => group.path)).toEqual(["/code/tcode", "/code/short_term_rs"]);
    expect(groups[0].sessions.map((entry) => entry.id)).toEqual(["s1", "s3"]);
    expect(groups[1].sessions.map((entry) => entry.id)).toEqual(["s2"]);
  });

  it("follows the arrangement the reader set", () => {
    const groups = railGroups(SESSIONS, ["/code/short_term_rs", "/code/tcode"]);
    expect(groups.map((group) => group.path)).toEqual(["/code/short_term_rs", "/code/tcode"]);
  });

  // The point of storing only the folders that were moved: opening a new one must
  // not scatter the ones already placed.
  it("appends a folder the arrangement has never heard of", () => {
    const groups = railGroups(
      [...SESSIONS, session("s4", "/code/pybond", "pybond")],
      ["/code/short_term_rs", "/code/tcode"],
    );
    expect(groups.map((group) => group.path)).toEqual([
      "/code/short_term_rs",
      "/code/tcode",
      "/code/pybond",
    ]);
  });

  it("leaves a stored folder that is no longer open out entirely", () => {
    const groups = railGroups(SESSIONS, ["/code/gone", "/code/short_term_rs"]);
    expect(groups.map((group) => group.path)).toEqual(["/code/short_term_rs", "/code/tcode"]);
  });
});

describe("moveProject", () => {
  const groups = railGroups(SESSIONS, []);

  it("writes out the whole order, not just the folder that moved", () => {
    expect(moveProject(groups, "/code/short_term_rs", 0)).toEqual([
      "/code/short_term_rs",
      "/code/tcode",
    ]);
  });

  it("refuses to move past either end rather than clamping silently", () => {
    expect(moveProject(groups, "/code/tcode", -1)).toEqual(["/code/tcode", "/code/short_term_rs"]);
    expect(moveProject(groups, "/code/short_term_rs", 2)).toEqual([
      "/code/tcode",
      "/code/short_term_rs",
    ]);
  });
});

describe("sessionTitle", () => {
  it("names a conversation after the first thing it was asked for", () => {
    const blocks: Block[] = [
      { kind: "note", text: "resumed" },
      { kind: "user", text: "Make the retry path testable.\nIt sleeps for real right now." },
      { kind: "user", text: "also cover the cap" },
    ];
    expect(sessionTitle(blocks)).toBe("Make the retry path testable.");
  });

  it("has nothing to say about a conversation nobody has typed into", () => {
    expect(sessionTitle([])).toBeNull();
    expect(sessionTitle([{ kind: "assistant", text: "ready" }])).toBeNull();
  });

  it("still names one that opened with an image", () => {
    expect(sessionTitle([{ kind: "user", text: "  ", images: ["data:image/png;base64,x"] }])).toBe(
      "image",
    );
  });
});
