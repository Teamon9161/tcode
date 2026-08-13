import { useMemo } from "react";

import type { Block } from "./blocks";
import type { SandboxKind } from "./sandbox/protocol";
import { workspaceRouteOf } from "./show";

/**
 * What an inspect pane is showing.
 *
 * A pane holds **one value**, not a set of tabs. That is the whole design:
 * every place in the app that can be looked into — a path in the transcript, a
 * file in the list, a sub-agent run, a tool's full output, an artifact — does
 * the same single thing, `open(...)`, and the pane dispatches on `kind`.
 *
 * Tabs were the obvious alternative and are worse: they add a second piece of
 * state (which tab is active) that has nothing to do with what the user asked
 * to see, they make "show me this file" ambiguous, and every new kind of
 * inspectable thing becomes a permanent tab whether or not it has content.
 *
 * Holding one value also makes navigation free. A stack of them is history, so
 * following a sub-agent to the file it edited and coming back is a back button
 * rather than a feature. Splitting the window did not weaken that rule: each
 * pane holds one value and its own history, so two things can be side by side
 * without either becoming a tab strip.
 */
export type Inspect =
  /** The index of everything this conversation touched. The panel's root: the
   *  list is what you return to, not a second column beside the viewer. */
  | { kind: "files" }
  /** A live, session-confined workspace tree. This is distinct from `files`,
   * which is the conversation's agent-touched history. */
  | { kind: "workspace-tree" }
  /** A workspace file selected from the live tree. Its editor arrives in the
   * next stage; it remains a distinct value from transcript snapshots. */
  | { kind: "workspace-file"; path: string }
  /** A PDF document opened as a live reading surface tied to this session's
   * composer, not as editable UTF-8 source and not as the window browser. */
  | { kind: "paper"; path: string; documentId?: string }
  /** A spreadsheet opened with IronCalc. Legacy .xls files are converted to a
   * value-only preview by the backend and stay read-only in this pane. */
  | { kind: "spreadsheet"; path: string }
  /** A docx document rendered read-only with docx-preview. */
  | { kind: "document"; path: string }
  /** A file as some call saw it. `at` pins a specific call; without it, the
   *  most recent one wins. */
  | { kind: "file"; path: string; at?: string }
  /** The change one edit/write call is making or made. */
  | { kind: "diff"; callId: string }
  /**
   * A sub-agent run's own transcript.
   *
   * `label` is the pane header's whole text — the run's kind and what it was
   * asked to do. It is carried rather than looked up because the header is drawn
   * by the pane frame, which has the value and not the conversation: reaching
   * back into the blocks for it would make the one label in the window that
   * cannot be written down the only one that needs the transcript. Without it the
   * header read "Sub-agent", which is true of every one of them.
   */
  | { kind: "run"; run: string; label?: string }
  /** A tool's complete output, when the preview was not enough. */
  | { kind: "output"; callId: string }
  /** Model-authored rich content, rendered behind the sandbox boundary. */
  | { kind: "artifact"; sandbox: SandboxKind; source: string; label: string }
  /**
   * An image the conversation holds, at the size the pane gives it.
   *
   * Carries the `data:` URL rather than a path because that is all there is: the
   * bytes were pasted into a prompt and went into the request, and the only copy
   * on this side is the one the transcript is already drawing at thumbnail size.
   * A thumbnail is the right size for "which image was that" and the wrong size
   * for reading anything in it, and the transcript must stay a column of prose —
   * so the full size belongs where every other enlargement in this app goes.
   */
  | { kind: "image"; url: string; label: string }
  /**
   * A file on disk the model asked to display (`show`).
   *
   * The one value here that is *not* a function of the transcript. Every other
   * kind answers "what did the agent do", and re-reading the file would answer a
   * different question than the one being asked. This one's whole question is
   * the file: it was produced by a script precisely so its contents never had to
   * enter the conversation, so there is nothing in the transcript to read it
   * from. It carries the path rather than the bytes for the same reason.
   */
  | { kind: "shown"; path: string; label: string }
  /** A document — a plan draft, a markdown file — read as prose. */
  | { kind: "doc"; path: string; text: string }
  /**
   * This conversation's plan, in full and editable.
   *
   * The second value here that is not a function of the transcript, and for the
   * same reason `shown` is the first: a progress file is externally mutable state
   * the user may edit by hand, so "what does the plan say" is a question about
   * the file and not about what the agent said. It carries no data — the plan
   * belongs to the session, not to this pane, so the review dock and this pane
   * are looking at one draft rather than two copies of one.
   */
  | { kind: "plan" };

