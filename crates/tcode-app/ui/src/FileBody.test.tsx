import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";

import { FileBody } from "./FileBody";
import { FRAME_SANDBOX } from "./Framed";

/**
 * The renderer both file entrances share, asserted at its boundary.
 *
 * A file in the project is untrusted for the same reason model output is: it
 * was not written by the person in this conversation. That did not change when
 * the workspace tree started drawing files instead of dumping them in a
 * textarea — it became load-bearing, because a `.html` in the tree is now
 * rendered rather than shown as text.
 *
 * There are **two** frames in this app and they isolate by different means; the
 * assertions below are split the same way, because conflating them is how one
 * of them would quietly get the other's attributes:
 *
 *  - `Sandbox` renders a *string* the model authored, into a frame with no
 *    origin at all. Isolation is the opaque origin, which exists only as long
 *    as `allow-same-origin` is absent — so that absence is the thing pinned.
 *  - `Framed` loads a *file* from the app's loopback origin. Isolation is that
 *    origin being a different one from the app's, which holds regardless of any
 *    attribute. What is pinned there is that it cannot navigate the window it
 *    sits in, and that the file's bytes never reach this document at all.
 */

const render = (path: string, body: string) =>
  renderToStaticMarkup(<FileBody path={path} label={path} body={body} />);

describe("FileBody draws a file as what it is", () => {
  it("keeps model-authored markup in the opaque-origin frame", () => {
    const svg = render("out/plot.svg", '<svg onload="alert(1)"/>');
    // The source goes across by message, so none of it is in this document.
    expect(svg).not.toContain("onload");
    expect(svg).toContain('sandbox="allow-scripts"');
    // The one attribute that would undo the whole boundary.
    expect(svg).not.toContain("allow-same-origin");
  });

  it("never lets a served file's bytes into this document", () => {
    // The body is what a caller that still reads the file would hand over. The
    // framed view must ignore it: it holds a reference to the file, not a copy,
    // and anything of the file appearing here would mean it had been parsed on
    // this side after all.
    const html = render("out/report.html", '<img src=x onerror="alert(1)">');
    expect(html).not.toContain("onerror");
    expect(html).not.toContain("<img");
  });

  it("does not let a shown report navigate the window it is embedded in", () => {
    // A cross-origin frame cannot reach the app's IPC whatever these say. Top
    // navigation is different in kind: it needs no access to the parent, only
    // permission, and it would turn "look at this file" into "replace the app
    // with a page of the file's choosing".
    expect(FRAME_SANDBOX).not.toContain("allow-top-navigation");
    // An alert from a report would block the window around it; nothing in a
    // report has a reason to open a second one. Both fail by doing nothing.
    expect(FRAME_SANDBOX).not.toContain("allow-modals");
    expect(FRAME_SANDBOX).not.toContain("allow-popups");
    // And the two it does need, which are only a warning sign when a frame is
    // same-origin with its parent — this one never is.
    expect(FRAME_SANDBOX).toContain("allow-scripts");
    expect(FRAME_SANDBOX).toContain("allow-same-origin");
  });

  it("draws an image from the bytes it was handed, and never as text", () => {
    const html = render("icons/mark.png", "data:image/png;base64,AAAA");
    expect(html).toContain('src="data:image/png;base64,AAAA"');
    expect(html).not.toContain("<textarea");
  });

  it("reads Markdown as prose and a source file as code", () => {
    expect(render("README.md", "# Title")).toContain("prose-h");
    // Not yet highlighted — the grammar loads asynchronously — but complete and
    // escaped, which is the property that matters here.
    const code = render("src/main.rs", "fn main() { let x = \"<b>\"; }");
    expect(code).toContain("code-block");
    expect(code).toContain("&lt;b&gt;");
    expect(code).not.toContain("<b>");
  });

  it("reads a delimited file as a table, quoted fields and all", () => {
    const html = render("out/pnl.csv", 'date,name\n2026-07-31,"CDB, 10Y"');
    expect(html).toContain("<th>date</th>");
    expect(html).toContain("<td>CDB, 10Y</td>");
  });
});
