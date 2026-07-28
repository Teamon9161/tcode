/**
 * Stand-in for `@tauri-apps/api/window` in the design preview.
 *
 * The window controls are part of the title bar's layout, so the preview has to
 * draw them; there is no window behind them in a browser tab, so every call is
 * a no-op that resolves. Aliased in only when `PREVIEW=1`.
 */
export function getCurrentWindow() {
  return {
    isMaximized: async () => false,
    onResized: async () => () => {},
    minimize: async () => {},
    toggleMaximize: async () => {},
    close: async () => {},
  };
}
