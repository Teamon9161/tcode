import { fieldAspect } from "./field";
import { focusPane, openWeb, webPane, type Tiling } from "./layout";
import { select, snapshot } from "./webHost";

/**
 * Reveal one exact native Browser capability from a transcript group.
 *
 * Validation precedes every visible side effect. A stale group therefore cannot
 * collapse the current pane, open the Browser, or select whichever tab happens
 * to be current. `aspect` is injectable only for the layout boundary test; the
 * live callback measures the field at the click.
 */
export function revealBrowserTab(
  tab: string,
  onTiling: (step: (current: Tiling) => Tiling) => void,
  collapse: () => void,
  aspect?: number,
): boolean {
  if (!snapshot().tabs.list.some((candidate) => candidate.id === tab)) return false;
  collapse();
  onTiling((current) => {
    const already = webPane(current);
    return already
      ? focusPane(current, already.id)
      : openWeb(current, aspect ?? fieldAspect());
  });
  select(tab);
  return true;
}
