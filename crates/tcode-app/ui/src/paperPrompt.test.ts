import { describe, expect, it } from "vitest";

import { paperPrompt, summarizePaperPrompt, summarizePagePrompt } from "./paperPrompt";

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
  it("includes all pages for short papers", () => {
    const texts = ["Page one text", "Page two text", "Page three text"];
    const result = summarizePaperPrompt("docs/paper.pdf", texts, 3);
    expect(result).toContain('@paper("paper.pdf")');
    expect(result).toContain("--- Page 1 ---\nPage one text");
    expect(result).toContain("--- Page 3 ---\nPage three text");
    expect(result).toContain("Core thesis");
    expect(result).toContain("Limitations & future work");
    expect(result).not.toContain("omitted");
  });

  it("truncates long papers keeping head and tail", () => {
    const texts = Array.from({ length: 50 }, (_, i) => `Text of page ${i + 1}`);
    const result = summarizePaperPrompt("long.pdf", texts, 50);
    expect(result).toContain("--- Page 1 ---");
    expect(result).toContain("--- Page 30 ---");
    expect(result).toContain("[... pages 31–45 omitted ...]");
    expect(result).toContain("--- Page 46 ---");
    expect(result).toContain("--- Page 50 ---");
    expect(result).not.toContain("--- Page 31 ---\nText of page 31");
  });
});

describe("summarize page prompt", () => {
  it("includes page number and text", () => {
    const result = summarizePagePrompt("paper.pdf", 5, "  Some page text here.  ");
    expect(result).toContain('page 5 of @paper("paper.pdf")');
    expect(result).toContain("Some page text here.");
    expect(result).toContain("key points");
  });
});
