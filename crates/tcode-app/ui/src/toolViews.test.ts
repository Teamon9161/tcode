import { describe, expect, it } from "vitest";

import {
  displayToolOutput,
  displayToolSummary,
  stripTerminalEscapes,
  viewFor,
} from "./toolViews";

/** What the pop-out button on a call row leads to. */
const inspectFor = (name: string, input: unknown, callId: string) =>
  viewFor(name).inspect?.(input, callId, undefined) ?? null;

describe("displayToolSummary", () => {
  it("replaces the bare replayed tool name with a read target", () => {
    expect(displayToolSummary("read", "read", { file_path: "src/main.rs" })).toBe("src/main.rs");
  });

  it("keeps a read range visible in the compact header", () => {
    expect(displayToolSummary("read", "read(src/main.rs)", { file_path: "src/main.rs", offset: 42, limit: 8 })).toBe(
      "src/main.rs:42-49",
    );
  });

  it("prioritizes a search pattern over the generic tool summary", () => {
    expect(displayToolSummary("grep", "grep(crates)", { pattern: "ToolStart", path: "crates" })).toBe(
      "ToolStart in crates",
    );
  });

  it("caps a long shell command while preserving its complete detail separately", () => {
    const command = "cargo test -p tcode-app --test bridge -- approval_round_trip --nocapture";
    expect(displayToolSummary("shell", `shell(${command})`, { command })).toBe(
      "cargo test -p tcode-app --test bridge -- approval_round_…",
    );
  });

  // The whole row for a skill call used to be the word `skill`: core's generic
  // summary picks the first of a fixed set of argument keys, and this tool's
  // argument is `name`, which is not one of them.
  it("says which skill was loaded, and nothing else", () => {
    expect(displayToolSummary("skill", "skill", { name: "impeccable" })).toBe("impeccable");
    // The arguments are a separate fact, behind the same disclosure a shell
    // command's full text uses. Joined onto the name they were one run of words
    // with no boundary in it.
    expect(
      displayToolSummary("skill", "skill", { name: "dolphindb", arguments: "bond curve" }),
    ).toBe("dolphindb");
    expect(viewFor("skill").detail?.({ name: "dolphindb", arguments: "bond curve" })).toBe(
      "bond curve",
    );
    expect(viewFor("skill").detail?.({ name: "dolphindb" })).toBeNull();
  });

  // The fallback describer feeds a batched call's row, where there is no
  // `ToolStart` summary at all.
  it("reaches a name when a call has nothing more specific", () => {
    expect(displayToolSummary("mcp__x__lookup", "mcp__x__lookup", { name: "GOVT-10Y" })).toBe(
      "GOVT-10Y",
    );
    // A call with both is about the path; `name` is the last resort, not the first.
    expect(
      displayToolSummary("mcp__x__lookup", "mcp__x__lookup", { name: "n", path: "src/a.rs" }),
    ).toBe("src/a.rs");
  });

  it("strips terminal CSI and OSC escapes only for shell output", () => {
    const raw = "\x1b[32mgreen\x1b[0m\n\x1b]0;title\x07plain\n\x1b]8;;https://example.test\x1b\\link\x1b]8;;\x1b\\";
    expect(stripTerminalEscapes(raw)).toBe("green\nplain\nlink");
    expect(displayToolOutput("shell", raw)).toBe("green\nplain\nlink");
    expect(displayToolOutput("read", raw)).toBe(raw);
  });
});

describe("a show call's pop-out target", () => {
  it("resolves to the file it names, with the label it was given", () => {
    expect(inspectFor("show", { path: "out/pnl.csv", label: "PnL" }, "c1")).toEqual({
      kind: "shown",
      path: "out/pnl.csv",
      label: "PnL",
    });
  });

  it("falls back to the file name when no label was supplied", () => {
    expect(inspectFor("show", { path: "out/pnl.csv" }, "c1")).toEqual({
      kind: "shown",
      path: "out/pnl.csv",
      label: "pnl.csv",
    });
  });

  /** The artifact draws at the call site; the pane is the same content with
   *  room, which is why both come from one value. */
  it("is the same value the transcript renders inline", () => {
    expect(viewFor("show").body?.({ path: "out/pnl.csv" })).not.toBeNull();
  });

  /** A read opens the snapshot it returned, an edit opens its diff — the
   *  button is on every row that has somewhere to go, not just show's. */
  it("does not displace the targets other tools already had", () => {
    expect(inspectFor("read", { file_path: "src/main.rs" }, "c1")).toEqual({
      kind: "file",
      path: "src/main.rs",
      at: "c1",
    });
    expect(inspectFor("edit", { file_path: "a", old_string: "x", new_string: "y" }, "c1")).toEqual({
      kind: "diff",
      callId: "c1",
    });
    expect(inspectFor("append", { path: "a", content: "new tail" }, "c2")).toEqual({
      kind: "diff",
      callId: "c2",
    });
    expect(viewFor("append").body?.({ path: "a", content: "new tail" })).not.toBeNull();
  });
});
