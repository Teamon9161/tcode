import type { EditorState } from "@codemirror/state";

import type { WorkspaceEditorPosition } from "./WorkspaceEditor";
import type { WorkspaceTextView } from "./types";

export type WorkspaceMode = "preview" | "edit";

/** A live file session outlives the inspect component that happens to draw it.
 * Navigation deliberately unmounts that component, so selection, history,
 * scroll and even a clean document stay in this module-lifetime cache. */
export type WorkspaceFileSession = {
  /** Disk baseline and the revision that the next save must compare. */
  file: WorkspaceTextView;
  /** Current complete document or, when `complete` is false, the read prefix. */
  text: string;
  complete: boolean;
  mode: WorkspaceMode;
  editorState: EditorState | null;
  editorScroll: WorkspaceEditorPosition;
  previewScroll: number;
  conflicted: boolean;
  /** Changes only when a confirmed reload must create a fresh EditorView and
   * sever the old undo chain. Saves intentionally leave it alone. */
  generation: number;
};

const sessions = new Map<string, WorkspaceFileSession>();
const ZERO = { top: 0, left: 0 };

export function workspaceFileSession(
  session: string,
  path: string,
): WorkspaceFileSession | null {
  return sessions.get(keyOf(session, path)) ?? null;
}

export function rememberWorkspaceFileSession(
  session: string,
  path: string,
  value: WorkspaceFileSession,
): void {
  sessions.set(keyOf(session, path), value);
}

export function newWorkspaceFileSession(
  file: WorkspaceTextView,
  markdown: boolean,
): WorkspaceFileSession {
  return {
    file,
    text: file.text,
    complete: !file.truncated,
    mode: markdown ? "preview" : "edit",
    editorState: null,
    editorScroll: ZERO,
    previewScroll: 0,
    conflicted: false,
    generation: 0,
  };
}

/** A confirmed reload adopts disk contents, preserves the chosen mode and a
 * reasonable viewport, and starts a fresh state so undo cannot resurrect the
 * discarded draft. */
export function reloadedWorkspaceFileSession(
  current: WorkspaceFileSession,
  file: WorkspaceTextView,
): WorkspaceFileSession {
  return {
    ...current,
    file,
    text: file.text,
    complete: !file.truncated,
    editorState: null,
    conflicted: false,
    generation: current.generation + 1,
  };
}

/** A save updates only the disk baseline/revision. The current editor state,
 * selection, history and scroll survive. If typing continued during the write,
 * `text` remains newer than the submitted baseline and therefore stays dirty. */
export function savedWorkspaceFileSession(
  current: WorkspaceFileSession,
  saved: WorkspaceTextView,
  submitted: string,
): WorkspaceFileSession {
  return {
    ...current,
    file: { ...saved, text: submitted },
    complete: true,
    conflicted: false,
  };
}

export function conflictedWorkspaceFileSession(
  current: WorkspaceFileSession,
): WorkspaceFileSession {
  return { ...current, conflicted: true };
}

export function workspaceFileDirty(value: WorkspaceFileSession): boolean {
  return value.text !== value.file.text;
}

/** A reload replaces unsaved text, so only a dirty editor needs an explicit choice. */
export function reloadNeedsConfirmation(dirty: boolean): boolean {
  return dirty;
}

/** A truncated response is only a prefix and must never become a file write. */
export function canSaveWorkspaceText({
  dirty,
  truncated,
  conflicted,
}: {
  dirty: boolean;
  truncated: boolean;
  conflicted: boolean;
}): boolean {
  return dirty && !truncated && !conflicted;
}

function keyOf(session: string, path: string): string {
  return JSON.stringify([session, path]);
}
