import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const probes = vi.hoisted(() => ({
  web: vi.fn(),
  transcript: vi.fn(),
  box: { left: 0, top: 0, width: 500, height: 600 },
  invoke: vi.fn((name: string) =>
    Promise.resolve(name === "browser_open" ? "tab-1" : undefined),
  ),
}));

vi.mock("./WebPane", () => ({
  WebPane: ({ bodyRef }: { bodyRef: { current: HTMLDivElement | null } }) => {
    probes.web();
    return (
      <div
        ref={(node) => {
          bodyRef.current = node;
          if (node) node.getBoundingClientRect = () => probes.box as DOMRect;
        }}
        data-fake-web
      />
    );
  },
}));
vi.mock("./TermPane", () => ({ TermPane: () => <div data-fake-term /> }));
vi.mock("./Transcript", () => ({
  Transcript: () => {
    probes.transcript();
    return <div data-fake-transcript />;
  },
}));
vi.mock("./Composer", () => ({ Composer: () => <div data-fake-composer /> }));
vi.mock("@ipc", () => ({
  invoke: probes.invoke,
  listen: vi.fn().mockResolvedValue(() => {}),
}));

import type { Leaf, Tiling } from "./layout";
import { Panes, type PaneContext } from "./Panes";
import { BLANK, type SessionState } from "./session";
import * as browser from "./webHost";
import { yieldBrowser } from "./browserYield";
import type { SessionInfo, Status } from "./types";

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

class FakeResizeObserver {
  observe() {}
  disconnect() {}
}
vi.stubGlobal("ResizeObserver", FakeResizeObserver);

const frames: FrameRequestCallback[] = [];
vi.stubGlobal("requestAnimationFrame", (frame: FrameRequestCallback) => {
  frames.push(frame);
  return frames.length;
});
vi.stubGlobal("cancelAnimationFrame", () => {});

async function flushFrames() {
  await act(async () => {
    for (const frame of frames.splice(0)) frame(0);
    await Promise.resolve();
    await Promise.resolve();
  });
}

const web = { kind: "leaf", id: "web", pane: { kind: "web" } } as const;
const terminal = {
  kind: "leaf",
  id: "terminal",
  pane: { kind: "terminal" },
} as const;
const sessionA = {
  kind: "leaf",
  id: "pane-a",
  pane: { kind: "session", session: "a" },
} as const;
const sessionB = {
  kind: "leaf",
  id: "pane-b",
  pane: { kind: "session", session: "b" },
} as const;

function split(a: Leaf, b: Leaf, ratio: number, focus = a.id): Tiling {
  return {
    root: {
      kind: "split",
      id: "split",
      dir: "row",
      ratio,
      a,
      b,
    },
    focus,
  };
}

function paneContext(sessions: SessionInfo[] = []): PaneContext {
  const none = () => {};
  return {
    sessions,
    focus: "web",
    split: true,
    onFocus: none,
    onClosePane: none,
    onRotate: none,
    onSwap: none,
    onRatio: none,
    onOpen: none,
    onOpenAside: none,
    onMention: none,
    onNavigate: none,
    onToggleFiles: none,
    onToggleWorkspace: none,
    onOpenBrowser: none,
    onHandOverTab: none,
    onRevealBrowserTab: none,
    onToggleTerminal: none,
    terminalCwd: "/project",
    onOpenUrl: none,
    webRequest: null,
    expanded: null,
    onToggleExpanded: none,
    onDraft: none,
    onAttach: none,
    onDetach: none,
    onSend: none,
    onInterrupt: none,
    onWithdrawQueued: none,
    onSendQueuedNow: none,
    onAskRewind: none,
    onRewind: none,
    onAnswer: none,
    onDecidePlan: none,
    onPlanDraft: none,
    onSavePlan: none,
    onPlanOpen: none,
    onPlanFirst: none,
    onOpenFolder: async () => {},
  };
}

const idleState = () => BLANK;
const idleStatus = (): Status => "idle";

let root: Root;
let container: HTMLDivElement;

