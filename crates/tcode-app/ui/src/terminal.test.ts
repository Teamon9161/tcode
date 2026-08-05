import { describe, expect, it } from "vitest";

import {
  addTab,
  closeTab,
  endTab,
  NO_TABS,
  renameTab,
  selectTab,
  stepTab,
  tabLabel,
  type Tab,
  type Tabs,
} from "./terminal";

const tab = (id: string, cwd = "/home/me/code/tcode"): Tab => ({
  id,
  title: "",
  cwd,
  exit: null,
});

const three = (): Tabs => addTab(addTab(addTab(NO_TABS, tab("a")), tab("b")), tab("c"));

describe("the terminal's tabs", () => {
  it("gives a new tab the focus, because you asked for it", () => {
    const tabs = three();
    expect(tabs.list.map((each) => each.id)).toEqual(["a", "b", "c"]);
    expect(tabs.current).toBe("c");
  });

  it("selects the tab that slid into the closed one's place", () => {
    const tabs = closeTab(selectTab(three(), "b"), "b");
    expect(tabs.current).toBe("c");
  });

  it("falls back to the previous tab at the end of the strip", () => {
    // Not the first one: the selection has to stay near where you were
    // looking, which is the thing every emulator gets wrong once.
    const tabs = closeTab(three(), "c");
    expect(tabs.current).toBe("b");
  });

  it("leaves the selection alone when some other tab closes", () => {
    const tabs = closeTab(selectTab(three(), "a"), "c");
    expect(tabs.current).toBe("a");
  });

  it("is empty once the last tab goes, which is what closes the pane", () => {
    expect(closeTab(addTab(NO_TABS, tab("only")), "only")).toEqual(NO_TABS);
  });

  it("ignores unknown ids rather than leaving nothing current", () => {
    const tabs = three();
    expect(selectTab(tabs, "gone").current).toBe("c");
    expect(closeTab(tabs, "gone")).toBe(tabs);
  });

  it("steps around the strip in both directions", () => {
    const tabs = three();
    expect(stepTab(tabs, 1).current).toBe("a");
    expect(stepTab(tabs, -1).current).toBe("b");
    expect(stepTab(addTab(NO_TABS, tab("only")), 1).current).toBe("only");
  });

  it("takes the shell's own title but never a blank one", () => {
    const named = renameTab(three(), "a", "me@box: ~/code/tcode");
    expect(tabLabel(named.list[0])).toBe("me@box: ~/code/tcode");
    // A program clearing the title is not asking for an unnamed tab.
    expect(renameTab(named, "a", "   ").list[0].title).toBe("me@box: ~/code/tcode");
  });

  it("names an untitled tab after its folder, never after its id", () => {
    expect(tabLabel(tab("0193f0-uuid-ish"))).toBe("tcode");
  });

  it("keeps a tab whose program ended, and remembers how", () => {
    const ended = endTab(three(), "b", 130);
    expect(ended.list.map((each) => each.id)).toEqual(["a", "b", "c"]);
    expect(ended.list[1].exit).toBe(130);
    // Still selectable: the scrollback is why it is still here.
    expect(selectTab(ended, "b").current).toBe("b");
  });
});
