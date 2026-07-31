import { useMemo, useState } from "react";

import {
  diffLines,
  fold,
  languageOf,
  parsePatch,
  readChanges,
  tally,
  type Row,
} from "../diff";
import { highlight, useGrammar } from "../syntax";

export { isEditShape } from "../diff";

/**
 * The change, wherever it is being read.
 *
 * One component serves all four places a diff appears — the approval dock,
 * the transcript, the inspector, and a ```diff fence — because they are the
 * same question asked at different sizes, and two implementations would drift
 * on the detail that matters (which lines are actually going to be written).
 *
 * `dense` is the only difference between them: in the transcript a diff sits
 * inside a conversation and yields to it, while in the inspector it *is* the
 * content and gets line numbers, a split view and full height.
 *
 * Lines carry a `−` / `+` glyph as well as a tint, so the two never depend on
 * colour alone.
 */
export function Diff({
  input,
  dense = false,
}: {
  input: unknown;
  dense?: boolean;
}) {
  const changes = useMemo(() => readChanges(input), [input]);
  if (changes.length === 0) return null;

  return (
    <div className={`diff-set${dense ? " is-dense" : ""}`}>
      {changes.map((change, index) => (
        <DiffView
          key={index}
          rows={change.rows}
          language={languageOf(change.path)}
          path={changes.length > 1 ? change.path : null}
          dense={dense}
        />
      ))}
    </div>
  );
}

/** A unified patch that arrived as text, e.g. a ```diff fence. */
export function Patch({ text }: { text: string }) {
  const rows = useMemo(() => parsePatch(text), [text]);
  return <DiffView rows={rows} language="" path={null} dense={false} />;
}

/** Before/after strings, for callers that already have both. */
export function TextDiff({
  before,
  after,
  language = "",
}: {
  before: string;
  after: string;
  language?: string;
}) {
  const rows = useMemo(() => diffLines(before, after), [before, after]);
  return <DiffView rows={rows} language={language} path={null} dense={false} />;
}

function DiffView({
  rows,
  language,
  path,
  dense,
}: {
  rows: Row[];
  language: string;
  path: string | null;
  dense: boolean;
}) {
  const [split, setSplit] = useState(false);
  // Held once for the whole diff rather than per line: every row wants the same
  // grammar, and this is the subscription that repaints them all when it lands.
  useGrammar(language);
  const counts = useMemo(() => tally(rows), [rows]);
  // A dense diff inside a conversation shows only what changed; the full view
  // keeps more of the surrounding file because there it is the thing being read.
  const folded = useMemo(() => fold(rows, dense ? 2 : 4), [rows, dense]);
  const numbered = rows.some((row) => row.before !== null || row.after !== null);

  return (
    <div className={`diff${dense ? " is-dense" : ""}`}>
      {(path || !dense) && (
        <div className="diff-head">
          {path && <span className="diff-path">{path}</span>}
          <span className="diff-tally">
            <span className="diff-added">+{counts.added}</span>
            <span className="diff-removed">−{counts.removed}</span>
          </span>
          {!dense && rows.length > 0 && (
            <div className="segmented segmented-xs" role="group">
              <button
                className={split ? undefined : "is-on"}
                onClick={() => setSplit(false)}
                aria-pressed={!split}
              >
                unified
              </button>
              <button
                className={split ? "is-on" : undefined}
                onClick={() => setSplit(true)}
                aria-pressed={split}
              >
                split
              </button>
            </div>
          )}
        </div>
      )}

      {split ? (
        <SplitBody rows={folded} language={language} numbered={numbered} />
      ) : (
        <UnifiedBody rows={folded} language={language} numbered={numbered && !dense} />
      )}
    </div>
  );
}

type Folded = Row | { kind: "gap"; count: number };

