import { useState, useSyncExternalStore } from "react";

import { CheckIcon, ChevronDown, ChevronRight, CloseIcon, DownloadIcon, FolderIcon } from "./components/Icons";
import type { Download } from "./types";
import * as browser from "./webHost";

/**
 * The download shelf: what the window's browser has saved or is saving.
 *
 * One shelf for the whole window, not one per tab, because the browser is one
 * shared instance and a file belongs to the browser rather than to the page
 * that fetched it — a user clicking a link in the visible pane and a model
 * driving a background tab put files on the same shelf. It closes the gap the
 * browser used to leave: a download that finished somewhere the person could
 * not see and could not open.
 *
 * It sits in the pane's normal flow, above the body the page composites over,
 * because a native webview would cover anything drawn inside that body. It is
 * rendered only when there is something to show, so the page keeps the whole
 * pane until the moment a download exists.
 *
 * ## Removing, and the webview overlap
 *
 * Removing a download offers two outcomes — drop the record, or delete the file
 * as well — and that choice is made *at the click*, never a persisted mode that
 * could be left armed and delete the next thing by surprise. It is offered by an
 * in-place confirm rather than a menu: a popover opening past the shelf's own
 * height would fall into the page's rectangle, where the native webview paints
 * over it. So the row (or the header) swaps its controls for the two choices in
 * the space it already occupies, and nothing is ever drawn where it cannot be
 * seen.
 */

/** A byte count in the largest unit that keeps it short — the shelf's own copy,
 *  because it renders far more often than anything backend-side would. */
function humanize(bytes: number): string {
  const units = ["B", "KB", "MB", "GB", "TB"];
  let value = Math.max(0, bytes);
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return unit === 0 ? `${Math.max(0, bytes)} B` : `${value.toFixed(1)} ${units[unit]}`;
}

/** What a row says about a download's size, matching the tool's phrasing: both
 *  halves while it runs and the server declared a length, one number once it is
 *  done or the length is unknown. */
function sizeLabel(download: Download): string {
  const { receivedBytes, totalBytes } = download;
  if (download.state === "progressing" && totalBytes > 0) {
    return `${humanize(receivedBytes)} of ${humanize(totalBytes)}`;
  }
  return humanize(Math.max(receivedBytes, totalBytes));
}

/** The two-choice confirm, shared by a row and the header. "Keep" drops the
 *  record only; "Delete" removes the file too and is styled as destructive. */
function ConfirmDelete({
  keepLabel,
  deleteLabel,
  onKeep,
  onDelete,
  onCancel,
}: {
  keepLabel: string;
  deleteLabel: string;
  onKeep: () => void;
  onDelete: () => void;
  onCancel: () => void;
}) {
  return (
    <span className="web-download-confirm">
      <button
        className="web-download-choice"
        onClick={onKeep}
        title="Remove from the list, keep the file on disk"
      >
        {keepLabel}
      </button>
      <button
        className="web-download-choice is-danger"
        onClick={onDelete}
        title="Delete the file from disk and remove it from the list"
      >
        {deleteLabel}
      </button>
      <button className="icon-btn" onClick={onCancel} aria-label="Cancel">
        <CloseIcon size={12} />
      </button>
    </span>
  );
}

