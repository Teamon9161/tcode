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
let restoring = false;
let restoreFrame = 0;
let restoreRevision = 0;
let placement: (() => void | Promise<void>) | null = null;

function apply() {
  return invoke("browser_visible", {
    visible: wanted && held === 0 && !restoring,
  }).catch(() => {});
}

/** True while native geometry updates are intentionally being coalesced. */
export function browserPlacementHeld(): boolean {
  return held > 0 || restoring;
}

/**
 * Register the one authoritative final geometry read for the browser pane.
 *
 * Divider and window resize events can outpace both paint and IPC. While the
 * native page is yielded, ordinary slot effects skip their reads; the final
 * release calls this once, then waits for the shell to acknowledge the bounds
 * before making the page visible again.
 */
export function registerBrowserPlacement(
  callback: () => void | Promise<void>,
): () => void {
  placement = callback;
  return () => {
    if (placement === callback) placement = null;
  };
}

function scheduleRestore() {
  // Popovers use the same coordinator even when this window has no browser
  // pane. In that state there is neither geometry to settle nor a native view
  // to reveal; scheduling a frame would only leak a late false-visibility IPC
  // into the next interaction.
  if (!wanted && !placement) return;
  restoring = true;
  const revision = ++restoreRevision;
  restoreFrame = requestAnimationFrame(() => {
    restoreFrame = 0;
    Promise.resolve()
      .then(() => placement?.())
      .finally(() => {
        if (revision !== restoreRevision || held > 0) return;
        restoring = false;
        void apply();
      });
  });
}

/** State what the pane wants. Temporary yields still take precedence. */
export function setBrowserShown(visible: boolean) {
  wanted = visible;
  void apply();
}

/**
 * Re-assert the composed state after creating or selecting a native view.
 * Those shell operations can change which view is current, but they do not
 * know whether a popover or drag currently owns the renderer.
 */
export function syncBrowserVisibility() {
  void apply();
}

/**
 * Make the browser stand down for a moment.
 *
 * Counted rather than a boolean because these can nest. Only the final release
 * restores the pane's requested state; it never assumes that state is visible.
 */
export function yieldBrowser(): () => void {
  held += 1;
  if (held === 1) {
    restoreRevision += 1;
    if (restoreFrame) cancelAnimationFrame(restoreFrame);
    restoreFrame = 0;
    restoring = false;
    void apply();
  }
  let released = false;
  return () => {
    // Guarded because a React cleanup can run twice under StrictMode, and a
    // count that went negative would leave the page hidden for good.
    if (released) return;
    released = true;
    held -= 1;
    if (held === 0) scheduleRestore();
  };
}

/** Tests only. The app owns one browser coordinator for its whole lifetime. */
export function resetBrowserVisibility() {
  wanted = false;
  held = 0;
  restoring = false;
  restoreRevision += 1;
  if (restoreFrame) cancelAnimationFrame(restoreFrame);
  restoreFrame = 0;
  placement = null;
}
