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

function key(k: string, opts: KeyboardEventInit = {}) {
  act(() => window.dispatchEvent(new KeyboardEvent("keydown", { key: k, bubbles: true, ...opts })));
}

function imgTransform() {
  return document.querySelector<HTMLImageElement>(".image-viewer-canvas img")?.style.transform ?? "";
}

describe("ImageViewer", () => {
  it("opens at the selected image, supports left/right navigation, and closes with Escape", () => {
    expect(document.querySelector('[role="dialog"] img')?.getAttribute("src")).toBe(images[0].url);

    key("ArrowRight");
    expect(document.querySelector('[role="dialog"] img')?.getAttribute("src")).toBe(images[1].url);

    key("ArrowLeft");
    expect(document.querySelector('[role="dialog"] img')?.getAttribute("src")).toBe(images[0].url);

    key("Escape");
    expect(document.querySelector('[role="dialog"]')).toBeNull();
  });

  it("zooms in with Ctrl++ and out with Ctrl+-", () => {
    expect(imgTransform()).toContain("scale(1)");

    key("+", { ctrlKey: true });
    expect(imgTransform()).not.toContain("scale(1)");
    const afterZoomIn = imgTransform();
    expect(afterZoomIn).toMatch(/scale\(1\.\d+\)/);

    key("-", { ctrlKey: true });
    expect(imgTransform()).toContain("scale(1)");
  });

  it("resets zoom with Ctrl+0", () => {
    key("+", { ctrlKey: true });
    key("+", { ctrlKey: true });
    expect(imgTransform()).not.toContain("scale(1)");

    key("0", { ctrlKey: true });
    expect(imgTransform()).toContain("scale(1)");
  });

  it("resets zoom when switching images", () => {
    key("+", { ctrlKey: true });
    expect(imgTransform()).not.toContain("scale(1)");

    key("ArrowRight");
    expect(imgTransform()).toContain("scale(1)");
  });

  it("shows zoom percentage in controls", () => {
    const level = document.querySelector(".image-viewer-zoom-level");
    expect(level?.textContent).toBe("100%");

    key("+", { ctrlKey: true });
    const after = document.querySelector(".image-viewer-zoom-level");
    expect(after?.textContent).toBe("120%");
  });

  it("disables fit button when already at default view", () => {
    const fitBtn = document.querySelector<HTMLButtonElement>('.image-viewer-btn[aria-label="Fit to screen"]');
    expect(fitBtn?.disabled).toBe(true);

    key("+", { ctrlKey: true });
    expect(fitBtn?.disabled).toBe(false);
  });
});
