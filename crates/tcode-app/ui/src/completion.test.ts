import { describe, expect, it } from "vitest";

import { complete, mentions, segments, tokenAt } from "./completion";

describe("tokenAt", () => {
  it("offers a command only as the first word of the message", () => {
    expect(tokenAt("/comp", 5)).toEqual({ kind: "command", start: 0, query: "comp" });
    // Past its own word the line is arguments, which no menu can finish.
    expect(tokenAt("/compact caching", 16)).toBeNull();
    // A path is not a command, however it starts.
    expect(tokenAt("cd /usr/bin", 11)).toBeNull();
  });

  it("opens a mention at an @ that begins a word", () => {
    expect(tokenAt("look at @src/ma", 15)).toEqual({
      kind: "mention",
      start: 8,
      query: "src/ma",
    });
    expect(tokenAt("@", 1)).toEqual({ kind: "mention", start: 0, query: "" });
    // An address is not a path.
    expect(tokenAt("write to me@example.com", 23)).toBeNull();
    // Only whitespace opens one. A bracket does not, which costs a keystroke
    // and is what keeps every address out.
    expect(tokenAt("see (@src", 9)).toBeNull();
  });

  it("ends the token where the caret is, not where the word is", () => {
    // Caret in the middle: the query is what precedes it.
    expect(tokenAt("@src/main.rs", 5)?.query).toBe("src/");
    // Caret past the token entirely.
    expect(tokenAt("@src/main.rs and more", 21)).toBeNull();
  });
});

describe("complete", () => {
  it("replaces the token and leaves the rest of the sentence alone", () => {
    const text = "look at @src/ma please";
    const token = tokenAt(text, 15)!;

    expect(complete(text, token, 15, "@src/main.rs ")).toEqual({
      text: "look at @src/main.rs please",
      caret: 20,
    });
  });

  // Going back to fix a path: the token goes whole, or the tail of the old one
  // is left behind as a second path nobody wrote.
  it("takes the whole token when the caret is inside it", () => {
    const text = "read @src/ma later";
    expect(complete(text, tokenAt(text, 10)!, 10, "@src/main.rs ")).toEqual({
      text: "read @src/main.rs later",
      caret: 17,
    });
  });

  it("finishes a command in place", () => {
    const text = "/comp";
    expect(complete(text, tokenAt(text, 5)!, 5, "/compact ")).toEqual({
      text: "/compact ",
      caret: 9,
    });
  });
});

describe("mentions", () => {
  it("finds each @path and where it sits", () => {
    expect(mentions("compare @a/b.rs with @c.rs")).toEqual([
      { start: 8, end: 15, path: "a/b.rs" },
      { start: 21, end: 26, path: "c.rs" },
    ]);
  });

  it("agrees with tokenAt about what opens one", () => {
    expect(mentions("me@example.com")).toEqual([]);
    expect(mentions("@ alone")).toEqual([]);
  });

  it("cuts a draft into drawable pieces that reassemble exactly", () => {
    const text = "read @src/main.rs then stop";
    expect(segments(text).map((piece) => piece.text).join("")).toBe(text);
    expect(segments(text)).toEqual([
      { text: "read ", mention: null },
      { text: "@src/main.rs", mention: "src/main.rs" },
      { text: " then stop", mention: null },
    ]);
  });
});
