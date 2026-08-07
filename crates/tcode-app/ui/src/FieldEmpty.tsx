/**
 * The window with no conversation on screen.
 *
 * There used to be a whole screen here — the launchpad — and it was doing two
 * jobs at once: listing what to open, and being the thing you looked at while
 * nothing was open. The rail took the first, which leaves this one with the
 * second alone, and the second is small: say where the conversations are, and
 * spend the room on the shortcuts.
 *
 * The shortcuts sit here for the reason DESIGN.md gives for putting them on an
 * empty conversation — this is a screen with room and a moment where nobody is
 * mid-task, and a shortcut nothing ever mentions is a shortcut nobody uses.
 *
 * **Centred, unlike every other empty state in the app**, and the exception is
 * the point. The others sit on their column's left edge because they are the
 * first thing in a conversation, and a conversation has one axis running down
 * through the transcript, the dock and the composer. Nothing here shares an
 * axis with anything: there is no composer under it and no transcript above it.
 * Left-aligning it anyway — which is what this first shipped as — produced a
 * small block in the corner of a wide empty window, lined up with nothing. A
 * rule is worth keeping where the thing it protects exists.
 */
export function FieldEmpty() {
  return (
    <div className="field-empty">
      <div className="field-empty-inner">
        {/* "No conversation on screen", not "nothing open": conversations can
            be running in this window right now with no pane showing one, which
            is the ordinary state of an app built to hold parallel work. Saying
            "nothing open" over a rail listing three of them would be the
            window contradicting itself. */}
        <h1>No conversation on screen</h1>
        <p>
          The rail has every conversation this window is holding, and every
          folder tcode has worked in. Pick one to bring it back on screen, or{" "}
          <b>New</b> to start one — the agent works inside the folder you choose
          and nowhere else.
        </p>

        <dl className="keymap">
          {KEYS.map(([keys, does]) => (
            <div key={does} className="keymap-row">
              <dt>
                {keys.map((key) => (
                  <kbd key={key}>{key}</kbd>
                ))}
              </dt>
              <dd>{does}</dd>
            </div>
          ))}
        </dl>
      </div>
    </div>
  );
}

/** `Mod` is Ctrl, or Cmd on a Mac — spelled out rather than drawn as a glyph
 *  that means one thing on one platform. Kept to what moves you *between*
 *  conversations: the pane verbs belong to a window that has panes in it. */
const KEYS: [string[], string][] = [
  [["Mod", "P"], "find a conversation or folder"],
  [["Mod", "1…9"], "show that conversation"],
  [["Mod", "Shift", "1…9"], "open it beside this one"],
  [["Mod", "N"], "new conversation in this pane's folder"],
  [["Mod", "J"], "show, focus, or hide the terminals"],
];
