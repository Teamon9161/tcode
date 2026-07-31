import { useState } from "react";

import { basename, groupTouchedFiles, relativeTo, type TouchedFile } from "./files";
import { shorten } from "./components/Path";
import { ChevronDown, ChevronRight, FileIcon } from "./components/Icons";
import type { Inspect } from "./inspect";

/**
 * The index of files this conversation has touched.
 *
 * The list is still derived purely from tool traffic (see `files.ts`), so a
 * resumed conversation rebuilds it by replaying. Changes lead because they are
 * actionable; read-only history is available without competing with them.
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
  const [readsOpen, setReadsOpen] = useState(false);
  const { changed, read } = groupTouchedFiles(files);

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
    <div className="files-index">
      {changed.length > 0 ? (
        <FileGroup label="changed" files={changed} cwd={cwd} onOpen={onOpen} />
      ) : (
        <p className="files-empty-note">no files were changed in this conversation</p>
      )}

      {read.length > 0 && (
        <section className="file-group">
          <button
            type="button"
            className="file-group-toggle"
            onClick={() => setReadsOpen((open) => !open)}
            aria-expanded={readsOpen}
          >
            {readsOpen ? <ChevronDown size={13} /> : <ChevronRight size={13} />}
            <span>read only</span>
            <span className="file-group-count">{read.length}</span>
          </button>
          {readsOpen && <FileList files={read} cwd={cwd} onOpen={onOpen} />}
        </section>
      )}
    </div>
  );
}

function FileGroup({
  label,
  files,
  cwd,
  onOpen,
}: {
  label: string;
  files: TouchedFile[];
  cwd: string;
  onOpen: (value: Inspect) => void;
}) {
  return (
    <section className="file-group">
      <h2 className="file-group-label">{label}</h2>
      <FileList files={files} cwd={cwd} onOpen={onOpen} />
    </section>
  );
}

function FileList({
  files,
  cwd,
  onOpen,
}: {
  files: TouchedFile[];
  cwd: string;
  onOpen: (value: Inspect) => void;
}) {
  return (
    <ul className="file-list">
      {files.map((file) => (
        <li key={file.path}>
          <button className="file-item" onClick={() => onOpen({ kind: "file", path: file.path })}>
            <FileIcon size={14} />
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
