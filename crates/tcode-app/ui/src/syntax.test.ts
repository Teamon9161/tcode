import { describe, expect, it } from "vitest";

import { highlight, isHighlightable, languageId, loadGrammar, type Token } from "./syntax";

/** The token kinds a snippet produces, as a compact string, so a case reads as
 *  the thing being asserted rather than as an array of objects. */
function kinds(tokens: Token[] | null, want: string): string[] {
  return (tokens ?? [])
    .filter((token) => token.text.trim() === want)
    .map((token) => token.kind);
}

describe("language tags", () => {
  it("resolves the aliases both entrances actually produce", () => {
    // A file extension, a fence tag, and the same language spelled the long way.
    expect(languageId("rs")).toBe("rust");
    expect(languageId("Rust")).toBe("rust");
    expect(languageId("tsx")).toBe("tsx");
    expect(languageId("yml")).toBe("yaml");
    // `.svg` is XML, which is what makes an svg file's source readable at all.
    expect(languageId("svg")).toBe("xml");
  });

  it("answers null for tags nothing here draws", () => {
    expect(languageId("brainfuck")).toBeNull();
    expect(languageId("")).toBeNull();
    expect(isHighlightable("cobol")).toBe(false);
  });
});

describe("highlighting", () => {
  it("draws nothing until the grammar has arrived", () => {
    // Not loaded, so callers get plain text rather than a wrong colouring.
    expect(highlight("let x = 1;", "zig")).toBeNull();
    expect(highlight("let x = 1;", "cobol")).toBeNull();
  });

  it("classifies a snippet into this app's own token kinds", async () => {
    await loadGrammar("rust");
    const tokens = highlight(
      `// a comment
pub fn user_turn(&self) -> Result<u32> {
    let delay = 30_000;
    self.sleep("hi");
}`,
      "rs",
    );

    expect(kinds(tokens, "// a comment")).toEqual(["comment"]);
    expect(kinds(tokens, "pub")).toEqual(["keyword"]);
    expect(kinds(tokens, "fn")).toEqual(["keyword"]);
    expect(kinds(tokens, "user_turn")).toEqual(["call"]);
    expect(kinds(tokens, "Result")).toEqual(["type"]);
    expect(kinds(tokens, "30_000")).toEqual(["number"]);
    // The quotes belong to the string, not to the punctuation around it.
    expect(kinds(tokens, '"hi"')).toEqual(["string"]);
  });

  it("keeps line breaks as tokens, so the run stays flat", async () => {
    await loadGrammar("rust");
    const tokens = highlight("let a = 1;\nlet b = 2;", "rust");
    expect(tokens?.filter((token) => token.text === "\n")).toHaveLength(1);
    expect(tokens?.map((token) => token.text).join("")).toBe("let a = 1;\nlet b = 2;");
  });

  /* The property the whole sentinel scheme rests on: what comes back is an
     index into `KINDS`, never a colour. A theme value leaking through here
     would be a literal colour no theme pack could reassign — the exact thing
     the hand-rolled highlighter existed to prevent. */
  it("never returns a kind outside the known set", async () => {
    await loadGrammar("markdown");
    const tokens = highlight("# Title\n\nSome **bold** and `code`.", "md") ?? [];
    const allowed = [
      "plain", "comment", "string", "number", "keyword", "type", "call", "punct",
      "heading", "link", "emphasis",
    ];
    for (const token of tokens) expect(allowed).toContain(token.kind);
    expect(tokens.length).toBeGreaterThan(0);
  });

  it("classifies markdown structural elements into markup kinds", async () => {
    await loadGrammar("markdown");
    const tokens = highlight("# Heading\n\n**bold** and [link](https://x.com)", "md") ?? [];
    expect(kinds(tokens, "# Heading")).toEqual(["heading"]);
    expect(kinds(tokens, "**bold**")).toEqual(["emphasis"]);
    expect(kinds(tokens, "link")).toEqual(["link"]);
    expect(kinds(tokens, "https://x.com")).toEqual(["link"]);
  });
});
