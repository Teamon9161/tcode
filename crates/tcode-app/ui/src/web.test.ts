import { describe, expect, it } from "vitest";

import {
  addTab,
  blankTab,
  closeTab,
  draftTab,
  isBlank,
  navigatedTab,
  newTab,
  NO_TABS,
  selectTab,
  sendingTab,
  tabLabel,
  type Tabs,
} from "./web";

const three = (): Tabs => addTab(addTab(addTab(NO_TABS, newTab("a")), newTab("b")), newTab("c"));

/** Type an address and press Enter, as the pane does it. */
const send = (tabs: Tabs, id: string, where: string) =>
  sendingTab(draftTab(tabs, id, where), id, where);

describe("the browser's tabs", () => {
  it("gives a new tab the focus, because you asked for it", () => {
    const tabs = three();
    expect(tabs.list.map((each) => each.id)).toEqual(["a", "b", "c"]);
    expect(tabs.current).toBe("c");
  });

  it("selects the tab that slid into the closed one's place", () => {
    expect(closeTab(selectTab(three(), "b"), "b").current).toBe("c");
  });

  it("blanks a tab in place, for the last webview that is never destroyed", () => {
    const one = navigatedTab(addTab(NO_TABS, newTab("a")), "a", "https://example.com", "Example");
    const blanked = blankTab(one, "a");
    expect(blanked.list).toHaveLength(1);
    expect(blanked.list[0]).toEqual(newTab("a"));
    expect(tabLabel(blanked.list[0])).toBe("New tab");
  });
});

/**
 * The bug the browser pane was rebuilt around: every navigation event — each
 * redirect, each title change, and the WebView2 runtime's slow initial
 * `about:blank` — used to wipe whatever was being typed, so the next Enter had
 * nothing to send.
 */
describe("the address bar against the page's own navigations", () => {
  it("keeps a typed address when an unrelated navigation arrives", () => {
    const tabs = navigatedTab(draftTab(three(), "c", "github.com"), "c", "about:blank", "");
    expect(tabs.list[2].draft).toBe("github.com");
  });

  it("drops the draft once its own navigation round-trips", () => {
    const tabs = navigatedTab(send(three(), "c", "github.com"), "c", "https://github.com", "");
    expect(tabs.list[2].draft).toBeNull();
    expect(tabs.list[2].url).toBe("https://github.com");
  });

  it("keeps a newer address typed while the old navigation is in flight", () => {
    const typing = draftTab(send(three(), "c", "github.com"), "c", "docs.rs");
    const tabs = navigatedTab(typing, "c", "https://github.com", "");
    expect(tabs.list[2].draft).toBe("docs.rs");
  });

  it("moves only the tab that navigated", () => {
    const tabs = navigatedTab(three(), "a", "https://example.com", "Example");
    expect(tabs.list[0].url).toBe("https://example.com");
    expect(tabs.list[2].url).toBe("");
  });
});

describe("what a tab is called", () => {
  it("prefers the document's own title", () => {
    const tabs = navigatedTab(three(), "a", "https://docs.rs/tauri", "tauri - Rust");
    expect(tabLabel(tabs.list[0])).toBe("tauri - Rust");
  });

  it("falls back to the address without its scheme", () => {
    const tabs = navigatedTab(three(), "a", "https://docs.rs/tauri", "");
    expect(tabLabel(tabs.list[0])).toBe("docs.rs/tauri");
  });

  it("never shows about:blank as a name", () => {
    expect(tabLabel(navigatedTab(three(), "a", "about:blank", "").list[0])).toBe("New tab");
  });
});

describe("a blank tab", () => {
  it("is the one a link can take over rather than opening beside", () => {
    expect(isBlank(newTab("a"))).toBe(true);
    expect(isBlank(navigatedTab(three(), "a", "about:blank", "").list[0])).toBe(true);
  });

  it("stops being blank as soon as it holds a page or a half-typed address", () => {
    expect(isBlank(navigatedTab(three(), "a", "https://example.com", "").list[0])).toBe(false);
    // Typing counts: taking the tab over would delete what is in the field.
    expect(isBlank(draftTab(three(), "a", "githu").list[0])).toBe(false);
  });
});