function UnifiedBody({
  rows,
  language,
  numbered,
}: {
  rows: Folded[];
  language: string;
  numbered: boolean;
}) {
  return (
    <div className={`diff-body${numbered ? " is-numbered" : ""}`}>
      {rows.map((row, index) =>
        row.kind === "gap" ? (
          <Gap key={index} count={row.count} />
        ) : (
          <div className={`diff-line diff-${row.kind}`} key={index}>
            {numbered && (
              <>
                <span className="diff-num">{row.before ?? ""}</span>
                <span className="diff-num">{row.after ?? ""}</span>
              </>
            )}
            <span className="diff-sign" aria-hidden>
              {SIGN[row.kind]}
            </span>
            <span className="diff-text">
              <Source text={row.text} language={row.kind === "meta" ? "" : language} />
            </span>
          </div>
        ),
      )}
    </div>
  );
}

function SplitBody({
  rows,
  language,
  numbered,
}: {
  rows: Folded[];
  language: string;
  numbered: boolean;
}) {
  return (
    <div className={`diff-body diff-split${numbered ? " is-numbered" : ""}`}>
      {pair(rows).map((row, index) =>
        row.gap !== undefined ? (
          <Gap key={index} count={row.gap} wide />
        ) : (
          <div className="diff-pair" key={index}>
            <Side row={row.left} side="left" language={language} numbered={numbered} />
            <Side row={row.right} side="right" language={language} numbered={numbered} />
          </div>
        ),
      )}
    </div>
  );
}

function Side({
  row,
  side,
  language,
  numbered,
}: {
  row: Row | null;
  side: "left" | "right";
  language: string;
  numbered: boolean;
}) {
  if (!row) return <div className="diff-line diff-blank" aria-hidden />;
  return (
    <div className={`diff-line diff-${row.kind}`}>
      {numbered && <span className="diff-num">{(side === "left" ? row.before : row.after) ?? ""}</span>}
      <span className="diff-sign" aria-hidden>
        {SIGN[row.kind]}
      </span>
      <span className="diff-text">
        <Source text={row.text} language={row.kind === "meta" ? "" : language} />
      </span>
    </div>
  );
}

/** Pairs removals with the additions that replaced them, so a changed line sits
 *  opposite its own replacement instead of drifting down the column. */
function pair(rows: Folded[]): { left: Row | null; right: Row | null; gap?: number }[] {
  const out: { left: Row | null; right: Row | null; gap?: number }[] = [];
  let at = 0;

  while (at < rows.length) {
    const row = rows[at];
    if (row.kind === "gap") {
      out.push({ left: null, right: null, gap: row.count });
      at += 1;
      continue;
    }
    if (row.kind === "same" || row.kind === "meta") {
      out.push({ left: row, right: row });
      at += 1;
      continue;
    }

    const removed: Row[] = [];
    const added: Row[] = [];
    while (at < rows.length && rows[at].kind === "del") removed.push(rows[at++] as Row);
    while (at < rows.length && rows[at].kind === "add") added.push(rows[at++] as Row);
    const height = Math.max(removed.length, added.length);
    for (let index = 0; index < height; index += 1) {
      out.push({ left: removed[index] ?? null, right: added[index] ?? null });
    }
  }
  return out;
}

function Gap({ count, wide }: { count: number; wide?: boolean }) {
  return (
    <div className={`diff-gap${wide ? " is-wide" : ""}`}>
      {count} unchanged {count === 1 ? "line" : "lines"}
    </div>
  );
}

/** A single line, highlighted once its grammar is loaded (`DiffView` holds the
 *  subscription that brings it). A line at a time is deliberate here: a diff
 *  body is not a program — its two sides interleave and its hunks skip — so
 *  tokenising it as one document would carry state across a gap that is not
 *  really there. */
function Source({ text, language }: { text: string; language: string }) {
  const tokens = language ? highlight(text, language) : null;
  if (!tokens) return <>{text || " "}</>;
  return (
    <>
      {tokens.map((token, index) =>
        token.kind === "plain" ? (
          token.text
        ) : (
          <span className={`tok-${token.kind}`} key={index}>
            {token.text}
          </span>
        ),
      )}
    </>
  );
}

const SIGN: Record<Row["kind"], string> = {
  add: "+",
  del: "−",
  same: " ",
  meta: " ",
};
