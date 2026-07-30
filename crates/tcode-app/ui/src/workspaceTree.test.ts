import { describe, expect, it } from "vitest";

import {
  WORKSPACE_ROOT,
  createWorkspaceEntry,
  deleteWorkspaceEntry,
  emptyWorkspaceTree,
  renameWorkspaceEntry,
  replaceWorkspaceChildren,
  toggleWorkspaceDirectory,
  visibleWorkspaceTree,
  workspaceHostPath,
  type WorkspaceEntry,
} from "./workspaceTree";

const directory = (name: string, path = name): WorkspaceEntry => ({ name, path, kind: "directory" });
const file = (name: string, path = name): WorkspaceEntry => ({ name, path, kind: "file" });

describe("workspace tree state", () => {
  it("sorts direct children with directories first while retaining equal-name order", () => {
    const tree = replaceWorkspaceChildren(emptyWorkspaceTree(), WORKSPACE_ROOT, [
      file("zeta"),
      file("Alpha", "Alpha-file"),
      directory("beta"),
      directory("Alpha", "Alpha-directory"),
      file("ALPHA", "ALPHA-file"),
    ]);

    expect(tree.children[WORKSPACE_ROOT]?.map((entry) => entry.path)).toEqual([
      "Alpha-directory",
      "beta",
      "Alpha-file",
      "ALPHA-file",
      "zeta",
    ]);
  });

  it("loads directories lazily and only draws descendants after expansion", () => {
    let tree = replaceWorkspaceChildren(emptyWorkspaceTree(), WORKSPACE_ROOT, [directory("src")]);
    tree = replaceWorkspaceChildren(tree, "src", [file("main.ts", "src/main.ts")]);

    expect(visibleWorkspaceTree(tree, "").map((entry) => entry.path)).toEqual(["src"]);

    tree = toggleWorkspaceDirectory(tree, "src");
    expect(visibleWorkspaceTree(tree, "").map((entry) => entry.path)).toEqual(["src", "src/main.ts"]);

    tree = toggleWorkspaceDirectory(tree, "src");
    expect(visibleWorkspaceTree(tree, "").map((entry) => entry.path)).toEqual(["src"]);
  });

  it("filters loaded branches without claiming an unloaded directory has no match", () => {
    let tree = replaceWorkspaceChildren(emptyWorkspaceTree(), WORKSPACE_ROOT, [
      directory("loaded"),
      directory("unknown"),
      file("README.md"),
    ]);
    tree = replaceWorkspaceChildren(tree, "loaded", [file("main.ts", "loaded/main.ts")]);

    expect(visibleWorkspaceTree(tree, "needle").map((entry) => entry.path)).toEqual(["unknown"]);
    expect(visibleWorkspaceTree(tree, "main").map((entry) => entry.path)).toEqual([
      "loaded",
      "loaded/main.ts",
      "unknown",
    ]);
  });

  it("keeps a parent in stable tree order after a rename", () => {
    let tree = replaceWorkspaceChildren(emptyWorkspaceTree(), WORKSPACE_ROOT, [file("zeta"), file("alpha")]);
    tree = renameWorkspaceEntry(tree, "alpha", file("zulu", "zulu"));

    expect(tree.children[WORKSPACE_ROOT]?.map((entry) => entry.path)).toEqual(["zeta", "zulu"]);
  });

  it("replaces a refreshed child list instead of merging stale entries", () => {
    let tree = replaceWorkspaceChildren(emptyWorkspaceTree(), WORKSPACE_ROOT, [directory("src")]);
    tree = replaceWorkspaceChildren(tree, "src", [file("old.ts", "src/old.ts")]);
    tree = replaceWorkspaceChildren(tree, "src", [file("new.ts", "src/new.ts")]);

    expect(tree.children.src?.map((entry) => entry.path)).toEqual(["src/new.ts"]);
  });

  it("applies create, rename, and delete locally, including a renamed directory cache", () => {
    let tree = replaceWorkspaceChildren(emptyWorkspaceTree(), WORKSPACE_ROOT, [directory("src")]);
    tree = replaceWorkspaceChildren(tree, "src", [file("main.ts", "src/main.ts")]);
    tree = toggleWorkspaceDirectory(tree, "src");
    tree = createWorkspaceEntry(tree, "src", file("lib.ts", "src/lib.ts"));
    tree = renameWorkspaceEntry(tree, "src", directory("code", "code"));

    expect(visibleWorkspaceTree(tree, "").map((entry) => entry.path)).toEqual([
      "code",
      "code/lib.ts",
      "code/main.ts",
    ]);

    tree = deleteWorkspaceEntry(tree, "code/lib.ts");
    expect(visibleWorkspaceTree(tree, "").map((entry) => entry.path)).toEqual(["code", "code/main.ts"]);
  });
});

describe("workspaceHostPath", () => {
  it("writes a Windows path with Windows separators", () => {
    expect(workspaceHostPath("C:\\code\\tcode", "crates/app/src/main.rs")).toBe(
      "C:\\code\\tcode\\crates\\app\\src\\main.rs",
    );
  });

  it("leaves a posix path alone", () => {
    expect(workspaceHostPath("/home/me/tcode", "src/main.rs")).toBe("/home/me/tcode/src/main.rs");
  });

  it("does not double a separator the folder already ends with", () => {
    expect(workspaceHostPath("/home/me/tcode/", "a.txt")).toBe("/home/me/tcode/a.txt");
    expect(workspaceHostPath("C:\\code\\", "a.txt")).toBe("C:\\code\\a.txt");
  });
});
