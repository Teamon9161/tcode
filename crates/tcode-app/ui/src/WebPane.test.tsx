import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

/**
 * The browser pane, at the boundary where it meets the backend.
 *
 * The pane draws only chrome; the pages are native child webviews the backend
 * owns (`src/browser.rs`), one per tab. What is testable here without a webview
 * is the contract between the two — which command each control sends, and how
 * a pane that comes and goes leaves the webviews alone. The tab *data* is
 * `web.test.ts`; this file is about the calls.
 */

const mocks = vi.hoisted(() => ({
  invoke: vi.fn(),
  off: vi.fn(),
  listener: null as null | ((event: { payload: { id: string; url: string; title: string } }) => void),
}));

// One module now, so one double: a second `vi.mock` of the same specifier
// silently replaces the first, which would leave `invoke` undefined here.
vi.mock("@ipc", () => ({
  invoke: mocks.invoke,
  listen: (
    _name: string,
    callback: (event: { payload: { id: string; url: string; title: string } }) => void,
  ) => {
    mocks.listener = callback;
    return Promise.resolve(mocks.off);
  },
}));

import { WebPane } from "./WebPane";
import * as browser from "./webHost";

class FakeResizeObserver {
  observe() {}
  unobserve() {}
  disconnect() {}
}
vi.stubGlobal("ResizeObserver", FakeResizeObserver);

// React's `act` warns without this flag, and the warning is noise here.
(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

/**
 * Animation frames, held.
 *
 * The pane measures its rectangle a frame after each render, and the first
 * webview is created from that measurement — so "a frame later" is when the
 * browser starts existing, and holding the frames is what lets a test see the
 * gap a link click falls into.
 */
const frames: FrameRequestCallback[] = [];
vi.stubGlobal("requestAnimationFrame", (fn: FrameRequestCallback) => frames.push(fn));
vi.stubGlobal("cancelAnimationFrame", () => {});

/** Run the pending frames and let the promises they start settle. */
async function settle() {
  await act(async () => {
    for (const frame of frames.splice(0)) frame(0);
    // `browser_open` resolves in a microtask, and only then is there a tab.
    await Promise.resolve();
    await Promise.resolve();
  });
}

let root: Root;
let container: HTMLDivElement;
let unmounted: boolean;
/** What the fake backend believes: one webview per tab, and the last one is
 *  blanked rather than destroyed (`browser.rs`). */
let openTabs: string[];

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
    await Promise.resolve();
  });
}

/** A `BROWSER_NAVIGATED` event arriving from the backend. */
function navigate(url: string, id = openTabs[openTabs.length - 1]) {
  act(() => {
    mocks.listener?.({ payload: { id, url, title: "" } });
  });
}

function button(label: string): HTMLButtonElement {
  const found = [...document.querySelectorAll("button")].find(
    (each) => each.getAttribute("aria-label") === label,
  );
  if (!found) throw new Error(`no control labelled "${label}"`);
  return found;
}

async function click(label: string) {
  await act(async () => {
    button(label).click();
    // Two: the command's promise, then the follow-up the store chains onto it
    // (selecting the neighbour of a closed tab).
    await Promise.resolve();
    await Promise.resolve();
  });
}

beforeEach(async () => {
  openTabs = [];
  mocks.invoke.mockReset().mockImplementation((name: string, args?: Record<string, unknown>) => {
    if (name === "browser_open") {
      const id = `tab-${openTabs.length + 1}`;
      openTabs.push(id);
      return Promise.resolve(id);
    }
    if (name === "browser_close") {
      // The last webview is never destroyed; it is blanked and kept.
      if (openTabs.length === 1) return Promise.resolve(false);
      openTabs = openTabs.filter((id) => id !== args?.id);
      return Promise.resolve(true);
    }
    return Promise.resolve(undefined);
  });
  mocks.off.mockReset();
  mocks.listener = null;
  frames.length = 0;
  browser.reset();
  render();
  await settle();
});

afterEach(() => {
  if (!unmounted) act(() => root.unmount());
  container.remove();
});

describe("the browser pane's first tab", () => {
  it("opens one when the pane appears, because an empty browser is not a state anybody asked for", () => {
    expect(mocks.invoke).toHaveBeenCalledWith("browser_open", { rect: expect.anything() });
    expect(container.querySelectorAll(".tab")).toHaveLength(1);
  });

  it("shows the tabs it already had rather than opening another", async () => {
    act(() => root.unmount());
    mocks.invoke.mockClear();
    render();
    await settle();

    expect(mocks.invoke).toHaveBeenCalledWith("browser_show", { rect: expect.anything() });
    expect(mocks.invoke).not.toHaveBeenCalledWith("browser_open", expect.anything());
    expect(container.querySelectorAll(".tab")).toHaveLength(1);
  });
});

describe("the browser pane's address bar", () => {
  it("sends what was typed when Enter is pressed", async () => {
    type("github.com");
    await pressEnter();
    expect(mocks.invoke).toHaveBeenCalledWith("browser_navigate", {
      id: "tab-1",
      url: "github.com",
    });
  });

  it("keeps the typed address when an unrelated navigation event arrives", async () => {
    // The WebView2 runtime starts up slowly and emits its initial about:blank
    // navigation (and later title changes) after the pane is already usable.
    // Wiping the field then would make the next Enter send nothing.
    type("github.com");
    navigate("about:blank");
    expect(container.querySelector("input")!.value).toBe("github.com");
    await pressEnter();
    expect(mocks.invoke).toHaveBeenCalledWith("browser_navigate", {
      id: "tab-1",
      url: "github.com",
    });
  });

  it("shows the canonical URL once its own navigation round-trips", async () => {
    type("github.com");
    await pressEnter();
    navigate("https://github.com");
    expect(container.querySelector("input")!.value).toBe("https://github.com");
  });

  it("leaves the address bar alone when a background tab navigates", async () => {
    type("github.com");
    await pressEnter();
    navigate("https://github.com");
    await click("New tab");

    navigate("https://redirected.example", "tab-1");
    expect(container.querySelector("input")!.value).toBe("");
  });
});

