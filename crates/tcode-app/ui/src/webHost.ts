import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

import {
  addTab,
  blankTab,
  closeTab,
  currentTab,
  discardDraft,
  draftTab,
  isBlank,
  navigatedTab,
  newTab,
  NO_TABS,
  selectTab,
  sendingTab,
  stepTab,
  type Tabs,
} from "./web";
import { BROWSER_NAVIGATED, type Navigated } from "./types";

/**
 * The window's browser: its tabs, and the bridge to the webviews behind them.
 *
 * ## Why this is a module and not a component's state
 *
 * The same reason `termHost.ts` gives. Hiding the browser pane unmounts
 * `WebPane`, and the pages do not go anywhere when it does — they are native
 * child webviews the backend owns, still loaded, still logged in. If the tab
 * list lived in React state that unmount would lose the list while the webviews
 * stayed, which is the worst of both: pages nobody can reach and a strip that
 * has forgotten them.
 *
 * So the tabs live here, outside React, exactly as far outside as the webviews
 * are. `layout.ts` says the same thing from its side: the tiling tree holds
 * `{kind: "web"}` and no tab list.
 *
 * ## The rectangle, and who reports it
 *
 * A native webview does not participate in layout — it sits where it was last
 * told. Only a mounted pane can measure that rectangle, so `WebPane` measures
 * and this module remembers: a tab opened later, or brought forward, is placed
 * from the last reported rect rather than asking the pane again.
 *
 * ## What "hidden" means here
 *
 * Two different things hide the browser and they must not fight. `wanted` is
 * the pane's own state — off screen because another pane is expanded, or
 * because the pane unmounted. The popover yield (`browserYield.ts`) is a
 * separate, momentary borrow of the window and talks to the backend directly.
 * This module only ever re-asserts `wanted`, and only right after creating or
 * selecting a webview, which is the one moment the backend cannot know it.
 */

type Snapshot = {
  tabs: Tabs;
  /** Anything that failed on the bridge, shown in the pane rather than left as
   *  a rejected promise nobody sees (AGENTS.md rule 7). */
  failure: string | null;
  /** True once the pane is mounted and the backend has confirmed a webview.
   *  A link followed elsewhere waits for this rather than racing the pane's
   *  first `browser_open`. */
  live: boolean;
};

const watchers = new Set<() => void>();

let state: Snapshot = { tabs: NO_TABS, failure: null, live: false };
/** The pane's rectangle, as last measured. */
let bounds: { x: number; y: number; width: number; height: number } | null = null;
/** Whether the pane wants the browser on screen. */
let wanted = true;
let listening = false;

// ------------------------------------------------------------------ the store

export function subscribe(watcher: () => void): () => void {
  watchers.add(watcher);
  return () => {
    watchers.delete(watcher);
  };
}

/** Referentially stable between changes, which `useSyncExternalStore` requires
 *  — a fresh object every call is an infinite render. */
export function snapshot(): Snapshot {
  return state;
}

function publish(next: Partial<Snapshot>) {
  state = { ...state, ...next };
  for (const watcher of watchers) watcher();
}

function failed(what: string, error: unknown) {
  publish({ failure: `${what}: ${String(error)}` });
}

/** Tests only. The app has one browser for its lifetime, so nothing else has
 *  any business emptying this. */
export function reset() {
  state = { tabs: NO_TABS, failure: null, live: false };
  bounds = null;
  wanted = true;
  listening = false;
}

// ------------------------------------------------------------------- the pane

/**
 * The pane appeared, at `rect`.
 *
 * First ever: there is nothing to show, so this is where the first tab comes
 * from — an empty browser pane is not a state anybody asked for, opening it is
 * asking for a page. Afterwards it is a re-show: the webviews were only hidden,
 * and they come back with their pages exactly as they were.
 */
export function mount(rect: NonNullable<typeof bounds>) {
  bounds = rect;
  listenOnce();
  if (!state.tabs.list.length) {
    void open();
    return;
  }
  invoke("browser_show", { rect })
    .then(() => {
      publish({ live: true });
      settle();
    })
    .catch((error) => failed("cannot show the browser", error));
}

/** The pane moved or was resized. Cheap on purpose: this runs for every frame
 *  of a divider drag. */
export function moved(rect: NonNullable<typeof bounds>) {
  bounds = rect;
  invoke("browser_bounds", { rect }).catch((error) => failed("cannot place the browser", error));
}

/**
 * The pane went away — hidden by the button that opened it, or because another
 * pane fills the field.
 *
 * The webviews are *hidden*, never closed: they are native children the DOM
 * cannot move, so nothing but this takes them off the screen, and destroying
 * them would throw away every page and every login the browser is holding.
 */
export function unmount() {
  publish({ live: false });
  invoke("browser_visible", { visible: false }).catch(() => {});
}

/** The pane is on screen but covered by an expanded pane. `visibility: hidden`
 *  does not reach a native webview, so it has to be told. */
export function shown(visible: boolean) {
  wanted = visible;
  invoke("browser_visible", { visible }).catch(() => {});
}

/** Re-assert what the pane wants, right after a call that shows a webview by
 *  creating or selecting it. The backend cannot know a pane mounted while
 *  another one is expanded. */
function settle() {
  if (!wanted) invoke("browser_visible", { visible: false }).catch(() => {});
}

