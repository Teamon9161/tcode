/**
 * Where the reader was in a rendered markdown preview, as a character offset
 * into the source text.
 *
 * The preview and the editor are two renderings of the same document with
 * different line heights, so a pixel position cannot cross between them — but
 * the text itself can. `offsetAtPoint` asks the engine where a point over the
 * preview falls (`caretRangeFromPoint`, which is Chromium's answer to "what is
 * under the cursor") and walks the preview's text nodes in document order to
 * turn that DOM position into an offset into the source string. The editor
 * then opens at that offset, so switching preview → edit keeps the place you
 * were reading instead of jumping back to the top.
 *
 * It is a hand-off, not an exact science: rendered math and images occupy
 * DOM nodes that are not the source characters, so the offset drifts after
 * one of those. The fallback is the top of the file, which is the honest
 * "I could not tell" answer — the reader who never scrolled was at the top.
 */

/** The absolute source-text offset at `(x, y)` in viewport coordinates, or
 *  `null` when the point lands on nothing text-like. */
export function offsetAtPoint(root: HTMLElement, x: number, y: number): number | null {
  if (typeof document.caretRangeFromPoint !== "function") return null;
  const range = document.caretRangeFromPoint(x, y);
  if (!range) return null;
  const target = range.startContainer;
  if (target.nodeType !== Node.TEXT_NODE) return null;
  return offsetOfTextNode(root, target as Text, range.startOffset);
}

/** Walk `root`'s text nodes in document order and sum their lengths up to
 *  `target`, then add the character position within it. `null` when `target`
 *  is not a descendant — the caller treats that as "no answer". */
export function offsetOfTextNode(root: HTMLElement, target: Text, within: number): number | null {
  let offset = 0;
  const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
  let node = walker.nextNode();
  while (node) {
    if (node === target) return offset + within;
    offset += (node as Text).data.length;
    node = walker.nextNode();
  }
  return null;
}
