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
  /**
   * Subscribers by event name.
   *
   * A single `listener` slot until the agent browser arrived, and that was
   * fine while the store subscribed to one event. It stopped being fine the
   * moment it subscribed to two: the slot held whichever registered last, so
   * every navigation in this file was delivered to the wrong handler — and the
   * failure looked like the address bar being broken, not like the fixture
   * being wrong. Keyed by name, a test says which event it is sending.
   */
  listeners: new Map<string, (event: { payload: Record<string, unknown> }) => void>(),
}));

// One module now, so one double: a second `vi.mock` of the same specifier
// silently replaces the first, which would leave `invoke` undefined here.
vi.mock("@ipc", () => ({
  invoke: mocks.invoke,
  listen: (name: string, callback: (event: { payload: Record<string, unknown> }) => void) => {
    mocks.listeners.set(name, callback);
    return Promise.resolve(mocks.off);
  },
}));

import { WebPane } from "./WebPane";
import * as browser from "./webHost";
import { BROWSER_NAVIGATED, BROWSER_TAB_OPENED } from "./types";

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
/** Tabs the pane asked to hand to a conversation. The pane has no session of
 *  its own, so all it can do is name the tab and let the window place it. */
let handedOver: string[];

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
        nameOf={(session) => `conversation ${session}`}
        onHandOver={(tab) => handedOver.push(tab)}
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

/** One event arriving from the backend, by name. Throws on an unsubscribed
 *  name rather than doing nothing: a test that fires into the void passes. */
function deliver(name: string, payload: Record<string, unknown>) {
  const listener = mocks.listeners.get(name);
  if (!listener) throw new Error(`nothing is listening for ${name}`);
  act(() => listener({ payload }));
}

