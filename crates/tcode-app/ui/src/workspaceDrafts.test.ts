import { EditorSelection, EditorState } from "@codemirror/state";
import { describe, expect, it } from "vitest";

import type { WorkspaceTextView } from "./types";
import {
  canForceSaveWorkspaceText,
  canSaveWorkspaceText,
  diskChangedWorkspaceFileSession,
  forgetWorkspaceFileSessions,
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
  fingerprint: `fingerprint-${revision}`,
  bytes: text.length,
  truncated,
});

describe("workspace file sessions", () => {
  it("forgets cached workspace drafts when its conversation moves folders", () => {
    const value = newWorkspaceFileSession(file("saved"), true);
    rememberWorkspaceFileSession("one", "notes.md", value);
    rememberWorkspaceFileSession("two", "notes.md", value);

    forgetWorkspaceFileSessions("one");

    expect(workspaceFileSession("one", "notes.md")).toBeNull();
    expect(workspaceFileSession("two", "notes.md")).toEqual(value);
    forgetWorkspaceFileSessions("two");
  });

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
    expect(saved.file.fingerprint).toBe("fingerprint-revision-2");
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
      diskChanged: true,
    };
    const reloaded = reloadedWorkspaceFileSession(current, file("remote", "revision-2"));

    expect(reloaded.text).toBe("remote");
    expect(reloaded.editorState).toBeNull();
    expect(reloaded.generation).toBe(current.generation + 1);
    expect(reloaded.editorScroll.top).toBe(80);
    expect(reloaded.mode).toBe("edit");
    expect(reloaded.diskChanged).toBe(false);
  });

  it("marks a disk change without replacing the draft", () => {
    const current = { ...newWorkspaceFileSession(file("saved"), false), text: "draft" };
    const changed = diskChangedWorkspaceFileSession(current);

    expect(changed.text).toBe("draft");
    expect(changed.file.text).toBe("saved");
    expect(changed.diskChanged).toBe(true);
  });
});

describe("workspace editor save and reload policy", () => {
  it("asks before a reload would replace an unsaved draft", () => {
    expect(reloadNeedsConfirmation(true)).toBe(true);
    expect(reloadNeedsConfirmation(false)).toBe(false);
  });

  it("never saves an unchanged, truncated, or disk-changed response", () => {
    expect(canSaveWorkspaceText({ dirty: true, truncated: false, diskChanged: false })).toBe(true);
    expect(canSaveWorkspaceText({ dirty: false, truncated: false, diskChanged: false })).toBe(false);
    expect(canSaveWorkspaceText({ dirty: true, truncated: true, diskChanged: false })).toBe(false);
    expect(canSaveWorkspaceText({ dirty: true, truncated: false, diskChanged: true })).toBe(false);
  });

  it("lets the overwrite answer skip only the disk-change guard", () => {
    expect(canForceSaveWorkspaceText({ dirty: true, truncated: false })).toBe(true);
    expect(canForceSaveWorkspaceText({ dirty: true, truncated: true })).toBe(false);
    expect(canForceSaveWorkspaceText({ dirty: false, truncated: false })).toBe(false);
  });
});


  it("does not mark a Windows line-ending document dirty after CodeMirror normalizes it", () => {
    const current = newWorkspaceFileSession(file("one\r\ntwo"), false);

    expect(workspaceFileDirty({ ...current, text: "one\ntwo" })).toBe(false);
  });

  it("still marks substantive text changes dirty after normalizing line endings", () => {
    const current = newWorkspaceFileSession(file("one\r\ntwo"), false);

    expect(workspaceFileDirty({ ...current, text: "one\nchanged" })).toBe(true);
  });
