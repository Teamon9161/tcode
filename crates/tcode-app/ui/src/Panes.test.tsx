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
  return { invoke: vi.fn() };
});
vi.mock("@ipc", () => ({ invoke: mocks.invoke }));

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