function DownloadRow({
  download,
  asking,
  onAsk,
  onCancelAsk,
  onRemove,
}: {
  download: Download;
  asking: boolean;
  onAsk: () => void;
  onCancelAsk: () => void;
  onRemove: (deleteFile: boolean) => void;
}) {
  const done = download.state === "completed";
  const progressing = download.state === "progressing";
  const fraction =
    progressing && download.totalBytes > 0
      ? Math.min(1, download.receivedBytes / download.totalBytes)
      : null;

  return (
    <li className={`web-download is-${download.state}`}>
      <span className="web-download-icon" aria-hidden="true">
        {done ? <CheckIcon size={13} /> : <DownloadIcon size={13} />}
      </span>
      <span className="web-download-body">
        <span className="web-download-name" title={download.path}>
          {download.filename}
        </span>
        <span className="web-download-meta">
          {done || progressing ? sizeLabel(download) : `${download.state} · ${humanize(download.receivedBytes)}`}
        </span>
        {fraction !== null && (
          <span className="web-download-bar" aria-hidden="true">
            <span className="web-download-fill" style={{ transform: `scaleX(${fraction})` }} />
          </span>
        )}
      </span>
      {asking ? (
        <ConfirmDelete
          keepLabel="Keep file"
          deleteLabel="Delete file"
          onKeep={() => onRemove(false)}
          onDelete={() => onRemove(true)}
          onCancel={onCancelAsk}
        />
      ) : (
        <span className="web-download-actions">
          {done && (
            <>
              <button
                className="icon-btn"
                onClick={() => browser.openDownload(download.path)}
                aria-label={`Open ${download.filename}`}
                title="Open"
              >
                <DownloadIcon size={13} />
              </button>
              <button
                className="icon-btn"
                onClick={() => browser.revealDownload(download.path)}
                aria-label={`Show ${download.filename} in folder`}
                title="Show in folder"
              >
                <FolderIcon size={13} />
              </button>
            </>
          )}
          <button
            className="icon-btn"
            onClick={onAsk}
            aria-label={`Remove ${download.filename}`}
            title={progressing ? "Cancel and remove" : "Remove"}
          >
            <CloseIcon size={13} />
          </button>
        </span>
      )}
    </li>
  );
}

/** Which delete confirm, if any, is open: one row (by id) or the whole shelf. */
type Asking = { kind: "row"; id: string } | { kind: "all" } | null;

export function Downloads() {
  const { downloads } = useSyncExternalStore(browser.subscribe, browser.snapshot);
  // Default open: a shelf that appears already collapsed would answer the
  // question "did my download work" with a closed box. The person can fold it
  // away to give the page back its room.
  const [open, setOpen] = useState(true);
  const [asking, setAsking] = useState<Asking>(null);

  if (downloads.length === 0) return null;

  const active = downloads.filter((download) => download.state === "progressing").length;
  // Newest first: the thing you just did is the thing you are asking about.
  const rows = [...downloads].reverse();

  return (
    <section className={`web-downloads${open ? " is-open" : ""}`} aria-label="Downloads">
      <div className="web-downloads-head">
        <button
          className="web-downloads-toggle"
          onClick={() => setOpen((was) => !was)}
          aria-expanded={open}
        >
          <span className="web-downloads-chevron" aria-hidden="true">
            {open ? <ChevronDown size={13} /> : <ChevronRight size={13} />}
          </span>
          <DownloadIcon size={13} />
          <span className="web-downloads-title">Downloads</span>
          <span className="web-downloads-count">
            {active > 0 ? `${active} in progress` : `${downloads.length}`}
          </span>
        </button>
        {open &&
          (asking?.kind === "all" ? (
            <ConfirmDelete
              keepLabel="Keep files"
              deleteLabel="Delete files"
              onKeep={() => {
                browser.clearDownloads(false);
                setAsking(null);
              }}
              onDelete={() => {
                browser.clearDownloads(true);
                setAsking(null);
              }}
              onCancel={() => setAsking(null)}
            />
          ) : (
            <button className="web-downloads-clear" onClick={() => setAsking({ kind: "all" })}>
              Clear all
            </button>
          ))}
      </div>
      {open && (
        <ul className="web-downloads-list">
          {rows.map((download) => (
            <DownloadRow
              key={download.id}
              download={download}
              asking={asking?.kind === "row" && asking.id === download.id}
              onAsk={() => setAsking({ kind: "row", id: download.id })}
              onCancelAsk={() => setAsking(null)}
              onRemove={(deleteFile) => {
                browser.removeDownload(download.id, deleteFile);
                setAsking(null);
              }}
            />
          ))}
        </ul>
      )}
    </section>
  );
}
