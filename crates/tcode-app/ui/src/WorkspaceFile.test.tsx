import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { EditorSelection } from "@codemirror/state";
import { EditorView } from "@codemirror/view";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { WorkspaceBinaryView, WorkspaceTextView } from "./types";

const mocks = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("@ipc", () => ({ invoke: mocks.invoke }));

import { SessionContext } from "./session";
import { WorkspaceFileControlsContext, type WorkspaceFileControls } from "./workspaceFileControls";
import { WorkspaceFile } from "./WorkspaceFile";

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
// CodeMirror measures selection layers after a grammar arrives. jsdom has no
// range geometry; an empty rect list is its honest no-layout answer.
Object.defineProperty(Range.prototype, "getClientRects", { value: () => [], configurable: true });

let root: Root;
let container: HTMLDivElement;
let session: string;
let controls: WorkspaceFileControls | null;
let serial = 0;

const register = (next: WorkspaceFileControls) => {
  controls = next;
  return () => {
    if (controls === next) controls = null;
  };
};

const textView = (path: string, text: string, truncated = false): WorkspaceTextView => ({
  path,
  text,
  revision: `revision:${path}`,
  fingerprint: `fingerprint:${path}`,
  bytes: text.length,
  truncated,
});

async function draw(path: string) {
  await act(async () => {
    root.render(
      <SessionContext.Provider value={session}>
        <WorkspaceFileControlsContext.Provider value={register}>
          <WorkspaceFile path={path} />
        </WorkspaceFileControlsContext.Provider>
      </SessionContext.Provider>,
    );
    await Promise.resolve();
    await Promise.resolve();
  });
}

function editor(): EditorView | null {
  const dom = container.querySelector<HTMLElement>(".cm-editor");
  return dom ? EditorView.findFromDOM(dom) : null;
}

/** One stat poll tick: advance the 2s interval and flush its promise. */
async function poll() {
  await act(async () => {
    vi.advanceTimersByTime(2000);
    await Promise.resolve();
    await Promise.resolve();
  });
}

beforeEach(() => {
  serial += 1;
  session = `workspace-file-test-${serial}`;
  controls = null;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
  mocks.invoke.mockReset();
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
  vi.useRealTimers();
});

