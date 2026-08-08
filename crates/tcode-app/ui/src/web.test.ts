import { describe, expect, it } from "vitest";

import {
  addTab,
  blankTab,
  closeTab,
  disownedTabs,
  draftTab,
  isBlank,
  navigatedTab,
  newTab,
  NO_TABS,
  ownedTab,
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

describe("a tab an agent opened", () => {
  const agentTab = (): Tabs => addTab(NO_TABS, newTab("x", true));

  /** Two facts, and they are not the same fact. Whether the user opened it is
   *  known when the tab is born; which conversation is driving it is worked out
   *  afterwards, from the calls that name the tab. */
  it("starts marked as an agent's and unclaimed", () => {
    const tab = agentTab().list[0];
    expect(tab.agent).toBe(true);
    expect(tab.owner).toBeNull();
    expect(newTab("y").agent).toBe(false);
  });

  it("takes the first conversation that claims it, and keeps it", () => {
    const claimed = ownedTab(agentTab(), "x", "s-1");
    expect(claimed.list[0].owner).toBe("s-1");
    // A model can mention an id it read anywhere. Handing a tab from one
    // conversation to another is the user's act, not a tool call's, so a second
    // claim changes nothing rather than making the label flicker.
    expect(ownedTab(claimed, "x", "s-2").list[0].owner).toBe("s-1");
  });

  /** Closing a conversation must not take away a page somebody is reading, so
   *  the tab stays and only the attribution goes. Other conversations' tabs are
   *  not touched, which is the whole reason this matches on `owner`. */
  it("is disowned rather than closed when its conversation ends", () => {
    const two = ownedTab(
      ownedTab(addTab(agentTab(), newTab("y", true)), "x", "s-1"),
      "y",
      "s-2",
    );
    const left = disownedTabs(two, "s-1");
    expect(left.list.map((tab) => tab.owner)).toEqual([null, "s-2"]);
    // Where it came from does not stop being true.
    expect(left.list[0].agent).toBe(true);
    // And a conversation that owned nothing changes nothing at all — identity,
    // because `useSyncExternalStore` compares snapshots that way.
    expect(disownedTabs(left, "s-1")).toBe(left);
  });

  /** Emptying a tab is not the same event as somebody else opening it: the page
   *  goes, the tab's provenance does not. */
  it("keeps its provenance when the page is cleared", () => {
    const tab = blankTab(ownedTab(agentTab(), "x", "s-1"), "x").list[0];
    expect(tab.url).toBe("");
    expect(tab.agent).toBe(true);
    expect(tab.owner).toBe("s-1");
  });
});
