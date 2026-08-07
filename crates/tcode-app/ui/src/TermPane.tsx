import { useEffect, useLayoutEffect, useRef, useSyncExternalStore } from "react";

import { CloseIcon, CollapseIcon, ExpandIcon, PlusIcon } from "./components/Icons";
import { tabLabel } from "./terminal";
import * as terminals from "./termHost";
import { appOwnedInTerminal } from "./keys";

/**
 * The window's terminals.
 *
 * Only the chrome is here — the tab strip, the frame — the same division
 * `WebPane` draws. What the terminals *are* lives in `termHost.ts`, outside
 * React, because `Mod+J` unmounts this component and the shells have to survive
 * it with their scrollback intact.
 *
 * So this file is short on purpose, and the three things it does are the three
 * things only a mounted component can do:
 *
 *  - **Hand the store somewhere to draw.** `attach`/`detach` move the live host
 *    elements in and out of this pane's body. They are moved, never rebuilt.
 *  - **Report the pane's size.** A terminal that has not been told its shape
 *    wraps every line in the wrong place, and the pane's size is a fact only
 *    the DOM has.
 *  - **Decide which keys are the app's.** See `ours` below; this is the one
 *    surface in the window where nearly every keystroke belongs to something
 *    else.
 */
export function TermPane({
  cwd,
  onClose,
  expanded,
  onToggleExpanded,
  focused,
}: {
  /** Where a new tab starts: the folder of the conversation that was current
   *  when the terminal opened. Captured per tab, so a tab keeps its folder even
   *  after the pane it came from is gone. */
  cwd: string;
  onClose: () => void;
  expanded: boolean;
  onToggleExpanded: () => void;
  /** This is the current pane. The cursor goes back into the shell when it
   *  becomes true — a focused terminal you have to click into is half a
   *  terminal, and `Mod+J` is supposed to land you in it. */
  focused: boolean;
}) {
  const body = useRef<HTMLDivElement>(null);
  const { tabs, failure } = useSyncExternalStore(terminals.subscribe, terminals.snapshot);

  // Layout rather than passive: the hosts have to be in the document before
  // anything measures them, and the first tab is opened off the back of this.
  useLayoutEffect(() => {
    const element = body.current;
    if (!element) return;
    terminals.attach(element);
    // An empty terminal pane is not a state anybody asked for: opening it is
    // asking for a shell. A second `Mod+J` finds the tabs still here and opens
    // nothing.
    if (terminals.isEmpty()) void terminals.open(cwd);
    return () => terminals.detach(element);
    // `cwd` deliberately absent: it decides where the *first* tab starts, and
    // re-running this because focus moved to another folder would detach and
    // re-attach every live terminal for nothing.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    const element = body.current;
    if (!element) return;
    const watch = new ResizeObserver(() => terminals.fitCurrent());
    watch.observe(element);
    return () => watch.disconnect();
  }, []);

  useEffect(() => {
    if (focused) terminals.focusCurrent();
  }, [focused]);

  return (
    <>
      <header className="pane-head tabstrip">
        <div className="tabs" role="tablist" aria-label="Terminals">
          {tabs.list.map((tab) => (
            <div
              key={tab.id}
              className={`tab${tab.id === tabs.current ? " is-current" : ""}${
                tab.exit === null ? "" : " is-ended"
              }`}
            >
              <button
                type="button"
                role="tab"
                aria-selected={tab.id === tabs.current}
                className="tab-name"
                title={tab.cwd}
                onClick={() => terminals.select(tab.id)}
              >
                {/* The name is its own element so it can be the thing that
                    ellipsises: the button beside it is a flex container, and a
                    bare text node in one cannot be truncated. */}
                <span className="tab-label">{tabLabel(tab)}</span>
                {/* Not a colour: a tab whose program ended says so in words,
                    because "this one is finished" and "this one failed" are
                    different facts and only one of them is worth alarm. */}
                {tab.exit !== null && (
                  <span className={`tab-exit${tab.exit ? " is-bad" : ""}`}>
                    {tab.exit ? `exit ${tab.exit}` : "exited"}
                  </span>
                )}
              </button>
              <button
                type="button"
                className="tab-close"
                aria-label={`Close ${tabLabel(tab)}`}
                title="Close this terminal"
                onClick={() => terminals.close(tab.id)}
              >
                <CloseIcon size={12} />
              </button>
            </div>
          ))}
        </div>
        <button
          className="icon-btn"
          onClick={() => void terminals.open(cwd)}
          aria-label="New terminal"
          title="New terminal"
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
          aria-label="Hide the terminals"
          title="Hide the terminals — what is running keeps running"
        >
          <CloseIcon size={14} />
        </button>
      </header>

      {failure && <p className="term-error">{failure}</p>}

      {/* The live terminals are appended here by the store. Nothing is rendered
          into it from this side: React must never own these children, or a
          re-render would rebuild the emulators it is holding. */}
      <div
        ref={body}
        className="pane-body is-term"
        onKeyDown={(event) => {
          if (!ours(event.nativeEvent)) return;
          // The tab keys are the pane's own, so they are answered here rather
          // than in the window's layout handler — that one stands down inside a
          // terminal (`inTerminal` in `keys.ts`).
          event.preventDefault();
          event.stopPropagation();
          if (event.key === "T" || event.key === "t") void terminals.open(cwd);
          if (event.key === "W" || event.key === "w") terminals.close(tabs.current);
        }}
      />
    </>
  );
}

/**
 * The two chords this pane answers itself: new tab and close tab.
 *
 * A subset of `appOwnedInTerminal`, which is the one list xterm is told to keep
 * its hands off. Deriving it rather than restating it is what stops a key from
 * being both sent to the shell and acted on here — the window's layout handler
 * takes the rest of that list.
 */
function ours(event: KeyboardEvent): boolean {
  return appOwnedInTerminal(event) && event.shiftKey && !event.altKey;
}
