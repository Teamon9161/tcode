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
      getPage: vi.fn().mockResolvedValue({
        getViewport: vi.fn(() => ({ width: 600, height: 800 })),
        render: vi.fn(() => ({ cancel: vi.fn(), promise: Promise.resolve() })),
        streamTextContent: vi.fn(),
      }),
    }),
  })),
}));
vi.mock("pdfjs-dist/build/pdf.worker.mjs?url", () => ({ default: "pdf.worker.mjs" }));

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

function paneContext(focus = "inspect-pane"): PaneContext {
  const none = () => {};
  return {
    sessions: [{ id: "s", cwd: "/project", name: "project", home: "/home/me", log_id: null }],
    focus,
    split: true,
    onFocus: none,
    onClosePane: none,
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

function editor(): EditorView | null {
  const dom = container.querySelector<HTMLElement>(".cm-editor");
  return dom ? EditorView.findFromDOM(dom) : null;
}

beforeEach(() => {
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
    mocks.invoke.mockResolvedValue("http://127.0.0.1:1000/token/paper.pdf");
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
    mocks.invoke.mockResolvedValue("http://127.0.0.1:1000/token/paper.pdf");
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
      container.querySelector<HTMLButtonElement>(".paper-selection-menu .chip:nth-child(2)")!.click();
      await Promise.resolve();
    });
    selectionSpy.mockRestore();

    expect(onPaperPrompt).toHaveBeenCalledWith(
      "s",
      'Explain the selected text from @paper("paper.pdf", page 1):\n\nPaper selection',
    );
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
