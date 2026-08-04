import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

import { BackIcon, CloseIcon, CollapseIcon, ExpandIcon, ForwardIcon, RefreshIcon } from "./components/Icons";
import { BROWSER_NAVIGATED, type Navigated } from "./types";
import { useSeat } from "./seat";

/**
 * The window's browser.
 *
 * What is drawn here is only the chrome — tab strip, toolbar, the frame. The
 * page itself is a **native child webview** the backend owns (`src/browser.rs`),
 * composited over the rectangle this pane's body occupies. It is not in the
 * DOM and cannot be reached from it.
 *
 * Three consequences shape this file:
 *
 *  - **The rectangle has to be reported, continuously.** A native webview does
 *    not participate in layout; it sits where it was last told. So every render
 *    measures the body and, when it moved, tells the backend. `useLayoutEffect`
 *    with no dependency array is deliberate: a pane can move without changing
 *    size (a sibling closed, a divider moved on the far side), and a
 *    `ResizeObserver` alone never fires for that.
 *  - **The address bar is a view, not the source of truth.** The webview owns
 *    where it is; typing here only asks it to go somewhere. The URL displayed
 *    comes back over `BROWSER_NAVIGATED`, so a redirect, a link click and a
 *    `history.back()` all update it the same way, and nothing has to guess
 *    whether a navigation actually happened.
 *  - **Leaving the screen is not the same as closing.** The webview is the
 *    window's, not the pane's: hiding the pane (the same button that opened
 *    it, or the close menu's "Hide for now") hides the webview and keeps its
 *    page, and re-opening shows it again. "Exit browser" closes the *page*
 *    (back to `about:blank`) but keeps the webview and its profile alive, so
 *    cookies and logins survive — the webview is created once per app session
 *    and only the app's own exit tears it down, which is also why a reopen
 *    never races a browser process that is still shutting down.
 *
 * The chrome is two rows, like a real browser: a tab strip (page title,
 * expand, close) above a toolbar (history, address, reload). They were one row
 * once, and reload and close sitting side by side read as the same kind of
 * button — which is how "refresh the page" and "leave the browser" became the
 * same question. Closing lives on the tab, refreshing on the toolbar.
 *
 * The history buttons cannot be greyed out. Whether there is anywhere to go
 * back to lives in the page's own history, across an origin this side cannot
 * read; `history.go` with nowhere to go does nothing, which is the harmless
 * direction. Greying them out would need a second, guessed copy of a stack the
 * page already keeps.
 */
