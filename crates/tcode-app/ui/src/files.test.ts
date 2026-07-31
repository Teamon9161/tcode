import { describe, expect, it } from "vitest";

import { groupTouchedFiles, type TouchedFile } from "./files";

const file = (path: string, action: TouchedFile["action"]): TouchedFile => ({
  path,
  action,
  calls: [],
  pending: false,
  failed: false,
  run: null,
});

describe("the touched-files index", () => {
  it("keeps edits and new files above read-only history", () => {
    const groups = groupTouchedFiles([
      file("first.md", "read"),
      file("next.md", "edited"),
      file("new.md", "created"),
      file("last.md", "read"),
    ]);

    expect(groups.changed.map(({ path }) => path)).toEqual(["next.md", "new.md"]);
    expect(groups.read.map(({ path }) => path)).toEqual(["first.md", "last.md"]);
  });
});
