import { invoke } from "@tauri-apps/api/core";

/**
 * Making the browser stand down for a moment.
 *
 * The browser's page is a native child webview the OS composites *above* the
 * whole document, outside any stacking context CSS can reach. Two things in
 * this window therefore need it off the screen while they happen, and neither
 * of them owns it:
 *
 *  - **A popover** would otherwise open behind the page (`seat.ts`, rule 17),
 *    which looks exactly like a button that does nothing.
 *  - **A divider drag** would otherwise ask the platform to move and resize a
 *    whole browser process on every pointer sample. `browser.rs` has said this
 *    is what should happen since the file was written; nothing did it.
 *
 * Counted rather than a boolean, because these nest — a popover open over a
 * drag, a submenu over a menu — and it is the *last* one finishing that should
 * bring the page back. A boolean would let one closing submenu reveal the page
 * underneath the menu still in front of it.
 *
 * Calling this with no browser open is free: `Browser::visible` returns without
 * a webview, so no caller has to know whether the pane exists.
 */
let held = 0;

export function yieldBrowser(): () => void {
  held += 1;
  if (held === 1) invoke("browser_visible", { visible: false }).catch(() => {});
  let released = false;
  return () => {
    // Guarded because a React cleanup can run twice under StrictMode, and a
    // count that went negative would leave the page hidden for good.
    if (released) return;
    released = true;
    held -= 1;
    if (held === 0) invoke("browser_visible", { visible: true }).catch(() => {});
  };
}