describe("WorkspaceFile routes live workspace content", () => {
  it("opens ordinary UTF-8 text directly in CodeMirror", async () => {
    mocks.invoke.mockResolvedValue(textView("src/main.rs", "fn main() {}"));
    await draw("src/main.rs");

    expect(mocks.invoke).toHaveBeenCalledWith("workspace_read_text", {
      session,
      path: "src/main.rs",
    });
    expect(editor()?.state.doc.toString()).toBe("fn main() {}");
    expect(container.querySelector("textarea")).toBeNull();
  });

  it("shows Markdown first, then restores editor selection and text across mode switches", async () => {
    mocks.invoke.mockResolvedValue(textView("README.md", "# title"));
    await draw("README.md");

    expect(container.querySelector(".doc")).not.toBeNull();
    expect(editor()).toBeNull();

    act(() => controls?.onMode?.("edit"));
    const first = editor()!;
    act(() =>
      first.dispatch({
        changes: { from: first.state.doc.length, insert: "\nbody" },
        selection: EditorSelection.cursor(3),
      }),
    );
    act(() => controls?.onMode?.("preview"));
    expect(container.textContent).toContain("body");

    act(() => controls?.onMode?.("edit"));
    expect(editor()?.state.doc.toString()).toBe("# title\nbody");
    expect(editor()?.state.selection.main.head).toBe(3);
  });

  it("samples Markdown text below preview padding before entering the editor", async () => {
    mocks.invoke.mockResolvedValue(textView("README.md", "# title\n\nbody"));
    await draw("README.md");

    const preview = container.querySelector<HTMLElement>(".workspace-file-preview")!;
    preview.style.paddingTop = "24px";
    Object.defineProperty(preview, "clientHeight", { configurable: true, value: 200 });
    vi.spyOn(preview, "getBoundingClientRect").mockReturnValue({
      x: 0,
      y: 100,
      top: 100,
      left: 0,
      right: 300,
      bottom: 300,
      width: 300,
      height: 200,
      toJSON: () => ({}),
    });
    const text = preview.querySelector(".prose-h1")!.firstChild!;
    const caret = vi.fn((_: number, y: number) =>
      y >= 124 ? { startContainer: text, startOffset: 2 } : null,
    );
    const previous = Object.getOwnPropertyDescriptor(document, "caretRangeFromPoint");
    Object.defineProperty(document, "caretRangeFromPoint", { configurable: true, value: caret });

    try {
      act(() => controls?.onMode?.("edit"));
      expect(caret).toHaveBeenCalledWith(80, 125);
    } finally {
      if (previous) Object.defineProperty(document, "caretRangeFromPoint", previous);
      else Reflect.deleteProperty(document, "caretRangeFromPoint");
    }
  });

  it("reads images as bytes and never offers an editor or save action", async () => {
    const binary: WorkspaceBinaryView = {
      path: "icons/mark.png",
      url: "data:image/png;base64,AAAA",
      bytes: 4,
    };
    mocks.invoke.mockResolvedValue(binary);
    await draw("icons/mark.png");

    expect(mocks.invoke).toHaveBeenCalledWith("workspace_read_binary", {
      session,
      path: "icons/mark.png",
    });
    expect(container.querySelector('img[src="data:image/png;base64,AAAA"]')).not.toBeNull();
    expect(editor()).toBeNull();
    expect(controls?.onSave).toBeNull();
  });

  it("opens workspace HTML as source without changing shown-file frame semantics", async () => {
    mocks.invoke.mockResolvedValue(textView("out/report.html", "<script>draw()</script>"));
    await draw("out/report.html");

    expect(mocks.invoke).toHaveBeenCalledWith("workspace_read_text", {
      session,
      path: "out/report.html",
    });
    expect(editor()?.state.doc.toString()).toBe("<script>draw()</script>");
    expect(container.querySelector("iframe")).toBeNull();
  });

  it("lands a rejected save in the disk-change banner instead of a dead end", async () => {
    mocks.invoke.mockImplementation((command: string) =>
      command === "workspace_read_text"
        ? Promise.resolve(textView("conflict.txt", "saved"))
        : Promise.reject(new Error("revision conflict: file changed")),
    );
    await draw("conflict.txt");
    act(() => editor()!.dispatch({ changes: { from: 5, insert: " draft" } }));

    await act(async () => {
      controls?.onSave?.();
      await Promise.resolve();
      await Promise.resolve();
    });

    expect(editor()?.state.doc.toString()).toBe("saved draft");
    expect(container.textContent).toContain("changed on disk");
    expect(controls?.saveDisabled).toBe(true);
    // The banner is the way out: overwrite is offered beside the draft.
    expect(container.querySelector(".workspace-file-disk")).not.toBeNull();
    expect(container.textContent).toContain("Overwrite the file");
  });

  it("renders a truncated prefix read-only and never enables save", async () => {
    mocks.invoke.mockResolvedValue(textView("large.log", "prefix", true));
    await draw("large.log");

    expect(editor()?.state.doc.toString()).toBe("prefix");
    expect(editor()?.state.readOnly).toBe(true);
    expect(container.textContent).toContain("only the first part");
    expect(controls?.saveDisabled).toBe(true);
  });

  it("restores an editor session after inspect navigation unmounts it", async () => {
    mocks.invoke.mockResolvedValue(textView("notes.txt", "saved"));
    await draw("notes.txt");
    const first = editor()!;
    act(() =>
      first.dispatch({
        changes: { from: 5, insert: " draft" },
        selection: EditorSelection.cursor(2),
      }),
    );

    act(() => root.unmount());
    root = createRoot(container);
    mocks.invoke.mockClear();
    await draw("notes.txt");

    // The draft comes back from the module-lifetime session, not from disk —
    // the one call a remount is allowed is the stat that re-checks the disk
    // baseline the session was read against.
    expect(mocks.invoke).not.toHaveBeenCalledWith("workspace_read_text", expect.anything());
    expect(mocks.invoke).toHaveBeenCalledWith("workspace_stat", { session, path: "notes.txt" });
    expect(editor()?.state.doc.toString()).toBe("saved draft");
    expect(editor()?.state.selection.main.head).toBe(2);
  });
});

