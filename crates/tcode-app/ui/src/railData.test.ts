import { describe, expect, it } from "vitest";

import type { Block } from "./blocks";
import type { ProjectInfo, SessionInfo } from "./types";
import {
  find,
  recentProjects,
  sessionTitle,
  type FoundSession,
} from "./railData";

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

describe("recentProjects", () => {
  it("lists projects newest first even when one also has an open session", () => {
    const recent = recentProjects(PROJECTS);
    expect(recent.projects.map((entry) => entry.path)).toEqual([
      "/code/tcode",
      "/code/short_term_rs",
      "/code/pybond",
      "/code/duck_ext",
    ]);
  });

  it("reveals a bounded step and reports the remaining project count", () => {
    const many = Array.from({ length: 12 }, (_at, index) =>
      project(`/code/p${index}`, `p${index}`, 1000 - index),
    );
    const recent = recentProjects(many, [], 8);
    expect(recent.projects).toHaveLength(8);
    expect(recent.overflow).toBe(4);
  });

  it("hides only the selected recent project", () => {
    expect(
      recentProjects(PROJECTS, ["/code/pybond"]).projects.map(
        (entry) => entry.path,
      ),
    ).toEqual([
      "/code/tcode",
      "/code/short_term_rs",
      "/code/duck_ext",
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
