import { describe, expect, it } from "vitest";

import {
  extensionOf,
  isBinary,
  isServed,
  parseRows,
  shownAs,
  workspaceRouteOf,
} from "./show";

describe("what a shown file is drawn as", () => {
  it("routes by extension, and anything unknown is still readable as text", () => {
    expect(shownAs("out/report.html", "<p>hi</p>")).toEqual({ as: "framed" });
    expect(shownAs("out/plot.svg", "<svg viewBox='0 0 10 10'/>")).toEqual({ as: "sandbox", sandbox: "svg" });
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
    // An icon is a picture like any other. It is spelled out because this is
    // the table both readers consult, and a `.ico` that is not in it arrives as
    // text — which for an icon means arriving as mojibake.
    expect(isBinary("icons/icon.ico")).toBe(true);
    expect(shownAs("icons/icon.ico", "")).toEqual({ as: "image" });
    // Not an image: an svg is a document that draws, and its source is the
    // thing a person opens it to change.
    expect(isBinary("plot.svg")).toBe(false);
    expect(isBinary("data.csv")).toBe(false);
  });

  /** A served file is the third answer to "how do the bytes get here", and the
   *  only one whose answer is "they do not". A report that came through the
   *  reader would be truncated at the text budget and would then be handed to a
   *  renderer that cannot run its scripts — so this must never quietly fall back
   *  to one of the other two. */
  it("knows which files it must not read, because the frame fetches them", () => {
    expect(isServed("out/report.HTML")).toBe(true);
    expect(isServed("notebook.htm")).toBe(true);
    // Everything else still arrives through this process, one way or the other.
    expect(isServed("plot.svg")).toBe(false);
    expect(isServed("plot.png")).toBe(false);
    expect(isServed("notes.md")).toBe(false);
    expect(isBinary("out/report.html")).toBe(false);
  });

  it("keeps artifact loading and live workspace editing as separate routes", () => {
    // A shown report is a served document, while the same workspace path is
    // UTF-8 source. Neither answer can leak into the other caller.
    expect(isServed("out/report.html")).toBe(true);
    expect(shownAs("out/report.html", "<p>hi</p>")).toEqual({ as: "framed" });
    expect(workspaceRouteOf("out/report.html")).toEqual({ load: "text", as: "editor" });

    expect(workspaceRouteOf("README.md")).toEqual({ load: "text", as: "markdown" });
    expect(workspaceRouteOf("plot.png")).toEqual({ load: "bytes", as: "image" });
    for (const path of ["plot.svg", "flow.mmd", "data.csv", "data.tsv", "chart.json", "main.rs"]) {
      expect(workspaceRouteOf(path)).toEqual({ load: "text", as: "editor" });
    }
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
