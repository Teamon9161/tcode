import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { ProjectInfo, StoredSessionsPage } from "./types";

const mocks = vi.hoisted(() => ({ invoke: vi.fn(), open: vi.fn() }));
vi.mock("@ipc", () => ({ invoke: mocks.invoke }));

import { RailProject } from "./RailProject";

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

let root: Root;
let container: HTMLDivElement;

const project: ProjectInfo = {
  path: "/code/tcode",
  name: "tcode",
  session_count: 42,
  last_active: 900,
  exists: true,
};

async function settle() {
  await act(async () => {
    for (let index = 0; index < 4; index += 1) await Promise.resolve();
  });
}

async function draw(openLogs = new Set<string>()) {
  await act(async () => {
    root.render(
      <RailProject
        project={project}
        now={1_000}
        openLogs={openLogs}
        onHide={() => {}}
        onOpenFolder={mocks.open}
      />,
    );
  });
}

function button(label: string) {
  return [...container.querySelectorAll<HTMLButtonElement>("button")].find((candidate) =>
    candidate.textContent?.trim().startsWith(label),
  );
}

beforeEach(() => {
  mocks.invoke.mockReset();
  mocks.open.mockReset();
  mocks.open.mockResolvedValue(undefined);
  container = document.createElement("div");
  document.body.append(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

describe("paged project history", () => {
  it("requests the first cursor page only after expansion, then appends the next page", async () => {
    const first: StoredSessionsPage = {
      sessions: [
        { id: "newest", preview: "newest work", modified: 990 },
        { id: "cursor", preview: "middle work", modified: 980 },
      ],
      next: "cursor",
    };
    const second: StoredSessionsPage = {
      sessions: [
        { id: "older", preview: "older work", modified: 970 },
        { id: "newest", preview: "duplicate must be ignored", modified: 960 },
      ],
      next: null,
    };
    mocks.invoke.mockImplementation((_command: string, args: { before: string | null }) =>
      Promise.resolve(args.before ? second : first),
    );

    await draw(new Set(["newest"]));
    expect(mocks.invoke).not.toHaveBeenCalled();

    act(() => container.querySelector<HTMLButtonElement>(".rail-project-head")?.click());
    await settle();

    expect(mocks.invoke).toHaveBeenCalledExactlyOnceWith("project_sessions", {
      path: "/code/tcode",
      before: null,
    });
    expect(container.textContent).toContain("newest work");
    expect(container.textContent).toContain("middle work");
    expect(button("Load older")).toBeDefined();
    expect(container.textContent).toContain("2 loaded");
    const held = [...container.querySelectorAll<HTMLButtonElement>(".rail-stored")].find(
      (row) => row.textContent?.includes("newest work"),
    );
    expect(held?.disabled).toBe(true);
    expect(held?.textContent).toContain("open");

    act(() => button("Load older")?.click());
    await settle();

    expect(mocks.invoke).toHaveBeenLastCalledWith("project_sessions", {
      path: "/code/tcode",
      before: "cursor",
    });
    expect(container.textContent).toContain("newest work");
    expect(container.textContent).toContain("middle work");
    expect(container.textContent).toContain("older work");
    expect(container.querySelectorAll(".rail-stored")).toHaveLength(3);
    expect(button("Load older")).toBeUndefined();
  });

  it("keeps loaded rows and offers the same cursor again when an older page fails", async () => {
    mocks.invoke
      .mockResolvedValueOnce({
        sessions: [{ id: "cursor", preview: "kept work", modified: 990 }],
        next: "cursor",
      } satisfies StoredSessionsPage)
      .mockRejectedValueOnce(new Error("disk asleep"))
      .mockResolvedValueOnce({
        sessions: [{ id: "older", preview: "recovered work", modified: 980 }],
        next: null,
      } satisfies StoredSessionsPage);

    await draw();
    act(() => container.querySelector<HTMLButtonElement>(".rail-project-head")?.click());
    await settle();
    act(() => button("Load older")?.click());
    await settle();

    expect(container.textContent).toContain("kept work");
    expect(container.textContent).toContain("disk asleep");
    expect(button("Retry")).toBeDefined();

    act(() => button("Retry")?.click());
    await settle();

    expect(mocks.invoke).toHaveBeenLastCalledWith("project_sessions", {
      path: "/code/tcode",
      before: "cursor",
    });
    expect(container.textContent).toContain("kept work");
    expect(container.textContent).toContain("recovered work");
  });
});
