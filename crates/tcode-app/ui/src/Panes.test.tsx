import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { EditorView } from "@codemirror/view";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { WorkspaceBinaryView, WorkspaceTextView } from "./types";

const mocks = vi.hoisted(() => {
  Object.defineProperty(HTMLCanvasElement.prototype, "getContext", {
    value: () => null,
    configurable: true,
  });
  if (!globalThis.IntersectionObserver) {
    (globalThis as any).IntersectionObserver = class {
      observe() {}
      unobserve() {}
      disconnect() {}
    };
  }
  return { invoke: vi.fn(), listen: vi.fn().mockResolvedValue(() => {}) };
});
vi.mock("@ipc", () => ({ invoke: mocks.invoke, listen: mocks.listen }));
vi.mock("pdfjs-dist", () => ({
  GlobalWorkerOptions: {},
  TextLayer: class {
    container: HTMLElement;

    constructor({ container }: { container: HTMLElement }) {
      this.container = container;
    }

    render = vi.fn().mockImplementation(() => {
      this.container.textContent = "Paper selection";
      return Promise.resolve();
    });
    cancel = vi.fn();
  },
  getDocument: vi.fn(() => ({
    destroy: vi.fn(),
    promise: Promise.resolve({
      numPages: 1,
      cleanup: vi.fn().mockResolvedValue(undefined),
      getOutline: vi.fn().mockResolvedValue(null),
      getPage: vi.fn().mockResolvedValue({
        getViewport: vi.fn(() => ({ width: 600, height: 800 })),
        render: vi.fn(() => ({ cancel: vi.fn(), promise: Promise.resolve() })),
        streamTextContent: vi.fn(),
      }),
    }),
  })),
}));
vi.mock("pdfjs-dist/build/pdf.worker.mjs?url", () => ({ default: "pdf.worker.mjs" }));

import { browserPlacementHeld, resetBrowserVisibility } from "./browserYield";
import { navOf, type Inspect } from "./inspect";
import type { Tiling } from "./layout";
import { Panes, type PaneContext } from "./Panes";
import { BLANK } from "./session";

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
Object.defineProperty(Range.prototype, "getClientRects", { value: () => [], configurable: true });

let root: Root;
let container: HTMLDivElement;
let context: PaneContext;

const textView = (path: string, text: string): WorkspaceTextView => ({
  path,
  text,
  revision: `revision:${path}`,
  fingerprint: `fingerprint:${path}`,
  bytes: text.length,
  truncated: false,
});

function tiling(value: Inspect): Tiling {
  return {
    root: {
      kind: "leaf",
      id: "inspect-pane",
      pane: { kind: "inspect", session: "s", nav: navOf(value) },
    },
    focus: "inspect-pane",
  };
}

function twoSessions(): Tiling {
  return {
    root: {
      kind: "split",
      id: "split",
      dir: "row",
      ratio: 0.5,
      a: { kind: "leaf", id: "pane-a", pane: { kind: "session", session: "s" } },
      b: { kind: "leaf", id: "pane-b", pane: { kind: "session", session: "t" } },
    },
    focus: "pane-a",
  };
}

function rect(left: number, top: number, width: number, height: number): DOMRect {
  return {
    x: left,
    y: top,
    left,
    top,
    width,
    height,
    right: left + width,
    bottom: top + height,
    toJSON: () => ({}),
  };
}

function pointer(type: string, x: number, y: number, pointerId = 1): PointerEvent {
  const event = new MouseEvent(type, {
    clientX: x,
    clientY: y,
    button: 0,
    bubbles: true,
    cancelable: true,
  }) as PointerEvent;
  Object.defineProperties(event, {
    pointerId: { value: pointerId },
    isPrimary: { value: true },
  });
  return event;
}