describe("the browser pane's tabs", () => {
  it("opens another one beside the first, and gives it the strip", async () => {
    await click("New tab");
    const tabs = [...container.querySelectorAll(".tab")];
    expect(tabs).toHaveLength(2);
    expect(tabs[1].classList.contains("is-current")).toBe(true);
    // Each tab is its own webview: two opens, never a re-navigation of one.
    expect(mocks.invoke.mock.calls.filter(([name]) => name === "browser_open")).toHaveLength(2);
  });

  it("brings a tab forward when it is clicked, because a hidden webview is not layered", async () => {
    await click("New tab");
    await act(async () => {
      container.querySelector<HTMLButtonElement>(".tab .tab-name")!.click();
      await Promise.resolve();
    });
    expect(mocks.invoke).toHaveBeenCalledWith("browser_select", { id: "tab-1" });
  });

  it("keeps each tab's half-typed address to itself", async () => {
    type("github.com");
    await click("New tab");
    expect(container.querySelector("input")!.value).toBe("");

    await act(async () => {
      container.querySelector<HTMLButtonElement>(".tab .tab-name")!.click();
      await Promise.resolve();
    });
    expect(container.querySelector("input")!.value).toBe("github.com");
  });

  it("selects the neighbour when the current tab is closed", async () => {
    navigate("https://github.com");
    await click("New tab");
    mocks.invoke.mockClear();
    // The second tab is still blank, so it is the one called "New tab" — the
    // first now carries its page's address.
    await click("Close New tab");
    expect(container.querySelectorAll(".tab")).toHaveLength(1);
    expect(mocks.invoke).toHaveBeenCalledWith("browser_select", { id: "tab-1" });
  });

  it("blanks the last tab rather than destroying the webview, and puts the pane away", async () => {
    const onClose = vi.fn();
    act(() => root.unmount());
    browser.reset();
    openTabs = [];
    render(onClose);
    await settle();
    navigate("https://github.com");

    await click("Close github.com");

    // The webview survives — it holds the profile every login lives in — so the
    // strip keeps one tab, back at its blank start, and the pane goes away.
    expect(onClose).toHaveBeenCalledTimes(1);
    expect(container.querySelectorAll(".tab")).toHaveLength(1);
    expect(container.querySelector(".tab-label")!.textContent).toBe("New tab");
  });
});

describe("the browser pane's hide vs close", () => {
  it("hides the webviews instead of destroying them when the pane goes away", () => {
    // The button that opened the browser takes it off screen without losing the
    // pages: re-opening later must show them as they were.
    act(() => root.unmount());
    unmounted = true;
    expect(mocks.invoke).toHaveBeenCalledWith("browser_visible", { visible: false });
    expect(mocks.invoke).not.toHaveBeenCalledWith("browser_close", expect.anything());
  });

  it("hides the webviews while another pane fills the field", async () => {
    // `visibility: hidden` on the slot does not reach a native webview, so the
    // pane has to tell the backend separately.
    act(() => root.unmount());
    render(() => {}, true);
    await settle();
    expect(mocks.invoke).toHaveBeenCalledWith("browser_visible", { visible: false });
  });
});

describe("following a link into the browser", () => {
  it("waits for the webview to exist rather than failing into an error banner", async () => {
    act(() => root.unmount());
    browser.reset();
    mocks.invoke.mockClear();
    openTabs = [];
    render(() => {}, false, { url: "https://example.com/report", at: 1 });

    // The mount that a link *causes*: the webview is still being created, and
    // navigating now would answer the click with "that browser tab is not open".
    expect(mocks.invoke).not.toHaveBeenCalledWith("browser_navigate", expect.anything());

    await settle();
    expect(mocks.invoke).toHaveBeenCalledWith("browser_navigate", {
      id: "tab-1",
      url: "https://example.com/report",
    });
  });

  it("visits once per request, however often the pane re-renders", async () => {
    act(() => root.unmount());
    browser.reset();
    mocks.invoke.mockClear();
    openTabs = [];
    render(() => {}, false, { url: "https://example.com/report", at: 1 });
    await settle();

    // A title change from the page, a divider moving — any of these re-render
    // the pane, and none of them are a second click.
    navigate("https://example.com/report");
    await settle();

    expect(mocks.invoke.mock.calls.filter(([name]) => name === "browser_navigate")).toHaveLength(1);
  });

  it("opens a new tab rather than taking over the page being read", async () => {
    navigate("https://github.com");
    await act(async () => {
      root.render(
        <WebPane
          onClose={() => {}}
          expanded={false}
          onToggleExpanded={() => {}}
          hidden={false}
          request={{ url: "https://example.com/report", at: 2 }}
        />,
      );
      await Promise.resolve();
      await Promise.resolve();
    });

    expect(container.querySelectorAll(".tab")).toHaveLength(2);
    expect(mocks.invoke).toHaveBeenCalledWith("browser_navigate", {
      id: "tab-2",
      url: "https://example.com/report",
    });
  });
});
