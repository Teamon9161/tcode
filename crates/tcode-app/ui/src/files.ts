import type { AgentEvent } from "./types";

/**
 * The files a conversation has touched.
 *
 * Derived from the tool traffic rather than tracked by the backend: `ToolStart`
 * already carries the tool's decoded input, and the edit and write tools name
 * their target in it. That keeps the side panel a pure function of the event
 * stream — the same property the transcript has — so a resumed session
 * reconstructs its file list by replaying, with nothing extra to persist.
 *
 * Sub-agent traffic counts too. A `task` run's tool calls arrive wrapped in
 * `TaskRunEvent`, and a file the parent never touched is still a file this
 * conversation changed; the run id is kept so the panel can say which of them
 * did it, which is the question a parallel run actually raises.
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
  /** The sub-agent run that last touched it, or null for the main agent. */
  run: string | null;
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

export function groupTouchedFiles(files: readonly TouchedFile[]): {
  changed: TouchedFile[];
  read: TouchedFile[];
} {
  const changed: TouchedFile[] = [];
  const read: TouchedFile[] = [];
  for (const file of files) {
    (file.action === "read" ? read : changed).push(file);
  }
  return { changed, read };
}

export function applyFileEvent(files: TouchedFile[], event: AgentEvent): TouchedFile[] {
  return apply(files, event, null);
}

function apply(files: TouchedFile[], event: AgentEvent, run: string | null): TouchedFile[] {
  if (event.type === "TaskRunEvent") {
    const data = event.data as { run: string; event: AgentEvent };
    return apply(files, data.event, data.run);
  }

  if (event.type === "ToolStart") {
    const data = event.data as { call_id: string; name: string; input: unknown };
    return touch(files, data.name, data.input, data.call_id, run);
  }

  // A batch announces its calls before any of them reports individually, and a
  // parallel read set is exactly where the panel earns its keep.
  if (event.type === "ToolBatchStart") {
    const data = event.data as { calls: [string, string, unknown][] };
    return data.calls.reduce(
      (list, [callId, name, input]) => touch(list, name, input, callId, run),
      files,
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

function touch(
  files: TouchedFile[],
  name: string,
  input: unknown,
  callId: string,
  run: string | null,
): TouchedFile[] {
  const action = FILE_TOOLS[name];
  if (!action) return files;
  const path = targetPath(input);
  if (!path) return files;

  const existing = files.find((file) => file.path === path);
  if (!existing) {
    return [...files, { path, action, calls: [callId], pending: true, failed: false, run }];
  }
  // A file read and then edited is an edited file; the stronger action wins so
  // the panel does not downgrade "edited" back to "read" on a re-read.
  const stronger = existing.action === "read" ? action : existing.action;
  return files.map((file) =>
    file.path === path
      ? {
          ...file,
          action: stronger,
          calls: [...file.calls, callId],
          pending: true,
          failed: false,
          run,
        }
      : file,
  );
}

/**
 * `/home/me/code/tcode/src/main.rs` → `src/main.rs`, given the session cwd.
 *
 * Separator- and case-agnostic on both sides: a Windows session's cwd is
 * `C:\code\tcode` while a tool's own input may spell the same path with forward
 * slashes or a different drive-letter case, and a panel that fails to match
 * shows every row as a full absolute path. Only the *prefix* is compared that
 * loosely; what is displayed is the original text, untouched.
 */
export function relativeTo(cwd: string, path: string): string {
  const root = cwd.replace(/[/\\]+$/, "");
  const flat = (text: string) => text.replace(/\\/g, "/").toLowerCase();
  const prefix = `${flat(root)}/`;
  return flat(path).startsWith(prefix) ? path.slice(root.length + 1) : path;
}

export function basename(path: string): string {
  const cut = Math.max(path.lastIndexOf("/"), path.lastIndexOf("\\"));
  return cut === -1 ? path : path.slice(cut + 1);
}
