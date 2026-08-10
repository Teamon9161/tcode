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

  it("preserves the draft and disables overwrite after a revision conflict", async () => {
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
    expect(container.textContent).toContain("changed outside this editor");
    expect(controls?.saveDisabled).toBe(true);
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

    expect(mocks.invoke).not.toHaveBeenCalled();
    expect(editor()?.state.doc.toString()).toBe("saved draft");
    expect(editor()?.state.selection.main.head).toBe(2);
  });
});
