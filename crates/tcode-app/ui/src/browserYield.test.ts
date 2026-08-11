import { beforeEach, describe, expect, it, vi } from "vitest";

const invoke = vi.hoisted(() => vi.fn().mockResolvedValue(undefined));
vi.mock("@ipc", () => ({ invoke }));

import {
  registerBrowserPlacement,
  resetBrowserVisibility,
  setBrowserShown,
  yieldBrowser,
} from "./browserYield";

const frames: FrameRequestCallback[] = [];
vi.stubGlobal("requestAnimationFrame", (frame: FrameRequestCallback) => {
  frames.push(frame);
  return frames.length;
});
vi.stubGlobal("cancelAnimationFrame", () => {});

async function flushFrames() {
  for (const frame of frames.splice(0)) frame(0);
  for (let tick = 0; tick < 4; tick += 1) await Promise.resolve();
}

beforeEach(() => {
  invoke.mockClear();
  frames.length = 0;
  resetBrowserVisibility();
});

describe("native browser visibility", () => {
  it("does not show an agent-opened tab before a browser pane asks for it", async () => {
    const release = yieldBrowser();
    release();
    await flushFrames();

    expect(invoke).not.toHaveBeenCalledWith("browser_visible", { visible: true });
  });

  it("does not reveal a browser pane that was already hidden when a drag ends", async () => {
    setBrowserShown(false);
    invoke.mockClear();

    const release = yieldBrowser();
    release();
    await flushFrames();

    expect(invoke).toHaveBeenCalledTimes(1);
    expect(invoke).not.toHaveBeenCalledWith("browser_visible", { visible: true });
    expect(invoke).toHaveBeenLastCalledWith("browser_visible", { visible: false });
  });

  it("restores a visible pane only after the last overlapping yield ends", async () => {
    setBrowserShown(true);
    invoke.mockClear();

    const releaseDrag = yieldBrowser();
    const releasePopover = yieldBrowser();
    releaseDrag();

    expect(invoke).toHaveBeenCalledTimes(1);
    expect(invoke).toHaveBeenLastCalledWith("browser_visible", { visible: false });

    releasePopover();
    expect(invoke).not.toHaveBeenLastCalledWith("browser_visible", { visible: true });
    await flushFrames();
    expect(invoke).toHaveBeenLastCalledWith("browser_visible", { visible: true });
  });

  it("places the final bounds before restoring a yielded browser", async () => {
    setBrowserShown(true);
    invoke.mockClear();
    registerBrowserPlacement(() => invoke("browser_bounds", { rect: "final" }));

    const release = yieldBrowser();
    release();
    await flushFrames();

    expect(invoke.mock.calls).toEqual([
      ["browser_visible", { visible: false }],
      ["browser_bounds", { rect: "final" }],
      ["browser_visible", { visible: true }],
    ]);
  });
});
