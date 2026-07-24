import type { AgentEvent } from "./types";

/**
 * The files a conversation has touched.
 *
 * Derived from the tool traffic rather than tracked by the backend: `ToolStart`
 * already carries the tool's decoded input, and the edit and write tools name
 * their target in it. That keeps the side panel a pure function of the event
 * stream — the same property the transcript has — so a resumed session
 * reconstructs its file list by replaying, with nothing extra to persist.
 */
export type TouchedFile = {
  path: string;
  /** Last thing that happened to it. */
  action: "read" | "edited" | "created";
  /** Call ids that touched it, newest last. */
  calls: string[];
  /** True until the tool call that is currently touching it returns. */
  pending: boolean;
  failed: boolean;
};

/** Tools that name a file in their input, and what touching it means. */
const FILE_TOOLS: Record<string, TouchedFile["action"]> = {
  read: "read",
  edit: "edited",
  write: "created",
  multi_edit: "edited",
  notebook_edit: "edited",
};

/** The path argument, under any of the names the tools use for it. */
function targetPath(input: unknown): string | null {
  if (typeof input !== "object" || input === null) return null;
  const record = input as Record<string, unknown>;
  for (const key of ["file_path", "path", "notebook_path", "filePath"]) {
    const value = record[key];
    if (typeof value === "string" && value.length > 0) return value;
  }
  return null;
}

export function applyFileEvent(files: TouchedFile[], event: AgentEvent): TouchedFile[] {
  if (event.type === "ToolStart") {
    const data = event.data as { call_id: string; name: string; input: unknown };
    const action = FILE_TOOLS[data.name];
    if (!action) return files;
    const path = targetPath(data.input);
    if (!path) return files;

    const existing = files.find((file) => file.path === path);
    if (!existing) {
      return [
        ...files,
        { path, action, calls: [data.call_id], pending: true, failed: false },
      ];
    }
    // A file read and then edited is an edited file; the stronger action wins
    // so the panel does not downgrade "edited" back to "read" on a re-read.
    const stronger = existing.action === "read" ? action : existing.action;
    return files.map((file) =>
      file.path === path
        ? {
            ...file,
            action: stronger,
            calls: [...file.calls, data.call_id],
            pending: true,
            failed: false,
          }
        : file,
    );
  }

  if (event.type === "ToolEnd") {
    const data = event.data as { call_id: string; is_error: boolean };
    return files.map((file) =>
      file.calls.includes(data.call_id)
        ? { ...file, pending: false, failed: data.is_error }
        : file,
    );
  }

  return files;
}

/** `/home/me/code/tcode/src/main.rs` → `src/main.rs`, given the session cwd. */
export function relativeTo(cwd: string, path: string): string {
  const root = cwd.endsWith("/") ? cwd : `${cwd}/`;
  return path.startsWith(root) ? path.slice(root.length) : path;
}

export function basename(path: string): string {
  const cut = path.lastIndexOf("/");
  return cut === -1 ? path : path.slice(cut + 1);
}
