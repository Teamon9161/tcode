import { describe, expect, it } from "vitest";

import type { Block } from "./blocks";
import type { ProjectInfo, SessionInfo } from "./types";
import {
  find,
  moveProject,
  railBands,
  sessionTitle,
  type FoundSession,
} from "./rail";

const session = (id: string, cwd: string, name: string): SessionInfo => ({
  id,
  cwd,
  name,
  home: "/home/me",
  log_id: `log-${id}`,
});

const project = (
  path: string,
  name: string,
  last: number | null,
): ProjectInfo => ({
  path,
  name,
  session_count: 3,
  last_active: last,
  exists: true,
});

const SESSIONS = [
  session("s1", "/code/tcode", "tcode"),
  session("s2", "/code/short_term_rs", "short_term_rs"),
  session("s3", "/code/tcode", "tcode"),
];

const PROJECTS = [
  project("/code/tcode", "tcode", 900),
  project("/code/short_term_rs", "short_term_rs", 800),
  project("/code/pybond", "pybond", 700),
  project("/code/duck_ext", "duck_ext", 600),
];

describe("railBands", () => {
  it("gathers a folder's conversations under one heading", () => {
    const { live } = railBands(SESSIONS, [], []);

    expect(live.map((group) => group.path)).toEqual([
      "/code/tcode",
      "/code/short_term_rs",
    ]);
    expect(live[0].sessions.map((entry) => entry.id)).toEqual(["s1", "s3"]);
    expect(live[1].sessions.map((entry) => entry.id)).toEqual(["s2"]);
  });

  it("follows the arrangement the reader set", () => {
    const { live } = railBands(
      SESSIONS,
      [],
      ["/code/short_term_rs", "/code/tcode"],
    );
    expect(live.map((group) => group.path)).toEqual([
      "/code/short_term_rs",
      "/code/tcode",
    ]);
  });

  // The point of storing only the folders that were moved: opening a new one must
  // not scatter the ones already placed.
  it("appends a folder the arrangement has never heard of", () => {
    const { live } = railBands(
      [...SESSIONS, session("s4", "/code/pybond", "pybond")],
      [],
      ["/code/short_term_rs", "/code/tcode"],
    );
    expect(live.map((group) => group.path)).toEqual([
      "/code/short_term_rs",
      "/code/tcode",
      "/code/pybond",
    ]);
  });

  it("leaves a stored folder that is no longer open out of the live band", () => {
    const { live } = railBands(
      SESSIONS,
      [],
      ["/code/gone", "/code/short_term_rs"],
    );
    expect(live.map((group) => group.path)).toEqual([
      "/code/short_term_rs",
      "/code/tcode",
    ]);
  });

  // The whole reason the rail could absorb the launchpad: a folder with nothing
  // open in it is still somewhere you can go.
  it("lists visited folders with nothing open under recent, newest first", () => {
    const { live, recent } = railBands(SESSIONS, PROJECTS, []);

    expect(live.map((group) => group.path)).toEqual([
      "/code/tcode",
      "/code/short_term_rs",
    ]);
    expect(recent.map((group) => group.path)).toEqual([
      "/code/pybond",
      "/code/duck_ext",
    ]);
    expect(recent[0].sessions).toEqual([]);
    // The row needs the project's own facts to say when you were last there.
    expect(recent[0].info?.last_active).toBe(700);
  });

  // A folder cannot be in both bands: that was the launchpad's actual defect,
  // where "Open" and "Projects" listed the same folder twice.
  it("never puts one folder in both bands", () => {
    const { live, recent } = railBands(SESSIONS, PROJECTS, []);
    const both = live.filter((group) =>
      recent.some((other) => other.path === group.path),
    );
    expect(both).toEqual([]);
  });

  it("caps recent and reports what it left for the finder", () => {
    const many = Array.from({ length: 12 }, (_at, index) =>
      project(`/code/p${index}`, `p${index}`, 1000 - index),
    );
    const { recent, overflow } = railBands([], many, [], [], 8);

    expect(recent).toHaveLength(8);
    expect(overflow).toBe(4);
    expect(recent[0].path).toBe("/code/p0");
  });

  it("hides a folder from recent without touching the live band", () => {
    const { live, recent } = railBands(
      SESSIONS,
      PROJECTS,
      [],
      ["/code/pybond", "/code/tcode"],
    );

    expect(recent.map((group) => group.path)).toEqual(["/code/duck_ext"]);
    // Hiding must not be able to swallow a conversation that is open: the rail's
    // whole job is to account for those.
    expect(live.map((group) => group.path)).toContain("/code/tcode");
  });
});