/**
 * One pane's browsing history.
 *
 * Cursor and entries are one piece of state on purpose: they are only ever
 * meaningful together, and splitting them invites an update that moves one
 * without the other.
 *
 * These are plain functions rather than a hook because a `Nav` lives inside the
 * pane tree (`layout.ts`), not inside a component. That is what survives a pane
 * being moved, resized or re-rendered — and what will let a saved layout come
 * back with its history intact.
 *
 * Every `Nav` holds at least one entry by construction: a pane that shows
 * nothing is a pane that should have been closed.
 */
export type Nav = { entries: Inspect[]; at: number };

export function navOf(value: Inspect): Nav {
  return { entries: [value], at: 0 };
}

export function inspectForPath(
  path: string,
): Extract<Inspect, { kind: "paper" | "spreadsheet" | "document" | "workspace-file" }> {
  const route = workspaceRouteOf(path);
  switch (route.as) {
    case "paper":
      return { kind: "paper", path };
    case "spreadsheet":
      return { kind: "spreadsheet", path };
    case "document":
      return { kind: "document", path };
    default:
      return { kind: "workspace-file", path };
  }
}

export function navValue(nav: Nav): Inspect {
  return nav.entries[nav.at] ?? nav.entries[0];
}

/** Opening something new abandons whatever was forward of here, the way every
 *  history in every browser behaves. */
export function navOpen(nav: Nav, next: Inspect): Nav {
  const kept = nav.entries.slice(0, nav.at + 1);
  return { entries: [...kept, next], at: kept.length };
}

export function navBack(nav: Nav): Nav {
  return { ...nav, at: Math.max(nav.at - 1, 0) };
}

export function navForward(nav: Nav): Nav {
  return { ...nav, at: Math.min(nav.at + 1, nav.entries.length - 1) };
}

export function canBack(nav: Nav): boolean {
  return nav.at > 0;
}

export function canForward(nav: Nav): boolean {
  return nav.at < nav.entries.length - 1;
}

/** A short label for the panel header. */
export function inspectTitle(value: Inspect): string {
  switch (value.kind) {
    case "files":
      return "Files";
    case "workspace-tree":
      return "Workspace";
    case "workspace-file":
      return basename(value.path);
    case "paper":
      return basename(value.path);
    case "spreadsheet":
      return basename(value.path);
    case "document":
      return basename(value.path);
    case "file":
      return basename(value.path);
    case "diff":
      return "Change";
    case "run":
      return value.label || "Sub-agent";
    case "output":
      return "Output";
    case "artifact":
      return value.label;
    case "image":
      return value.label;
    case "shown":
      return value.label;
    case "doc":
      return basename(value.path);
    case "plan":
      return "Plan";
  }
}

/** Every call that touched a path, in transcript order.
 *
 *  This is what lets the file view say when a file was read and whether
 *  anything changed it since — the one thing a snapshot must not leave the
 *  reader guessing about. */
export type Touch = { callId: string; name: string; changed: boolean };

export function fileHistory(blocks: Block[], path: string): Touch[] {
  const out: Touch[] = [];
  const walk = (list: Block[]) => {
    for (const block of list) {
      if (block.kind === "tool") {
        if (pathOf(block.input) === path) {
          out.push({ callId: block.callId, name: block.name, changed: block.name !== "read" });
        }
      } else if ("blocks" in block) {
        walk(block.blocks);
      }
    }
  };
  walk(blocks);
  return out;
}

/** Memoised, so the panel does not re-walk the tree on every streamed delta. */
export function useFileHistory(blocks: Block[], path: string): Touch[] {
  return useMemo(() => fileHistory(blocks, path), [blocks, path]);
}

function pathOf(input: unknown): string | null {
  if (typeof input !== "object" || input === null) return null;
  const record = input as Record<string, unknown>;
  for (const key of ["file_path", "path", "notebook_path", "filePath"]) {
    const value = record[key];
    if (typeof value === "string" && value.length > 0) return value;
  }
  return null;
}

function basename(path: string): string {
  const cut = Math.max(path.lastIndexOf("/"), path.lastIndexOf("\\"));
  return cut === -1 ? path : path.slice(cut + 1);
}
