/**
 * Theme colours, in a form something other than a browser can read.
 *
 * The theme is authored in OKLCH (`theme/porcelain.css`) and most of it is
 * `color-mix()` on top of that. Everything drawn by CSS is fine with that;
 * everything drawn by *somebody else* is not, and this window has two of those:
 *
 *  - the artifact sandbox, where mermaid's colour helper rejects `oklch(…)`
 *    outright (`Sandbox.tsx`, AGENTS.md rule 12);
 *  - the terminal, where xterm paints cells itself and takes a colour table
 *    rather than a stylesheet (`termHost.ts`).
 *
 * Both need the same thing — a token resolved to sRGB — so they ask the same
 * function. Two copies of this would be two chances to reintroduce the bug the
 * comment below is about, in a place where the failure is a wrong colour rather
 * than an error.
 */

/**
 * Any CSS colour the browser understands, as `#rrggbb`. Null for values that
 * are not colours at all — the font and size tokens go through the same loop.
 *
 * Two steps, both needed. The sentinel probe establishes that the value *is* a
 * colour: an invalid assignment leaves `fillStyle` untouched, so starting from
 * black and from white and arriving at the same answer means it parsed. Then
 * the colour is actually painted and read back, because reading `fillStyle`
 * alone does not convert — the engine serialises `oklch(…)` straight back out,
 * which is exactly the value neither consumer can use.
 */
export function asColor(value: string): string | null {
  const ctx = paint();
  if (!ctx) return null;

  ctx.fillStyle = "#000000";
  ctx.fillStyle = value;
  const fromBlack = ctx.fillStyle;
  ctx.fillStyle = "#ffffff";
  ctx.fillStyle = value;
  if (fromBlack !== ctx.fillStyle) return null;

  ctx.clearRect(0, 0, 1, 1);
  ctx.fillStyle = value;
  ctx.fillRect(0, 0, 1, 1);
  const [red, green, blue, alpha] = ctx.getImageData(0, 0, 1, 1).data;
  const hex = (channel: number) => channel.toString(16).padStart(2, "0");
  return alpha === 255
    ? `#${hex(red)}${hex(green)}${hex(blue)}`
    : `rgba(${red}, ${green}, ${blue}, ${(alpha / 255).toFixed(3)})`;
}

/** One token's value as the document currently resolves it. */
export function tokenValue(token: string): string {
  return getComputedStyle(document.documentElement).getPropertyValue(token).trim();
}

let scratch: CanvasRenderingContext2D | null | undefined;
function paint() {
  if (scratch === undefined) {
    scratch = document
      .createElement("canvas")
      .getContext("2d", { willReadFrequently: true });
  }
  return scratch;
}
