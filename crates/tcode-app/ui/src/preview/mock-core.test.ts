import { beforeEach, describe, expect, it } from "vitest";

import { invoke, resetPreviewFixtures } from "./mock-core";

type Entry = { name: string; path: string; kind: "file" | "directory" | "link" };
type TextView = {
  path: string;
  text: string;
  revision: string;
  fingerprint: string;
  bytes: number;
  truncated: boolean;
};
type BinaryView = { path: string; url: string; bytes: number };

const call = <T>(command: string, args: Record<string, unknown>) => invoke<T>(command, args);

beforeEach(resetPreviewFixtures);

describe("preview workspace fixture", () => {
  it("keeps relative workspace trees independent for each session", async () => {
    const root = await call<Entry[]>("workspace_list", { session: "a", path: null });
    expect(root).toEqual(expect.arrayContaining([
      { name: "crates", path: "crates", kind: "directory" },
      { name: "empty-fixture", path: "empty-fixture", kind: "directory" },
      { name: "outside-workspace", path: "outside-workspace", kind: "link" },
    ]));

    const nested = await call<Entry[]>("workspace_list", { session: "a", path: "crates/tcode-app/src" });
    expect(nested.map((entry) => entry.path)).toContain("crates/tcode-app/src/Workspace.tsx");
    await expect(call<Entry[]>("workspace_list", { session: "a", path: "empty-fixture" })).resolves.toEqual([]);

    const aReadme = await call<TextView>("workspace_read_text", { session: "a", path: "README.md" });
    const bReadme = await call<TextView>("workspace_read_text", { session: "b", path: "README.md" });
    expect(aReadme.text).toContain("Markdown editor preview");
    expect(bReadme.text).toContain("duck_ext");
    expect(aReadme.path).toBe("README.md");

    const longSource = await call<TextView>("workspace_read_text", {
      session: "a",
      path: "crates/tcode-app/src/Workspace.tsx",
    });
    expect(longSource.text.split("\n").length).toBeGreaterThan(150);

    const truncated = await call<TextView>("workspace_read_text", {
      session: "a",
      path: "fixtures/truncated.log",
    });
    expect(truncated.truncated).toBe(true);
    expect(truncated.bytes).toBe(4_800_000);
  });

  /* The second door, and the fixture that proves the scene can show a picture
     at all: before it existed, opening `mark.png` from the tree could only fail,
     because the only reader refused anything that was not UTF-8. */
  it("serves the files that arrive as bytes, and only those", async () => {
    const png = await call<BinaryView>("workspace_read_binary", {
      session: "a",
      path: "icons/mark.png",
    });
    expect(png.url.startsWith("data:image/png;base64,")).toBe(true);
    expect(png.bytes).toBeGreaterThan(0);

    // A text file has no bytes door, exactly as the real workspace has none:
    // which door a file uses is `show.ts`'s answer, and asking for the wrong one
    // is a mistake rather than a fallback.
    await expect(
      call<BinaryView>("workspace_read_binary", { session: "a", path: "README.md" }),
    ).rejects.toThrow(/not a regular file/);
  });

  it("creates, renames, and deletes entries through the fixture contract", async () => {
    const created = await call<Entry>("workspace_create", {
      session: "a",
      parent: "empty-fixture",
      name: "draft.md",
      kind: "file",
    });
    expect(created).toEqual({ name: "draft.md", path: "empty-fixture/draft.md", kind: "file" });

    const renamed = await call<Entry>("workspace_rename", {
      session: "a",
      path: "empty-fixture/draft.md",
      name: "final.md",
    });
    expect(renamed).toEqual({ name: "final.md", path: "empty-fixture/final.md", kind: "file" });

    await call<void>("workspace_delete", { session: "a", path: "empty-fixture/final.md" });
    await expect(call<Entry[]>("workspace_list", { session: "a", path: "empty-fixture" })).resolves.toEqual([]);
  });

  it("returns deterministic revisions and exposes the post-save remote conflict", async () => {
    const loaded = await call<TextView>("workspace_read_text", { session: "a", path: "README.md" });
    expect(loaded.revision).toBe("fixture:a:README.md:1");
    expect(loaded.fingerprint).toBe("fixture:fp:a:README.md:1");

    const saved = await call<TextView>("workspace_write_text", {
      session: "a",
      path: "README.md",
      text: `${loaded.text}\n\nlocal preview edit`,
      revision: loaded.revision,
    });
    expect(saved.revision).toBe("fixture:a:README.md:2");

    await expect(call<TextView>("workspace_write_text", {
      session: "a",
      path: "README.md",
      text: `${saved.text}\nsecond local edit`,
      revision: saved.revision,
    })).rejects.toThrow("revision conflict");

    // The overwrite answer skips the revision guard, exactly as the backend's
    // `write_force` does — the fixture has to stay the acceptance surface.
    const forced = await call<TextView>("workspace_write_text", {
      session: "a",
      path: "README.md",
      text: "overwritten",
      revision: "stale",
      force: true,
    });
    expect(forced.text).toBe("overwritten");
    expect(forced.fingerprint).toBe("fixture:fp:a:README.md:4");
  });
});
