import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { ProjectInfo, ProjectList, SessionInfo } from "./types";
import { BLANK, type SessionState } from "./session";

const mocks = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("@ipc", () => ({ invoke: mocks.invoke }));
vi.mock("./RailProject", () => ({
  RailProject: ({ project }: { project: ProjectInfo }) => (
    <li data-project={project.path}>{project.name}</li>
  ),
}));

import { Rail } from "./Rail";

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

let root: Root;
let container: HTMLDivElement;

const sessions: SessionInfo[] = [
  { id: "first", cwd: "/code/tcode", name: "tcode", home: "/home/me", log_id: "log-a" },
  { id: "second", cwd: "/code/duck", name: "duck", home: "/home/me", log_id: "log-b" },
  { id: "third", cwd: "/code/bonds", name: "bonds", home: "/home/me", log_id: null },
];

const states: Record<string, SessionState> = {
  first: { ...BLANK, blocks: [{ kind: "user", text: "First prompt" }], activity: "waiting" },
  second: { ...BLANK, blocks: [{ kind: "user", text: "Second prompt" }], activity: "thinking" },
  third: { ...BLANK, blocks: [{ kind: "user", text: "Third prompt" }], activity: "idle" },
};

const project = (index: number): ProjectInfo => ({
  path: `/code/project-${index}`,
  name: `project-${index}`,
  session_count: index,
  last_active: 1_000 - index,
  exists: true,
});

async function draw(projects: ProjectInfo[]) {
  const data: ProjectList = { projects, now: 1_000, home: "/home/me" };
  mocks.invoke.mockResolvedValue(data);
  await act(async () => {
    root.render(
      <Rail
        sessions={sessions}
        onScreen={new Set(["second"])}
        stateOf={(id) => states[id]}
        statusOf={(id) => (id === "first" ? "waiting" : "idle")}
        onShow={() => {}}
        onCloseSession={() => {}}
        onOpenFolder={async () => {}}
      />,
    );
    await Promise.resolve();
    await Promise.resolve();
  });
}

beforeEach(() => {
  localStorage.clear();
  mocks.invoke.mockReset();
  container = document.createElement("div");
  document.body.append(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

describe("the session-first rail", () => {
  it("numbers live sessions in the exact order used by Mod+1…9", async () => {
    await draw([]);

    const rows = [...container.querySelectorAll<HTMLButtonElement>(".rail-item")];
    expect(rows).toHaveLength(3);
    expect(rows.map((row) => row.querySelector(".rail-shortcut")?.textContent)).toEqual([
      "1",
      "2",
      "3",
    ]);
    expect(rows.map((row) => row.getAttribute("aria-keyshortcuts"))).toEqual([
      "Control+1 Meta+1",
      "Control+2 Meta+2",
      "Control+3 Meta+3",
    ]);
    expect(rows.map((row) => row.querySelector(".rail-name")?.textContent)).toEqual([
      "First prompt",
      "Second prompt",
      "Third prompt",
    ]);
    expect(rows[1].classList.contains("is-onscreen")).toBe(true);
  });

  it("reveals Recent projects eight at a time rather than expanding the whole list", async () => {
    await draw(Array.from({ length: 18 }, (_, index) => project(index + 1)));

    expect(container.querySelectorAll("[data-project]")).toHaveLength(8);
    const more = [...container.querySelectorAll<HTMLButtonElement>("button")].find((button) =>
      button.textContent?.includes("Show 8 more"),
    );
    expect(more).toBeDefined();

    act(() => more?.click());

    expect(container.querySelectorAll("[data-project]")).toHaveLength(16);
    expect(container.textContent).toContain("Show 2 more");
    expect(container.textContent).toContain("2 remaining");
  });
});
