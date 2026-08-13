import { useEffect, useState } from "react";
import { invoke } from "@ipc";

import { RefreshIcon } from "./components/Icons";
import { FileBody } from "./FileBody";
import { relativeTo } from "./files";
import type { Inspect } from "./inspect";
import { useSession } from "./session";
import { extensionOf, isBinary, isServed } from "./show";

/** Mirrors `ShownFile` in `src/commands.rs`. */
type ShownFile = { body: string; bytes: number; truncated: boolean };

/**
 * A file the model asked to display.
 *
 * It draws in two places and they are the *same* component: at the call site in
 * the transcript, and filling an inspect pane. That is the whole affordance —
 * the artifact appears where the conversation is, in the flow of reading, and
 * one small button moves it somewhere bigger when it deserves the room. The
 * alternative shipped first and was wrong: the call opened a pane by itself,
 * which answers "look at this" by rearranging the window before anyone asked.
 *
 * Inline differs only in chrome and a height cap (`.shown.is-inline`), never in
 * what is rendered. A chart that looks one way in the transcript and another in
 * the pane would make the button a gamble instead of a magnifier.
 *
 * The one view in the app that reads from disk. Every other inspect view is a
 * pure function of the transcript, and deliberately so — they answer "what did
 * the agent do", which a fresh read would quietly replace with "what is there
 * now". This view's question *is* the file: it exists precisely so a chart or a
 * 500k-row table never had to travel through the conversation, so there is
 * nothing in the transcript to draw it from.
 *
 * That makes staleness real, and it is handled by saying so rather than by
 * watching: a file rewritten after it was shown is normal (re-run the script,
 * look again), and a watcher would be a permanent background mechanism bought
 * for a button.
 */
export function ShownView({
  value,
  cwd,
  inline = false,
  onOpen,
}: {
  value: Extract<Inspect, { kind: "shown" }>;
  /** Only the pane spells out the path; inline, the tool row above already has
   *  it, and repeating it costs a line of every artifact in the transcript. */
  cwd?: string;
  inline?: boolean;
  onOpen?: (next: Inspect) => void;
}) {
  const session = useSession();
  const [file, setFile] = useState<ShownFile | null>(null);
  const [failure, setFailure] = useState<string | null>(null);
  const [reload, setReload] = useState(0);
  const extension = extensionOf(value.path);
  const canPreviewSpreadsheet = !inline && (extension === "csv" || extension === "tsv");

  useEffect(() => {
    let live = true;
    setFile(null);
    setFailure(null);

    // A served file is never read here — the frame requests it from the origin
    // itself. Pre-reading it would buy the bytes twice and, for the reports
    // this path exists for, truncate them at `VIEWER_TEXT_BUDGET` on the way.
    if (isServed(value.path)) {
      setFile({ body: "", bytes: 0, truncated: false });
      return;
    }

    invoke<ShownFile>("shown_file", {
      session,
      path: value.path,
      binary: isBinary(value.path),
    })
      .then((loaded) => live && setFile(loaded))
      .catch((error) => live && setFailure(String(error)));
    return () => {
      live = false;
    };
  }, [session, value.path, reload]);

  return (
    <div
      className={`shown${inline ? " is-inline" : ""}${isServed(value.path) ? " is-framed" : ""}`}
    >
      <div className="shown-bar">
        {!inline && (
          <p className="inspect-path" title={value.path}>
            {relativeTo(cwd ?? "", value.path)}
          </p>
        )}
        {canPreviewSpreadsheet && (
          <button
            type="button"
            className="chip"
            onClick={() => onOpen?.({ kind: "spreadsheet", path: value.path })}
            aria-label="Preview as spreadsheet"
          >
            Preview as spreadsheet
          </button>
        )}
        <button
          type="button"
          className="icon-btn"
          onClick={() => setReload((at) => at + 1)}
          aria-label="Read this file again"
          title="Read this file again"
        >
          <RefreshIcon size={14} />
        </button>
      </div>

      {file?.truncated && (
        <p className="shown-clipped">
          showing the first part of a {human(file.bytes)} file
        </p>
      )}

      {failure ? (
        <p className="inspect-empty">{failure}</p>
      ) : file ? (
        <FileBody
          path={value.path}
          label={value.label}
          body={file.body}
          // The reload button's whole job, for the one view that holds a
          // reference to the file rather than a copy of its bytes.
          revision={reload}
          inline={inline}
        />
      ) : (
        <p className="inspect-empty">loading…</p>
      )}
    </div>
  );
}

/** Matches the tool's own phrasing, so the pane and the transcript line under it
 *  describe the same file the same way. */
function human(bytes: number): string {
  const units = ["B", "KB", "MB", "GB"];
  let size = bytes;
  let unit = 0;
  while (size >= 1024 && unit + 1 < units.length) {
    size /= 1024;
    unit += 1;
  }
  return unit === 0 ? `${bytes} B` : `${size.toFixed(1)} ${units[unit]}`;
}
