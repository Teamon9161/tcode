import { describe, expect, it, vi } from "vitest";

import { offsetAtPoint, offsetOfTextNode } from "./previewOffset";

describe("offsetOfTextNode", () => {
  it("sums the text before the target node and adds the position within it", () => {
    const root = document.createElement("div");
    root.innerHTML =
      "<p>alpha</p><p><strong>beta</strong> <del>gamma</del></p><p>delta</p>";

    const text = [...root.querySelectorAll("del")][0].firstChild as Text;
    expect(offsetOfTextNode(root, text, 2)).toBe("alpha".length + "beta".length + 1 + 2);
  });

  it("answers null for a node outside the walked root", () => {
    const root = document.createElement("div");
    root.textContent = "inside";
    const outside = document.createElement("p");
    outside.textContent = "outside";

    expect(offsetOfTextNode(root, outside.firstChild as Text, 0)).toBeNull();
  });
});

describe("offsetAtPoint", () => {
  /** jsdom's Document has no `caretRangeFromPoint`; the engine's answer is
   *  defined and stubbed per test. The stub is the two fields the helper
   *  reads, not a real Range. */
  function stubCaret(range: { startContainer: Node; startOffset: number } | null) {
    Object.defineProperty(document, "caretRangeFromPoint", {
      configurable: true,
      value: vi.fn(() => range),
    });
  }

  it("turns a caret position over the preview into a source offset", () => {
    const root = document.createElement("div");
    root.innerHTML = "<p>hello</p><p>world</p>";
    const world = root.querySelectorAll("p")[1].firstChild as Text;
    stubCaret({ startContainer: world, startOffset: 3 });

    expect(offsetAtPoint(root, 10, 10)).toBe("hello".length + 3);
  });

  it("returns null when the engine has no caret answer or it is not text", () => {
    const root = document.createElement("div");
    root.textContent = "any";

    stubCaret(null);
    expect(offsetAtPoint(root, 0, 0)).toBeNull();

    stubCaret({ startContainer: root, startOffset: 0 });
    expect(offsetAtPoint(root, 0, 0)).toBeNull();
  });
});
