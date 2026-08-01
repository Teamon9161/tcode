import { useEffect, useState } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";

import { CloseIcon, MaximizeIcon, MinimizeIcon, RestoreIcon } from "./Icons";

/**
 * Legacy custom window controls.
 *
 * The desktop app uses the native caption so an embedded child browser webview
 * cannot intercept their clicks. Keep this component unrendered unless a
 * platform-specific native hit-test layer is introduced alongside it.
 */
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
