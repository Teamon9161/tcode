import { describe, expect, it } from "vitest";

import { extensionOf, isBinary, parseRows, shownAs } from "./show";

describe("what a shown file is drawn as", () => {
  it("routes by extension, and anything unknown is still readable as text", () => {
    expect(shownAs("out/report.html", "<p>hi</p>")).toEqual({ as: "sandbox", sandbox: "html" });
    expect(shownAs("flow.mmd", "graph TD")).toEqual({ as: "sandbox", sandbox: "mermaid" });
    expect(shownAs("pnl.csv", "a,b")).toEqual({ as: "table", separator: "," });
    expect(shownAs("pnl.tsv", "a\tb")).toEqual({ as: "table", separator: "\t" });
    expect(shownAs("notes.md", "# hi")).toEqual({ as: "doc" });
    expect(shownAs("plot.png", "")).toEqual({ as: "image" });
    expect(shownAs("go.mod", "module x")).toEqual({ as: "text" });
    expect(shownAs("Makefile", "all:")).toEqual({ as: "text" });
  });

  /** `.json` is a container, not a kind of thing, so it is the one entry that
   *  looks at the body — and a config file must not become a blank chart. */
  it("treats a json file as a chart only when it is one", () => {
    expect(shownAs("c.json", '{"series":[{"type":"line","data":[1,2]}]}')).toEqual({
      as: "sandbox",
      sandbox: "echarts",
    });
    expect(shownAs("tsconfig.json", '{"compilerOptions":{}}')).toEqual({ as: "text" });
    expect(shownAs("broken.json", "{not json")).toEqual({ as: "text" });
    expect(shownAs("list.json", "[1,2,3]")).toEqual({ as: "text" });
  });

  it("knows which files must arrive as bytes before anything is loaded", () => {
    expect(isBinary("a/b/plot.PNG")).toBe(true);
    expect(isBinary("plot.svg")).toBe(false);
    expect(isBinary("data.csv")).toBe(false);
  });

  it("does not mistake a dotfile for an extension", () => {
    expect(extensionOf(".gitignore")).toBe("");
    expect(extensionOf("C:\\code\\a.b\\report.html")).toBe("html");
  });
});

describe("parsing delimited data", () => {
  it("keeps a separator that is inside a quoted field", () => {
    const rows = parseRows('name,ytm\n"CDB, 10Y",2.15\n', ",");
    expect(rows).toEqual([
      ["name", "ytm"],
      ["CDB, 10Y", "2.15"],
    ]);
  });

  it("unescapes doubled quotes and survives CRLF", () => {
    const rows = parseRows('a,b\r\n"say ""hi""",2\r\n', ",");
    expect(rows[1]).toEqual(['say "hi"', "2"]);
  });

  it("does not invent a trailing empty row", () => {
    expect(parseRows("a,b\n1,2\n", ",")).toHaveLength(2);
    expect(parseRows("a,b\n1,2", ",")).toHaveLength(2);
  });

  it("keeps empty fields, because a blank cell is data", () => {
    expect(parseRows("a,b,c\n1,,3", ",")[1]).toEqual(["1", "", "3"]);
  });
});
