import { describe, expect, it } from "vitest";

import { appOwnedInTerminal } from "./keys";

function key(
  value: string,
  modifiers: Partial<Pick<KeyboardEvent, "altKey" | "ctrlKey" | "metaKey" | "shiftKey">> = {},
): KeyboardEvent {
  return new KeyboardEvent("keydown", {
    key: value,
    ctrlKey: true,
    ...modifiers,
  });
}

describe("terminal-owned app shortcuts", () => {
  it.each([
    "ArrowLeft",
    "ArrowRight",
    "ArrowUp",
    "ArrowDown",
    "Enter",
    " ",
    "f",
    "F",
    "r",
    "R",
    "s",
    "S",
  ])("keeps Mod+Alt+%s for pane layout", (value) => {
    expect(appOwnedInTerminal(key(value, { altKey: true }))).toBe(true);
  });

  it("keeps shifted arrows for exchanging panes", () => {
    expect(
      appOwnedInTerminal(key("ArrowLeft", { altKey: true, shiftKey: true })),
    ).toBe(true);
  });

  it("leaves ordinary terminal chords to the shell", () => {
    expect(appOwnedInTerminal(key("c"))).toBe(false);
    expect(appOwnedInTerminal(key("ArrowLeft"))).toBe(false);
    expect(appOwnedInTerminal(key("f", { altKey: false }))).toBe(false);
  });

  it("requires the platform command modifier", () => {
    expect(
      appOwnedInTerminal(
        key("Enter", { altKey: true, ctrlKey: false, metaKey: false }),
      ),
    ).toBe(false);
  });
});
