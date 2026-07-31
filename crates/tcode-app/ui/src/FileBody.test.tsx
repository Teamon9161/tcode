import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";

import { FileBody } from "./FileBody";

/**
 * The renderer both file entrances share, asserted at its boundary.
 *
 * A file in the project is untrusted for the same reason model output is: it
 * was not written by the person in this conversation. That did not change when
 * the workspace tree started drawing files instead of dumping them in a
 * textarea — it became load-bearing, because a `.html` in the tree is now
 * rendered rather than shown as text.
 */

const render = (path: string, body: string) =>
  renderToStaticMarkup(<FileBody path={path} label={path} body={body} />);

describe("FileBody draws a file as what it is", () => {
  it("puts model- and disk-authored HTML behind an opaque-origin frame", () => {
    const html = render("out/report.html", '<img src=x onerror="alert(1)">');
    // The source goes across by message, so none of it is in this document.
    expect(html).not.toContain("onerror");
    expect(html).toContain('sandbox="allow-scripts"');
    // The one attribute that would undo the whole boundary.
    expect(html).not.toContain("allow-same-origin");
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
