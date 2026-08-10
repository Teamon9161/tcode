import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { EditorSelection, EditorState, type Extension } from "@codemirror/state";
import { EditorView } from "@codemirror/view";
import { undo } from "@codemirror/commands";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  WorkspaceEditor,
  type WorkspaceEditorSnapshot,
} from "./WorkspaceEditor";

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

let container: HTMLDivElement;
let root: Root;
let snapshot: WorkspaceEditorSnapshot | null;
let resolveLanguage: ((extension: Extension | null) => void) | null;
const loaded = vi.fn(
  () =>
    new Promise<Extension | null>((resolve) => {
      resolveLanguage = resolve;
    }),
);

function draw(doc = "one", state?: EditorState | null) {
  act(() => {
    root.render(
      <WorkspaceEditor
        path="src/main.rs"
        initialDoc={doc}
        initialState={state}
        onSnapshot={(next) => {
          snapshot = next;
        }}
        languageLoader={loaded}
      />,
    );
  });
  return view();
}

function view(): EditorView {
  const found = EditorView.findFromDOM(container.querySelector(".cm-editor") as HTMLElement);
  if (!found) throw new Error("editor did not mount");
  return found;
}

function keyboardEvent(key: string, code: string, keyCode: number): KeyboardEvent {
  return new KeyboardEvent("keydown", {
    key,
    code,
    keyCode,
    which: keyCode,
    bubbles: true,
    cancelable: true,
  });
}

beforeEach(() => {
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
  snapshot = null;
  resolveLanguage = null;
  loaded.mockClear();
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

describe("WorkspaceEditor", () => {
  it("creates an accessible editor from the external initial document and reports user changes", () => {
    const editor = draw("hello");

    expect(editor.state.doc.toString()).toBe("hello");
    expect(editor.contentDOM.getAttribute("aria-label")).toBe("Workspace file editor");

    act(() => editor.dispatch({ changes: { from: 5, insert: " world" } }));
    expect(snapshot?.state.doc.toString()).toBe("hello world");
  });

  it("does not push a React value into the editor during composition", () => {
    const editor = draw("local");
    act(() => editor.contentDOM.dispatchEvent(new CompositionEvent("compositionstart", { bubbles: true })));
    draw("remote");

    expect(view()).toBe(editor);
    expect(editor.state.doc.toString()).toBe("local");
  });

  it("injects an asynchronously loaded parser without replacing the document", async () => {
    const editor = draw("before");
    act(() => editor.dispatch({ changes: { from: 6, insert: " after" } }));
    const stateBeforeParser = editor.state;

    await act(async () => {
      resolveLanguage?.([]);
      await Promise.resolve();
    });

    expect(editor.state).not.toBe(stateBeforeParser);
    expect(editor.state.doc.toString()).toBe("before after");
  });

  it("restores selection and undo history in a fresh view", () => {
    const first = draw("one");
    act(() =>
      first.dispatch({
        changes: { from: 3, insert: " two" },
        selection: EditorSelection.cursor(7),
      }),
    );
    const cached = snapshot!.state;

    act(() => root.unmount());
    root = createRoot(container);
    const restored = draw("ignored", cached);

    expect(restored.state.selection.main.head).toBe(7);
    act(() => {
      expect(undo(restored)).toBe(true);
    });
    expect(restored.state.doc.toString()).toBe("one");
  });

  it("indents with Tab but leaves Escape followed by Tab to browser focus", () => {
    const editor = draw("one");
    editor.contentDOM.focus();

    const indent = keyboardEvent("Tab", "Tab", 9);
    act(() => editor.contentDOM.dispatchEvent(indent));
    expect(indent.defaultPrevented).toBe(true);
    expect(editor.state.doc.toString()).toBe("  one");

    const escape = keyboardEvent("Escape", "Escape", 27);
    act(() => editor.contentDOM.dispatchEvent(escape));
    const leave = keyboardEvent("Tab", "Tab", 9);
    act(() => editor.contentDOM.dispatchEvent(leave));

    expect(leave.defaultPrevented).toBe(false);
    expect(editor.state.doc.toString()).toBe("  one");
  });

  it("destroys the EditorView when React unmounts it", () => {
    const destroy = vi.spyOn(EditorView.prototype, "destroy");
    draw();

    act(() => root.unmount());
    root = createRoot(container);

    expect(destroy).toHaveBeenCalledTimes(1);
    destroy.mockRestore();
  });
});
