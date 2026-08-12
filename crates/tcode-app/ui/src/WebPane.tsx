import { useEffect, useRef, useSyncExternalStore, type RefObject } from "react";
import {
  ArrowUp,
  BackIcon,
  CloseIcon,
  CollapseIcon,
  ExpandIcon,
  ForwardIcon,
  PlusIcon,
  RefreshIcon,
} from "./components/Icons";
import { currentTab, tabLabel, type Tab } from "./web";
import * as browser from "./webHost";

/**
 * What the tooltip says about a tab an agent opened.
 *
 * In words, and the dot's `aria-label` says the same thing — because the strip
 * already has a rule about this (`.tab-exit` in `app.css`): a fact is never
 * carried by colour alone. The colour is here to tell two conversations' tabs
 * apart at a glance, which is a job colour is good at; *what it means* is text.
 */
function agentHint(tab: Tab, nameOf: (session: string) => string): string {
  if (!tab.agent) return "";
  const who = tab.owner ? nameOf(tab.owner) : null;
  return who ? `Opened by "${who}", not by you` : "Opened by a conversation, not by you";
}

/**
 * A stable colour per conversation, from its id.
 *
 * A hash and not a palette, because a palette is a table somebody has to keep
 * in step with a list of open conversations that changes all day. Lightness and
 * chroma are fixed so every hue lands at the same weight against either theme,
 * and only the hue turns — which is the one dimension the eye reads as
 * "different thing" rather than "more important thing".
 *
 * Local to this file on purpose: one consumer, one place. If the rail ever
 * wants the same colour beside a conversation, that is when it moves somewhere
 * both can reach — not before (the narrowest-scope rule in the root CLAUDE.md).
 */
function tint(owner: string | null): React.CSSProperties | undefined {
  if (!owner) return undefined;
  let hash = 0;
  for (let at = 0; at < owner.length; at += 1) {
    hash = (hash * 31 + owner.charCodeAt(at)) >>> 0;
  }
  // Only the hue is computed here, because only the hue is this file's answer.
  // How light and how saturated a mark on this paper may be is the theme's
  // (`--tint-lightness` / `--tint-chroma`): a fixed pair baked in here would
  // survive a palette change and turn into the one chip on screen still lit for
  // the previous one.
  return { "--tint-hue": hash % 360 } as React.CSSProperties;
}

/**
 * The window's browser.
 *
 * What is drawn here is only the chrome — tab strip, toolbar, the frame. The
 * pages themselves are **native child webviews** the backend owns
 * (`src/browser.rs`), one per tab, composited over the rectangle this pane's
 * body occupies. They are not in the DOM and cannot be reached from it. What
 * the tabs *are* lives in `webHost.ts`, outside React, because hiding the pane
 * unmounts this component and the pages survive it — the same division
 * `TermPane` draws.
 *
 * Three consequences shape this file:
 *
 *  - **The rectangle has to be reported, continuously.** A native webview does
 *    not participate in layout; it sits where it was last told. The lightweight
 *    slot wrapper in `Panes.tsx` measures this component's body after every slot
 *    render and on resize. It lives outside the memoized pane subtree so a pane
 *    can move without changing size while the expensive chrome and neighbours
 *    still skip their renders.
 *  - **The address bar is a view, not the source of truth.** The webview owns
 *    where it is; typing here only asks it to go somewhere. The URL displayed
 *    comes back over `BROWSER_NAVIGATED`, so a redirect, a link click and a
 *    `history.back()` all update it the same way, and nothing has to guess
 *    whether a navigation actually happened.
 *  - **Leaving the screen is not the same as closing.** The webviews are the
 *    window's, not the pane's: hiding the pane hides them and keeps their
 *    pages, and re-opening shows them again. Closing a *tab* closes that page;
 *    the last one is blanked rather than destroyed, because that webview holds
 *    the profile every login lives in (`browser.rs`).
 *
 * The chrome is two rows, like a real browser: a tab strip above a toolbar
 * (history, address, reload). They were one row once, and reload and close
 * sitting side by side read as the same kind of button — which is how "refresh
 * the page" and "leave the browser" became the same question. Closing lives on
 * the tab, refreshing on the toolbar.
 *
 * The history buttons cannot be greyed out. Whether there is anywhere to go
 * back to lives in the page's own history, across an origin this side cannot
 * read; `history.go` with nowhere to go does nothing, which is the harmless
 * direction. Greying them out would need a second, guessed copy of a stack the
 * page already keeps.
 */
