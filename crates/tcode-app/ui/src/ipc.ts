/**
 * The one seam between this frontend and whatever shell is hosting it.
 *
 * Every `invoke` and `listen` in `src/` comes through here — imported as
 * `@ipc`, never by relative path, because that specifier is what the build
 * swaps for fixtures in the design preview (`vite.config.ts`). The rule from
 * AGENTS.md holds unchanged: the mocks are reachable only in `--mode preview`,
 * so a shipped bundle cannot contain them.
 *
 * The signatures are the ones `@tauri-apps/api` had, because they *were* those
 * functions until the backend grew its own registry. Keeping the shape meant
 * the nineteen call sites changed one import line and nothing else — and it is
 * why the second shell arrived here as a branch in one function rather than as
 * a rewrite. See `MIGRATION-ELECTRON.md`.
 *
 * ## Which shell
 *
 * Decided at runtime, not at build time, because both shells load the same
 * `ui/dist`: Electron's preload puts `window.tcode` there and Tauri does not.
 * A build-time switch would mean two bundles and therefore two things to test.
 * Phase 6 deletes the Tauri side of every branch below and this file goes back
 * to having no branches at all.
 */

import { invoke as tauriInvoke } from "@tauri-apps/api/core";
import { listen as tauriListen } from "@tauri-apps/api/event";

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
 * Which shell is drawing the window.
 *
 * Read by `main.tsx` and used for exactly one thing: the title bar's drag
 * surface, which is an attribute under Tauri and a CSS property under Electron
 * and cannot be both at once without risking one shell honouring the other's.
 * Resist adding a second reader — anything else that differs between shells
 * belongs behind `invoke`, where the shell answers for itself.
 */
export const SHELL = bridge ? "electron" : "tauri";

/**
 * Call a backend command.
 *
 * One method name and one argument object, whichever shell is listening: the
 * backend's `dispatch::Registry` does the argument-by-name and serialization
 * that `#[tauri::command]` used to generate, so the same table answers a Tauri
 * `invoke` and a JSON-RPC line from an Electron main process. Under Tauri the
 * `rpc` envelope is this function's whole job, and it is why no caller had to
 * learn about it.
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
  return bridge
    ? (bridge.invoke(command, args ?? {}) as Promise<T>)
    : tauriInvoke<T>("rpc", { method: command, args: args ?? {} });
}

/**
 * Subscribe to a backend event.
 *
 * Returns a promise for the same reason it always did: under Tauri registering
 * a listener is itself an IPC round trip. Electron's is synchronous, so that
 * side resolves immediately — kept asynchronous rather than "simplified"
 * because every call site already unsubscribes through the promise, and two
 * shapes for one function is the change that would actually cost something.
 */
export function listen<T>(
  name: string,
  handler: (event: Event<T>) => void,
): Promise<UnlistenFn> {
  if (!bridge) return tauriListen<T>(name, handler);
  return Promise.resolve(
    bridge.listen(name, (payload) => handler({ payload: payload as T })),
  );
}
