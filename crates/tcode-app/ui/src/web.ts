import { mapTab, noTabs, type TabList } from "./tabs";

/**
 * The browser's tabs, as data.
 *
 * The strip itself — add, close, select, step — is `tabs.ts`, shared with the
 * terminals'; what is here is what a *browser* tab is: a page, and the address
 * bar's state for it.
 *
 * ## The address bar is a view, not the source of truth
 *
 * A tab's `url` is only ever set from what the webview reported
 * (`BROWSER_NAVIGATED`), so a redirect, a link click and a `history.back()`
 * all reach the strip the same way and nothing has to guess whether a
 * navigation happened. What someone is *typing* is `draft`, a separate field,
 * and the two are reconciled by exactly one rule:
 *
 *   the draft is dropped when the navigation it asked for comes back.
 *
 * `sent` is what makes that rule expressible — the address whose navigation is
 * still in flight. Without it the pane had the bug this file's tests are named
 * after: every event (each navigation, each title change, and the WebView2
 * runtime's slow initial `about:blank`) wiped the field, so Enter had nothing
 * left to send.
 *
 * Both live per tab rather than in the component, because a tab you switch away
 * from and come back to should still be holding the address you were halfway
 * through typing — the same promise `termHost` makes about scrollback.
 */

/** One page. `id` is the backend's, and it is the identity of the native
 *  webview behind the tab — there is no second numbering to keep in step. */
export type Tab = {
  id: string;
  /** Where the page is, as the webview last reported it. */
  url: string;
  /** What the document called itself. Empty until it says. */
  title: string;
  /** A half-typed address, or `null` when the field is showing `url`. */
  draft: string | null;
  /** The address whose navigation has not round-tripped yet, if any. */
  sent: string | null;
  /**
   * Opened by the backend for a model rather than by the strip's `+`.
   *
   * Known when the tab is born, and never revised: a page the user takes over
   * still arrived without them asking, and a strip that quietly dropped the
   * mark would be answering "did I open this" with "am I using it".
   */
  agent: boolean;
  /**
   * The conversation driving it, once one has shown itself. `null` for the
   * user's own tabs, and for an agent's between `open` and its first act.
   *
   * Learnt rather than announced, because there is nothing to announce it:
   * `ToolCtx` carries no session id (deliberately — `../../AGENT-BROWSER.md`),
   * so the backend cannot put one in the event. What *is* session-tagged is the
   * event stream, and a `browser` call that names a tab is both. See
   * `webHost.ts::claim`.
   */
  owner: string | null;
};

export type Tabs = TabList<Tab>;

export const NO_TABS: Tabs = noTabs<Tab>();

export { addTab, addTabBehind, closeTab, currentTab, mapTab, selectTab, stepTab } from "./tabs";

/** A tab as the backend hands it over: a webview at its blank start. */
export function newTab(id: string, agent = false): Tab {
  return { id, url: "", title: "", draft: null, sent: null, agent, owner: null };
}

/** A conversation showed itself as the one driving this tab.
 *
 *  First claim wins. A tab is handed between conversations by the user, not by
 *  a model mentioning an id it read somewhere, and re-pointing the label on a
 *  tool call would make "who is driving this" a thing that flickered. */
export function ownedTab(tabs: Tabs, id: string, session: string): Tabs {
  return mapTab(tabs, id, (tab) => (tab.owner ? tab : { ...tab, owner: session }));
}

/**
 * A conversation ended. Its tabs stay; they just stop belonging to anybody.
 *
 * The same sentence the browser pane is built on — closing a conversation must
 * not take away the page you are reading — and the same one `closeSession`
 * already says by filtering on `paneSession`. A tab is a window-level thing
 * that a conversation was *using*, so the end of the conversation ends the
 * using and nothing else. `agent` is not touched: it is a fact about where the
 * tab came from, and that does not stop being true.
 */
export function disownedTabs(tabs: Tabs, session: string): Tabs {
  if (!tabs.list.some((tab) => tab.owner === session)) return tabs;
  return {
    ...tabs,
    list: tabs.list.map((tab) => (tab.owner === session ? { ...tab, owner: null } : tab)),
  };
}

/** Someone typed. */
export function draftTab(tabs: Tabs, id: string, draft: string): Tabs {
  return mapTab(tabs, id, (tab) => ({ ...tab, draft }));
}

/** Esc: give the field back to the page's real address rather than leaving a
 *  half-typed one on screen. */
export function discardDraft(tabs: Tabs, id: string): Tabs {
  return mapTab(tabs, id, (tab) => ({ ...tab, draft: null }));
}

/** An address was sent to the webview. Recorded rather than applied: whether
 *  the page actually goes there is the page's answer, and it comes back as a
 *  navigation. */
export function sendingTab(tabs: Tabs, id: string, where: string | null): Tabs {
  return mapTab(tabs, id, (tab) => ({ ...tab, sent: where }));
}

/**
 * The webview reported where it is.
 *
 * The draft is dropped only when this is the round trip of what *we* sent and
 * the field still holds it. A newer address typed while the old navigation was
 * in flight survives, which is the whole point of comparing rather than
 * clearing.
 */
export function navigatedTab(tabs: Tabs, id: string, url: string, title: string): Tabs {
  return mapTab(tabs, id, (tab) => ({
    ...tab,
    url,
    title,
    draft: tab.sent !== null && tab.draft === tab.sent ? null : tab.draft,
    sent: null,
  }));
}

/** Back to the blank start, keeping the tab. This is what closing the last tab
 *  does: the webview holding the browser's profile is never destroyed
 *  (`browser.rs`), so the page goes and the tab stays. */
export function blankTab(tabs: Tabs, id: string): Tabs {
  // Everything about the *page* goes; who the tab belongs to stays. Emptying a
  // tab is not the same event as somebody else opening it.
  return mapTab(tabs, id, (tab) => ({ ...newTab(id, tab.agent), owner: tab.owner }));
}

/** True while a tab is at its blank start — nothing loaded, nothing typed that
 *  went anywhere. A blank tab is the one a link can take over rather than
 *  opening beside. */
export function isBlank(tab: Tab): boolean {
  return (tab.url === "" || tab.url === "about:blank") && !tab.draft;
}

/**
 * What the strip writes on a tab.
 *
 * The document's own title when it has one — it is what the page calls itself,
 * and it is what every browser shows. Otherwise the address without its scheme,
 * because `https://` down a whole strip is eight characters of nothing. Never
 * the id: it is a UUID.
 */
export function tabLabel(tab: Tab): string {
  if (tab.title) return tab.title;
  const bare = tab.url.replace(/^[a-z0-9+-]+:\/\//i, "").replace(/\/$/, "");
  return bare && bare !== "about:blank" ? bare : "New tab";
}
