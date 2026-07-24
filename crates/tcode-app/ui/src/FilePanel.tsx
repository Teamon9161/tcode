import { useMemo, useState } from "react";

import type { Block } from "./transcript";
import { basename, relativeTo, type TouchedFile } from "./files";
import { shorten } from "./components/Path";
import { CloseIcon, FileIcon } from "./components/Icons";

/**
 * The files this conversation has touched.
 *
 * Selecting one shows what the tool actually reported for it — the diff an
 * edit produced, the content a read returned. That is deliberately not a fresh
 * read of the file from disk: what matters while reviewing a turn is what the
 * agent did, and the transcript already holds it, so the panel needs no file
 * access of its own and cannot drift from the conversation it belongs to.
 */
export function FilePanel({
  files,
  blocks,
  cwd,
  selected,
  onSelect,
  onClose,
}: {
  files: TouchedFile[];
  blocks: Block[];
  cwd: string;
  selected: string | null;
  onSelect: (path: string | null) => void;
  onClose: () => void;
}) {
  const active = files.find((file) => file.path === selected) ?? null;

  return (
    <aside className="files">
      <div className="files-head">
        <h2 className="panel-title">Files</h2>
        <span className="panel-count">{files.length}</span>
        <button className="icon-btn" onClick={onClose} aria-label="Hide the file panel">
          <CloseIcon size={15} />
        </button>
      </div>

      {files.length === 0 ? (
        <div className="files-empty">
          <FileIcon size={20} />
          <p>
            Files the agent reads or edits in this conversation collect here.
            Select one to see what changed.
          </p>
        </div>
      ) : (
        <ul className="file-list">
          {files.map((file) => (
            <li key={file.path}>
              <button
                className={`file-item${file.path === selected ? " is-selected" : ""}`}
                onClick={() => onSelect(file.path === selected ? null : file.path)}
              >
                <span className="file-name">{basename(file.path)}</span>
                <span className="file-dir" title={file.path}>
                  {shorten(relativeTo(cwd, file.path), null, 2)}
                </span>
                <span className={`file-tag tag-${file.failed ? "failed" : file.action}`}>
                  {file.pending ? "…" : file.failed ? "failed" : file.action}
                </span>
              </button>
            </li>
          ))}
        </ul>
      )}

      {active && <FileDetail file={active} blocks={blocks} cwd={cwd} />}
    </aside>
  );
}

function FileDetail({
  file,
  blocks,
  cwd,
}: {
  file: TouchedFile;
  blocks: Block[];
  cwd: string;
}) {
  const [wrap, setWrap] = useState(false);

  // The newest result among the calls that touched this file.
  const output = useMemo(() => {
    const last = file.calls[file.calls.length - 1];
    const call = blocks.find(
      (block) => block.kind === "tool" && block.callId === last,
    );
    if (call?.kind !== "tool") return null;
    return call.result?.content || call.result?.preview || null;
  }, [file, blocks]);

  return (
    <div className="file-detail">
      <div className="file-detail-head">
        <span className="file-detail-path" title={file.path}>
          {relativeTo(cwd, file.path)}
        </span>
        <button className="link-btn" onClick={() => setWrap((was) => !was)}>
          {wrap ? "no wrap" : "wrap"}
        </button>
      </div>
      {output ? (
        <pre className={`file-body${wrap ? " is-wrapped" : ""}`}>{output}</pre>
      ) : (
        <p className="file-pending">
          {file.pending ? "still running…" : "no output was recorded for this call"}
        </p>
      )}
    </div>
  );
}
