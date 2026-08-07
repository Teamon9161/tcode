/**
 * The one seam between this frontend and the shell hosting it.
 *
 * Every `invoke` and `listen` in `src/` comes through here — imported as
 * `@ipc`, never by relative path, because that specifier is what the build
 * swaps for fixtures in the design preview (`vite.config.ts`). The rule from
 * AGENTS.md holds unchanged: the mocks are reachable only in `--mode preview`,
 * so a shipped bundle cannot contain them.
 *
 * The signatures are the ones `@tauri-apps/api` had, because they *were* those
 * functions until the backend grew its own registry. Keeping the shape meant
 * the nineteen call sites changed one import line and nothing else. The Tauri
 * branch is gone (Phase 6): Electron is the only shell, so the seam is the
 * preload's bridge and nothing else.
 */

/** An event as a listener receives it. Ours rather than Tauri's, because it is
 *  the only field any caller reads and the Electron side has no other. */
export type Event<T> = { payload: T };
export type UnlistenFn = () => void;

/**
 * What `electron/preload.js` exposes, and the entire attack surface of the app
 * webview: two functions and no `ipcRenderer`. The names are a contract with
 * that file — see it for why nothing else may be added here.
 */
type Bridge = {
  invoke(method: string, args: Record<string, unknown>): Promise<unknown>;
  listen(event: string, deliver: (payload: unknown) => void): UnlistenFn;
};

declare global {
  interface Window {
    tcode?: Bridge;
  }
}

const bridge = window.tcode;

/**
 * Which shell is drawing the window. Electron, always — the Tauri branch that
 * used to answer `"tauri"` here is gone. The value is still read by `main.tsx`
 * for the title bar's drag surface, and the design preview answers `"preview"`
 * instead, so a preview cannot inherit a drag region it has no window for.
 */
export const SHELL = "electron";

/**
 * The bridge, or a boot-failure error that names what is missing.
 *
 * There is exactly one shell left and it always injects the preload, so a
 * missing `window.tcode` is a boot failure (the preload did not run, or was
 * removed) — not a signal to fall back to something. The check is per call
 * rather than at module top because importing this module must not crash a
 * non-Electron environment (the design preview, tests): a caller that never
 * talks to the backend should be able to import the seam.
 */
function requireBridge(): Bridge {
  if (!bridge) {
    throw new Error("the tcode bridge is missing — did electron/preload.js load?");
  }
  return bridge;
}

/**
 * Call a backend command.
 *
 * One method name and one argument object: the backend's `dispatch::Registry`
 * does the argument-by-name and serialization that `#[tauri::command]` used to
 * generate, and the Electron main process forwards the call down its pipe to
 * the sidecar, which is that registry.
 *
 * **Some of these commands are answered by the shell rather than the backend**
 * — `window_*`, `dialog_open_folder`, `browser_*`. That is deliberate and it is
 * why they are not a separate import: a caller asks for what it wants, and who
 * owns a window is not its problem.
 */
export function invoke<T>(
  command: string,
  args?: Record<string, unknown>,
): Promise<T> {
  return requireBridge().invoke(command, args ?? {}) as Promise<T>;
}

/**
 * Subscribe to a backend event.
 */
export function listen<T>(
  name: string,
  handler: (event: Event<T>) => void,
): Promise<UnlistenFn> {
  return Promise.resolve(
    requireBridge().listen(name, (payload) => handler({ payload: payload as T })),
  );
}
