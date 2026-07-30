import { useCallback, useEffect, useLayoutEffect, type RefObject } from "react";

/**
 * Anchoring a portalled popover to the control that opened it.
 *
 * Every popover in this window has to be portalled and `position: fixed`: a pane
 * clips what is inside it, and the composer's controls sit inside a `<form>`
 * where a stray field would submit the message on Enter. That leaves the same
 * three chores at every call site — measure the trigger, keep it measured
 * through a resize, and dismiss on Escape or a click outside — which were
 * copied into the model panel first and would have been copied twice more for
 * the usage panel and the folder picker.
 *
 * The geometry is published as custom properties rather than inline `top`/`left`
 * so the gap, the width and the ceiling stay in the stylesheet with every other
 * spacing decision:
 *
 * - `--seat-right` / `--seat-bottom`: the trigger's top-right corner, for the
 *   panels that open upward out of the composer.
 * - `--seat-left` / `--seat-top`: its bottom-left corner, for the ones that drop
 *   down out of a header.
 * - `--seat-room` / `--seat-below`: how much room there is above and below it, so
 *   a panel caps its own height rather than growing off the edge of the window.
 *
 * A context menu is the same popover with a different anchor: it belongs where
 * the pointer was, not where a control is. `at` supplies that point as a
 * zero-size rect, so one implementation still places every popover in the window
 * and a right-click menu does not become the fourth copy rule 17 exists to
 * prevent.
 */
export function useSeat({
  open,
  trigger,
  at,
  box,
  onEscape,
  onOutside,
}: {
  open: boolean;
  trigger: RefObject<HTMLElement | null>;
  /** Anchor to this viewport point instead of the trigger's box. */
  at?: { x: number; y: number } | null;
  box: RefObject<HTMLElement | null>;
  /** Escape was pressed. A panel with its own levels can go back one instead of
   *  closing; most callers pass the same function as `onOutside`. */
  onEscape: () => void;
  /** A pointer went down outside both the panel and its trigger. */
  onOutside: () => void;
}) {
  const place = useCallback(() => {
    const panel = box.current;
    if (!panel) return;
    const rect = at
      ? { left: at.x, right: at.x, top: at.y, bottom: at.y }
      : trigger.current?.getBoundingClientRect();
    if (!rect) return;
    panel.style.setProperty("--seat-right", `${window.innerWidth - rect.right}px`);
    panel.style.setProperty("--seat-left", `${rect.left}px`);
    panel.style.setProperty("--seat-top", `${rect.bottom}px`);
    panel.style.setProperty("--seat-bottom", `${window.innerHeight - rect.top}px`);
    panel.style.setProperty("--seat-room", `${rect.top}px`);
    panel.style.setProperty("--seat-below", `${window.innerHeight - rect.bottom}px`);
  }, [trigger, at, box]);

  // Before paint: a panel that flashes at the window's corner on the way to its
  // anchor is worse than one that appears a frame later.
  useLayoutEffect(() => {
    if (open) place();
  }, [open, place]);

  useEffect(() => {
    if (!open) return;
    const onDown = (event: MouseEvent) => {
      const target = event.target as Node;
      if (box.current?.contains(target) || trigger.current?.contains(target)) return;
      onOutside();
    };
    const onKey = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      // Ahead of the pane's own Escape handler, which would close the pane out
      // from under a popover that was the only thing meant to go.
      event.stopPropagation();
      onEscape();
    };
    // Capture, and that is load-bearing rather than tidy: Tauri's own drag-region
    // script listens for `mousedown` on `document` and calls
    // `stopImmediatePropagation()` on anything that hits a `data-tauri-drag-region`
    // element (rule 9c puts that attribute on the whole title bar). A bubble-phase
    // listener therefore never hears a click on the bar — which is exactly where
    // the display menu's own trigger sits, so the one popover most likely to be
    // dismissed by clicking beside it was the one that could not be.
    window.addEventListener("mousedown", onDown, true);
    window.addEventListener("keydown", onKey, true);
    window.addEventListener("resize", place);
    return () => {
      window.removeEventListener("mousedown", onDown, true);
      window.removeEventListener("keydown", onKey, true);
      window.removeEventListener("resize", place);
    };
  }, [open, box, trigger, onEscape, onOutside, place]);

  return place;
}