export function WebPane({
  bodyRef,
  onClose,
  expanded,
  onToggleExpanded,
  hidden,
  request,
  nameOf,
  onHandOver,
}: {
  /** The slot owns native-view geometry so its cheap wrapper can update while
   *  this pane's memoized subtree stays untouched. */
  bodyRef: RefObject<HTMLDivElement | null>;
  onClose: () => void;
  /** This pane is the one filling the field. */
  expanded: boolean;
  onToggleExpanded: () => void;
  /** Another pane is filling the field and this one is `visibility: hidden`.
   *  The native webviews do not participate in CSS, so they have to be told. */
  hidden: boolean;
  /**
   * A page the window was asked for — a link followed in a conversation.
   *
   * It arrives here rather than going straight to the backend because the
   * native webview does not exist until this pane mounts and creates it, and
   * navigating a webview that is not there fails. The pane is the only side
   * that knows when it exists, so it is the side that sends this on. The serial
   * is what distinguishes "again" from "still".
   */
  request?: { url: string; at: number } | null;
  /**
   * What to call the conversation that owns a tab.
   *
   * A lookup and not a session id, which is the distinction the note in
   * `Panes.tsx` makes: this pane is still the window's rather than any
   * conversation's, and nothing under it may ask "which session am I in". It
   * asks "what is that one called", about an id it was given.
   */
  nameOf: (session: string) => string;
  /**
   * Hand the page on screen to the conversation being worked in.
   *
   * The one direction Phase 2 left out. A model's own tabs it already knows the
   * ids of; a tab *you* opened it has never heard of, and must not be able to
   * enumerate — a list of every tab would put the user's browsing into model
   * context, which is a leak wearing a capability's clothes
   * (`../AGENT-BROWSER.md`). So the way across is the user handing one over,
   * from the strip, about the page they are looking at.
   */
  onHandOver: (tab: string) => void;
}) {
  const field = useRef<HTMLInputElement>(null);
  const { tabs, failure, live } = useSyncExternalStore(browser.subscribe, browser.snapshot);
  const tab = currentTab(tabs);
  /** The serial of the last request acted on, so a re-render is not a re-visit. */
  const visited = useRef(0);

  // The pane is going away — hidden by the same button that opened it, or
  // because another pane fills the field. Either way the webviews must stop
  // covering the window: they are native children the DOM cannot move, so
  // nothing but this cleanup takes them off the screen. They are *hidden*
  // rather than destroyed, so a re-open shows the pages as they were and no
  // login is ever lost to a pane closing.
  useEffect(() => browser.unmount, []);

  // Same hiding, for the pane that stays mounted while another pane fills the
  // field: `visibility: hidden` hides the HTML but a native webview would keep
  // compositing over the expanded pane, outside any stacking context.
  useEffect(() => {
    browser.shown(!hidden);
  }, [hidden]);

  // A link followed elsewhere in the window. It waits for `live` rather than
  // racing the pane's own first `browser_open`: on the mount that a link
  // *causes*, the webview is still being created when this request arrives, and
  // navigating early fails with "that browser tab is not open" — an error about
  // our own ordering, shown to somebody who just clicked a link.
  useEffect(() => {
    if (!live || !request || request.at === visited.current) return;
    visited.current = request.at;
    browser.visit(request.url);
  }, [live, request]);

  /**
   * Close a tab, and the pane with it when it was the last one.
   *
   * A browser with no tabs is not a state anybody asked for, so closing the
   * last tab takes the pane off screen — which is where a re-open starts,
   * exactly where a new browser starts. What the strip is left holding is the
   * shell's answer and not this component's assumption: a blank tab under
   * Tauri, where the last webview owns the profile and is never destroyed, and
   * nothing at all under Electron, where it does not (`webHost.ts::close`).
   * Either way the pane goes, which is the only part this decides.
   */
  const closeTab = (id: string) => {
    const last = browser.isLast(id);
    browser.close(id);
    if (last) onClose();
  };

  /**
   * `Mod+Shift+T` / `Mod+Shift+W`, spelled the way every browser and every
   * terminal emulator already spells them — and the same pair `TermPane`
   * answers, so the two tab strips in this window take the same keys.
   *
   * Answered by the pane rather than by the window's layout handler, because
   * "which tab" is a question only a pane has an answer to. It is bound to both
   * chrome rows and to nothing else, which is the honest reach: a page is
   * another process's input, so a keystroke inside one never arrives here at
   * all, and the `+` and the tab's own cross are what the strip is really
   * driven by.
   */
  const onTabKeys = (event: React.KeyboardEvent) => {
    if (!(event.ctrlKey || event.metaKey) || !event.shiftKey || event.altKey) return;
    if (event.key === "T" || event.key === "t") {
      event.preventDefault();
      void browser.open();
    }
    if ((event.key === "W" || event.key === "w") && tab) {
      event.preventDefault();
      closeTab(tab.id);
    }
  };

  return (
    <>
      <header className="pane-head tabstrip web-tabstrip" onKeyDown={onTabKeys}>
        <div className="tabs" role="tablist" aria-label="Pages">
          {tabs.list.map((each) => (
            <div key={each.id} className={`tab${each.id === tabs.current ? " is-current" : ""}`}>
              <button
                type="button"
                role="tab"
                aria-selected={each.id === tabs.current}
                className="tab-name"
                title={[each.url || tabLabel(each), agentHint(each, nameOf)]
                  .filter(Boolean)
                  .join("\n\n")}
                onClick={() => browser.select(each.id)}
              >
                {/* A tab nobody in this window asked for. It is marked from the
                    moment it exists — a page starts loading in it, and a strip
                    that drew it like any other would be saying somebody opened
                    it. The colour separates two conversations' tabs; the label
                    is what says what it means. */}
                {each.agent && (
                  <span
                    className={`tab-agent${each.owner ? " is-owned" : ""}`}
                    style={tint(each.owner)}
                    role="img"
                    aria-label={agentHint(each, nameOf)}
                  />
                )}
                {/* The name is its own element so it can be the thing that
                    ellipsises: the button beside it is a flex container, and a
                    bare text node in one cannot be truncated. */}
                <span className="tab-label">{tabLabel(each)}</span>
              </button>
              <button
                type="button"
                className="tab-close"
                aria-label={`Close ${tabLabel(each)}`}
                title="Close this tab"
                onClick={() => closeTab(each.id)}
              >
                <CloseIcon size={12} />
              </button>
            </div>
          ))}
        </div>
        <button
          className="icon-btn"
          onClick={() => void browser.open()}
          aria-label="New tab"
          title="New tab"
        >
          <PlusIcon size={14} />
        </button>
        <button
          className="icon-btn"
          onClick={onToggleExpanded}
          aria-pressed={expanded}
          aria-label={expanded ? "Restore this pane's size" : "Expand this pane"}
          title={expanded ? "Restore this pane's size" : "Expand this pane"}
        >
          {expanded ? <CollapseIcon size={14} /> : <ExpandIcon size={14} />}
        </button>
        <button
          className="icon-btn"
          onClick={onClose}
          aria-label="Hide the browser"
          title="Hide the browser — the pages stay exactly as they are"
        >
          <CloseIcon size={14} />
        </button>
      </header>

      <header className="pane-head web-toolbar" onKeyDown={onTabKeys}>
        <div className="pane-history">
          <button className="icon-btn" onClick={browser.back} aria-label="Back" title="Back">
            <BackIcon size={14} />
          </button>
          <button
            className="icon-btn"
            onClick={browser.forward}
            aria-label="Forward"
            title="Forward"
          >
            <ForwardIcon size={14} />
          </button>
        </div>

        <form
          className="web-address"
          onSubmit={(event) => {
            event.preventDefault();
            // Enter sends what the field is showing, not what a state variable
            // happens to hold: a navigation event can arrive while an address
            // is being typed, and guarding on the draft then makes Enter do
            // nothing at all — the browser pane's whole failure mode. The field
            // is the source of truth for what the user is looking at.
            if (tab) browser.go(tab.id, field.current?.value ?? "");
          }}
        >
          <input
            ref={field}
            className="web-address-field"
            value={tab?.draft ?? tab?.url ?? ""}
            onChange={(event) => tab && browser.draft(tab.id, event.target.value)}
            onFocus={(event) => event.target.select()}
            // Esc gives the field back to the page's real address rather than
            // leaving a half-typed one on screen. Stopped here so it does not
            // also reach the pane's own Esc handling.
            onKeyDown={(event) => {
              if (event.key !== "Escape" || !tab) return;
              event.stopPropagation();
              browser.discard(tab.id);
              event.currentTarget.blur();
            }}
            placeholder="localhost:5173, or an address"
            spellCheck={false}
            aria-label="Address"
          />
        </form>

        <button
          className="icon-btn"
          onClick={browser.reload}
          aria-label="Reload"
          title="Reload"
        >
          <RefreshIcon size={14} />
        </button>

        {/* The handoff. Deliberately an action on the page you are *looking at*
            rather than a per-tab control: deciding to show somebody a page is
            something you do while reading it, and a second cross-sized button
            on every tab would crowd the strip the close control already fills.

            It writes into the composer instead of sending, the same as the file
            tree's "Mention in the message" — a page handed over with no
            question attached is of no use to anyone, and "look at this, and…"
            is a sentence somebody is still writing. */}
        <button
          className="icon-btn"
          onClick={() => tab && onHandOver(tab.id)}
          disabled={!tab}
          aria-label="Mention this page in the message"
          title="Mention this page in the message — the conversation can then read it"
        >
          <ArrowUp size={14} />
        </button>
      </header>

      {failure && <p className="web-error">{failure}</p>}

      {/* Empty on purpose: the page is composited over this box by the OS, not
          rendered into it. Anything drawn here would be underneath a webview
          and therefore invisible — which is also why the failure line above
          sits outside it. */}
      <div ref={bodyRef} className="pane-body is-web" />
    </>
  );
}
