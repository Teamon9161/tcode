import { EditorSelection, EditorState } from "@codemirror/state";
import { describe, expect, it } from "vitest";

import type { WorkspaceTextView } from "./types";
import {
  canSaveWorkspaceText,
  conflictedWorkspaceFileSession,
  newWorkspaceFileSession,
  reloadNeedsConfirmation,
  reloadedWorkspaceFileSession,
  rememberWorkspaceFileSession,
  savedWorkspaceFileSession,
  workspaceFileDirty,
  workspaceFileSession,
} from "./workspaceDrafts";

const file = (text: string, revision = "revision", truncated = false): WorkspaceTextView => ({
  path: "notes.md",
  text,
  revision,
  bytes: text.length,
  truncated,
});

describe("workspace file sessions", () => {
  it("keeps clean and dirty state scoped to its session and path", () => {
    const value = newWorkspaceFileSession(file("saved"), true);
    rememberWorkspaceFileSession("one", "notes.md", { ...value, text: "edited" });

    expect(workspaceFileSession("one", "notes.md")?.text).toBe("edited");
    expect(workspaceFileSession("two", "notes.md")).toBeNull();
    expect(workspaceFileSession("one", "other.md")).toBeNull();
  });

  it("opens Markdown in preview and ordinary UTF-8 text in edit mode", () => {
    expect(newWorkspaceFileSession(file("# title"), true).mode).toBe("preview");
    expect(newWorkspaceFileSession(file("fn main() {}"), false).mode).toBe("edit");
  });

  it("advances the save baseline without losing editor state, mode or viewport", () => {
    const editorState = EditorState.create({
      doc: "edited",
      selection: EditorSelection.cursor(4),
    });
    const current = {
      ...newWorkspaceFileSession(file("saved"), true),
      text: "edited after submit",
      mode: "edit" as const,
      editorState,
      editorScroll: { top: 120, left: 8 },
    };
    const saved = savedWorkspaceFileSession(current, file("echo", "revision-2"), "edited");

    expect(saved.file.text).toBe("edited");
    expect(saved.text).toBe("edited after submit");
    expect(workspaceFileDirty(saved)).toBe(true);
    expect(saved.file.revision).toBe("revision-2");
    expect(saved.editorState).toBe(editorState);
    expect(saved.editorScroll).toEqual({ top: 120, left: 8 });
    expect(saved.mode).toBe("edit");
  });

  it("reloads from disk with a fresh editor generation and no old state", () => {
    const current = {
      ...newWorkspaceFileSession(file("saved"), true),
      text: "draft",
      mode: "edit" as const,
      editorState: EditorState.create({ doc: "draft" }),
      editorScroll: { top: 80, left: 0 },
      conflicted: true,
    };
    const reloaded = reloadedWorkspaceFileSession(current, file("remote", "revision-2"));

    expect(reloaded.text).toBe("remote");
    expect(reloaded.editorState).toBeNull();
    expect(reloaded.generation).toBe(current.generation + 1);
    expect(reloaded.editorScroll.top).toBe(80);
    expect(reloaded.mode).toBe("edit");
    expect(reloaded.conflicted).toBe(false);
  });

  it("marks a conflict without replacing the draft", () => {
    const current = { ...newWorkspaceFileSession(file("saved"), false), text: "draft" };
    const conflicted = conflictedWorkspaceFileSession(current);

    expect(conflicted.text).toBe("draft");
    expect(conflicted.file.text).toBe("saved");
    expect(conflicted.conflicted).toBe(true);
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
