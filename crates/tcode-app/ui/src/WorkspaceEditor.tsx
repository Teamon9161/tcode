import { useEffect, useRef } from "react";
import { Compartment, EditorState, type Extension } from "@codemirror/state";
import { EditorView } from "@codemirror/view";

import { loadEditorLanguage } from "./editorLanguage";
import {
  restoreWorkspaceEditorState,
  workspaceEditorExtensions,
} from "./workspaceEditor";

export type WorkspaceEditorPosition = {
  top: number;
  left: number;
};

export type WorkspaceEditorSnapshot = {
  state: EditorState;
  scroll: WorkspaceEditorPosition;
};

export function WorkspaceEditor({
  path,
  initialDoc,
  initialState,
  initialScroll = ZERO_SCROLL,
  initialOffset,
  readOnly = false,
  onSnapshot,
  languageLoader = loadEditorLanguage,
}: {
  path: string;
  /** Used only when this editor instance is created. Later React renders never
   *  write into CodeMirror, which keeps IME composition and undo history local. */
  initialDoc: string;
  initialState?: EditorState | null;
  initialScroll?: WorkspaceEditorPosition;
  /** Where in the document to open when there is no saved editor scroll yet —
   *  the reader's position handed over from the markdown preview. Ignored once
   *  `initialScroll` carries a real position. */
  initialOffset?: number | null;
  readOnly?: boolean;
  onSnapshot: (snapshot: WorkspaceEditorSnapshot) => void;
  /** Kept injectable so the async parser boundary can be tested without
   *  importing every language package. Production callers use language-data. */
  languageLoader?: (path: string) => Promise<Extension | null>;
}) {
  const host = useRef<HTMLDivElement>(null);
  const latestSnapshot = useRef(onSnapshot);
  latestSnapshot.current = onSnapshot;

  useEffect(() => {
    const parent = host.current;
    if (!parent) return;
    // The view can be destroyed between now and the first paint of a position
    // hand-off, so the deferred scroll checks this before dispatching.
    let live = true;

    const language = new Compartment();
    const extensions = [
      workspaceEditorExtensions(readOnly),
      language.of([]),
    ];
    const state = initialState
      ? restoreWorkspaceEditorState(initialState, extensions)
      : EditorState.create({ doc: initialDoc, extensions });

    const publish = (view: EditorView) =>
      latestSnapshot.current({
        state: view.state,
        scroll: {
          top: view.scrollDOM.scrollTop,
          left: view.scrollDOM.scrollLeft,
        },
      });

    const view = new EditorView({
      state,
      parent,
      dispatchTransactions(transactions, current) {
        current.update(transactions);
        publish(current);
      },
    });
    // The editor's own remembered position wins; the preview hand-off is only
    // for a first entry, and `scrollIntoView` is deferred one frame so the
    // document has been measured before CodeMirror is asked where a position
    // is. A scroll failure must never stop the editor from existing.
    if (initialScroll.top !== 0 || initialScroll.left !== 0) {
      view.scrollDOM.scrollTop = initialScroll.top;
      view.scrollDOM.scrollLeft = initialScroll.left;
    } else if (initialOffset != null) {
      const pos = Math.min(Math.max(initialOffset, 0), view.state.doc.length);
      const defer =
        typeof requestAnimationFrame === "function"
          ? requestAnimationFrame
          : (run: FrameRequestCallback) => window.setTimeout(() => run(0), 0);
      defer(() => {
        if (!live) return;
        try {
          view.dispatch({ effects: EditorView.scrollIntoView(pos, { y: "start" }) });
        } catch {
          // A zero-height first frame is not a reason to drop the editor.
        }
      });
    }
    const rememberScroll = () => publish(view);
    view.scrollDOM.addEventListener("scroll", rememberScroll, { passive: true });
    publish(view);

    void languageLoader(path)
      .then((support) => {
        if (!live || !support) return;
        view.dispatch({ effects: language.reconfigure(support) });
      })
      .catch(() => {
        // A parser is optional enhancement. A failed dynamic import leaves the
        // complete document available as plain text.
      });

    return () => {
      live = false;
      view.scrollDOM.removeEventListener("scroll", rememberScroll);
      publish(view);
      view.destroy();
    };
    // This is an intentionally uncontrolled editor. A different file or a
    // confirmed reload is represented by a different component key; changing
    // props on the live instance must never rewrite an active composition.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  return <div className="workspace-editor" ref={host} />;
}

const ZERO_SCROLL: WorkspaceEditorPosition = { top: 0, left: 0 };
