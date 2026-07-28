/**
 * The attribute that makes a piece of chrome behave like a title bar.
 *
 * Tauri starts a window drag when the element under the pointer *itself* has
 * `data-tauri-drag-region` — it does not look at ancestors. So the bar carries
 * it and so does every inert thing sitting in the bar (the title, the path),
 * or the window would only be draggable by the gaps between them.
 *
 * Interactive children must never carry it: the drag would swallow their click.
 */
export const DRAG = { "data-tauri-drag-region": true } as const;
