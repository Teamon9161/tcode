/**
 * Stand-in for `@ipc` in the design preview.
 *
 * The fixtures answer command names directly — `mock-core` switches on
 * `"shown_file"`, `"sessions"` and the rest — so this does **not** wrap calls in
 * the `rpc` envelope the real seam adds under Tauri. That envelope exists to
 * reach a Rust registry, and the preview has no backend to reach: putting it in
 * here would mean every fixture had to unwrap it before recognizing its own
 * command.
 *
 * Aliased in by `vite.config.ts` under `--mode preview` only, which is what
 * keeps every fixture out of the shipped bundle. It is the whole alias list now
 * — the window and folder-dialog stand-ins are gone, because both are ordinary
 * commands the shell answers and `mock-core` answers them the same way it
 * answers the rest.
 */

export { invoke } from "./mock-core";
export { listen } from "./mock-event";
export type { Event, UnlistenFn } from "../ipc";

/**
 * Neither shell: a browser tab.
 *
 * The one thing this drives is the title bar's drag surface, and there is no
 * window here to drag. Naming the preview rather than borrowing a shell's name
 * is what keeps that true — `[data-shell="electron"]` would hand the preview a
 * drag region that swallows the clicks of the menus sitting in the bar, which
 * are exactly what the preview exists to look at.
 */
export const SHELL = "preview";
