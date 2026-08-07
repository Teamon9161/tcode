import { useEffect, useState } from "react";
import { invoke, listen } from "@ipc";

import { WINDOW_STATE, type WindowState } from "../types";
import { CloseIcon, MaximizeIcon, MinimizeIcon, RestoreIcon } from "./Icons";

/**
 * Controls for the app-owned title bar.
 *
 * The native browser webview is confined to pane bodies below the title bar, so
 * it cannot cover these controls even though native child webviews compose over
 * the document.
 *
 * **The window is the shell's, so these go through `invoke` like everything
 * else.** They used to call Tauri's window API directly, which made this one of
 * two files that knew which shell was hosting the app; both shells now answer
 * `window_*` in the same command table (`main.rs::register_shell`,
 * `electron/main.js`), and this file knows nothing about either. A test pins
 * the absence — see `browser.rs::the_title_bar_does_not_reach_the_window_from_
 * the_webview` for what it used to check instead.
 */
export function WindowDragRegion() {
  return (
    <span
      className="topbar-gap"
      data-tauri-drag-region=""
      // Fires under Tauri only: an Electron drag region consumes mouse events
      // before the document sees them, and the platform's own caption
      // behaviour — double-click to maximize — applies instead. Same gesture,
      // different owner, so there is nothing here to branch on.
      onDoubleClick={() => invoke("window_toggle_maximize").catch(complain)}
    />
  );
}

export function WindowControls() {
  const [maximized, setMaximized] = useState(false);

  useEffect(() => {
    // One read for where the window is now, then follow it: the state changes
    // without a button here being pressed — a snap gesture, a double-click on
    // the bar, the OS restoring the window — so the icon follows the window
    // rather than the last click. See `bridge::WINDOW_STATE`.
    invoke<boolean>("window_is_maximized").then(setMaximized).catch(complain);
    const pending = listen<WindowState>(WINDOW_STATE, ({ payload }) =>
      setMaximized(payload.maximized),
    );
    return () => {
      pending.then((unlisten) => unlisten()).catch(() => {});
    };
  }, []);

  return (
    <div className="window-controls">
      <button
        className="window-btn"
        onClick={() => invoke("window_minimize").catch(complain)}
        aria-label="Minimize"
        title="Minimize"
      >
        <MinimizeIcon size={13} />
      </button>
      <button
        className="window-btn"
        onClick={() => invoke("window_toggle_maximize").catch(complain)}
        aria-label={maximized ? "Restore" : "Maximize"}
        title={maximized ? "Restore" : "Maximize"}
      >
        {maximized ? <RestoreIcon size={13} /> : <MaximizeIcon size={13} />}
      </button>
      <button
        className="window-btn is-close"
        onClick={() => invoke("window_close").catch(complain)}
        aria-label="Close"
        title="Close"
      >
        <CloseIcon size={13} />
      </button>
    </div>
  );
}

function complain(error: unknown) {
  console.warn("window control unavailable:", error);
}
