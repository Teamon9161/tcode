import { useCallback, useMemo, useState } from "react";

import type { Block } from "./blocks";
import type { SandboxKind } from "./sandbox/protocol";

/**
 * What the right-hand panel is showing.
 *
 * The panel holds **one value**, not a set of tabs. That is the whole design:
 * every place in the app that can be looked into — a path in the transcript, a
 * file in the list, a sub-agent run, a tool's full output, an artifact — does
 * the same single thing, `open(...)`, and the panel dispatches on `kind`.
 *
 * Tabs were the obvious alternative and are worse: they add a second piece of
 * state (which tab is active) that has nothing to do with what the user asked
 * to see, they make "show me this file" ambiguous, and every new kind of
 * inspectable thing becomes a permanent tab whether or not it has content.
 *
 * Holding one value also makes navigation free. A stack of them is history, so
 * following a sub-agent to the file it edited and coming back is a back button
 * rather than a feature.
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
  /** A document — a plan draft, a markdown file — read as prose. */
  | { kind: "doc"; path: string; text: string };

export type Inspector = {
  value: Inspect | null;
  open: (next: Inspect) => void;
  close: () => void;
  back: () => void;
  forward: () => void;
  canBack: boolean;
  canForward: boolean;
};

/** Cursor and entries are one piece of state on purpose: they are only ever
 *  meaningful together, and splitting them invites an update that moves one
 *  without the other. */
type Nav = { entries: Inspect[]; at: number };

const EMPTY: Nav = { entries: [], at: -1 };

export function useInspector(): Inspector {
  const [nav, setNav] = useState<Nav>(EMPTY);

  const open = useCallback((next: Inspect) => {
    setNav(({ entries, at }) => {
      // Opening something new abandons whatever was forward of here, the way
      // every history in every browser behaves.
      const kept = entries.slice(0, at + 1);
      return { entries: [...kept, next], at: kept.length };
    });
  }, []);

  const close = useCallback(() => setNav(EMPTY), []);

  const back = useCallback(
    () => setNav((current) => ({ ...current, at: Math.max(current.at - 1, 0) })),
    [],
  );

  const forward = useCallback(
    () =>
      setNav((current) => ({
        ...current,
        at: Math.min(current.at + 1, current.entries.length - 1),
      })),
    [],
  );

  return {
    value: nav.at >= 0 ? (nav.entries[nav.at] ?? null) : null,
    open,
    close,
    back,
    forward,
    canBack: nav.at > 0,
    canForward: nav.at >= 0 && nav.at < nav.entries.length - 1,
  };
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
