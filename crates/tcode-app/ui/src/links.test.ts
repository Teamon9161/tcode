import { describe, expect, it } from "vitest";

import { linkTarget, resolve } from "./links";

/**
 * Where a link in model prose is allowed to go.
 *
 * Two halves, and the refusals are the half worth reading. A link arrives as
 * model output — from a fetched page, a file's contents, an MCP result — so
 * every scheme this window has no answer for has to fall out as "nothing
 * happens", not as "something plausible".
 */

const CWD = "/home/me/proj";

describe("linkTarget sends a page to the browser", () => {
  it("takes http and https as they are written", () => {
    expect(linkTarget("https://example.com/a?b=1", CWD)).toEqual({
      kind: "web",
      url: "https://example.com/a?b=1",
    });
    expect(linkTarget("http://localhost:5173", CWD)).toEqual({
      kind: "web",
      url: "http://localhost:5173",
    });
  });

  it("refuses every other scheme", () => {
    for (const href of [
      "javascript:alert(1)",
      "data:text/html,<script>alert(1)</script>",
      "vbscript:x",
      // Not dangerous, just not this window's business: handing model output to
      // another application is a larger decision than making prose clickable.
      "mailto:someone@example.com",
      "tel:+123",
    ]) {
      expect(linkTarget(href, CWD)).toEqual({ kind: "none" });
    }
  });

  it("swallows a bare fragment rather than moving the app's own URL", () => {
    expect(linkTarget("#results", CWD)).toEqual({ kind: "none" });
    expect(linkTarget("   ", CWD)).toEqual({ kind: "none" });
  });
});

describe("linkTarget sends a path to the file viewer", () => {
  it("resolves a relative path against the conversation's folder", () => {
    expect(linkTarget("./out/plot.csv", CWD)).toEqual({
      kind: "file",
      path: "/home/me/proj/out/plot.csv",
    });
    expect(linkTarget("report.html", CWD)).toEqual({
      kind: "file",
      path: "/home/me/proj/report.html",
    });
    expect(linkTarget("../shared/data.csv", CWD)).toEqual({
      kind: "file",
      path: "/home/me/shared/data.csv",
    });
  });

  it("keeps an absolute path", () => {
    expect(linkTarget("/tmp/out.png", CWD)).toEqual({ kind: "file", path: "/tmp/out.png" });
  });

  it("reads a file URL as the path it is", () => {
    expect(linkTarget("file:///home/me/proj/out.csv", CWD)).toEqual({
      kind: "file",
      path: "/home/me/proj/out.csv",
    });
  });

  it("drops a query or fragment, because no viewer here takes an argument", () => {
    expect(linkTarget("out/report.html?v=2#top", CWD)).toEqual({
      kind: "file",
      path: "/home/me/proj/out/report.html",
    });
  });

  it("decodes what markdown had to encode", () => {
    expect(linkTarget("out/my%20plot.csv", CWD)).toEqual({
      kind: "file",
      path: "/home/me/proj/out/my plot.csv",
    });
    // A stray percent is a character in a filename, not a broken encoding.
    expect(linkTarget("out/100%.csv", CWD)).toEqual({
      kind: "file",
      path: "/home/me/proj/out/100%.csv",
    });
  });

  it("has nowhere to resolve against without a folder", () => {
    expect(linkTarget("out.csv", "")).toEqual({ kind: "none" });
  });
});

describe("resolve agrees with the boundary that will re-check it", () => {
  it("drops `..` past the root instead of escaping it", () => {
    // Matching `normalize` in tcode-tools' show.rs. The backend re-checks
    // containment, so a path that means one thing here and another there is a
    // refusal nobody can act on.
    expect(resolve("/home/me/proj", "../../../../etc/passwd")).toBe("/etc/passwd");
  });

  it("keeps a Windows path a Windows path, drive included", () => {
    expect(resolve("C:\\code\\proj", "out\\plot.csv")).toBe("C:\\code\\proj\\out\\plot.csv");
    // Markdown writes forward slashes even on Windows; the separator comes from
    // the folder, which is the one piece of evidence that says which shape this is.
    expect(resolve("C:\\code\\proj", "out/plot.csv")).toBe("C:\\code\\proj\\out\\plot.csv");
    expect(resolve("C:\\code\\proj", "D:\\other\\x.csv")).toBe("D:\\other\\x.csv");
    expect(resolve("C:\\code\\proj", "..\\..\\..\\..\\x")).toBe("C:\\x");
  });
});
