import { describe, expect, it } from "vitest";

import { isTyping } from "./typing";

describe("isTyping", () => {
  it("recognises form fields and contenteditable editor surfaces", () => {
    expect(isTyping(document.createElement("input"))).toBe(true);
    expect(isTyping(document.createElement("textarea"))).toBe(true);
    const editor = document.createElement("div");
    Object.defineProperty(editor, "isContentEditable", { value: true });
    expect(isTyping(editor)).toBe(true);
  });

  it("does not claim ordinary pane content", () => {
    expect(isTyping(document.createElement("div"))).toBe(false);
    expect(isTyping(window)).toBe(false);
    expect(isTyping(null)).toBe(false);
  });
});
