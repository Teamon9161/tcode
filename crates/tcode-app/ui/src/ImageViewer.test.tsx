import { act, useState } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { ImageViewer } from "./ImageViewer";

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

const images = [
  { url: "data:image/png;base64,first", label: "first image" },
  { url: "data:image/png;base64,second", label: "second image" },
];

let root: Root;
let container: HTMLDivElement;

function Harness() {
  const [index, setIndex] = useState<number | null>(0);
  return <ImageViewer images={images} index={index} onIndex={setIndex} onClose={() => setIndex(null)} />;
}

beforeEach(() => {
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
  act(() => root.render(<Harness />));
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

describe("ImageViewer", () => {
  it("opens at the selected image, supports left/right navigation, and closes with Escape", () => {
    expect(document.querySelector('[role="dialog"] img')?.getAttribute("src")).toBe(images[0].url);

    act(() => window.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowRight", bubbles: true })));
    expect(document.querySelector('[role="dialog"] img')?.getAttribute("src")).toBe(images[1].url);

    act(() => window.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowLeft", bubbles: true })));
    expect(document.querySelector('[role="dialog"] img')?.getAttribute("src")).toBe(images[0].url);

    act(() => window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true })));
    expect(document.querySelector('[role="dialog"]')).toBeNull();
  });
});
