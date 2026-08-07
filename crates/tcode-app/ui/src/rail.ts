import type { Block } from "./blocks";
import type { ProjectInfo, SessionInfo, Status } from "./types";

/**
 * The rail, as data.
 *
 * The rail lists **projects**, and a project's live conversations are the rows
 * under it. That is one list where there used to be two surfaces: a full-screen
 * launchpad accounting for every folder tcode had ever worked in, and a rail
 * accounting for the conversations open right now. The launchpad's "Open"
 * section *was* the rail drawn a second time as cards, and the price of the
 * duplication was a whole navigation mode — a screen you left the window to
 * reach and came back from.
 *
 * So the group heading stopped meaning "a folder that happens to hold a live
 * conversation" and became the project itself. Two bands come out of that, and
 * they are the product's own question asked twice:
 *
 * - **live** — projects with a conversation open. What is running and what is
 *   waiting for you lives here, at the top, where nothing can push it down.
 * - **recent** — every other folder tcode has worked in, newest first. Not
 *   state, just where you have been; capped, because a rail is a column and the
 *   long tail belongs in the finder.
 *
 * Pure functions here, drawn by `Workspace.tsx`, for the reason `layout.ts` is
 * separate from `Panes.tsx`: grouping, ordering and matching are decisions with
 * right answers, and a test can hold them.
 */

export type RailGroup = {
  /** The folder, which is the group's identity as well as its heading. */
  path: string;
  name: string;
  /** Open conversations in this folder. Empty for a `recent` group. */
  sessions: SessionInfo[];
  /** What the project store knows: how many logs, and when it was last worked
   *  in. Absent for a folder whose first conversation is open but whose logs
   *  have not been re-scanned yet. */
  info: ProjectInfo | null;
};

export type RailBands = {
  live: RailGroup[];
  recent: RailGroup[];
  /** Recent projects the cap left out. The finder is where they are. */
  overflow: number;
};

/** How many folders with no live conversation the rail will show. Beyond this
 *  the column stops being scannable, which is the one thing it is for. */
export const RECENT_CAP = 8;

/**
 * The rail's two bands.
 *
 * `order` holds only the folders that have been moved; everything else keeps
 * the order its first conversation arrived in. That is what makes an
 * arrangement survive opening a new folder: an unlisted project appends rather
 * than scattering the ones already placed.
 *
 * `hidden` drops a folder from `recent` only. A project with a conversation
 * open is never hidden — the rail's whole job is to account for those, and a
 * setting made last month must not be able to swallow one.
 */
export function railBands(
  sessions: SessionInfo[],
  projects: ProjectInfo[],
  order: string[],
  hidden: string[] = [],
  cap: number = RECENT_CAP,
): RailBands {
  const known = new Map(projects.map((project) => [project.path, project]));
  const live: RailGroup[] = [];
  const at = new Map<string, number>();
  for (const session of sessions) {
    const found = at.get(session.cwd);
    if (found !== undefined) {
      live[found].sessions.push(session);
      continue;
    }
    at.set(session.cwd, live.length);
    live.push({
      path: session.cwd,
      name: session.name,
      sessions: [session],
      info: known.get(session.cwd) ?? null,
    });
  }

  const rank = (path: string) => {
    const placed = order.indexOf(path);
    return placed === -1 ? order.length + (at.get(path) ?? 0) : placed;
  };
  live.sort((a, b) => rank(a.path) - rank(b.path));

  const away = new Set(hidden);
  const rest = projects
    .filter((project) => !at.has(project.path) && !away.has(project.path))
    .sort(
      (a, b) =>
        (b.last_active ?? 0) - (a.last_active ?? 0) ||
        a.path.localeCompare(b.path),
    )
    .map((project) => ({
      path: project.path,
      name: project.name,
      sessions: [],
      info: project,
    }));

  return {
    live,
    recent: rest.slice(0, cap),
    overflow: Math.max(0, rest.length - cap),
  };
}

/**
 * Move one folder to a position, returning the new order.
 *
 * It writes out the *whole* current order rather than editing the stored list,
 * because the stored list may not mention the folder being moved or the one it
 * lands next to. Storing the arrangement as it now reads is the only version of
 * this that cannot drift from what is on screen.
 */
export function moveProject(
  groups: RailGroup[],
  path: string,
  to: number,
): string[] {
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

const KEY = "tcode.rail.order";
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

export const loadOrder = () => loadList(KEY);
export const saveOrder = (order: string[]) => saveList(KEY, order);
export const loadHidden = () => loadList(AWAY);
export const saveHidden = (hidden: string[]) => saveList(AWAY, hidden);