/** A `BROWSER_NAVIGATED` event arriving from the backend. */
function navigate(url: string, id = openTabs[openTabs.length - 1]) {
  deliver(BROWSER_NAVIGATED, { id, url, title: "" });
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
  handedOver = [];
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
  mocks.listeners.clear();
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
    // `select: true` is part of the call, not decoration. The shell reads it
    // strictly, and the other caller — the backend, opening a tab for a model
    // — omits it so that tab never takes the screen. A `+` that stopped
    // sending it would open tabs nobody could see.
    expect(mocks.invoke).toHaveBeenCalledWith("browser_open", {
      rect: expect.anything(),
      select: true,
    });
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

describe("a tab the strip did not open", () => {
  /**
   * The agent browser's half of the strip (`../AGENT-BROWSER.md`).
   *
   * A model asks the backend for a tab; the backend asks the shell; the shell
   * announces it. Before this, `browser_open`'s return value was the only way
   * a tab could come into existence, so a tab opened anywhere else was a page
   * on screen that the strip did not know about — which is exactly why a
   * page's own `window.open` had to be denied and loaded in the same tab.
   */
  it("appears in the strip", () => {
    deliver(BROWSER_TAB_OPENED, { id: "agent-tab", url: "about:blank", agent: true });
    expect(container.querySelectorAll(".tab")).toHaveLength(2);
  });

  /**
   * And it says so, from the first frame.
   *
   * A page is about to start loading in this tab without anybody in the window
   * having asked for one. A strip that drew it like every other tab would be
   * answering "did I open this?" with a shrug — so the mark rides on the birth
   * event rather than waiting for the ownership that is worked out later.
   */
  it("is marked as an agent's before anyone knows whose it is", () => {
    deliver(BROWSER_TAB_OPENED, { id: "agent-tab", url: "about:blank", agent: true });
    const dot = container.querySelector(".tab-agent");
    expect(dot).not.toBeNull();
    expect(dot!.getAttribute("aria-label")).toBe("Opened by a conversation, not by you");
    // The user's own tab carries nothing extra.
    expect(container.querySelectorAll(".tab-agent")).toHaveLength(1);
  });

  /**
   * Whose it is arrives afterwards, off the session-tagged event stream.
   *
   * There is nothing to announce it: the tool is a singleton and `ToolCtx`
   * carries no session id, on purpose. What identifies the conversation is that
   * its own `browser` call names the tab — structured arguments the model sent,
   * not the sentence the tool answered with.
   */
  it("takes the name of the conversation whose call names it", () => {
    deliver(BROWSER_TAB_OPENED, { id: "agent-tab", url: "about:blank", agent: true });
    act(() =>
      browser.claim("s-7", {
        type: "ToolStart",
        data: {
          call_id: "c1",
          name: "browser",
          summary: "",
          input: { action: "snapshot", tab: "agent-tab" },
        },
      }),
    );
    expect(container.querySelector(".tab-agent")!.getAttribute("aria-label")).toBe(
      'Opened by "conversation s-7", not by you',
    );
  });

  /**
   * A conversation ending does not close its pages.
   *
   * Same sentence the whole pane is built on — hiding is not closing,
   * `closeSession` leaves the browser pane alone — applied to the one thing
   * Phase 2 added. The page stays logged in and where it was; it just stops
   * being attributed to a conversation that no longer exists.
   */
  it("keeps the page and loses the owner when the conversation is closed", () => {
    deliver(BROWSER_TAB_OPENED, { id: "agent-tab", url: "about:blank", agent: true });
    act(() =>
      browser.claim("s-7", {
        type: "ToolStart",
        data: {
          call_id: "c1",
          name: "browser",
          summary: "",
          input: { action: "snapshot", tab: "agent-tab" },
        },
      }),
    );
    navigate("https://example.com/report", "agent-tab");

    act(() => browser.disown("s-7"));

    expect(container.querySelectorAll(".tab")).toHaveLength(2);
    // Still an agent's tab — where it came from does not stop being true — and
    // no longer anybody's.
    expect(container.querySelector(".tab-agent")!.getAttribute("aria-label")).toBe(
      "Opened by a conversation, not by you",
    );
    expect(mocks.invoke).not.toHaveBeenCalledWith("browser_close", { id: "agent-tab" });
  });

  /** A call from a different tool, or one that names no tab, says nothing about
   *  who owns anything. */
  it("is not claimed by a call that does not name it", () => {
    deliver(BROWSER_TAB_OPENED, { id: "agent-tab", url: "about:blank", agent: true });
    act(() => {
      browser.claim("s-7", {
        type: "ToolStart",
        data: { call_id: "c1", name: "browser", summary: "", input: { action: "open" } },
      });
      browser.claim("s-9", {
        type: "ToolStart",
        data: { call_id: "c2", name: "read", summary: "", input: { tab: "agent-tab" } },
      });
    });
    expect(container.querySelector(".tab-agent")!.getAttribute("aria-label")).toBe(
      "Opened by a conversation, not by you",
    );
  });

  /** And it must not take the screen. Whoever is reading the current tab did
   *  not ask for this one; the whole point of driving the window's own browser
   *  rather than a headless one is that watching it is optional, not forced. */
  it("does not become the current tab", () => {
    deliver(BROWSER_TAB_OPENED, { id: "agent-tab", url: "about:blank", agent: true });
    const current = container.querySelectorAll(".tab.is-current");
    expect(current).toHaveLength(1);
    expect(mocks.invoke).not.toHaveBeenCalledWith("browser_select", { id: "agent-tab" });
  });

  /** The id also comes back from `browser_open`, so both arrive for a tab the
   *  strip *did* open. Whichever is second must change nothing — the ordering
   *  of two IPC messages out of one call is not something to depend on. */
  it("is not added twice when it is also the strip's own tab", () => {
    deliver(BROWSER_TAB_OPENED, { id: "tab-1", url: "about:blank", agent: false });
    expect(container.querySelectorAll(".tab")).toHaveLength(1);
    expect(container.querySelectorAll(".tab.is-current")).toHaveLength(1);
    // And it is still the user's: the row was already there, so the second
    // arrival changes nothing at all.
    expect(container.querySelectorAll(".tab-agent")).toHaveLength(0);
  });
});

describe("handing a page to a conversation", () => {
  /**
   * The only way a model learns about a tab it did not open.
   *
   * There is deliberately no action that lists tabs: an agent able to enumerate
   * them would have the user's browsing in its context, which is a leak wearing
   * a capability's clothes (`../AGENT-BROWSER.md`). The way across is the user
   * pointing at one page.
   */
  it("names the tab on screen and lets the window place it", async () => {
    navigate("https://github.com/rust-lang/rust/pull/1");
    await click("Mention this page in the message");
    expect(handedOver).toEqual(["tab-1"]);
  });

  /** What lands in the composer is an id and an address. The page's *title* is
   *  left out on purpose: that is prose a website wrote, and a user's own turn
   *  is not where it belongs. */
  it("carries the address and nothing the page wrote", () => {
    navigate("https://github.com/rust-lang/rust/pull/1");
    expect(browser.handOverText("tab-1")).toBe(
      "browser tab tab-1 (https://github.com/rust-lang/rust/pull/1)",
    );
    expect(browser.handOverText("no-such-tab")).toBeNull();
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
          nameOf={(session) => `conversation ${session}`}
          onHandOver={(tab) => handedOver.push(tab)}
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
