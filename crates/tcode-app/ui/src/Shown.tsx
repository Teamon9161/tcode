import { useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

import { Code } from "./components/Code";
import { languageOf } from "./diff";
import { relativeTo } from "./files";
import type { Inspect } from "./inspect";
import { rich } from "./rich";
import { Sandbox } from "./Sandbox";
import { useSession } from "./session";
import { isBinary, parseRows, shownAs } from "./show";

/** Mirrors `ShownFile` in `src/commands.rs`. */
type ShownFile = { body: string; bytes: number; truncated: boolean };

/** How many rows of a table are on screen before the reader asks for more.
 *  A bound, not a page size: 200k rows of DOM is a frozen window, and nobody
 *  reads past the first screen without deciding to. */
const ROW_STEP = 200;

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
}: {
  value: Extract<Inspect, { kind: "shown" }>;
  /** Only the pane spells out the path; inline, the tool row above already has
   *  it, and repeating it costs a line of every artifact in the transcript. */
  cwd?: string;
  inline?: boolean;
}) {
  const session = useSession();
  const [file, setFile] = useState<ShownFile | null>(null);
  const [failure, setFailure] = useState<string | null>(null);
  const [reload, setReload] = useState(0);

  useEffect(() => {
    let live = true;
    setFile(null);
    setFailure(null);
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
    <div className={`shown${inline ? " is-inline" : ""}`}>
      <div className="shown-bar">
        {!inline && (
          <p className="inspect-path" title={value.path}>
            {relativeTo(cwd ?? "", value.path)}
          </p>
        )}
        <button className="link-btn" onClick={() => setReload((at) => at + 1)}>
          reload
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
        <Body path={value.path} label={value.label} body={file.body} />
      ) : (
        <p className="inspect-empty">loading…</p>
      )}
    </div>
  );
}

/** Dispatch on what the registry says this file is. Every branch is a component
 *  the app already had, which is the point of keeping one table. */
function Body({ path, label, body }: { path: string; label: string; body: string }) {
  const view = useMemo(() => shownAs(path, body), [path, body]);

  switch (view.as) {
    case "sandbox":
      return <Sandbox kind={view.sandbox} source={body} label={label} />;
    case "image":
      // `body` is a `data:` URL built by the backend, which is why this needs no
      // asset protocol and no `same-origin` anywhere near it.
      return <img className="shown-image" src={body} alt={label} />;
    case "doc":
      return <div className="doc">{rich(body)}</div>;
    case "table":
      return <Table body={body} separator={view.separator} />;
    case "text":
      return <Code source={body} language={languageOf(path)} />;
  }
}

/**
 * Delimited data, with a bounded number of rows in the DOM.
 *
 * The first row is the header. That is an assumption, and it is the right one:
 * every table a script writes for a human to read has one, and a file that does
 * not simply loses one row to the header band rather than becoming unreadable.
 */
function Table({ body, separator }: { body: string; separator: string }) {
  const rows = useMemo(() => parseRows(body, separator), [body, separator]);
  const [limit, setLimit] = useState(ROW_STEP);

  if (rows.length === 0) return <p className="inspect-empty">no rows</p>;

  const [header, ...data] = rows;
  const shown = data.slice(0, limit);

  return (
    <div className="shown-table-wrap">
      <table className="shown-table">
        <thead>
          <tr>
            {header.map((cell, at) => (
              <th key={at}>{cell}</th>
            ))}
          </tr>
        </thead>
        <tbody>
          {shown.map((row, at) => (
            <tr key={at}>
              {row.map((cell, column) => (
                <td key={column}>{cell}</td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>
      {data.length > shown.length && (
        <p className="shown-more">
          {shown.length} of {data.length} rows
          <button className="link-btn" onClick={() => setLimit((at) => at + ROW_STEP)}>
            show {Math.min(ROW_STEP, data.length - shown.length)} more
          </button>
        </p>
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