// ------------------------------------------------------------------- the tabs

/**
 * Opens a tab, optionally at an address, and gives it the strip's focus.
 *
 * Creating a webview is slow — a whole browser engine — and the pane goes on
 * measuring while it happens. Any rect that moves in that window reaches a
 * backend with no tab to put it on, and the pane will not send it again,
 * because it only reports a rect when it *changes*. So the freshest rect is
 * re-sent here once there is something to place. The backend re-places from
 * its own remembered rect too; this is the half that also covers a move
 * arriving in the gap between the two.
 */
export async function open(url?: string) {
  if (!bounds) return;
  listenOnce();
  const asked = bounds;
  let id: string;
  try {
    id = await invoke<string>("browser_open", { rect: asked });
  } catch (error) {
    failed("cannot open a browser tab", error);
    return;
  }
  publish({ tabs: addTab(state.tabs, newTab(id)), failure: null, live: true });
  // Identity, not equality: `moved` replaces the object, so this is exactly
  // "did the pane report anything while we were waiting".
  if (bounds !== asked) invoke("browser_bounds", { rect: bounds }).catch(() => {});
  settle();
  if (url) go(id, url);
}

export function select(id: string) {
  publish({ tabs: selectTab(state.tabs, id) });
  invoke("browser_select", { id })
    .then(settle)
    .catch((error) => failed("cannot show that tab", error));
}

export function step(delta: number) {
  const next = stepTab(state.tabs, delta);
  if (next.current !== state.tabs.current) select(next.current);
}

/**
 * Closes a tab and the page in it.
 *
 * The backend answers whether the webview is gone. It says no for the last one:
 * that webview holds the browser's profile and is never destroyed, so closing
 * the last tab blanks it instead (`browser.rs` explains what re-opening the
 * profile folder costs). The strip says whichever of the two happened rather
 * than deciding for itself — one of them is a tab that disappears, the other a
 * tab back at "New tab", and guessing wrong leaves the strip describing a
 * browser that is not there.
 */
export function close(id: string) {
  const wasCurrent = state.tabs.current === id;
  invoke<boolean>("browser_close", { id })
    .then((gone) => {
      publish({ tabs: gone ? closeTab(state.tabs, id) : blankTab(state.tabs, id), failure: null });
      // Closing the current tab hands the front to its neighbour, and a native
      // webview only comes forward when it is told to.
      if (gone && wasCurrent && state.tabs.current) select(state.tabs.current);
    })
    .catch((error) => failed("cannot close that tab", error));
}

/** Whether closing this tab would leave the browser with nothing to show —
 *  which is when the pane goes away too. */
export function isLast(id: string): boolean {
  return state.tabs.list.length === 1 && state.tabs.list[0].id === id;
}

// ---------------------------------------------------------------- the address

/** Someone typed in the address bar. Per tab, so switching away and back does
 *  not lose a half-typed address. */
export function draft(id: string, text: string) {
  publish({ tabs: draftTab(state.tabs, id, text) });
}

/** Esc: back to the address the page is actually at. */
export function discard(id: string) {
  publish({ tabs: discardDraft(state.tabs, id) });
}

/** Ask a tab to go somewhere. What comes back is a navigation event, which is
 *  the only thing that moves the address bar. */
export function go(id: string, where: string) {
  publish({ tabs: sendingTab(state.tabs, id, where) });
  invoke("browser_navigate", { id, url: where })
    .then(() => publish({ failure: null }))
    .catch((error) => {
      // Nothing is in flight after a refusal, so the draft must stop waiting
      // for a round trip that will never arrive — otherwise the next unrelated
      // navigation would clear what is still in the field.
      publish({ tabs: sendingTab(state.tabs, id, null) });
      failed("cannot go there", error);
    });
}

export function back() {
  history(-1);
}

export function forward() {
  history(1);
}

function history(delta: number) {
  const tab = currentTab(state.tabs);
  if (!tab) return;
  invoke("browser_step", { id: tab.id, delta }).catch((error) => failed("cannot go there", error));
}

export function reload() {
  const tab = currentTab(state.tabs);
  if (!tab) return;
  invoke("browser_reload", { id: tab.id }).catch((error) => failed("cannot reload", error));
}

/**
 * A page the window was asked for — a link followed in a conversation.
 *
 * It opens a new tab unless the current one is blank, which is what a browser
 * does with a link handed to it from outside, and it is the same reasoning the
 * pane is built on: the page you are reading is not something a click somewhere
 * else should take away.
 */
export function visit(url: string) {
  const tab = currentTab(state.tabs);
  if (tab && isBlank(tab)) {
    go(tab.id, url);
    return;
  }
  void open(url);
}

// ----------------------------------------------------------------- the bridge

/** Subscribed once for the whole app, not once per pane: hiding and re-opening
 *  the pane would otherwise add a listener each time, and every navigation
 *  would be applied as many times as the pane had been opened. */
function listenOnce() {
  if (listening) return;
  listening = true;

  listen<Navigated>(BROWSER_NAVIGATED, (event) => {
    const { id, url, title } = event.payload;
    publish({ tabs: navigatedTab(state.tabs, id, url, title), failure: null });
  }).catch((error) => failed("cannot follow the browser", error));
}