function paneContext(focus = "inspect-pane"): PaneContext {
  const none = () => {};
  return {
    sessions: [{ id: "s", cwd: "/project", name: "project", home: "/home/me", log_id: null }],
    focus,
    split: true,
    onFocus: none,
    onClosePane: none,
    onMovePane: none,
    onPaneDragging: none,
    onRotate: none,
    onSwap: none,
    onRatio: none,
    onOpen: none,
    onOpenAside: none,
    onMention: none,
    onPaperPrompt: none,
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
    onChangeFolder: async () => {},
  };
}

async function draw(value: Inspect, nextContext = context) {
  await act(async () => {
    root.render(
      <Panes
        tiling={tiling(value)}
        context={nextContext}
        stateOf={() => BLANK}
        statusOf={() => "idle"}
      />,
    );
    await Promise.resolve();
    await Promise.resolve();
  });
}

async function drawSession(nextContext = context) {
  await act(async () => {
    root.render(
      <Panes
        tiling={{
          root: { kind: "leaf", id: "session-pane", pane: { kind: "session", session: "s" } },
          focus: "session-pane",
        }}
        context={nextContext}
        stateOf={() => BLANK}
        statusOf={() => "idle"}
      />,
    );
    await Promise.resolve();
    await Promise.resolve();
  });
}

async function drawTwo(nextContext: PaneContext) {
  mocks.invoke.mockResolvedValue({
    models: [],
    role_models: [],
    model: -1,
    effort: null,
    context_window: 0,
    presets: [],
    preset: null,
    roles: [],
    modes: [],
    mode: "default",
    mode_staged: false,
    can_view_images: false,
  });
  await act(async () => {
    root.render(
      <Panes
        tiling={twoSessions()}
        context={nextContext}
        stateOf={() => BLANK}
        statusOf={() => "idle"}
      />,
    );
    await Promise.resolve();
  });
  const field = container.querySelector<HTMLElement>(".panes-field")!;
  const a = container.querySelector<HTMLElement>('[data-pane="pane-a"]')!;
  const b = container.querySelector<HTMLElement>('[data-pane="pane-b"]')!;
  field.getBoundingClientRect = () => rect(0, 0, 1000, 600);
  a.getBoundingClientRect = () => rect(0, 0, 500, 600);
  b.getBoundingClientRect = () => rect(500, 0, 500, 600);
  return { a, b };
}

function editor(): EditorView | null {
  const dom = container.querySelector<HTMLElement>(".cm-editor");
  return dom ? EditorView.findFromDOM(dom) : null;
}

function textSpan(text: string, box: DOMRect): HTMLSpanElement {
  const span = document.createElement("span");
  span.textContent = text;
  span.getBoundingClientRect = () => box;
  return span;
}

