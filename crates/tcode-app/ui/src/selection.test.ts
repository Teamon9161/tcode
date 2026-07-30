import { describe, expect, it } from "vitest";

import { normalizeQuote, selectedText } from "./selection";

describe("what the user selected", () => {
  it("reads a text field's own selection, which never reaches the document one", () => {
    const field = { value: "risk: rewind crosses the Summary boundary", selectionStart: 6, selectionEnd: 21 };
    expect(selectedText(field, "something else entirely")).toBe("rewind crosses");
  });

  it("is empty for a caret, so a bare click offers to comment on nothing", () => {
    expect(selectedText({ value: "abc", selectionStart: 2, selectionEnd: 2 }, "")).toBe("");
    expect(selectedText({ value: "abc", selectionStart: null, selectionEnd: null }, "")).toBe("");
  });

  it("ignores the document selection when the event came from a field", () => {
    // The document's selection belongs to whatever the pointer left behind
    // elsewhere; quoting it here would anchor the comment to the wrong text.
    const field = { value: "abc", selectionStart: 0, selectionEnd: 0 };
    expect(selectedText(field, "text from another phase")).toBe("");
  });

  it("falls back to the document selection outside a field", () => {
    expect(selectedText(null, "  a rendered passage  ")).toBe("a rendered passage");
  });
});

describe("the quote that travels to the model", () => {
  it("trims without reflowing: the words are the anchor", () => {
    expect(normalizeQuote("\n  two   spaces kept\n")).toBe("two   spaces kept");
  });

  it("cuts a selection that has stopped being an anchor", () => {
    const long = "x".repeat(900);
    const quote = normalizeQuote(long);
    expect(quote.endsWith("…")).toBe(true);
    expect(quote.length).toBeLessThan(long.length);
  });
});
