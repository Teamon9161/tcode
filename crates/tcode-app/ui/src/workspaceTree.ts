/** The workspace commands' direct-child entry contract. Paths are slash-separated
 * and relative to the session workspace; the webview never turns them into host paths. */
export type WorkspaceEntry = {
  name: string;
  path: string;
  kind: "file" | "directory" | "link";
};

export type WorkspaceList = {
  entries: WorkspaceEntry[];
  warnings: string[];
};

/** A loaded directory's direct children, plus the directories open in this pane.
 * Missing `children[path]` means deliberately not loaded yet; an empty array means
 * it was loaded and is empty. That distinction keeps filtering honest. */
export type WorkspaceTreeState = {
  children: Readonly<Record<string, readonly WorkspaceEntry[] | undefined>>;
  expanded: ReadonlySet<string>;
};

export type WorkspaceTreeNode = WorkspaceEntry & {
  depth: number;
  expanded: boolean;
  loaded: boolean;
};

export const WORKSPACE_ROOT = "";

/**
 * A workspace-relative path as the host operating system writes it.
 *
 * Only for handing to a human — the clipboard. Nothing sent back over the wire
 * uses it: the workspace commands take slash-separated relative paths and turn
 * them into host paths themselves, which is the confinement (`AGENTS.md` rule 3
 * — what the webview says about paths is data). The separator is read off `cwd`
 * rather than sniffed from a user-agent string, because `cwd` is a real path
 * from the machine this is running on.
 */
export function workspaceHostPath(cwd: string, path: string): string {
  const separator = cwd.includes("\\") ? "\\" : "/";
  const root = cwd.replace(/[\\/]+$/, "");
  const rest = path.split("/").join(separator);
  return root ? `${root}${separator}${rest}` : rest;
}

export function emptyWorkspaceTree(): WorkspaceTreeState {
  return { children: {}, expanded: new Set() };
}

/** Directory-first, case-insensitive name order. Equal names retain backend order. */
export function sortWorkspaceEntries(entries: readonly WorkspaceEntry[]): WorkspaceEntry[] {
  return entries
    .map((entry, index) => ({ entry, index }))
    .sort((left, right) => {
      const kind = Number(right.entry.kind === "directory") - Number(left.entry.kind === "directory");
      if (kind !== 0) return kind;
      const name = left.entry.name.localeCompare(right.entry.name, undefined, { sensitivity: "accent" });
      return name || left.index - right.index;
    })
    .map(({ entry }) => entry);
}

/** Replaces one directory's known children after an initial or local refresh. */
export function replaceWorkspaceChildren(
  state: WorkspaceTreeState,
  path: string,
  entries: readonly WorkspaceEntry[],
): WorkspaceTreeState {
  return { ...state, children: { ...state.children, [path]: sortWorkspaceEntries(entries) } };
}

export function toggleWorkspaceDirectory(state: WorkspaceTreeState, path: string): WorkspaceTreeState {
  const expanded = new Set(state.expanded);
  if (expanded.has(path)) expanded.delete(path);
  else expanded.add(path);
  return { ...state, expanded };
}

export function isWorkspaceDirectoryLoaded(state: WorkspaceTreeState, path: string): boolean {
  return state.children[path] !== undefined;
}

/**
 * The tree rows that should be drawn for a query.
 *
 * A directory without a fetched child list is retained during filtering: it might
 * contain a match. Loaded directories are omitted only once their known subtree
 * has no match. This is a partial, lazy search rather than a claim to have
 * searched the workspace.
 */
export function visibleWorkspaceTree(state: WorkspaceTreeState, filter: string): WorkspaceTreeNode[] {
  const query = filter.trim().toLocaleLowerCase();
  const out: WorkspaceTreeNode[] = [];

  const walk = (parent: string, depth: number) => {
    for (const entry of state.children[parent] ?? []) {
      if (query && !matchesOrMayContain(state, entry, query)) continue;
      const loaded = entry.kind === "directory" && isWorkspaceDirectoryLoaded(state, entry.path);
      // A filter temporarily reveals loaded matching descendants without
      // mutating expansion state. Clearing it returns to exactly the folders
      // the person had opened; unloaded folders still stop at their own row.
      const filterExpanded =
        query.length > 0 &&
        entry.kind === "directory" &&
        Boolean(state.children[entry.path]?.some((child) => matchesOrMayContain(state, child, query)));
      const expanded = entry.kind === "directory" && (state.expanded.has(entry.path) || filterExpanded);
      out.push({ ...entry, depth, expanded, loaded });
      if (expanded && loaded) walk(entry.path, depth + 1);
    }
  };

  walk(WORKSPACE_ROOT, 0);
  return out;
}

/** Adds a just-created entry to its loaded parent without requesting the root again. */
export function createWorkspaceEntry(
  state: WorkspaceTreeState,
  parent: string,
  entry: WorkspaceEntry,
): WorkspaceTreeState {
  const children = state.children[parent];
  if (!children) return state;
  return replaceWorkspaceChildren(state, parent, [...children, entry]);
}

/** Renames an entry and rebases any loaded descendants of a renamed directory. */
export function renameWorkspaceEntry(
  state: WorkspaceTreeState,
  path: string,
  renamed: WorkspaceEntry,
): WorkspaceTreeState {
  const children: Record<string, readonly WorkspaceEntry[] | undefined> = {};
  for (const [parent, entries] of Object.entries(state.children)) {
    const nextParent = rebasePath(parent, path, renamed.path);
    const rebased = entries?.map((entry) => ({
      ...entry,
      ...(entry.path === path ? renamed : { path: rebasePath(entry.path, path, renamed.path) }),
    }));
    children[nextParent] = rebased && sortWorkspaceEntries(rebased);
  }
  const expanded = new Set([...state.expanded].map((entry) => rebasePath(entry, path, renamed.path)));
  return { children, expanded };
}

/** Removes an entry and all locally cached state below it. */
export function deleteWorkspaceEntry(state: WorkspaceTreeState, path: string): WorkspaceTreeState {
  const children: Record<string, readonly WorkspaceEntry[] | undefined> = {};
  for (const [parent, entries] of Object.entries(state.children)) {
    if (parent === path || parent.startsWith(`${path}/`)) continue;
    children[parent] = entries?.filter((entry) => entry.path !== path);
  }
  const expanded = new Set(
    [...state.expanded].filter((entry) => entry !== path && !entry.startsWith(`${path}/`)),
  );
  return { children, expanded };
}

function matchesOrMayContain(state: WorkspaceTreeState, entry: WorkspaceEntry, query: string): boolean {
  if (entry.name.toLocaleLowerCase().includes(query)) return true;
  if (entry.kind !== "directory") return false;
  const children = state.children[entry.path];
  if (children === undefined) return true;
  return children.some((child) => matchesOrMayContain(state, child, query));
}

function rebasePath(path: string, from: string, to: string): string {
  if (path === from) return to;
  return path.startsWith(`${from}/`) ? `${to}${path.slice(from.length)}` : path;
}
