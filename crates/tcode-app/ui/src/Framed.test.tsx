import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: mocks.invoke }));

import { FIT_ASPECT, FIT_VIEWPORT, Framed, fit } from "./Framed";

/**
 * The inline report's viewport — the band a `.html` file gets at a call site.
 *
 * Worth pinning because the geometry lives in two places that must agree: this
 * file computes the band and the frame's logical size, and the stylesheet
 * positions the frame inside it. The failure it guards against is not a crash
 * — it is a report quietly missing its bottom strip, or a gap of empty page
 * under one, which looks like the report's own problem.
 */

// jsdom lays nothing out and has no observer for it either. The band's own
// arithmetic is covered by `fit` below; what the component owes is that it
// wires one up at all.
class FakeResizeObserver {
  observe() {}
  unobserve() {}
  disconnect() {}
}
vi.stubGlobal("ResizeObserver", FakeResizeObserver);

// React's `act` warns without this flag, and the warning is noise here.
(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

describe("fit() sizes an inline report against its column", () => {
  it("scales a page laid out at the viewport width down to the column", () => {
    const { logical, scale, band } = fit(700);
    expect(logical).toBe(FIT_VIEWPORT);
    expect(scale).toBeCloseTo(0.7);
    // What the wrapper reserves is what the scaled frame paints.
    expect(band).toBe(Math.round(logical * FIT_ASPECT * scale));
  });

  it("never magnifies: a wide column gets more page, not a bigger one", () => {
    const { logical, scale } = fit(1400);
    expect(scale).toBe(1);
    expect(logical).toBe(1400);
  });

  it("survives being asked before anything has been measured", () => {
    // Width 0 is the single frame between mount and the layout effect. It must
    // produce a sane box rather than a scale of 0 or a NaN height.
    const { scale, band } = fit(0);
    expect(scale).toBe(1);
    expect(band).toBeGreaterThan(0);
  });
});

let root: Root;
let container: HTMLDivElement;

beforeEach(() => {
  mocks.invoke.mockReset();
  mocks.invoke.mockResolvedValue("http://127.0.0.1:9/t/report.html");
  container = document.createElement("div");
  document.body.append(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

const draw = async (inline: boolean) => {
  await act(async () => {
    root.render(<Framed path="/p/report.html" label="report.html" inline={inline} />);
  });
};

describe("Framed", () => {
  it("puts an inline report in a fitted band, scaled and clipped to the column", async () => {
    await draw(true);
    const box = container.querySelector<HTMLDivElement>(".framed-fit");
    const frame = container.querySelector<HTMLIFrameElement>("iframe");
    expect(box).not.toBeNull();
    // jsdom lays nothing out, so the measurement is 0 and this is the unmeasured
    // case — the point here is that both halves exist and are driven by `fit`.
    expect(box?.style.height).toBe(`${fit(0).band}px`);
    expect(frame?.style.width).toBe(`${FIT_VIEWPORT}px`);
    expect(frame?.style.transform).toBe(`scale(${fit(0).scale})`);
  });

  it("gives the pane the frame itself, at 1:1 and with no wrapper", async () => {
    await draw(false);
    expect(container.querySelector(".framed-fit")).toBeNull();
    const frame = container.querySelector<HTMLIFrameElement>("iframe");
    expect(frame?.style.transform).toBe("");
    expect(frame?.className).toBe("framed");
  });

  it("holds the band open while the URL is still being resolved", async () => {
    // The wrapper is what gets measured, so it cannot wait for the file: a band
    // that appears late shoves the conversation under it down the screen.
    mocks.invoke.mockReturnValue(new Promise(() => {}));
    await draw(true);
    expect(container.querySelector(".framed-fit")).not.toBeNull();
    expect(container.querySelector("iframe")).toBeNull();
  });
});