describe("moveProject", () => {
  const { live } = railBands(SESSIONS, [], []);

  it("writes out the whole order, not just the folder that moved", () => {
    expect(moveProject(live, "/code/short_term_rs", 0)).toEqual([
      "/code/short_term_rs",
      "/code/tcode",
    ]);
  });

  it("refuses to move past either end rather than clamping silently", () => {
    expect(moveProject(live, "/code/tcode", -1)).toEqual([
      "/code/tcode",
      "/code/short_term_rs",
    ]);
    expect(moveProject(live, "/code/short_term_rs", 2)).toEqual([
      "/code/tcode",
      "/code/short_term_rs",
    ]);
  });
});

describe("find", () => {
  const open: FoundSession[] = [
    {
      kind: "session",
      session: SESSIONS[0],
      title: "Make the retry path testable",
      activity: "thinking",
      status: "running",
    },
    {
      kind: "session",
      session: SESSIONS[1],
      title: "port the ledger",
      activity: "done",
      status: "idle",
    },
  ];

  it("shows everything with an empty query, conversations first", () => {
    const hits = find("", open, PROJECTS);
    expect(hits).toHaveLength(open.length + PROJECTS.length);
    expect(hits.slice(0, 2).every((entry) => entry.kind === "session")).toBe(
      true,
    );
  });

  it("matches a conversation by what it was asked to do", () => {
    const hits = find("retry", open, PROJECTS);
    expect(
      hits.map((entry) =>
        entry.kind === "session" ? entry.title : entry.project.name,
      ),
    ).toEqual(["Make the retry path testable"]);
  });

  // The folder is a conversation's other name, so typing it must find both the
  // conversations in it and the folder itself.
  it("matches a folder by name and by path, across both kinds", () => {
    const hits = find("tcode", open, PROJECTS);
    expect(
      hits.map((entry) =>
        entry.kind === "session" ? entry.session.id : entry.project.path,
      ),
    ).toEqual(["s1", "/code/tcode"]);

    expect(find("/code/py", open, PROJECTS).map((entry) => entry.kind)).toEqual(
      ["project"],
    );
  });

  it("is case-insensitive and ignores surrounding space", () => {
    expect(find("  PYBOND ", open, PROJECTS)).toHaveLength(1);
  });

  it("answers nothing rather than something approximate", () => {
    expect(find("nothing here", open, PROJECTS)).toEqual([]);
  });
});

describe("sessionTitle", () => {
  it("names a conversation after the first thing it was asked for", () => {
    const blocks: Block[] = [
      { kind: "note", text: "resumed" },
      {
        kind: "user",
        text: "Make the retry path testable.\nIt sleeps for real right now.",
      },
      { kind: "user", text: "also cover the cap" },
    ];
    expect(sessionTitle(blocks)).toBe("Make the retry path testable.");
  });

  it("has nothing to say about a conversation nobody has typed into", () => {
    expect(sessionTitle([])).toBeNull();
    expect(sessionTitle([{ kind: "assistant", text: "ready" }])).toBeNull();
  });

  it("still names one that opened with an image", () => {
    expect(
      sessionTitle([
        { kind: "user", text: "  ", images: ["data:image/png;base64,x"] },
      ]),
    ).toBe("image");
  });
});