export function WebPane({
  onClose,
  expanded,
  onToggleExpanded,
  hidden,
  request,
}: {
  onClose: () => void;
  /** This pane is the one filling the field. */
  expanded: boolean;
  onToggleExpanded: () => void;
  /** Another pane is filling the field and this one is `visibility: hidden`.
   *  The native webview does not participate in CSS, so it has to be told. */
  hidden: boolean;
  /**
   * A page the window was asked for — a link followed in a conversation.
   *
   * It arrives here rather than going straight to the backend because the
   * native webview does not exist until this pane mounts and creates it, and
   * `browser_navigate` on a webview that is not there fails. The pane is the
   * only side that knows when it exists, so it is the side that sends this on.
   * The serial is what distinguishes "again" from "still".
   */
  request?: { url: string; at: number } | null;
}) {
  const body = useRef<HTMLDivElement>(null);
  const field = useRef<HTMLInputElement>(null);
  const placed = useRef<string>("");
  const [url, setUrl] = useState("");
  const [title, setTitle] = useState("");
  const [typed, setTyped] = useState<string | null>(null);
  const [failure, setFailure] = useState<string | null>(null);
  /** The address whose navigation has not round-tripped yet, if any. */
  const pending = useRef<string | null>(null);
  /** True once the backend has confirmed the webview exists. Until then there
   *  is nothing to navigate, and a request has to wait rather than fail. */
  const [live, setLive] = useState(false);
  /** The serial of the last request acted on, so a re-render is not a re-visit. */
  const visited = useRef(0);
  const hiddenRef = useRef(hidden);
  hiddenRef.current = hidden;

  /** The frame a pending measurement is booked for, so the render that follows
   *  a keystroke does not book a second one. */
  const booked = useRef(0);

  /** Measure, and only cross the bridge when it actually moved. This runs on
   *  every render and on every resize, so it is the one place that has to stay
   *  cheap. */
  const place = useCallback((first: boolean) => {
    const box = body.current?.getBoundingClientRect();
    if (!box) return;
    const rect = { x: box.left, y: box.top, width: box.width, height: box.height };
    const key = `${rect.x},${rect.y},${rect.width},${rect.height}`;
    if (!first && key === placed.current) return;
    placed.current = key;
    invoke(first ? "browser_open" : "browser_bounds", { rect })
      .then(() => {
        setLive(true);
        // `browser_open` shows the webview it (re)creates, and a pane mounted
        // while another one is expanded must end up hidden — re-assert it here,
        // after creation, not just in the visibility effect that ran before the
        // webview existed.
        if (hiddenRef.current) {
          invoke("browser_visible", { visible: false }).catch(() => {});
        }
      })
      .catch((error) => setFailure(String(error)));
  }, []);

  /**
   * Book the measurement for the next frame, at most once.
   *
   * Measuring is `getBoundingClientRect`, and calling it in a layout effect —
   * straight after a commit, while the layout it would read is dirty — forces
   * the platform to lay the whole document out synchronously before it can
   * answer. With this pane open that bill was paid after *every* render in the
   * window, including the one behind each keystroke in a composer three panes
   * away, on a document holding a conversation hundreds of blocks long.
   *
   * A frame later the layout is already computed and the same call is a read.
   * Nothing is lost by the wait: the webview is composited by the OS, so it was
   * never going to move in the same frame as the HTML underneath it anyway.
   */
  const schedule = useCallback(
    (first: boolean) => {
      if (booked.current) return;
      booked.current = requestAnimationFrame(() => {
        booked.current = 0;
        place(first);
      });
    },
    [place],
  );

  useLayoutEffect(() => {
    schedule(placed.current === "");
  });

  useLayoutEffect(() => {
    if (!body.current) return;
    const watch = new ResizeObserver(() => schedule(false));
    watch.observe(body.current);
    return () => watch.disconnect();
  }, [schedule]);

  useEffect(
    () => () => {
      if (booked.current) cancelAnimationFrame(booked.current);
    },
    [],
  );

  // The pane is going away — hidden by the same button that opened it, by the
  // close menu, or because another pane fills the field. Either way the
  // webview must stop covering the window: it is a native child the DOM cannot
  // move, so nothing but this cleanup takes it off the screen. It is *hidden*
  // rather than destroyed — the webview lives for the whole app session, page
  // and profile intact — so a re-open shows it again, and cookies and logins
  // are never lost to a pane closing.
  useEffect(() => {
    return () => {
      invoke("browser_visible", { visible: false }).catch(() => {});
    };
  }, []);

  // Same hiding, for the pane that stays mounted while another pane fills the
  // field: `visibility: hidden` hides the HTML but the native webview would
  // keep compositing over the expanded pane, outside any stacking context.
  useEffect(() => {
    invoke("browser_visible", { visible: !hidden }).catch(() => {});
  }, [hidden]);

  useEffect(() => {
    const stop = listen<Navigated>(BROWSER_NAVIGATED, (event) => {
      setUrl(event.payload.url);
      setTitle(event.payload.title);
      // Whatever was half-typed is abandoned only once the navigation *we*
      // asked for comes back and the field still holds it. Every navigation and
      // title change emits here, and the WebView2 runtime starts up slowly — an
      // event arriving while an address is being typed must not wipe it, or the
      // next Enter has nothing to send.
      if (pending.current !== null) {
        const sent = pending.current;
        pending.current = null;
        if (field.current?.value === sent) setTyped(null);
      }
      setFailure(null);
    }).catch((error) => {
      setFailure(`could not follow the browser: ${String(error)}`);
      return () => {};
    });
    return () => {
      void stop.then((off) => off());
    };
  }, []);

  const go = (where: string) => {
    pending.current = where;
    invoke("browser_navigate", { url: where })
      .then(() => setFailure(null))
      .catch((error) => {
        pending.current = null;
        setFailure(String(error));
      });
  };

  // A link followed elsewhere in the window. It waits for `live` rather than
  // racing the pane's own first `browser_open`: on the mount that a link
  // *causes*, the webview is still being created when this request arrives, and
  // navigating early fails with "the browser is not open" — an error about our
  // own ordering, shown to somebody who just clicked a link.
  useEffect(() => {
    if (!live || !request || request.at === visited.current) return;
    visited.current = request.at;
    // `go` is re-made every render and is deliberately not a dependency: what
    // decides whether to navigate is the serial, which is the point of it.
    go(request.url);
  }, [live, request]);

  const act = (command: string, args?: Record<string, unknown>) => {
    invoke(command, args).catch((error) => setFailure(String(error)));
  };

  /** Closing is a choice, not an accident: the tab strip's X asks whether to
   *  leave the browser for good or just put it away. */
  const [closing, setClosing] = useState(false);
  const closeTrigger = useRef<HTMLButtonElement>(null);
  const closeBox = useRef<HTMLDivElement>(null);
  const dismiss = useCallback(() => setClosing(false), []);
  useSeat({
    open: closing,
    trigger: closeTrigger,
    box: closeBox,
    onEscape: dismiss,
    onOutside: dismiss,
  });

  const exitBrowser = () => {
    setClosing(false);
    // Closing the tab closes the *page*, not the browser: go back to the blank
    // start, and let the pane go away (the unmount cleanup hides the webview).
    // The webview itself stays alive for the session — that is what keeps
    // cookies and logins across "Exit browser" — and only the app's own exit
    // tears it down (`main.rs`), which is also why a reopen never races a
    // shutting-down browser process.
    invoke("browser_navigate", { url: "about:blank" }).catch(() => {});
    onClose();
  };

  const hideBrowser = () => {
    setClosing(false);
    // Just put the pane away: the unmount cleanup hides the webview, page and
    // scroll position intact.
    onClose();
  };

  /** True while the browser is at its blank start — nothing loaded, nothing
   *  typed that went anywhere. A blank browser has nothing a "hide" would
   *  preserve, so closing it skips the question entirely. */
  const blank = url === "" || url === "about:blank";

  return (
    <>
      <header className="pane-head web-tabstrip">
        <span className="web-tab-title" title={title || url}>
          {title || url || "Browser"}
        </span>
        <button
          className="icon-btn"
          onClick={onToggleExpanded}
          aria-pressed={expanded}
          aria-label={expanded ? "Restore this pane's size" : "Expand this pane"}
          title={expanded ? "Restore this pane's size" : "Expand this pane"}
        >
          {expanded ? <CollapseIcon size={14} /> : <ExpandIcon size={14} />}
        </button>
        <div className="web-close-box">
          <button
            ref={closeTrigger}
            type="button"
            className="icon-btn"
            aria-expanded={closing}
            aria-label="Close the browser"
            onClick={() => {
              if (blank) {
                // Nothing to preserve: a blank browser has no page that a
                // "hide" would keep, so close it outright instead of asking
                // whether to exit or hide.
                onClose();
              } else {
                setClosing((was) => !was);
              }
            }}
          >
            <CloseIcon size={14} />
          </button>
          {closing &&
            createPortal(
              <div className="seated web-close-menu" ref={closeBox} role="menu" aria-label="Close the browser">
                <p className="web-close-head">Close the browser?</p>
                <button type="button" className="fmenu-item" role="menuitem" onClick={exitBrowser}>
                  <span className="fmenu-item-name">Exit browser</span>
                  <span className="web-close-note">the page closes; logins stay</span>
                </button>
                <button type="button" className="fmenu-item" role="menuitem" onClick={hideBrowser}>
                  <span className="fmenu-item-name">Hide for now</span>
                  <span className="web-close-note">the page stays exactly as it is</span>
                </button>
              </div>,
              document.body,
            )}
        </div>
      </header>

      <header className="pane-head web-toolbar">
        <div className="pane-history">
          <button
            className="icon-btn"
            onClick={() => act("browser_step", { delta: -1 })}
            aria-label="Back"
            title="Back"
          >
            <BackIcon size={14} />
          </button>
          <button
            className="icon-btn"
            onClick={() => act("browser_step", { delta: 1 })}
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
            // is being typed, and guarding on `typed` then makes Enter do
            // nothing at all — the browser pane's whole failure mode. The field
            // is the source of truth for what the user is looking at.
            go(field.current?.value ?? "");
          }}
        >
          <input
            ref={field}
            className="web-address-field"
            value={typed ?? url}
            onChange={(event) => setTyped(event.target.value)}
            onFocus={(event) => event.target.select()}
            // Esc gives the field back to the page's real address rather than
            // leaving a half-typed one on screen. Stopped here so it does not
            // also reach the pane's own Esc handling.
            onKeyDown={(event) => {
              if (event.key !== "Escape") return;
              event.stopPropagation();
              setTyped(null);
              event.currentTarget.blur();
            }}
            placeholder="localhost:5173, or an address"
            spellCheck={false}
            aria-label="Address"
          />
        </form>

        <button
          className="icon-btn"
          onClick={() => act("browser_reload")}
          aria-label="Reload"
          title="Reload"
        >
          <RefreshIcon size={14} />
        </button>
      </header>

      {failure && <p className="web-error">{failure}</p>}

      {/* Empty on purpose: the page is composited over this box by the OS, not
          rendered into it. Anything drawn here would be underneath a webview
          and therefore invisible — which is also why the failure line above
          sits outside it. */}
      <div ref={body} className="pane-body is-web" />
    </>
  );
}
