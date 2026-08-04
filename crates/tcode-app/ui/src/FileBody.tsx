import { useMemo, useState } from "react";

import { Code } from "./components/Code";
import { languageOf } from "./diff";
import { Framed } from "./Framed";
import { Prose } from "./Prose";
import { Sandbox } from "./Sandbox";
import { parseRows, shownAs } from "./show";

/**
 * A file, drawn as the kind of thing it is.
 *
 * One component, because there is one question — "what is this and how should
 * it be drawn" — and `show.ts` is its one answer. It was written for the files
 * the model puts on screen with `show`; the workspace tree then grew a second,
 * much poorer answer of its own (everything is a textarea, and a `preview`
 * toggle that meant nothing for a `.rs`). Both entrances share this now, so a
 * `.svg` is a picture whether the model showed it or somebody opened it, and
 * neither can drift.
 *
 * Every branch is a component the app already had. That is the point of keeping
 * one table: adding a kind of file is an entry in `show.ts`, not a renderer.
 */
export function FileBody({
  path,
  label,
  body,
  revision,
  inline = false,
}: {
  path: string;
  label: string;
  /** The file's text — or, for the kinds `isBinary` claims, a `data:` URL. It
   *  is empty for the kinds `isServed` claims, which are never read here. */
  body: string;
  /** Which version of the file this is, for the one view that holds a reference
   *  to it rather than a copy of it (`Framed`). */
  revision?: string | number;
  /** Drawn in the flow of the transcript rather than filling a pane. Only the
   *  views with no intrinsic height care. */
  inline?: boolean;
}) {
  const view = useMemo(() => shownAs(path, body), [path, body]);

  switch (view.as) {
    case "sandbox":
      return <Sandbox kind={view.sandbox} source={body} label={label} />;
    case "framed":
      return <Framed path={path} label={label} revision={revision} inline={inline} />;
    case "image":
      // `body` is a `data:` URL built by the backend, which is why this needs no
      // asset protocol and no `same-origin` anywhere near it.
      return <img className="shown-image" src={body} alt={label} />;
    case "doc":
      return <Prose className="doc" text={body} />;
    case "table":
      return <Table body={body} separator={view.separator} />;
    case "text":
      return <Code source={body} language={languageOf(path)} />;
  }
}

/** How many rows of a table are on screen before the reader asks for more.
 *  A bound, not a page size: 200k rows of DOM is a frozen window, and nobody
 *  reads past the first screen without deciding to. */
const ROW_STEP = 200;

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
