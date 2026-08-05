import { basename } from "./show";

/**
 * The terminal's tabs, as data.
 *
 * Pure, for the same reason `layout.ts` is: "what does closing the middle tab
 * select?" and "does a tab that exited still count?" are questions with one
 * right answer each, and answering them inside a component means answering them
 * by clicking. Nothing here knows what a terminal looks like or that a PTY
 * exists — `termHost.ts` owns both.
 *
 * ## Why tabs live here at all
 *
 * `layout.ts` notes that the window's browser has no session and there is only
 * ever one of it, "so when tabs arrive there is exactly one tab strip and no
 * question of which browser a tab belongs to". The terminal is the same shape
 * and inherits the same conclusion: one pane, one strip, and a tab is a place
 * inside it rather than a second kind of pane.
 */

/** One shell. `id` is the backend's — the PTY is the tab's identity, so there
 *  is never a second, frontend-side numbering to keep in step. */
export type Tab = {
  id: string;
  /** What the shell called itself, via OSC 2. Empty until it says. */
  title: string;
  cwd: string;
  /** The exit code, once the program ended. `null` while it runs.
   *
   *  An ended tab stays: the scrollback is the record of what happened, and a
   *  tab that vanishes the moment a command fails takes the error message with
   *  it. Closing it is the user's act. */
  exit: number | null;
};

export type Tabs = { list: Tab[]; current: string };

export const NO_TABS: Tabs = { list: [], current: "" };

/** A new tab always takes focus: you asked for it, so you are typing in it. */
export function addTab(tabs: Tabs, tab: Tab): Tabs {
  return { list: [...tabs.list, tab], current: tab.id };
}

/**
 * Closes one tab, selecting the next one along — the tab that slid into the
 * place the closed one occupied.
 *
 * Falling back to the previous one at the end of the strip is what keeps the
 * selection *near* where you were looking; jumping to the first tab because the
 * last one closed is the behaviour every terminal emulator got wrong once.
 */
export function closeTab(tabs: Tabs, id: string): Tabs {
  const at = tabs.list.findIndex((tab) => tab.id === id);
  if (at < 0) return tabs;
  const list = tabs.list.filter((tab) => tab.id !== id);
  if (!list.length) return NO_TABS;
  if (tabs.current !== id) return { list, current: tabs.current };
  return { list, current: (list[at] ?? list[list.length - 1]).id };
}

/** Unknown ids are ignored rather than clearing the selection, for the same
 *  reason `focusPane` ignores them: a stale click must not leave the strip with
 *  nothing current. */
export function selectTab(tabs: Tabs, id: string): Tabs {
  return tabs.list.some((tab) => tab.id === id) ? { ...tabs, current: id } : tabs;
}

/** The next tab along, wrapping. Negative steps go the other way. */
export function stepTab(tabs: Tabs, delta: number): Tabs {
  if (tabs.list.length < 2) return tabs;
  const at = tabs.list.findIndex((tab) => tab.id === tabs.current);
  const next = (at + delta + tabs.list.length) % tabs.list.length;
  return { ...tabs, current: tabs.list[next].id };
}

/** What the shell says it is (OSC 2). Blank titles are ignored: a program that
 *  clears the title is not asking for an unnamed tab, and `tabLabel` has a
 *  better answer than an empty strip. */
export function renameTab(tabs: Tabs, id: string, title: string): Tabs {
  const clean = title.trim();
  if (!clean) return tabs;
  return {
    ...tabs,
    list: tabs.list.map((tab) => (tab.id === id ? { ...tab, title: clean } : tab)),
  };
}

/** The program ended. The tab stays; only its status changes. */
export function endTab(tabs: Tabs, id: string, code: number): Tabs {
  return {
    ...tabs,
    list: tabs.list.map((tab) => (tab.id === id ? { ...tab, exit: code } : tab)),
  };
}

export function currentTab(tabs: Tabs): Tab | null {
  return tabs.list.find((tab) => tab.id === tabs.current) ?? null;
}

/**
 * What the strip writes on a tab.
 *
 * The shell's own title when it has one — it knows the host, the folder and
 * often the running command, which is more than this side can work out — and
 * the folder's name otherwise. Never the id: it is a UUID.
 */
export function tabLabel(tab: Tab): string {
  return tab.title || basename(tab.cwd) || "shell";
}
