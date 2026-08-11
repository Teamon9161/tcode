import type { Block } from "./blocks";
import type { ProjectInfo, SessionInfo, Status } from "./types";

/** The project list below open sessions. Projects are places rather than
 * containers for the live rows, so an open project's folder may also appear
 * here; the two rows answer different questions. */
export type RecentProjects = {
  projects: ProjectInfo[];
  overflow: number;
};

/** Projects are revealed in deliberate, scan-sized steps. The rail itself
 * scrolls; this limit controls DOM and visual density, not reachability. */
export const RECENT_STEP = 8;

export function recentProjects(
  projects: ProjectInfo[],
  hidden: string[] = [],
  limit: number = RECENT_STEP,
): RecentProjects {
  const away = new Set(hidden);
  const visible = projects
    .filter((project) => !away.has(project.path))
    .sort(
      (a, b) =>
        (b.last_active ?? 0) - (a.last_active ?? 0) ||
        a.path.localeCompare(b.path),
    );
  return {
    projects: visible.slice(0, limit),
    overflow: Math.max(0, visible.length - limit),
  };
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

/** One thing the finder can take you to. A conversation arrives already named,
 *  captioned and statused by the rail's own rules, so the finder cannot end up
 *  describing it differently from the row two inches to its left. */
export type FoundSession = {
  kind: "session";
  session: SessionInfo;
  title: string;
  activity: string;
  status: Status;
};

export type Found = FoundSession | { kind: "project"; project: ProjectInfo };

/**
 * The finder's answer: open conversations first, then folders.
 *
 * Conversations lead because they are the ones that can be *waiting for you* —
 * the same reason they hold the top of the rail. Folders follow, and they are
 * every folder rather than the rail's capped band: reaching the long tail is
 * exactly what the finder is for.
 *
 * Matching is a plain case-insensitive substring over the words a reader can
 * see (a conversation's title and its folder, a project's name and its path).
 * Not fuzzy: this list is tens of items long, and a fuzzy matcher's whole value
 * is ranking thousands. What it buys instead is that a query which matches
 * nothing means the thing is not there, rather than that you spelled it in a way
 * the scorer disliked.
 */
export function find(
  query: string,
  sessions: FoundSession[],
  projects: ProjectInfo[],
): Found[] {
  const needle = query.trim().toLowerCase();
  const hits = (...fields: string[]) =>
    !needle || fields.some((field) => field.toLowerCase().includes(needle));

  const found: Found[] = sessions.filter((entry) =>
    hits(entry.title, entry.session.name, entry.session.cwd),
  );
  for (const project of projects) {
    if (hits(project.name, project.path))
      found.push({ kind: "project", project });
  }
  return found;
}

/** The identity a found row is keyed and compared by. */
export function foundKey(entry: Found): string {
  return entry.kind === "session"
    ? `s:${entry.session.id}`
    : `p:${entry.project.path}`;
}

const AWAY = "tcode.rail.hidden";

function loadList(key: string): string[] {
  try {
    const raw = window.localStorage.getItem(key);
    if (!raw) return [];
    const stored = JSON.parse(raw) as unknown;
    // Stored data, so it is checked rather than trusted: an entry that is not a
    // path simply does not match any folder and ranks last.
    return Array.isArray(stored)
      ? stored.filter((entry): entry is string => typeof entry === "string")
      : [];
  } catch {
    return [];
  }
}

function saveList(key: string, value: string[]): void {
  try {
    window.localStorage.setItem(key, JSON.stringify(value));
  } catch {
    // An arrangement that has to be redone next launch beats a click that fails.
  }
}

export const loadHidden = () => loadList(AWAY);
export const saveHidden = (hidden: string[]) => saveList(AWAY, hidden);
