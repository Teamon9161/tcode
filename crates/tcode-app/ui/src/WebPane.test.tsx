import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

/**
 * The browser pane's address bar, at the boundary where it meets the backend.
 *
 * The pane draws only chrome; the page is a native child webview owned by the
 * backend (`src/browser.rs`). What is testable here without a webview is the
 * contract between the two: what the address bar sends when Enter is pressed,
 * and how navigation events update it. The second half is where the pane's
 * bug lived — every `BROWSER_NAVIGATED` event (each navigation, each title
 * change, and the initial slow startup of the WebView2 runtime) used to wipe
 * whatever was being typed, and the form then had nothing to send.
 */

const mocks = vi.hoisted(() => ({
  invoke: vi.fn(),
  off: vi.fn(),
  listener: null as null | ((event: { payload: { url: string; title: string } }) => void),
}));

vi.mock("@tauri-apps/api/core", () => ({ invoke: mocks.invoke }));
vi.mock("@tauri-apps/api/event", () => ({
  listen: (
    _name: string,
    callback: (event: { payload: { url: string; title: string } }) => void,
  ) => {
    mocks.listener = callback;
    return Promise.resolve(mocks.off);
  },
}));

import { WebPane } from "./WebPane";

class FakeResizeObserver {
  observe() {}
  unobserve() {}
  disconnect() {}
}
vi.stubGlobal("ResizeObserver", FakeResizeObserver);

// React's `act` warns without this flag, and the warning is noise here.
(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

let root: Root;
let container: HTMLDivElement;
let unmounted: boolean;

function render(
  onClose: () => void = () => {},
  hidden = false,
  request?: { url: string; at: number },
) {
  unmounted = false;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
  act(() => {
    root.render(
      <WebPane
        onClose={onClose}
        expanded={false}
        onToggleExpanded={() => {}}
        hidden={hidden}
        request={request}
      />,
    );
  });
}

/** Type into the controlled field the way a user's keystrokes arrive: the
 *  native value setter plus an `input` event, so React's `onChange` fires. */
function type(text: string) {
  const input = container!.querySelector("input")!;
  const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, "value")!.set!;
  act(() => {
    setter.call(input, text);
    input.dispatchEvent(new Event("input", { bubbles: true }));
  });
}

async function pressEnter() {
  const form = container!.querySelector("form")!;
  await act(async () => {
    form.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
    // The mocked `invoke` resolves in a microtask; its `.then` sets state, so
    // it has to land inside this `act` or React warns about an update outside
    // one.
    await Promise.resolve();
  });
}

/** A `BROWSER_NAVIGATED` event arriving from the backend. */
function navigate(url: string) {
  act(() => {
    mocks.listener?.({ payload: { url, title: "" } });
  });
}

beforeEach(() => {
  mocks.invoke.mockReset().mockResolvedValue(undefined);
  mocks.off.mockReset();
  mocks.listener = null;
  render();
});

afterEach(() => {
  if (!unmounted) {
    act(() => root.unmount());
  }
  container.remove();
});

describe("the browser pane's address bar", () => {
  it("sends what was typed when Enter is pressed", async () => {
    type("github.com");
    await pressEnter();
    expect(mocks.invoke).toHaveBeenCalledWith("browser_navigate", { url: "github.com" });
  });

  it("keeps the typed address when an unrelated navigation event arrives", async () => {
    // The WebView2 runtime starts up slowly and emits its initial about:blank
    // navigation (and later title changes) after the pane is already usable.
    // Wiping the field then would make the next Enter send nothing.
    type("github.com");
    navigate("about:blank");
    expect(container!.querySelector("input")!.value).toBe("github.com");
    await pressEnter();
    expect(mocks.invoke).toHaveBeenCalledWith("browser_navigate", { url: "github.com" });
  });

  it("shows the canonical URL once its own navigation round-trips", async () => {
    type("github.com");
    await pressEnter();
    navigate("https://github.com");
    expect(container!.querySelector("input")!.value).toBe("https://github.com");
  });

  it("does not wipe a newer address while the old navigation is still in flight", async () => {
    type("github.com");
    await pressEnter();
    type("docs.rs");
    navigate("https://github.com");
    expect(container!.querySelector("input")!.value).toBe("docs.rs");
  });
});