describe("WorkspaceFile notices the disk moved", () => {
  it("asks a clean editor to reload when the file changed on disk", async () => {
    vi.useFakeTimers();
    // A file the agent rewrote: the read's fingerprint and the stat's no longer
    // match, the way a real rewrite moves both mtime and length.
    mocks.invoke.mockImplementation((command: string, args: { path?: string }) => {
      const path = typeof args?.path === "string" ? args.path : "watch.txt";
      return command === "workspace_stat"
        ? Promise.resolve({ path, fingerprint: "fingerprint-moved", bytes: 5 })
        : Promise.resolve(textView(path, "one"));
    });
    await draw("watch.txt");
    expect(container.querySelector(".workspace-file-disk")).toBeNull();

    await poll();

    expect(container.querySelector(".workspace-file-disk")).not.toBeNull();
    expect(container.textContent).toContain("changed on disk");
    // Clean editor: no overwrite answer, because there is nothing to overwrite
    // with — reload is the whole answer.
    expect(container.textContent).toContain("Reload");
    expect(container.textContent).not.toContain("Overwrite");
  });

  it("offers overwrite when both sides changed, and it clears the banner", async () => {
    vi.useFakeTimers();
    mocks.invoke.mockImplementation(
      (command: string, args: { path?: string; text?: string }) => {
        const path = typeof args?.path === "string" ? args.path : "both.txt";
        if (command === "workspace_stat") {
          return Promise.resolve({ path, fingerprint: "fingerprint-moved", bytes: 12 });
        }
        return Promise.resolve(textView(path, typeof args?.text === "string" ? args.text : "saved"));
      },
    );
    await draw("both.txt");
    act(() => editor()!.dispatch({ changes: { from: 5, insert: " draft" } }));

    await poll();
    expect(container.querySelector(".workspace-file-disk")).not.toBeNull();
    expect(container.textContent).toContain("Overwrite the file");

    mocks.invoke.mockClear();
    await act(async () => {
      container
        .querySelector<HTMLButtonElement>(".workspace-file-disk .btn-primary")!
        .click();
      await Promise.resolve();
      await Promise.resolve();
    });

    expect(mocks.invoke).toHaveBeenCalledWith(
      "workspace_write_text",
      expect.objectContaining({ path: "both.txt", force: true }),
    );
    expect(container.querySelector(".workspace-file-disk")).toBeNull();
    expect(editor()?.state.doc.toString()).toBe("saved draft");
  });

  it("reloads from the disk answer and drops the banner", async () => {
    vi.useFakeTimers();
    const changed = { path: "notes.txt", fingerprint: "fingerprint-moved", bytes: 9 };
    mocks.invoke.mockImplementation((command: string, args: { path?: string }) => {
      const path = typeof args?.path === "string" ? args.path : "notes.txt";
      if (command === "workspace_stat") return Promise.resolve(changed);
      if (command === "workspace_read_text") {
        return Promise.resolve(textView(path, "agent rewrite"));
      }
      return Promise.resolve(textView(path, ""));
    });
    await draw("notes.txt");
    act(() => editor()!.dispatch({ changes: { from: 5, insert: " draft" } }));

    await poll();
    expect(container.textContent).toContain("Reload and discard my changes");

    await act(async () => {
      container.querySelector<HTMLButtonElement>(".workspace-file-disk .btn")!.click();
      await Promise.resolve();
      await Promise.resolve();
    });

    expect(container.querySelector(".workspace-file-disk")).toBeNull();
    expect(editor()?.state.doc.toString()).toBe("agent rewrite");
  });
});
