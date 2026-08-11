import { invoke, listen } from "@ipc";

import {
  resetBrowserVisibility,
  setBrowserShown,
  syncBrowserVisibility,
} from "./browserYield";
import {
  addTabBehind,
  blankTab,
  closeTab,
  currentTab,
  discardDraft,
  disownedTabs,
  draftTab,
  isBlank,
  navigatedTab,
  newTab,
  NO_TABS,
  ownedTab,
  selectTab,
  sendingTab,
  stepTab,
  type Tabs,
} from "./web";
import {
  BROWSER_NAVIGATED,
  BROWSER_TAB_OPENED,
  BROWSER_THUMBNAIL,
  type AgentEvent,
  type BrowserThumbnail,
  type Navigated,
  type TabOpened,
} from "./types";

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
 * Two different things hide the browser and they must not fight. The pane's own
 * requested state — off screen because another pane is expanded, or because
 * the pane unmounted — and temporary yields for popovers or divider drags are
 * composed by `browserYield.ts`. Creating or selecting a view re-asserts that
 * same composed state rather than bypassing it.
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
  /** Latest renderer-only page previews, keyed by exact tab capability. */
  thumbnails: ReadonlyMap<string, BrowserThumbnail>;
};

const watchers = new Set<() => void>();

let state: Snapshot = {
  tabs: NO_TABS,
  failure: null,
  live: false,
  thumbnails: new Map(),
};
/** The pane's rectangle, as last measured. */
let bounds: { x: number; y: number; width: number; height: number } | null = null;
let listening = false;
/** Browser calls awaiting `ToolEnd`, including their session so a late event
 *  cannot re-assign a tab after that conversation has closed. */
const pendingBrowserCalls = new Map<
  string,
  { session: string; tab?: string; action: string }
>();
const closedSessions = new Set<string>();

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

/** Referentially stable until this exact tab receives a newer preview. */
export function thumbnail(id: string): BrowserThumbnail | undefined {
  return state.thumbnails.get(id);
}

/** Referentially stable until this exact tab navigates or is closed. */
export function tab(id: string) {
  return state.tabs.list.find((candidate) => candidate.id === id);
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
  state = { tabs: NO_TABS, failure: null, live: false, thumbnails: new Map() };
  pendingBrowserCalls.clear();
  closedSessions.clear();
  bounds = null;
  listening = false;
  resetBrowserVisibility();
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
      // Nothing is current when every tab in the strip was opened by an agent:
      // those arrive behind, on purpose, and nobody has selected one. Opening
      // the pane *is* asking to look at a page, so the first one comes forward
      // — otherwise the pane draws a strip of tabs over an empty rectangle.
      if (!currentTab(state.tabs)) select(state.tabs.list[0].id);
      settle();
    })
    .catch((error) => failed("cannot show the browser", error));
}

/** The pane moved or was resized. During a continuous resize the geometry
 *  coordinator calls this once with the final rectangle; ordinary structural
 *  moves still arrive directly. The promise preserves bounds-before-visible
 *  ordering when a yielded native page is restored. */
export function moved(rect: NonNullable<typeof bounds>): Promise<void> {
  bounds = rect;
  return invoke("browser_bounds", { rect })
    .then(() => undefined)
    .catch((error) => failed("cannot place the browser", error));
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
  setBrowserShown(false);
}

/** The pane is on screen but covered by an expanded pane. `visibility: hidden`
 *  does not reach a native webview, so it has to be told. */
export function shown(visible: boolean) {
  setBrowserShown(visible);
}

/** Re-assert what the pane wants, right after a call that shows a webview by
 *  creating or selecting it. The backend cannot know a pane mounted while
 *  another one is expanded. */
function settle() {
  syncBrowserVisibility();
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
    // `select: true` because somebody clicked `+`: this tab is the one they
    // want to be looking at. The shell reads the flag strictly and the other
    // caller — the backend, opening a tab for a model — passes nothing, which
    // is how an agent's page never takes the screen (`../AGENT-BROWSER.md`).
    id = await invoke<string>("browser_open", { rect: asked, select: true });
  } catch (error) {
    failed("cannot open a browser tab", error);
    return;
  }
  // Present, then selected. `+` is somebody asking to look at a new page, so
  // this is the caller that says so — `knownTab` alone never selects, which is
  // what lets the agent's tabs arrive by the same door without taking over.
  publish({ tabs: selectTab(knownTab(state.tabs, id, false), id), failure: null, live: true });
  // Identity, not equality: `moved` replaces the object, so this is exactly
  // "did the pane report anything while we were waiting".
  if (bounds !== asked) invoke("browser_bounds", { rect: bounds }).catch(() => {});
  settle();
  if (url) go(id, url);
}