describe("the browser pane's hide vs close", () => {
  it("hides the webview instead of destroying it when the pane goes away", () => {
    // The button that opened the browser takes it off screen without losing
    // the page: re-opening later must show the page as it was. The webview
    // lives for the whole app session; only the app's own exit destroys it.
    act(() => {
      root.unmount();
    });
    unmounted = true;
    expect(mocks.invoke).toHaveBeenCalledWith("browser_visible", { visible: false });
    expect(mocks.invoke).not.toHaveBeenCalledWith("browser_close", expect.anything());
  });

  it("hides the webview while another pane fills the field", () => {
    // `visibility: hidden` on the slot does not reach the native webview, so
    // the pane has to tell the backend separately — including the mount-while-
    // -hidden case, where `browser_open` would otherwise show it.
    act(() => {
      root.unmount();
    });
    render(() => {}, true);
    expect(mocks.invoke).toHaveBeenCalledWith("browser_visible", { visible: false });
  });

  it("closes a blank browser directly without asking", () => {
    const onClose = vi.fn();
    act(() => {
      root.unmount();
    });
    render(onClose);

    // Freshly opened, the browser is at its blank start: there is no page a
    // "hide" would preserve, so the X closes the pane outright — no menu, no
    // navigation.
    const closeButton = [...container!.querySelectorAll("button")].find(
      (button) => button.getAttribute("aria-label") === "Close the browser",
    )!;
    act(() => {
      closeButton.click();
    });

    expect(onClose).toHaveBeenCalledTimes(1);
    expect(mocks.invoke).not.toHaveBeenCalledWith("browser_navigate", expect.anything());
    expect(document.querySelector(".web-close-menu")).toBeNull();
  });

  it("asks before closing once a page is open", () => {
    const onClose = vi.fn();
    act(() => {
      root.unmount();
    });
    render(onClose);
    navigate("https://github.com");

    const closeButton = [...container!.querySelectorAll("button")].find(
      (button) => button.getAttribute("aria-label") === "Close the browser",
    )!;
    act(() => {
      closeButton.click();
    });
    // The X itself only asks; nothing is navigated or closed by opening the
    // menu.
    expect(document.querySelector(".web-close-menu")).not.toBeNull();
    expect(mocks.invoke).not.toHaveBeenCalledWith("browser_navigate", expect.anything());
    expect(onClose).not.toHaveBeenCalled();

    const exit = [...document.querySelectorAll("button")].find(
      (button) => button.textContent?.includes("Exit browser"),
    )!;
    act(() => {
      exit.click();
    });

    // Closing the tab sends the page back to the blank start and closes the
    // pane, but the webview and its profile stay alive — that is what keeps
    // cookies and logins across an exit, and why a reopen cannot freeze.
    expect(mocks.invoke).toHaveBeenCalledWith("browser_navigate", { url: "about:blank" });
    expect(mocks.invoke).not.toHaveBeenCalledWith("browser_close");
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("hides the pane without destroying the webview when Hide for now is chosen", () => {
    const onClose = vi.fn();
    act(() => {
      root.unmount();
    });
    render(onClose);
    navigate("https://github.com");

    act(() => {
      const closeButton = [...container!.querySelectorAll("button")].find(
        (button) => button.getAttribute("aria-label") === "Close the browser",
      )!;
      closeButton.click();
    });
    const hide = [...document.querySelectorAll("button")].find(
      (button) => button.textContent?.includes("Hide for now"),
    )!;
    act(() => {
      hide.click();
    });

    expect(onClose).toHaveBeenCalledTimes(1);
    expect(mocks.invoke).not.toHaveBeenCalledWith("browser_navigate", expect.anything());
    expect(mocks.invoke).not.toHaveBeenCalledWith("browser_close");
  });
});

describe("following a link into the browser", () => {
  /** The pane's rectangle is measured a frame after each render, and the
   *  webview is created from that measurement — so "a frame later" is when the
   *  browser starts existing. Holding the frames here is what lets the test see
   *  the gap a link click falls into. */
  function heldFrames() {
    const frames: FrameRequestCallback[] = [];
    const real = globalThis.requestAnimationFrame;
    globalThis.requestAnimationFrame = ((fn: FrameRequestCallback) =>
      frames.push(fn)) as typeof requestAnimationFrame;
    return {
      run: async () => {
        await act(async () => {
          for (const frame of frames.splice(0)) frame(0);
          // `browser_open` resolves in a microtask, and only then is there a
          // webview to navigate.
          await Promise.resolve();
        });
      },
      restore: () => {
        globalThis.requestAnimationFrame = real;
      },
    };
  }

  it("waits for the webview to exist rather than failing into an error banner", async () => {
    const frames = heldFrames();
    try {
      act(() => root.unmount());
      render(() => {}, false, { url: "https://example.com/report", at: 1 });

      // The mount that a link *causes*: the webview is still being created, and
      // navigating now would answer the click with "the browser is not open".
      expect(mocks.invoke).not.toHaveBeenCalledWith("browser_navigate", expect.anything());

      await frames.run();
      expect(mocks.invoke).toHaveBeenCalledWith("browser_navigate", {
        url: "https://example.com/report",
      });
    } finally {
      frames.restore();
    }
  });

  it("visits once per request, however often the pane re-renders", async () => {
    const frames = heldFrames();
    try {
      act(() => root.unmount());
      const request = { url: "https://example.com/report", at: 1 };
      render(() => {}, false, request);
      await frames.run();

      // A title change from the page, a divider moving — any of these re-render
      // the pane, and none of them are a second click.
      navigate("https://example.com/report");
      await frames.run();

      const visits = mocks.invoke.mock.calls.filter(([name]) => name === "browser_navigate");
      expect(visits).toHaveLength(1);
    } finally {
      frames.restore();
    }
  });
});
