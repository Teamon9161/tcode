import { createContext, useContext, type MouseEvent, type ReactNode } from "react";

import { rich } from "./rich";

/**
 * Rendered markdown, with its links connected to the window.
 *
 * Every place that draws a model's prose — a message, a compact summary, a
 * sub-agent's report, a `.md` file, a plan's detail — goes through here, so a
 * link behaves the same in all of them. That is the whole reason this is a
 * component and not a handler copied into six call sites.
 *
 * **The click is delegated, and that is load-bearing.** `rich()` is a pure
 * function cached on its source text (rule 21) — a hundred-turn transcript
 * re-lexing on every stream delta is what that cache exists to prevent — so it
 * cannot take a callback: two panes would want two callbacks for one cache
 * entry. Listening on the container instead keeps the document a pure function
 * of the text, and the destination a property of *where it was clicked*, which
 * is what it actually is.
 *
 * The anchors keep their `href` and their `target="_blank"`. Neither is
 * decoration: the href is what a right-click copies, and the target is the
 * reason a click that this handler somehow misses still cannot navigate the app
 * away from itself.
 */
export type Follow = (href: string, aside: boolean) => void;

/**
 * Who answers a link click, supplied by whichever pane the prose is drawn in.
 *
 * A context rather than a prop because prose is drawn at the bottom of several
 * unrelated recursions — a report inside a run inside a batch — and threading a
 * callback through each of them would put a second copy of this decision in
 * every one. `null` is a legitimate value: the design preview and the tests
 * draw prose with nowhere for a link to go, and a dead link there is correct.
 */
export const LinkContext = createContext<Follow | null>(null);

export function Prose({ text, className }: { text: string; className?: string }): ReactNode {
  const follow = useContext(LinkContext);

  const onClick = (event: MouseEvent<HTMLDivElement>) => {
    if (event.defaultPrevented || event.button !== 0) return;
    const target = event.target as HTMLElement | null;
    const anchor = target?.closest?.("a.prose-link");
    if (!(anchor instanceof HTMLAnchorElement)) return;

    // `getAttribute`, never `.href`: the property is resolved against this
    // document's own URL, which turns `out/plot.csv` into `tauri://localhost/…`
    // — the one form the router cannot make sense of.
    const href = anchor.getAttribute("href");
    if (!href) return;

    // Swallowed whether or not anything comes of it. A followed link is handled
    // here; an unfollowable one must do nothing at all, which is what it did
    // before this file existed.
    event.preventDefault();
    // The same modifier the file tree and the transcript use for "open this as
    // well" (`openAside`), so one gesture means one thing everywhere.
    follow?.(href, event.metaKey || event.ctrlKey);
  };

  return (
    <div className={className} onClick={onClick}>
      {rich(text)}
    </div>
  );
}
