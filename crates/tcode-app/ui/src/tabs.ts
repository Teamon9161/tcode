/**
 * A strip of tabs, as data.
 *
 * Pure, for the same reason `layout.ts` is: "what does closing the middle tab
 * select?" and "where does a new one go?" are questions with one right answer
 * each, and answering them inside a component means answering them by clicking.
 *
 * ## Why this is generic rather than written twice
 *
 * Two panes in this window have tabs — the terminals (`terminal.ts`) and the
 * browser (`web.ts`) — and what a tab *is* differs completely between them: one
 * is a shell with an exit code, the other a page with an address. What does not
 * differ is the strip: add, close, select, step. Those four had one careful
 * implementation with a bug-shaped rationale on `closeTab`, and copying it for
 * the second pane would have meant two strips that agree today and drift the
 * first time either is touched.
 *
 * So the strip lives here and knows nothing but `id`; each pane keeps its own
 * tab type and its own verbs (`renameTab`, `endTab`, `navigated`) beside it.
 */

/** The one thing this file needs to know about a tab. */
export type Tabbed = { id: string };

export type TabList<T extends Tabbed> = { list: T[]; current: string };

/** Shared rather than rebuilt per call: `useSyncExternalStore` compares
 *  snapshots by identity, and a fresh empty object every time is an infinite
 *  render. */
const EMPTY: TabList<Tabbed> = { list: [], current: "" };

export function noTabs<T extends Tabbed>(): TabList<T> {
  return EMPTY as TabList<T>;
}

/** A new tab takes focus: you asked for it, so you are looking at it. */
export function addTab<T extends Tabbed>(tabs: TabList<T>, tab: T): TabList<T> {
  return { list: [...tabs.list, tab], current: tab.id };
}

/**
 * A new tab that does *not* take focus.
 *
 * Because the sentence above stopped being true for one caller: an agent opens
 * browser tabs, and nobody asked for those — the whole point of the agent
 * browser is that a model can work in the window without taking the screen
 * from whoever is reading (`../AGENT-BROWSER.md`). The strip still has to list
 * the tab; it just must not select it.
 *
 * Two functions rather than `addTab(tabs, tab, focus)`, because a boolean at a
 * call site says nothing and the two behaviours have names. A caller that
 * wants both composes: `selectTab(addTabBehind(…), id)`, which is also what
 * makes the two ways a tab can be announced converge on the same state
 * whichever arrives first.
 */
export function addTabBehind<T extends Tabbed>(tabs: TabList<T>, tab: T): TabList<T> {
  return { ...tabs, list: [...tabs.list, tab] };
}

/**
 * Closes one tab, selecting the next one along — the tab that slid into the
 * place the closed one occupied.
 *
 * Falling back to the previous one at the end of the strip is what keeps the
 * selection *near* where you were looking; jumping to the first tab because the
 * last one closed is the behaviour every terminal emulator and every browser
 * got wrong once.
 */
export function closeTab<T extends Tabbed>(tabs: TabList<T>, id: string): TabList<T> {
  const at = tabs.list.findIndex((tab) => tab.id === id);
  if (at < 0) return tabs;
  const list = tabs.list.filter((tab) => tab.id !== id);
  if (!list.length) return noTabs<T>();
  if (tabs.current !== id) return { list, current: tabs.current };
  return { list, current: (list[at] ?? list[list.length - 1]).id };
}

/** Unknown ids are ignored rather than clearing the selection, for the same
 *  reason `focusPane` ignores them: a stale click must not leave the strip with
 *  nothing current. */
export function selectTab<T extends Tabbed>(tabs: TabList<T>, id: string): TabList<T> {
  return tabs.list.some((tab) => tab.id === id) ? { ...tabs, current: id } : tabs;
}

/** The next tab along, wrapping. Negative steps go the other way. */
export function stepTab<T extends Tabbed>(tabs: TabList<T>, delta: number): TabList<T> {
  if (tabs.list.length < 2) return tabs;
  const at = tabs.list.findIndex((tab) => tab.id === tabs.current);
  const next = (at + delta + tabs.list.length) % tabs.list.length;
  return { ...tabs, current: tabs.list[next].id };
}

/** Changes one tab in place, leaving the rest of the strip alone. The one
 *  primitive every pane-specific verb is written on, so an unknown id is a
 *  no-op in exactly one place rather than in each of them. */
export function mapTab<T extends Tabbed>(
  tabs: TabList<T>,
  id: string,
  change: (tab: T) => T,
): TabList<T> {
  if (!tabs.list.some((tab) => tab.id === id)) return tabs;
  return { ...tabs, list: tabs.list.map((tab) => (tab.id === id ? change(tab) : tab)) };
}

export function currentTab<T extends Tabbed>(tabs: TabList<T>): T | null {
  return tabs.list.find((tab) => tab.id === tabs.current) ?? null;
}