beforeEach(() => {
  probes.web.mockClear();
  probes.transcript.mockClear();
  probes.invoke.mockClear();
  probes.box = { left: 0, top: 0, width: 500, height: 600 };
  frames.length = 0;
  browser.reset();
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

describe("pane rendering cost", () => {
  it("updates divider geometry without re-rendering an unchanged pane subtree", async () => {
    const context = paneContext();
    act(() =>
      root.render(
        <Panes
          tiling={split(web, terminal, 0.5)}
          context={context}
          stateOf={idleState}
          statusOf={idleStatus}
        />,
      ),
    );
    await flushFrames();
    expect(probes.web).toHaveBeenCalledTimes(1);

    probes.box = { left: 0, top: 0, width: 700, height: 600 };
    act(() =>
      root.render(
        <Panes
          tiling={split(web, terminal, 0.7)}
          context={context}
          stateOf={idleState}
          statusOf={idleStatus}
        />,
      ),
    );
    await flushFrames();

    expect(probes.web).toHaveBeenCalledTimes(1);
    expect(container.querySelector<HTMLElement>(".pane-slot")?.style.width).toBe("70%");
    expect(probes.invoke).toHaveBeenCalledWith("browser_bounds", {
      rect: { x: 0, y: 0, width: 700, height: 600 },
    });
  });

  it("coalesces browser geometry to one final IPC update while resizing", async () => {
    const context = paneContext();
    act(() =>
      root.render(
        <Panes
          tiling={split(web, terminal, 0.5)}
          context={context}
          stateOf={idleState}
          statusOf={idleStatus}
        />,
      ),
    );
    await flushFrames();
    probes.invoke.mockClear();

    const restore = yieldBrowser();
    probes.invoke.mockClear();
    for (const [ratio, width] of [
      [0.55, 550],
      [0.62, 620],
      [0.7, 700],
    ] as const) {
      probes.box = { left: 0, top: 0, width, height: 600 };
      act(() =>
        root.render(
          <Panes
            tiling={split(web, terminal, ratio)}
            context={context}
            stateOf={idleState}
            statusOf={idleStatus}
          />,
        ),
      );
      await flushFrames();
    }

    expect(
      probes.invoke.mock.calls.filter(([name]) => name === "browser_bounds"),
    ).toHaveLength(0);

    restore();
    await flushFrames();

    expect(
      probes.invoke.mock.calls.filter(([name]) => name === "browser_bounds"),
    ).toEqual([
      ["browser_bounds", { rect: { x: 0, y: 0, width: 700, height: 600 } }],
    ]);
  });

  it("reports a pure browser-pane translation without re-rendering its subtree", async () => {
    const context = paneContext();
    act(() =>
      root.render(
        <Panes
          tiling={split(web, terminal, 0.5)}
          context={context}
          stateOf={idleState}
          statusOf={idleStatus}
        />,
      ),
    );
    await flushFrames();
    probes.invoke.mockClear();

    probes.box = { left: 500, top: 0, width: 500, height: 600 };
    act(() =>
      root.render(
        <Panes
          tiling={split(terminal, web, 0.5, "web")}
          context={context}
          stateOf={idleState}
          statusOf={idleStatus}
        />,
      ),
    );
    await flushFrames();

    expect(probes.web).toHaveBeenCalledTimes(1);
    expect(probes.invoke).toHaveBeenCalledWith("browser_bounds", {
      rect: { x: 500, y: 0, width: 500, height: 600 },
    });
  });

  it("re-renders only the session whose selected state changed", () => {
    const sessions: SessionInfo[] = [
      { id: "a", cwd: "/a", name: "a", home: "/home", log_id: null },
      { id: "b", cwd: "/b", name: "b", home: "/home", log_id: null },
    ];
    const context = { ...paneContext(sessions), focus: "pane-a" };
    const first: Record<string, SessionState> = {
      a: { ...BLANK },
      b: { ...BLANK },
    };
    act(() =>
      root.render(
        <Panes
          tiling={split(sessionA, sessionB, 0.5, "pane-a")}
          context={context}
          stateOf={(id) => first[id]}
          statusOf={idleStatus}
        />,
      ),
    );
    expect(probes.transcript).toHaveBeenCalledTimes(2);
    probes.transcript.mockClear();

    const second: Record<string, SessionState> = {
      ...first,
      a: { ...first.a, running: true },
    };
    act(() =>
      root.render(
        <Panes
          tiling={split(sessionA, sessionB, 0.5, "pane-a")}
          context={context}
          stateOf={(id) => second[id]}
          statusOf={idleStatus}
        />,
      ),
    );

    expect(probes.transcript).toHaveBeenCalledTimes(1);
  });
});
