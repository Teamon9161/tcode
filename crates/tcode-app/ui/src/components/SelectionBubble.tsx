import { useEffect, useRef } from "react";

import type { BubbleAt } from "../selection";

/**
 * "Comment on this" where the hand already is.
 *
 * It appears on a selection rather than living permanently beside every phase,
 * because a control that is only relevant once something is selected should only
 * exist then — and because the alternative (a comment button per field) puts four
 * affordances on a row that mostly wants to be read.
 *
 * `position: fixed`: a pane's body scrolls, and an absolutely positioned bubble
 * inside a scroll container is clipped at its edge — which is precisely where a
 * selection near the bottom of a long plan would put it.
 *
 * Three ways in, one code path: drag a passage (mouseup), right-click it
 * (contextmenu), or select with the keyboard and press the shortcut. The bubble
 * itself is the same button in all three cases, so there is one thing to learn.
 */
export function SelectionBubble({
  at,
  onComment,
  onDismiss,
}: {
  at: BubbleAt;
  onComment: () => void;
  onDismiss: () => void;
}) {
  const button = useRef<HTMLButtonElement>(null);

  // Escape closes it, and so does scrolling the plan out from under it: a bubble
  // pinned to the viewport while its passage moves away is pointing at the wrong
  // text, and pointing confidently at the wrong text is worse than disappearing.
  useEffect(() => {
    const close = (event: Event) => {
      if (event instanceof KeyboardEvent && event.key !== "Escape") return;
      onDismiss();
    };
    window.addEventListener("keydown", close);
    window.addEventListener("scroll", close, true);
    window.addEventListener("resize", close);
    return () => {
      window.removeEventListener("keydown", close);
      window.removeEventListener("scroll", close, true);
      window.removeEventListener("resize", close);
    };
  }, [onDismiss]);

  return (
    <div
      className="selection-bubble"
      // Placement is a measurement, not a style: it is the pointer's own
      // position, so it cannot be a token. `translate` keeps the bubble clear of
      // the cursor and clamped inside the window.
      style={{ top: `${Math.max(at.y, 8)}px`, left: `${Math.max(at.x, 8)}px` }}
      role="presentation"
      // The pointer went down inside the bubble, so the selection it is about
      // must survive the click that answers it.
      onMouseDown={(event) => event.preventDefault()}
    >
      <button
        ref={button}
        type="button"
        className="selection-bubble-btn"
        onClick={onComment}
        aria-label="Comment on the selected passage"
      >
        comment
      </button>
    </div>
  );
}
