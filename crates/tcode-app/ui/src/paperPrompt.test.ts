import { describe, expect, it } from "vitest";

import { paperPrompt, summarizePaperPrompt } from "./paperPrompt";

describe("paper selection prompts", () => {
  it("keeps the source and selected text editable in the draft", () => {
    expect(paperPrompt("translate", "docs/rates paper.pdf", 3, "  Duration measures sensitivity. ")).toBe(
      'Translate the selected text from @paper("rates paper.pdf", page 3):\n\nDuration measures sensitivity.',
    );
    expect(paperPrompt("explain", "paper.pdf", 1, "convexity")).toBe(
      'Explain the selected text from @paper("paper.pdf", page 1):\n\nconvexity',
    );
    expect(paperPrompt("ask", "paper.pdf", 2, "term premium")).toBe(
      'I have a question about the selected text from @paper("paper.pdf", page 2):\n\nterm premium\n\n',
    );
  });
});

describe("summarize paper prompt", () => {
  it("mentions the file with @path so the AI reads it directly", () => {
    const result = summarizePaperPrompt("docs/paper.pdf");
    expect(result).toContain("@docs/paper.pdf");
    expect(result).toContain("Summarize");
  });

  it("quotes paths that contain spaces", () => {
    const result = summarizePaperPrompt("docs/my paper.pdf");
    expect(result).toContain('@"docs/my paper.pdf"');
  });
});
