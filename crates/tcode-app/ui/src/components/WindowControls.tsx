import { useEffect, useState } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";

import { CloseIcon, MaximizeIcon, MinimizeIcon, RestoreIcon } from "./Icons";

/**
 * Controls for the app-owned title bar.
 *
 * The native browser webview is confined to pane bodies below the title bar, so
 * it cannot cover these controls even though native child webviews compose over
 * the document.
 */
export function WindowDragRegion() {
  return (
    <span
      className="topbar-gap"
      data-tauri-drag-region=""
      onDoubleClick={() => getCurrentWindow().toggleMaximize().catch(complain)}
    />
  );
}

export function WindowControls() {
  const [maximized, setMaximized] = useState(false);

  useEffect(() => {
    const self = getCurrentWindow();
    const read = () => self.isMaximized().then(setMaximized).catch(complain);
    read();
    // The state can change without a button here being pressed — a snap
    // gesture, a double-click on the bar, the OS restoring the window — so the
    // icon follows the window rather than the last click.
    let stop: (() => void) | null = null;
    self
      .onResized(read)
      .then((off) => {
        stop = off;
      })
      .catch(complain);
    return () => stop?.();
  }, []);

  const self = () => getCurrentWindow();

  return (
    <div className="window-controls">
      <button
        className="window-btn"
        onClick={() => self().minimize().catch(complain)}
        aria-label="Minimize"
        title="Minimize"
      >
        <MinimizeIcon size={13} />
      </button>
      <button
        className="window-btn"
        onClick={() => self().toggleMaximize().catch(complain)}
        aria-label={maximized ? "Restore" : "Maximize"}
        title={maximized ? "Restore" : "Maximize"}
      >
        {maximized ? <RestoreIcon size={13} /> : <MaximizeIcon size={13} />}
      </button>
      <button
        className="window-btn is-close"
        onClick={() => self().close().catch(complain)}
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
