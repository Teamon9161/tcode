import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";

import { rich } from "./rich";
import { Sandbox } from "./Sandbox";

/**
 * The rendering boundary, asserted.
 *
 * These are not style tests. In this window a script that executes reaches
 * `window.__TAURI__` and therefore an arbitrary command on this machine, and
 * model output routinely carries file contents, fetched pages and MCP results
 * inside it. The guarantee is that `rich()` can only *construct* known elements
 * — so the test is that hostile input comes out as text, every time.
 *
 * `renderToStaticMarkup` is used deliberately: it shows what the built React
 * tree would actually become, so an escape would be visible as real markup here
 * rather than hidden behind a node type.
 */

const render = (markdown: string) => renderToStaticMarkup(<>{rich(markdown)}</>);

describe("rich() never produces markup from model output", () => {
  it("renders raw HTML as its own literal text", () => {
    const html = render('before <img src=x onerror="alert(1)"> after');
    expect(html).not.toContain("<img");
    // The word survives as text, which is the point — what must not exist is
    // the attribute form, where the quote is a real quote rather than `&quot;`.
    expect(html).not.toContain('onerror="');
    expect(html).toContain("&lt;img");
  });

  it("does not emit a script element", () => {
    const html = render("<script>fetch('http://evil')</script>");
    expect(html).not.toContain("<script");
    expect(html).toContain("&lt;script");
  });

  it("refuses a javascript: link but keeps its words", () => {
    const html = render("[click me](javascript:alert(document.cookie))");
    expect(html).not.toContain("javascript:");
    expect(html).not.toContain("<a ");
    expect(html).toContain("click me");
  });

  it("refuses data: and vbscript: links", () => {
    for (const scheme of ["data:text/html;base64,PHNjcmlwdD4=", "vbscript:msgbox"]) {
      const html = render(`[x](${scheme})`);
      expect(html).not.toContain("<a ");
    }
  });

  it("keeps ordinary links, with the window it opens locked down", () => {
    const html = render("[docs](https://example.com/a)");
    expect(html).toContain('href="https://example.com/a"');
    expect(html).toContain("noopener");
  });

  it("never loads a remote picture, however the reference is written", () => {
    const html = render("![alt](https://evil.example/track.png)");
    expect(html).not.toContain("<img");
    expect(html).toContain("alt");
  });

  it("passes an event-handler attribute through as text inside a code fence", () => {
    const html = render("```html\n<div onclick=\"steal()\">hi</div>\n```");
    expect(html).not.toContain("onclick=\"steal()\"");
    expect(html).toContain("&lt;div");
  });
});

describe("rich() still renders the document", () => {
  it("builds tables, lists and headings", () => {
    const html = render("## Title\n\n- one\n- two\n\n| a | b |\n|---|---|\n| 1 | 2 |");
    expect(html).toContain("<table");
    expect(html).toContain("<li>");
    expect(html).toMatch(/<h[1-6]/);
  });

  it("typesets display math but leaves currency alone", () => {
    expect(render("$$x^2$$")).toContain("katex");
    expect(render("it costs $30 and $40 total")).not.toContain("katex");
  });

  it("typesets the TeX delimiters models actually emit", () => {
    // Without a tokenizer of its own this loses the backslashes to CommonMark
    // escaping and renders as a bracket around raw source.
    const block = render("\\[\n\\sum_{i} |p_i| \\leq 0.5\n\\]");
    expect(block).toContain("katex");
    expect(block).not.toContain("sum_");

    const inline = render("cash \\(C_t\\) at time t");
    expect(inline).toContain("katex");
    expect(inline).not.toContain("C_t\\");
  });
});

describe("the artifact frame stays an opaque origin", () => {
  it("carries allow-scripts and nothing else", () => {
    const html = renderToStaticMarkup(
      <Sandbox kind="html" source="<b>x</b>" label="artifact" />,
    );
    expect(html).toContain('sandbox="allow-scripts"');
    // The single attribute this whole design rests on never being present.
    expect(html).not.toContain("allow-same-origin");
  });
});
