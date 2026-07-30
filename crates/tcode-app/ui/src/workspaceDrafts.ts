import type { WorkspaceTextView } from "./types";

/**
 * Unsaved workspace text belongs to a session and path, rather than to the
 * inspect pane that happened to show it. Inspect navigation deliberately
 * unmounts its body, so a module-lifetime cache keeps a draft available when
 * that body returns without making it durable outside this app session.
 */
export type WorkspaceDraft = {
  file: WorkspaceTextView;
  text: string;
  /** Whether `text` represents the complete file, rather than a loaded prefix. */
  complete: boolean;
};

const drafts = new Map<string, WorkspaceDraft>();

export function workspaceDraft(session: string, path: string): WorkspaceDraft | null {
  return drafts.get(keyOf(session, path)) ?? null;
}

export function rememberWorkspaceDraft(session: string, path: string, draft: WorkspaceDraft): void {
  drafts.set(keyOf(session, path), draft);
}

export function discardWorkspaceDraft(session: string, path: string): void {
  drafts.delete(keyOf(session, path));
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
