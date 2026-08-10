import { beforeEach, describe, expect, it, vi } from "vitest";

const invoke = vi.hoisted(() => vi.fn().mockResolvedValue(undefined));
vi.mock("@ipc", () => ({ invoke }));

import {
  resetBrowserVisibility,
  setBrowserShown,
  yieldBrowser,
} from "./browserYield";

beforeEach(() => {
  invoke.mockClear();
  resetBrowserVisibility();
});

describe("native browser visibility", () => {
  it("does not show an agent-opened tab before a browser pane asks for it", () => {
    const release = yieldBrowser();
    release();

    expect(invoke).not.toHaveBeenCalledWith("browser_visible", { visible: true });
  });

  it("does not reveal a browser pane that was already hidden when a drag ends", () => {
    setBrowserShown(false);
    invoke.mockClear();

    const release = yieldBrowser();
    release();

    expect(invoke).toHaveBeenCalledTimes(2);
    expect(invoke).not.toHaveBeenCalledWith("browser_visible", { visible: true });
    expect(invoke).toHaveBeenLastCalledWith("browser_visible", { visible: false });
  });

  it("restores a visible pane only after the last overlapping yield ends", () => {
    setBrowserShown(true);
    invoke.mockClear();

    const releaseDrag = yieldBrowser();
    const releasePopover = yieldBrowser();
    releaseDrag();

    expect(invoke).toHaveBeenCalledTimes(1);
    expect(invoke).toHaveBeenLastCalledWith("browser_visible", { visible: false });

    releasePopover();
    expect(invoke).toHaveBeenLastCalledWith("browser_visible", { visible: true });
  });
});
