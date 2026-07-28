import { basename, relativeTo, type TouchedFile } from "./files";
import { shorten } from "./components/Path";
import { FileIcon } from "./components/Icons";
import type { Inspect } from "./inspect";

/**
 * The index of files this conversation has touched.
 *
 * This is the panel's root view, not a column of its own. Selecting a file
 * pushes it onto the inspector's stack, so "back" returns here — which is why
 * there is no detail pane below the list any more. One region shows one thing;
 * a list and a viewer side by side would be a panel inside a panel, which the
 * visual system bans outright.
 *
 * The list is still derived purely from tool traffic (see `files.ts`), so a
 * resumed conversation rebuilds it by replaying.
 */
export function FilesView({
  files,
  cwd,
  onOpen,
}: {
  files: TouchedFile[];
  cwd: string;
  onOpen: (value: Inspect) => void;
}) {
  if (files.length === 0) {
    return (
      <div className="files-empty">
        <FileIcon size={20} />
        <p>
          Files the agent reads or edits in this conversation collect here.
          Select one to see what it saw, and what changed.
        </p>
      </div>
    );
  }

  return (
    <ul className="file-list">
      {files.map((file) => (
        <li key={file.path}>
          <button className="file-item" onClick={() => onOpen({ kind: "file", path: file.path })}>
            <span className="file-name">{basename(file.path)}</span>
            <span className="file-dir" title={file.path}>
              {shorten(relativeTo(cwd, file.path), null, 2)}
            </span>
            {/* Which agent touched it is the question a parallel run raises, so
                it is on the row rather than one click in. */}
            {file.run && <span className="file-run">sub-agent</span>}
            <span className={`file-tag tag-${file.failed ? "failed" : file.action}`}>
              {file.pending ? "…" : file.failed ? "failed" : file.action}
            </span>
          </button>
        </li>
      ))}
    </ul>
  );
}
