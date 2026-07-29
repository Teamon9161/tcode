import { useMemo } from "react";

import type { Block } from "./blocks";
import type { SandboxKind } from "./sandbox/protocol";

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
  /** A file as some call saw it. `at` pins a specific call; without it, the
   *  most recent one wins. */
  | { kind: "file"; path: string; at?: string }
  /** The change one edit/write call is making or made. */
  | { kind: "diff"; callId: string }
  /** A sub-agent run's own transcript. */
  | { kind: "run"; run: string }
  /** A tool's complete output, when the preview was not enough. */
  | { kind: "output"; callId: string }
  /** Model-authored rich content, rendered behind the sandbox boundary. */
  | { kind: "artifact"; sandbox: SandboxKind; source: string; label: string }
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
  | { kind: "doc"; path: string; text: string };

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
    case "file":
      return basename(value.path);
    case "diff":
      return "Change";
    case "run":
      return "Sub-agent";
    case "output":
      return "Output";
    case "artifact":
      return value.label;
    case "shown":
      return value.label;
    case "doc":
      return basename(value.path);
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
