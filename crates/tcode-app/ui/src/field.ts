/**
 * How wide the pane field is against how tall.
 *
 * The one fact `layout.ts` needs and cannot derive: its tree is in fractions,
 * and "is this pane wider than it is tall" is a question about pixels. It lives
 * here rather than in that file because that file is pure, and rather than in a
 * component because the layout callbacks that need it are bound once and must
 * not close over a size that changes with every window resize — measuring at
 * the moment of the click is both simpler and always right.
 *
 * `undefined` when there is no field to measure (the launchpad, a test): the
 * layout functions read that as side by side, which is what this window did
 * before splits could stack on their own.
 */
export function fieldAspect(): number | undefined {
  const field = document.querySelector(".panes-field");
  if (!field) return undefined;
  const { width, height } = field.getBoundingClientRect();
  return width > 0 && height > 0 ? width / height : undefined;
}
