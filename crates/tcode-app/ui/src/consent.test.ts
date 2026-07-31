import { describe, expect, it } from "vitest";

import { named, readCall, title } from "./Approval";
import type { ApprovalRequest } from "./types";

/**
 * What the consent panel says a call is.
 *
 * Core describes every authorization twice — `descriptor` is the rule a lasting
 * yes would save, `summary` is the same thing as a sentence — and the panel used
 * to print both, so the reader compared `run(git status)` with `run: git status`
 * to discover they were equal. The display now comes out of the call's own
 * input, and these are the shapes it has to read: the descriptor strings below
 * are core's own (`shell.rs` / `edit.rs` / `web.rs` `::permission`), because a
 * fixture that tidies them up is a fixture that cannot see the problem.
 */

const request = (over: Partial<ApprovalRequest>): ApprovalRequest => ({
  session: "s1",
  id: "ap1",
  tool: "shell",
  summary: "",
  descriptor: "",
  is_edit: false,
  allows_project: true,
  input: {},
  ...over,
});

const RUN = request({
  tool: "shell",
  summary: "run: cargo test\n  --all",
  descriptor: "run(cargo test\n  --all)",
  input: { command: "cargo test\n  --all" },
});

const EDIT = request({
  tool: "edit",
  is_edit: true,
  summary: "edit crates/tcode-core/src/agent/mod.rs",
  descriptor: "edit(crates/tcode-core/src/agent/mod.rs)",
  input: { file_path: "crates/tcode-core/src/agent/mod.rs", old_string: "a", new_string: "b" },
});

const FETCH = request({
  tool: "web_fetch",
  summary: "fetch: https://example.com/retry",
  descriptor: "web_fetch(example.com)",
  input: { url: "https://example.com/retry" },
});

describe("readCall", () => {
  it("takes the command from the call, with its line breaks", () => {
    // Not the descriptor: `run(…)` is permission-rule punctuation wrapped
    // around the same text, and it arrives as one line either way.
    expect(readCall(RUN).command).toBe("cargo test\n  --all");
  });

  it("has no command for a call that is not one", () => {
    expect(readCall(EDIT).command).toBeNull();
    expect(readCall(FETCH).command).toBeNull();
  });

  it("targets the bare path, not the rule that would allow it", () => {
    expect(readCall(EDIT).target).toBe("crates/tcode-core/src/agent/mod.rs");
    expect(readCall(FETCH).target).toBe("https://example.com/retry");
  });

  it("falls back to core's summary for a tool it has never seen", () => {
    const mcp = request({
      tool: "mcp__deploy__rollout",
      summary: "rollout (mcp server 'deploy')",
      descriptor: "mcp__deploy__rollout",
      input: { release: "2026.7" },
    });
    expect(readCall(mcp).target).toBe("rollout (mcp server 'deploy')");
  });

  it("reports a working directory only when the call named one", () => {
    expect(readCall(RUN).cwd).toBeNull();
    expect(readCall(request({ input: { command: "ls", cwd: "/tmp" } })).cwd).toBe("/tmp");
  });
});

describe("title", () => {
  it("names the action when the shape says what it is", () => {
    expect(title(RUN, null, false, readCall(RUN))).toBe("Run this?");
    expect(title(EDIT, null, false, readCall(EDIT))).toBe("Change a file?");
  });

  it("asks the generic question when it cannot", () => {
    // Everything that is not an edit used to be "Run this?", which said the
    // wrong verb about a fetch and about every MCP tool there will ever be.
    expect(title(FETCH, null, false, readCall(FETCH))).toBe("Allow this?");
  });
});

describe("named", () => {
  // The chip beside the title is the tool's display name, and `shell`'s is
  // literally "Run" — so it may only appear where the title did not already
  // say the verb, or the header prints one word twice.
  it("is true exactly when the title carried the verb", () => {
    expect(named(RUN, readCall(RUN))).toBe(true);
    expect(named(EDIT, readCall(EDIT))).toBe(true);
    expect(named(FETCH, readCall(FETCH))).toBe(false);
  });
});
