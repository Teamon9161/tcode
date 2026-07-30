import type { Block } from "./blocks";
import type { SessionInfo } from "./types";

/**
 * The conversation rail, as data.
 *
 * The rail was a flat list of conversations named after their folders, which is
 * fine until a folder holds two of them: then it is two identical rows, and the
 * list can account for both without saying which is which. Two facts were
 * missing, and they are different facts — *where* a conversation is, and *what it
 * is for* — so they belong to different elements. The folder becomes a heading
 * over its conversations, and each conversation is named by what it was asked to
 * do.
 *
 * Pure functions here, drawn by `Workspace.tsx`, for the reason `layout.ts` is
 * separate from `Panes.tsx`: grouping and ordering are decisions with right
 * answers, and a test can hold them.
 */

export type RailGroup = {
  /** The folder, which is the group's identity as well as its heading. */
  path: string;
  name: string;
  sessions: SessionInfo[];
};

/**
 * Conversations under their folders, in the order the reader arranged.
 *
 * `order` holds only the folders that have been moved; everything else keeps the
 * order its first conversation arrived in. That is what makes an arrangement
 * survive opening a new folder: an unlisted project appends rather than
 * scattering the ones already placed.
 */
export function railGroups(sessions: SessionInfo[], order: string[]): RailGroup[] {
  const groups: RailGroup[] = [];
  const at = new Map<string, number>();
  for (const session of sessions) {
    const found = at.get(session.cwd);
    if (found !== undefined) {
      groups[found].sessions.push(session);
      continue;
    }
    at.set(session.cwd, groups.length);
    groups.push({ path: session.cwd, name: session.name, sessions: [session] });
  }

  const rank = (path: string) => {
    const placed = order.indexOf(path);
    return placed === -1 ? order.length + (at.get(path) ?? 0) : placed;
  };
  return groups.sort((a, b) => rank(a.path) - rank(b.path));
}

/**
 * Move one folder to a position, returning the new order.
 *
 * It writes out the *whole* current order rather than editing the stored list,
 * because the stored list may not mention the folder being moved or the one it
 * lands next to. Storing the arrangement as it now reads is the only version of
 * this that cannot drift from what is on screen.
 */
export function moveProject(groups: RailGroup[], path: string, to: number): string[] {
  const paths = groups.map((group) => group.path);
  const from = paths.indexOf(path);
  if (from === -1 || to < 0 || to >= paths.length || to === from) return paths;
  paths.splice(from, 1);
  paths.splice(to, 0, path);
  return paths;
}

/**
 * What to call a conversation in a list: the first thing it was asked for.
 *
 * The first prompt and not the last: a conversation is *about* what it was
 * started for, and naming it after the most recent message would rename it every
 * time somebody typed. Trimmed to one line, because a list row is one line — the
 * whole message is a scroll away in the conversation itself.
 */
export function sessionTitle(blocks: Block[]): string | null {
  for (const block of blocks) {
    if (block.kind !== "user") continue;
    const line = block.text.trim().split("\n", 1)[0].trim();
    if (line) return line;
    // A prompt that was only an image still names the conversation as well as
    // anything else can.
    if (block.images?.length) return "image";
  }
  return null;
}

const KEY = "tcode.rail.order";

export function loadOrder(): string[] {
  try {
    const raw = window.localStorage.getItem(KEY);
    if (!raw) return [];
    const stored = JSON.parse(raw) as unknown;
    // Stored data, so it is checked rather than trusted: an entry that is not a
    // path simply does not match any folder and ranks last.
    return Array.isArray(stored) ? stored.filter((entry): entry is string => typeof entry === "string") : [];
  } catch {
    return [];
  }
}

export function saveOrder(order: string[]): void {
  try {
    window.localStorage.setItem(KEY, JSON.stringify(order));
  } catch {
    // An arrangement that has to be redone next launch beats a click that fails.
  }
}
