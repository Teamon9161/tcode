import { describe, expect, it } from "vitest";

import type { WorkspaceTextView } from "./types";
import {
  canSaveWorkspaceText,
  discardWorkspaceDraft,
  reloadNeedsConfirmation,
  rememberWorkspaceDraft,
  workspaceDraft,
} from "./workspaceDrafts";

const file = (text: string): WorkspaceTextView => ({
  path: "notes.md",
  text,
  revision: "revision",
  bytes: text.length,
  truncated: false,
});

describe("workspace drafts", () => {
  it("keeps an unsaved draft scoped to its session and path", () => {
    rememberWorkspaceDraft("one", "notes.md", { file: file("saved"), text: "edited", complete: true });

    expect(workspaceDraft("one", "notes.md")?.text).toBe("edited");
    expect(workspaceDraft("two", "notes.md")).toBeNull();
    expect(workspaceDraft("one", "other.md")).toBeNull();

    discardWorkspaceDraft("one", "notes.md");
  });

  it("clears a draft when reload discards it", () => {
    rememberWorkspaceDraft("one", "notes.md", { file: file("saved"), text: "edited", complete: true });
    discardWorkspaceDraft("one", "notes.md");

    expect(workspaceDraft("one", "notes.md")).toBeNull();
  });
});

describe("workspace editor save and reload policy", () => {
  it("asks before a reload would replace an unsaved draft", () => {
    expect(reloadNeedsConfirmation(true)).toBe(true);
    expect(reloadNeedsConfirmation(false)).toBe(false);
  });

  it("never saves an unchanged, truncated, or conflicted response", () => {
    expect(canSaveWorkspaceText({ dirty: true, truncated: false, conflicted: false })).toBe(true);
    expect(canSaveWorkspaceText({ dirty: false, truncated: false, conflicted: false })).toBe(false);
    expect(canSaveWorkspaceText({ dirty: true, truncated: true, conflicted: false })).toBe(false);
    expect(canSaveWorkspaceText({ dirty: true, truncated: false, conflicted: true })).toBe(false);
  });
});
