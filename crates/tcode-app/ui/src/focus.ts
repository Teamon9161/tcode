/**
 * Which pane is "the one to the left" — the question a tiling window manager
 * answers on every arrow press.
 *
 * The tree cannot answer it. `row(a, col(b, c))` says b and c are siblings, but
 * whether pressing → from a should land on b or on c depends entirely on where
 * the pointer-free eye is looking, which is a fact about pixels. So this takes
 * boxes rather than a `Layout`, which also makes it a pure function of numbers
 * and therefore testable without a DOM.
 */

export type Dir4 = "left" | "right" | "up" | "down";

/** The part of a `DOMRect` this needs. */
export type Box = { left: number; right: number; top: number; bottom: number };

/**
 * The nearest pane in `dir`, or null when there is nothing that way.
 *
 * Only panes whose centre actually lies in the requested direction are
 * candidates — moving right must never land on something to the left, however
 * close it is. Among those, the score prefers straight ahead over merely near:
 * drift across the axis costs double, so from a tall pane the neighbour level
 * with your eye wins over one that is technically closer but two rows down.
 */
export function nearestPane(boxes: Map<string, Box>, from: string, dir: Dir4): string | null {
  const here = boxes.get(from);
  if (!here) return null;

  const horizontal = dir === "left" || dir === "right";
  let best: string | null = null;
  let bestScore = Infinity;

  for (const [id, box] of boxes) {
    if (id === from) continue;

    const dx = centre(box.left, box.right) - centre(here.left, here.right);
    const dy = centre(box.top, box.bottom) - centre(here.top, here.bottom);
    const along = dir === "left" ? -dx : dir === "right" ? dx : dir === "up" ? -dy : dy;
    if (along <= 0) continue;

    const across = Math.abs(horizontal ? dy : dx);
    const score = along + across * 2;
    if (score < bestScore) {
      bestScore = score;
      best = id;
    }
  }

  return best;
}

function centre(low: number, high: number): number {
  return (low + high) / 2;
}
