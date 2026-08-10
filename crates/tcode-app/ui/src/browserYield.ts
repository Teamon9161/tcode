import { invoke } from "@ipc";

/**
 * The browser's effective visibility.
 *
 * A native page has two independent reasons to be off screen:
 *
 *  - the browser pane itself is hidden (`wanted`), and
 *  - the app temporarily needs the renderer above it (`held`), for a popover
 *    or a divider drag.
 *
 * They must be composed here rather than sent as competing booleans. In
 * particular, releasing a divider used to send `visible: true` even when an
 * expanded pane had already hidden the browser, which exposed a strip of the
 * page at its last native bounds.
 */
let wanted = false;
let held = 0;

function apply() {
  invoke("browser_visible", { visible: wanted && held === 0 }).catch(() => {});
}

/** State what the pane wants. Temporary yields still take precedence. */
export function setBrowserShown(visible: boolean) {
  wanted = visible;
  apply();
}

/**
 * Re-assert the composed state after creating or selecting a native view.
 * Those shell operations can change which view is current, but they do not
 * know whether a popover or drag currently owns the renderer.
 */
export function syncBrowserVisibility() {
  apply();
}

/**
 * Make the browser stand down for a moment.
 *
 * Counted rather than a boolean because these can nest. Only the final release
 * restores the pane's requested state; it never assumes that state is visible.
 */
export function yieldBrowser(): () => void {
  held += 1;
  if (held === 1) apply();
  let released = false;
  return () => {
    // Guarded because a React cleanup can run twice under StrictMode, and a
    // count that went negative would leave the page hidden for good.
    if (released) return;
    released = true;
    held -= 1;
    if (held === 0) apply();
  };
}

/** Tests only. The app owns one browser coordinator for its whole lifetime. */
export function resetBrowserVisibility() {
  wanted = false;
  held = 0;
}
