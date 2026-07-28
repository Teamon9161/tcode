import { useEffect, useState } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";

import { CloseIcon, MaximizeIcon, MinimizeIcon, RestoreIcon } from "./Icons";

/**
 * The window's own buttons, because the app draws its own title bar.
 *
 * `decorations: false` is not a stylistic preference. A native caption bar sits
 * above our toolbar as a second horizontal band doing the same job, and it
 * carries an icon and a title the app has nothing to say with — the whole strip
 * is dead space in a window whose top row already names the session and its
 * folder. Removing it means the toolbar *is* the title bar, which is why the
 * bar carries `data-tauri-drag-region` and why these three controls have to
 * exist: without them the window cannot be minimized or closed at all.
 *
 * Failures are logged, not fatal (unlike the event listeners in `App.tsx`): a
 * minimize button that does nothing is a broken button, while a window that
 * refuses to open because of one is a broken app. The likely cause is a missing
 * grant in `capabilities/default.json` — see AGENTS.md rule 6.
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
