import { useMemo } from "react";
import katex from "katex";
import "katex/dist/katex.min.css";

/**
 * The one exemption.
 *
 * Everywhere else in this UI, model output becomes React nodes and never
 * markup — that is what guarantees it cannot reach `innerHTML`. TeX cannot be
 * rendered that way: KaTeX's entire output is a markup string, and it is not
 * worth reimplementing a typesetter to avoid one `dangerouslySetInnerHTML`.
 *
 * So the boundary is drawn around KaTeX instead of around the string:
 *
 *  - `trust: false` is the load-bearing flag. It disables every command that
 *    can emit a URL or arbitrary class/style — `\href`, `\url`,
 *    `\includegraphics`, `\htmlClass`, `\htmlData`, `\htmlStyle`. Without it
 *    this file would be a straightforward injection point.
 *  - The options are frozen and this module exports no way to pass more. A
 *    caller cannot loosen what it cannot reach, which is the difference between
 *    a rule and a structure.
 *  - The app's CSP must never gain `unsafe-inline` in `script-src`. KaTeX emits
 *    no scripts, so anything script-shaped arriving here is already an escape,
 *    and the CSP is the layer that stops it executing.
 *
 * `strict: "ignore"` is a usability choice, not a safety one: strict mode
 * rejects ordinary things like CJK inside `\text{}`, and rejecting the user's
 * own notation would be a bad trade for warnings nobody reads.
 */
const OPTIONS = Object.freeze({
  throwOnError: false,
  trust: false,
  strict: "ignore",
  output: "html",
}) satisfies katex.KatexOptions;

export function Math({ tex, display }: { tex: string; display: boolean }) {
  const html = useMemo(
    () => katex.renderToString(tex, { ...OPTIONS, displayMode: display }),
    [tex, display],
  );

  return display ? (
    <div className="math math-block" dangerouslySetInnerHTML={{ __html: html }} />
  ) : (
    <span className="math math-inline" dangerouslySetInnerHTML={{ __html: html }} />
  );
}