beforeEach(() => {
  resetBrowserVisibility();
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
  context = paneContext();
  mocks.invoke.mockReset();
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

describe("pane header drag", () => {
  it("commits one center exchange only after release", async () => {
    const onMovePane = vi.fn();
    const onPaneDragging = vi.fn();
    const next = {
      ...paneContext("pane-a"),
      sessions: [
        { id: "s", cwd: "/a", name: "a", home: "/home/me", log_id: null },
        { id: "t", cwd: "/b", name: "b", home: "/home/me", log_id: null },
      ],
      onMovePane,
      onPaneDragging,
    };
    const { a } = await drawTwo(next);
    const header = a.querySelector<HTMLElement>(".pane-head")!;

    act(() => {
      header.dispatchEvent(pointer("pointerdown", 100, 16));
      window.dispatchEvent(pointer("pointermove", 750, 300));
    });
    expect(onMovePane).not.toHaveBeenCalled();
    expect(onPaneDragging).toHaveBeenCalledWith(true);
    expect(browserPlacementHeld()).toBe(true);
    expect(container.querySelector(".pane-drop-preview.is-center")?.textContent).toBe(
      "Exchange panes",
    );

    act(() => window.dispatchEvent(pointer("pointerup", 750, 300)));
    expect(onMovePane).toHaveBeenCalledOnce();
    expect(onMovePane).toHaveBeenCalledWith("pane-a", "pane-b", "center");
    expect(onPaneDragging).toHaveBeenLastCalledWith(false);
  });

  it.each([
    [510, 300, "left", "Place left"],
    [990, 300, "right", "Place right"],
    [750, 5, "up", "Place above"],
    [750, 595, "down", "Place below"],
  ] as const)("classifies the target edge as %s,%s %s", async (x, y, zone, label) => {
    const onMovePane = vi.fn();
    const next = {
      ...paneContext("pane-a"),
      sessions: [
        { id: "s", cwd: "/a", name: "a", home: "/home/me", log_id: null },
        { id: "t", cwd: "/b", name: "b", home: "/home/me", log_id: null },
      ],
      onMovePane,
    };
    const { a } = await drawTwo(next);
    const header = a.querySelector<HTMLElement>(".pane-head")!;

    act(() => {
      header.dispatchEvent(pointer("pointerdown", 100, 16));
      window.dispatchEvent(pointer("pointermove", x, y));
      window.dispatchEvent(pointer("pointerup", x, y));
    });
    expect(onMovePane).toHaveBeenCalledWith("pane-a", "pane-b", zone);
    expect(container.textContent).not.toContain(label);
  });

  it("survives the focus render caused by its own pointer down", async () => {
    const onMovePane = vi.fn();
    let next = {
      ...paneContext("pane-b"),
      sessions: [
        { id: "s", cwd: "/a", name: "a", home: "/home/me", log_id: null },
        { id: "t", cwd: "/b", name: "b", home: "/home/me", log_id: null },
      ],
      onMovePane,
      onFocus: (focus: string) => {
        next = { ...next, focus };
        root.render(
          <Panes
            tiling={twoSessions()}
            context={next}
            stateOf={() => BLANK}
            statusOf={() => "idle"}
          />,
        );
      },
    };
    const { a } = await drawTwo(next);
    const header = a.querySelector<HTMLElement>(".pane-head")!;

    act(() => {
      header.dispatchEvent(pointer("pointerdown", 100, 16));
      window.dispatchEvent(pointer("pointermove", 750, 300));
      window.dispatchEvent(pointer("pointerup", 750, 300));
    });
    expect(onMovePane).toHaveBeenCalledWith("pane-a", "pane-b", "center");
  });

  it("cancels with Escape and does not steal a header button", async () => {
    const onMovePane = vi.fn();
    const onPaneDragging = vi.fn();
    const next = {
      ...paneContext("pane-a"),
      sessions: [
        { id: "s", cwd: "/a", name: "a", home: "/home/me", log_id: null },
        { id: "t", cwd: "/b", name: "b", home: "/home/me", log_id: null },
      ],
      onMovePane,
      onPaneDragging,
    };
    const { a } = await drawTwo(next);
    const header = a.querySelector<HTMLElement>(".pane-head")!;
    act(() => {
      header.dispatchEvent(pointer("pointerdown", 100, 16));
      window.dispatchEvent(pointer("pointermove", 750, 300));
      window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", cancelable: true }));
    });
    expect(onMovePane).not.toHaveBeenCalled();
    expect(onPaneDragging).toHaveBeenLastCalledWith(false);
    expect(browserPlacementHeld()).toBe(false);
    expect(container.querySelector(".pane-drop-preview")).toBeNull();

    const close = a.querySelector<HTMLElement>('[aria-label="Hide a"]')!;
    act(() => {
      close.dispatchEvent(pointer("pointerdown", 450, 16));
      window.dispatchEvent(pointer("pointermove", 750, 300));
      window.dispatchEvent(pointer("pointerup", 750, 300));
    });
    expect(onPaneDragging).toHaveBeenCalledTimes(2);
    expect(onMovePane).not.toHaveBeenCalled();
  });
});

describe("conversation pane folder switcher", () => {
  it("shows only the directory name and keeps its full path on hover", async () => {
    mocks.invoke.mockResolvedValue({
      models: [],
      role_models: [],
      model: -1,
      effort: null,
      context_window: 0,
      presets: [],
      preset: null,
      roles: [],
      modes: [],
      mode: "default",
      mode_staged: false,
      can_view_images: false,
    });
    await drawSession();

    const chip = container.querySelector<HTMLButtonElement>(".folder-chip")!;
    expect(chip.textContent).toBe("project");
    expect(chip.title).toBe("/project");
    expect(chip.getAttribute("aria-label")).toBe("Switch directory for project: /project");
  });
});

describe("paper pane", () => {
  it("names the PDF and keeps workspace file controls away", async () => {
    mocks.invoke.mockImplementation((cmd: string) =>
      cmd === "paper_highlights_load" ? Promise.resolve([]) : Promise.resolve("http://127.0.0.1:1000/token/paper.pdf"),
    );
    await draw({ kind: "paper", path: "docs/paper.pdf" });

    const name = container.querySelector<HTMLElement>(".pane-name")!;
    expect(name.textContent).toBe("paper.pdf");
    expect(name.title).toBe("docs/paper.pdf");
    expect(container.querySelector(".pane-body.is-paper")).not.toBeNull();
    expect(container.querySelector('[aria-label="Read this file again"]')).toBeNull();
    expect(mocks.invoke).toHaveBeenCalledWith("serve_url", { session: "s", path: "docs/paper.pdf" });
  });

  it("turns selected PDF text into a composer draft action", async () => {
    const onPaperPrompt = vi.fn();
    context = { ...context, onPaperPrompt };
    mocks.invoke.mockImplementation((cmd: string) =>
      cmd === "paper_highlights_load" ? Promise.resolve([]) : Promise.resolve("http://127.0.0.1:1000/token/paper.pdf"),
    );
    await draw({ kind: "paper", path: "docs/paper.pdf" }, context);

    const layer = container.querySelector<HTMLElement>(".paper-text-layer")!;
    layer.textContent = "Paper selection";
    const textNode = layer.firstChild!;
    const selectionSpy = vi.spyOn(window, "getSelection").mockReturnValue({
      rangeCount: 1,
      isCollapsed: false,
      anchorNode: textNode,
      focusNode: textNode,
      toString: () => "Paper selection",
      getRangeAt: () => ({
        getClientRects: () => [{ left: 120, top: 160, width: 180, height: 24 }],
      }),
      removeAllRanges: vi.fn(),
    } as unknown as Selection);

    const stage = container.querySelector<HTMLElement>(".paper-stage")!;
    await act(async () => {
      stage.dispatchEvent(new MouseEvent("mouseup", { bubbles: true }));
      await Promise.resolve();
    });
    await act(async () => {
      container.querySelectorAll<HTMLButtonElement>(".paper-selection-menu .paper-menu-sep ~ .chip")[1]!.click();
      await Promise.resolve();
    });
    selectionSpy.mockRestore();

    expect(onPaperPrompt).toHaveBeenCalledWith(
      "s",
      'Explain the selected text from @paper("paper.pdf", page 1):\n\nPaper selection',
    );
  });
  it("adds selected PDF text as a readable highlight and removes it with one click", async () => {
    mocks.invoke.mockImplementation((cmd: string) =>
      cmd === "paper_highlights_load" ? Promise.resolve([]) : Promise.resolve("http://127.0.0.1:1000/token/paper.pdf"),
    );
    await draw({ kind: "paper", path: "docs/paper.pdf" });

    const layer = container.querySelector<HTMLElement>(".paper-text-layer")!;
    const shell = container.querySelector<HTMLElement>(".paper-page-shell")!;
    shell.getBoundingClientRect = () => rect(100, 100, 600, 800);
    layer.replaceChildren();
    const first = document.createElement("span");
    first.style.left = "16px";
    first.style.top = "180px";
    first.textContent = "risk";
    first.getBoundingClientRect = () => rect(116, 280, 180, 19);
    const second = document.createElement("span");
    second.style.left = "360px";
    second.style.top = "180px";
    second.textContent = "return";
    second.getBoundingClientRect = () => rect(460, 280, 221, 19);
    layer.append(first, second);
    const textNode = first.firstChild!;
    vi.spyOn(layer, "getBoundingClientRect").mockReturnValue(rect(100, 100, 600, 800));
    const selection = {
      rangeCount: 1,
      isCollapsed: false,
      anchorNode: textNode,
      focusNode: textNode,
      toString: () => "risk and return",
      getRangeAt: () => ({
        getClientRects: () => [
          rect(116, 280, 565, 19),
        ],
        intersectsNode: (node: Node) => node === first || node === second,
      }),
      removeAllRanges: vi.fn(),
    } as unknown as Selection;
    const selectionSpy = vi.spyOn(window, "getSelection").mockReturnValue(selection);

    const stage = container.querySelector<HTMLElement>(".paper-stage")!;
    await act(async () => {
      stage.dispatchEvent(new MouseEvent("mouseup", { bubbles: true }));
      await Promise.resolve();
    });
    await act(async () => {
      container.querySelector<HTMLButtonElement>('.paper-selection-menu [title="Highlight selected text"]')!.click();
      await Promise.resolve();
    });

    const saveCalls = mocks.invoke.mock.calls.filter(([cmd]) => cmd === "paper_highlights_save");
    expect(saveCalls).toHaveLength(1);
    expect(saveCalls[0][1].highlights[0].rects).toEqual([[16, 180, 565, 19]]);
    const marker = container.querySelector<HTMLElement>(".paper-highlight-rect")!;
    expect(marker.tagName).toBe("DIV");
    expect(marker.getAttribute("title")).toBe("risk and return");

    await act(async () => {
      stage.dispatchEvent(new MouseEvent("mouseup", { bubbles: true }));
      await Promise.resolve();
    });
    await act(async () => {
      container.querySelector<HTMLButtonElement>(".paper-selection-menu .chip")!.click();
      await Promise.resolve();
    });
    const lastSave = mocks.invoke.mock.calls.filter(([cmd]) => cmd === "paper_highlights_save").at(-1)!;
    expect(lastSave[1].highlights).toEqual([]);
    selectionSpy.mockRestore();
  });

  it("limits PDF text selection to the column under right-column whitespace", async () => {
    mocks.invoke.mockImplementation((cmd: string) =>
      cmd === "paper_highlights_load" ? Promise.resolve([]) : Promise.resolve("http://127.0.0.1:1000/token/paper.pdf"),
    );
    await draw({ kind: "paper", path: "docs/paper.pdf" });

    const layer = container.querySelector<HTMLElement>(".paper-text-layer")!;
    layer.replaceChildren();
    const left = textSpan("left", rect(116, 280, 220, 18));
    const right = textSpan("right", rect(706, 280, 240, 18));
    const leftNext = textSpan("left next", rect(116, 306, 250, 18));
    const rightNext = textSpan("right next", rect(706, 306, 230, 18));
    layer.append(left, right, leftNext, rightNext);

    act(() => {
      layer.dispatchEvent(new MouseEvent("mousedown", { button: 0, bubbles: true, clientX: 690, clientY: 280 }));
    });
    expect(left.style.userSelect).toBe("none");
    expect(right.style.userSelect).toBe("");
    act(() => {
      window.dispatchEvent(new MouseEvent("mouseup", { bubbles: true }));
    });

    act(() => {
      layer.dispatchEvent(new MouseEvent("mousedown", { button: 0, bubbles: true, clientX: 980, clientY: 280 }));
    });
    expect(left.style.userSelect).toBe("none");
    expect(right.style.userSelect).toBe("");
    act(() => {
      window.dispatchEvent(new MouseEvent("mouseup", { bubbles: true }));
    });
    expect(left.style.userSelect).toBe("");
  });

  it("keeps right-column text selected when the drag ends in trailing whitespace", async () => {
    mocks.invoke.mockImplementation((cmd: string) =>
      cmd === "paper_highlights_load" ? Promise.resolve([]) : Promise.resolve("http://127.0.0.1:1000/token/paper.pdf"),
    );
    await draw({ kind: "paper", path: "docs/paper.pdf" });

    const layer = container.querySelector<HTMLElement>(".paper-text-layer")!;
    layer.replaceChildren();
    const left = textSpan("left", rect(116, 280, 220, 18));
    const right = textSpan("right words", rect(706, 280, 240, 18));
    const leftNext = textSpan("left next", rect(116, 306, 250, 18));
    const rightNext = textSpan("right next", rect(706, 306, 230, 18));
    layer.append(left, right, leftNext, rightNext);

    const originalCaret = document.caretPositionFromPoint;
    Object.defineProperty(document, "caretPositionFromPoint", {
      configurable: true,
      value: (x: number) => {
        if (x < 800) return { offsetNode: right.firstChild!, offset: 0, getClientRect: () => null };
        return { offsetNode: layer, offset: 0, getClientRect: () => null };
      },
    });
    try {
      act(() => {
        layer.dispatchEvent(new MouseEvent("mousedown", { button: 0, bubbles: true, clientX: 706, clientY: 280 }));
        window.dispatchEvent(new MouseEvent("mouseup", { bubbles: true, clientX: 980, clientY: 280 }));
      });
      expect(window.getSelection()?.toString()).toBe("right words");
      expect(left.style.userSelect).toBe("");
    } finally {
      Object.defineProperty(document, "caretPositionFromPoint", {
        configurable: true,
        value: originalCaret,
      });
    }
  });

  it("does not synthesize a selection from a click in column whitespace", async () => {
    mocks.invoke.mockImplementation((cmd: string) =>
      cmd === "paper_highlights_load" ? Promise.resolve([]) : Promise.resolve("http://127.0.0.1:1000/token/paper.pdf"),
    );
    await draw({ kind: "paper", path: "docs/paper.pdf" });

    const layer = container.querySelector<HTMLElement>(".paper-text-layer")!;
    layer.replaceChildren();
    const left = textSpan("left", rect(116, 280, 220, 18));
    const right = textSpan("right words", rect(706, 280, 240, 18));
    const leftNext = textSpan("left next", rect(116, 306, 250, 18));
    const rightNext = textSpan("right next", rect(706, 306, 230, 18));
    layer.append(left, right, leftNext, rightNext);

    const originalCaret = document.caretPositionFromPoint;
    Object.defineProperty(document, "caretPositionFromPoint", {
      configurable: true,
      value: () => ({ offsetNode: layer, offset: 0, getClientRect: () => null }),
    });
    window.getSelection()?.removeAllRanges();
    try {
      act(() => {
        layer.dispatchEvent(new MouseEvent("mousedown", { button: 0, bubbles: true, clientX: 690, clientY: 280 }));
        window.dispatchEvent(new MouseEvent("mouseup", { bubbles: true, clientX: 690, clientY: 280 }));
      });
      expect(window.getSelection()?.toString()).toBe("");
      expect(container.querySelector(".paper-selection-menu")).toBeNull();
    } finally {
      Object.defineProperty(document, "caretPositionFromPoint", {
        configurable: true,
        value: originalCaret,
      });
    }
  });

  it("does not treat normal single-column word gaps as columns", async () => {
    mocks.invoke.mockImplementation((cmd: string) =>
      cmd === "paper_highlights_load" ? Promise.resolve([]) : Promise.resolve("http://127.0.0.1:1000/token/paper.pdf"),
    );
    await draw({ kind: "paper", path: "docs/paper.pdf" });

    const layer = container.querySelector<HTMLElement>(".paper-text-layer")!;
    layer.replaceChildren();
    const first = textSpan("one", rect(116, 280, 72, 18));
    const second = textSpan("two", rect(206, 280, 84, 18));
    const third = textSpan("three", rect(116, 306, 90, 18));
    const fourth = textSpan("four", rect(226, 306, 92, 18));
    layer.append(first, second, third, fourth);

    act(() => {
      layer.dispatchEvent(new MouseEvent("mousedown", { button: 0, bubbles: true, clientX: 196, clientY: 280 }));
    });
    expect(first.style.userSelect).toBe("");
    expect(second.style.userSelect).toBe("");
    expect(third.style.userSelect).toBe("");
    expect(fourth.style.userSelect).toBe("");
  });
});

describe("workspace file pane header", () => {
  it("names the file once and keeps the header to icon controls", async () => {
    mocks.invoke.mockResolvedValue(textView("src/main.rs", "fn main() {}"));
    await draw({ kind: "workspace-file", path: "src/main.rs" });

    const name = container.querySelector<HTMLElement>(".pane-name")!;
    expect(name.textContent).toBe("main.rs");
    expect(name.title).toBe("src/main.rs");
    expect(container.querySelector(".workspace-file-bar")).toBeNull();
    expect(container.querySelector('[aria-label="Read this file again"]')).not.toBeNull();
    // No save button: an unsaved draft is signalled by the dot ahead of the
    // name, and saved with Mod+S.
    expect(container.querySelector(".workspace-file-save")).toBeNull();
    expect(container.querySelector(".workspace-file-dirty")).toBeNull();
  });

  it("toggles Markdown in the pane header without resetting its draft", async () => {
    mocks.invoke.mockResolvedValue(textView("README.md", "# title"));
    await draw({ kind: "workspace-file", path: "README.md" });

    expect(editor()).toBeNull();
    act(() => container.querySelector<HTMLButtonElement>('[aria-label="Edit Markdown"]')!.click());
    act(() => editor()!.dispatch({ changes: { from: 7, insert: "\nbody" } }));
    act(() => container.querySelector<HTMLButtonElement>('[aria-label="Preview Markdown"]')!.click());
    expect(container.textContent).toContain("body");
    act(() => container.querySelector<HTMLButtonElement>('[aria-label="Edit Markdown"]')!.click());
    expect(editor()?.state.doc.toString()).toBe("# title\nbody");
  });

  it("makes the Markdown mode switch an icon without a word label", async () => {
    mocks.invoke.mockResolvedValue(textView("docs/guide.md", "# title"));
    await draw({ kind: "workspace-file", path: "docs/guide.md" });

    const chip = container.querySelector<HTMLButtonElement>(".workspace-file-mode")!;
    expect(chip.textContent).toBe("");
    expect(chip.getAttribute("aria-label")).toBe("Edit Markdown");
  });

  it("opens shown CSV artifacts as spreadsheet previews on request", async () => {
    const onOpen = vi.fn();
    context = { ...context, onOpen };
    mocks.invoke.mockResolvedValue({ body: "a,b\n1,2\n", bytes: 8, truncated: false });
    await draw({ kind: "shown", path: "out/data.csv", label: "data.csv" }, context);

    const button = container.querySelector<HTMLButtonElement>('[aria-label="Preview as spreadsheet"]')!;
    expect(button).not.toBeNull();
    act(() => button.click());

    expect(onOpen).toHaveBeenCalledWith("inspect-pane", "s", {
      kind: "spreadsheet",
      path: "out/data.csv",
    });
  });

  it("shows an unsaved dot ahead of the name and clears it on save", async () => {
    mocks.invoke.mockImplementation((command: string, args: { path: string; text?: string }) =>
      command === "workspace_read_text"
        ? Promise.resolve(textView(args.path, "saved"))
        : Promise.resolve(textView(args.path, args.text ?? "")),
    );
    await draw({ kind: "workspace-file", path: "dot.txt" });
    expect(container.querySelector(".workspace-file-dirty")).toBeNull();

    act(() => editor()!.dispatch({ changes: { from: 5, insert: " draft" } }));
    expect(container.querySelector(".workspace-file-dirty")).not.toBeNull();

    const event = new KeyboardEvent("keydown", {
      key: "s",
      ctrlKey: true,
      bubbles: true,
      cancelable: true,
    });
    await act(async () => {
      window.dispatchEvent(event);
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(container.querySelector(".workspace-file-dirty")).toBeNull();
  });

  it("keeps images render-only while retaining reload", async () => {
    const binary: WorkspaceBinaryView = {
      path: "mark.png",
      url: "data:image/png;base64,AAAA",
      bytes: 4,
    };
    mocks.invoke.mockResolvedValue(binary);
    await draw({ kind: "workspace-file", path: "mark.png" });

    expect(container.querySelector('[aria-label="Read this file again"]')).not.toBeNull();
    expect(container.querySelector(".workspace-file-mode")).toBeNull();
    expect(container.querySelector(".workspace-file-save")).toBeNull();
  });

  it("opens CSV files as a spreadsheet from the pane header action", async () => {
    const onOpen = vi.fn();
    context = { ...context, onOpen };
    mocks.invoke.mockResolvedValue(textView("data.csv", "a,b\n1,2\n"));
    await draw({ kind: "workspace-file", path: "data.csv" }, context);

    const button = container.querySelector<HTMLButtonElement>('[aria-label="Preview as spreadsheet"]')!;
    expect(button).not.toBeNull();
    act(() => button.click());

    expect(onOpen).toHaveBeenCalledWith("inspect-pane", "s", {
      kind: "spreadsheet",
      path: "data.csv",
    });
  });

  it("hides a stale registration immediately when navigation selects another path", async () => {
    mocks.invoke.mockResolvedValueOnce(textView("a.txt", "a"));
    await draw({ kind: "workspace-file", path: "a.txt" });
    expect(container.querySelector('[aria-label="Read this file again"]')).not.toBeNull();

    mocks.invoke.mockReturnValueOnce(new Promise(() => {}));
    await draw({ kind: "workspace-file", path: "b.txt" });

    expect(container.querySelector<HTMLElement>(".pane-name")?.title).toBe("b.txt");
    expect(
      container.querySelector<HTMLButtonElement>('[aria-label="Read this file again"]')?.disabled,
    ).toBe(true);
  });
});

describe("workspace file pane save shortcut", () => {
  it("saves only the matching focused pane", async () => {
    mocks.invoke.mockImplementation((command: string, args: { path: string; text?: string }) =>
      command === "workspace_read_text"
        ? Promise.resolve(textView(args.path, "saved"))
        : Promise.resolve(textView(args.path, args.text ?? "")),
    );
    await draw({ kind: "workspace-file", path: "focus.txt" });
    act(() => editor()!.dispatch({ changes: { from: 5, insert: " draft" } }));

    const focused = new KeyboardEvent("keydown", {
      key: "s",
      ctrlKey: true,
      bubbles: true,
      cancelable: true,
    });
    await act(async () => {
      window.dispatchEvent(focused);
      await Promise.resolve();
    });
    expect(focused.defaultPrevented).toBe(true);
    expect(mocks.invoke).toHaveBeenCalledWith(
      "workspace_write_text",
      expect.objectContaining({ path: "focus.txt", text: "saved draft" }),
    );

    mocks.invoke.mockClear();
    context = paneContext("another-pane");
    await draw({ kind: "workspace-file", path: "focus.txt" }, context);
    const unfocused = new KeyboardEvent("keydown", {
      key: "s",
      ctrlKey: true,
      bubbles: true,
      cancelable: true,
    });
    act(() => window.dispatchEvent(unfocused));
    expect(unfocused.defaultPrevented).toBe(false);
    expect(mocks.invoke).not.toHaveBeenCalled();
  });

  it("owns Mod+S when focused but does not call a disabled save", async () => {
    mocks.invoke.mockResolvedValue(textView("clean.txt", "clean"));
    await draw({ kind: "workspace-file", path: "clean.txt" });
    mocks.invoke.mockClear();

    const event = new KeyboardEvent("keydown", {
      key: "s",
      metaKey: true,
      bubbles: true,
      cancelable: true,
    });
    act(() => window.dispatchEvent(event));

    expect(event.defaultPrevented).toBe(true);
    expect(mocks.invoke).not.toHaveBeenCalled();
  });
});
