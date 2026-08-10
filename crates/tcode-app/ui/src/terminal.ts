import { focused, paneSession, type Tiling } from "./layout";
import { basename } from "./show";
import { mapTab, noTabs, type TabList } from "./tabs";

/**
 * The terminal's tabs, as data.
 *
 * The strip itself — add, close, select, step — is `tabs.ts`, shared with the
 * browser's; what is here is what a *terminal* tab is: a shell, a folder, and
 * an exit code once its program ends. Nothing here knows what a terminal looks
 * like or that a PTY exists — `termHost.ts` owns both.
 *
 * ## Why tabs live here at all
 *
 * `layout.ts` notes that the terminals have no session and there is only ever
 * one pane of them, so there is exactly one tab strip and no question of which
 * terminal a tab belongs to. A tab is a place *inside* the pane rather than a
 * second kind of pane, and the tiling tree stays free of it.
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

export type Tabs = TabList<Tab>;

export const NO_TABS: Tabs = noTabs<Tab>();

export { addTab, closeTab, currentTab, selectTab, stepTab } from "./tabs";

/** What the shell says it is (OSC 2). Blank titles are ignored: a program that
 *  clears the title is not asking for an unnamed tab, and `tabLabel` has a
 *  better answer than an empty strip. */
export function renameTab(tabs: Tabs, id: string, title: string): Tabs {
  const clean = title.trim();
  if (!clean) return tabs;
  return mapTab(tabs, id, (tab) => ({ ...tab, title: clean }));
}

/** The program ended. The tab stays; only its status changes. */
export function endTab(tabs: Tabs, id: string, code: number): Tabs {
  return mapTab(tabs, id, (tab) => ({ ...tab, exit: code }));
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

/** Chooses the folder for the next terminal tab without treating the terminal
 * itself as a session. Once focus enters that session-less pane, the most
 * recently focused session remains the only unambiguous source of a folder. */
export function cwdForTerminal(
  tiling: Tiling,
  sessions: readonly { id: string; cwd: string }[],
  remembered: string | null,
): string {
  const current = focused(tiling);
  const session = current && paneSession(current.pane);
  return (
    sessions.find((open) => open.id === session)?.cwd ??
    remembered ??
    sessions[0]?.cwd ??
    ""
  );
}
