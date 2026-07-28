import { describe, expect, it } from "vitest";

import { displayToolOutput, displayToolSummary, stripTerminalEscapes } from "./toolViews";

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

  it("strips terminal CSI and OSC escapes only for shell output", () => {
    const raw = "\x1b[32mgreen\x1b[0m\n\x1b]0;title\x07plain\n\x1b]8;;https://example.test\x1b\\link\x1b]8;;\x1b\\";
    expect(stripTerminalEscapes(raw)).toBe("green\nplain\nlink");
    expect(displayToolOutput("shell", raw)).toBe("green\nplain\nlink");
    expect(displayToolOutput("read", raw)).toBe(raw);
  });
});