export function select(id: string) {
  if (!state.tabs.list.some((tab) => tab.id === id)) return;
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
 * The shell answers whether the view is gone, **and the two shells answer
 * differently** — which is exactly why this asks instead of deciding. Under
 * Tauri the last webview holds the browser's profile and is never destroyed, so
 * closing the last tab blanks it and the answer is `false` (`browser.rs`
 * explains what re-opening the profile folder costs). Under Electron the
 * profile belongs to a session partition rather than to a view, so every tab
 * closes for real and the answer is always `true` (`electron/browser.js`).
 *
 * One of those is a tab that disappears and the other a tab back at "New tab";
 * guessing wrong leaves the strip describing a browser that is not there.
 */
export function close(id: string) {
  invoke<boolean>("browser_close", { id })
    .then((gone) => {
      if (gone) dropClosedTab(id);
      else publish({ tabs: blankTab(state.tabs, id), failure: null });
    })
    .catch((error) => failed("cannot close that tab", error));
}

function dropClosedTab(id: string) {
  const wasCurrent = state.tabs.current === id;
  const tabs = closeTab(state.tabs, id);
  const thumbnails = new Map(state.thumbnails);
  thumbnails.delete(id);
  publish({ tabs, thumbnails, failure: null });
  // The shell deliberately leaves nothing current after closing its selected
  // native view. The store chooses the neighbour and must tell the shell too.
  if (wasCurrent && tabs.current) select(tabs.current);
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

/**
 * Subscribed once for the whole app, not once per pane: hiding and re-opening
 * the pane would otherwise add a listener each time, and every navigation would
 * be applied as many times as the pane had been opened.
 *
 * **Called from the window's own startup as well as from the pane** (`watch`),
 * and that is not belt-and-braces. A model can open a tab in a window whose
 * browser pane has never been mounted; with the subscription starting at mount,
 * that tab's announcement went to nobody, and opening the pane afterwards found
 * an empty strip and opened a *second* tab. The page existed and nothing could
 * reach it.
 */
function listenOnce() {
  if (listening) return;
  listening = true;

  listen<Navigated>(BROWSER_NAVIGATED, (event) => {
    const { id, url, title } = event.payload;
    publish({ tabs: navigatedTab(state.tabs, id, url, title), failure: null });
  }).catch((error) => failed("cannot follow the browser", error));

  // A tab the strip did not open — the backend opened one for a model. Adding
  // it is the whole handler: it is not selected, so nothing on screen moves,
  // and its address arrives the same way every other tab's does.
  listen<TabOpened>(BROWSER_TAB_OPENED, (event) => {
    publish({ tabs: knownTab(state.tabs, event.payload.id, event.payload.agent) });
  }).catch((error) => failed("cannot follow the browser", error));

  listen<BrowserThumbnail>(BROWSER_THUMBNAIL, (event) => {
    const preview = event.payload;
    if (!state.tabs.list.some((tab) => tab.id === preview.id)) return;
    const prior = state.thumbnails.get(preview.id);
    if (prior && prior.revision >= preview.revision) return;
    const thumbnails = new Map(state.thumbnails);
    thumbnails.set(preview.id, preview);
    publish({ thumbnails });
  }).catch((error) => failed("cannot follow browser previews", error));
}

/** Start following the browser for the window's lifetime. Idempotent, and the
 *  pane still calls the same thing on mount. */
export function watch() {
  listenOnce();
}

/**
 * A conversation's `browser` call went past — learn what it says about a tab.
 *
 * This is where a tab acquires an owner, and it is the whole of the mechanism.
 * The backend cannot label the tab itself: a tool is a singleton shared by every
 * session and `ToolCtx` carries no session id, which is a decision rather than a
 * gap (`../../AGENT-BROWSER.md` argues it at length — a tab id is a capability,
 * and the isolation a session-to-tab table would buy is already free). What is
 * session-tagged is the event stream. So ownership is read off the one thing
 * that is both session-tagged and tab-naming: the call's own arguments.
 *
 * Calls with an input tab are claimed immediately. `open` has no such input,
 * so its successful structured `ToolEnd.ui_metadata` supplies the id without
 * parsing model-facing prose. The same start/end pair lets a successful model
 * `close` remove exactly that tab from the strip; failures leave it untouched.
 */
/**
 * A conversation was closed. Its tabs are not.
 *
 * The pages stay exactly as they are — logged in, half-scrolled, mid-form — and
 * only stop being attributed. This is the same promise the pane makes
 * everywhere else (hiding is not closing; `closeSession` filters browser panes
 * out), applied to the one new thing Phase 2 added: an owner.
 */
export function disown(session: string) {
  closedSessions.add(session);
  const tabs = disownedTabs(state.tabs, session);
  if (tabs !== state.tabs) publish({ tabs });
}

/**
 * The user is handing one of their tabs to a conversation.
 *
 * What comes back is text for a composer, not a message and not a note in the
 * ledger. A tab handed over with no question attached is of no use to anybody
 * — the point is always "look at *this*, and…" — so it lands in the draft the
 * same way `@path` does from the file tree, and the user finishes the sentence.
 *
 * It is also what makes the trust boundary trivial here: this ends up inside a
 * **user** message, which is an instruction source, and the user reads it
 * before pressing enter. The URL is the only thing taken from the page. The
 * *title* deliberately is not: that is prose a website wrote, and there is no
 * reason to put it in the user's own turn when the address says as much.
 */
export function handOverText(id: string): string | null {
  const tab = state.tabs.list.find((each) => each.id === id);
  if (!tab) return null;
  return tab.url ? `browser tab ${id} (${tab.url})` : `browser tab ${id}`;
}

export function claim(session: string, event: AgentEvent) {
  if (event.type === "ToolStart") {
    const call = event.data as {
      call_id?: string;
      name?: string;
      input?: { tab?: unknown; action?: unknown };
    };
    if (call.name !== "browser" || closedSessions.has(session)) return;
    const tab = call.input?.tab;
    if (call.call_id) {
      pendingBrowserCalls.set(call.call_id, {
        session,
        tab: typeof tab === "string" && tab ? tab : undefined,
        action: typeof call.input?.action === "string" ? call.input.action : "",
      });
    }
    if (typeof tab !== "string" || !tab) return;
    const tabs = ownedTab(state.tabs, tab, session);
    if (tabs !== state.tabs) publish({ tabs });
    return;
  }

  if (event.type !== "ToolEnd") return;
  const result = event.data as {
    call_id?: string;
    name?: string;
    is_error?: boolean;
    ui_metadata?: { kind?: string; id?: unknown };
  };
  if (result.name !== "browser") return;
  const pending = result.call_id ? pendingBrowserCalls.get(result.call_id) : undefined;
  if (result.call_id) pendingBrowserCalls.delete(result.call_id);
  if (result.is_error) return;

  const tagged = result.ui_metadata;
  const tab = tagged?.kind === "browser_tab" && typeof tagged.id === "string"
    ? tagged.id
    : pending?.tab;
  if (!tab) return;
  if (pending?.action === "close") {
    dropClosedTab(tab);
    return;
  }
  const known = knownTab(state.tabs, tab, true);
  if (closedSessions.has(session) || (pending && pending.session !== session)) {
    if (known !== state.tabs) publish({ tabs: known });
    return;
  }
  const tabs = ownedTab(known, tab, session);
  if (tabs !== state.tabs) publish({ tabs });
}

/**
 * Make sure the strip has a row for this tab, without disturbing one it
 * already has and without selecting it.
 *
 * Both paths that learn about a tab go through here: the id `browser_open`
 * answers with, and the event announcing the same tab. They race — two IPC
 * messages out of one call — and neither ordering is worth depending on, so
 * the second to arrive has to be a no-op rather than a duplicate row or a row
 * reset back to blank.
 *
 * Selecting is the caller's decision and is spelt separately (`selectTab`),
 * which is what makes both orderings land in the same place: whichever
 * arrives second finds the row there and changes nothing.
 *
 * `agent` is passed by both callers rather than inferred from which one it is,
 * for the same reason: the event can arrive before `browser_open` has answered,
 * and a row created by the wrong door must still describe the tab correctly.
 */
function knownTab(tabs: Tabs, id: string, agent: boolean): Tabs {
  return tabs.list.some((tab) => tab.id === id) ? tabs : addTabBehind(tabs, newTab(id, agent));
}
