/**
 * Line diffing, and reading the shapes the edit tools actually send.
 *
 * The transcript already carries everything needed: `ToolStart` decodes the
 * call's input, and the edit tools name their before/after text in it. So a
 * diff is derived, never fetched — the same property that lets the whole
 * transcript rebuild from a replayed event stream.
 *
 * What is deliberately *not* produced here is an absolute line number. The edit
 * tools match a fragment; where that fragment sits in the file is not in the
 * call, and a plausible-looking wrong line number in a review surface is worse
 * than none. A real unified patch carries its own `@@` numbers and those are
 * parsed and kept.
 */

export type RowKind = "same" | "add" | "del" | "meta";

export type Row = {
  kind: RowKind;
  text: string;
  /** Line number in the original file, when it is actually known. */
  before: number | null;
  /** Line number in the new file, when it is actually known. */
  after: number | null;
};

/** One edited file, in whatever shape the calling tool used. */
export type Change = { path: string | null; rows: Row[] };

const MAX_CELLS = 4_000_000; // ~2000×2000 lines; beyond this, don't try to align.

/** Longest-common-subsequence diff over lines. */
export function diffLines(before: string, after: string): Row[] {
  const a = before.length ? before.split("\n") : [];
  const b = after.length ? after.split("\n") : [];

  // Shared head and tail are the common case for an edit and cost nothing to
  // strip, which is also what keeps the table below inside its budget.
  let head = 0;
  while (head < a.length && head < b.length && a[head] === b[head]) head += 1;
  let tail = 0;
  while (
    tail < a.length - head &&
    tail < b.length - head &&
    a[a.length - 1 - tail] === b[b.length - 1 - tail]
  ) {
    tail += 1;
  }

  const midA = a.slice(head, a.length - tail);
  const midB = b.slice(head, b.length - tail);

  const rows: Row[] = [];
  let beforeAt = 1;
  let afterAt = 1;
  const push = (kind: RowKind, text: string) => {
    rows.push({
      kind,
      text,
      before: kind === "add" ? null : beforeAt,
      after: kind === "del" ? null : afterAt,
    });
    if (kind !== "add") beforeAt += 1;
    if (kind !== "del") afterAt += 1;
  };

  for (let index = 0; index < head; index += 1) push("same", a[index]);

  if (midA.length * midB.length > MAX_CELLS) {
    // Too large to align honestly; show it as a wholesale replacement rather
    // than spending seconds to produce a prettier guess.
    for (const line of midA) push("del", line);
    for (const line of midB) push("add", line);
  } else {
    for (const step of align(midA, midB)) push(step[0], step[1]);
  }

  for (let index = 0; index < tail; index += 1) push("same", b[b.length - tail + index]);
  return rows;
}

function align(a: string[], b: string[]): [RowKind, string][] {
  const rows = a.length;
  const cols = b.length;
  // table[i][j] = LCS length of a[i..] and b[j..]
  const table: number[][] = Array.from({ length: rows + 1 }, () =>
    new Array<number>(cols + 1).fill(0),
  );
  for (let i = rows - 1; i >= 0; i -= 1) {
    for (let j = cols - 1; j >= 0; j -= 1) {
      table[i][j] =
        a[i] === b[j] ? table[i + 1][j + 1] + 1 : Math.max(table[i + 1][j], table[i][j + 1]);
    }
  }

  const out: [RowKind, string][] = [];
  let i = 0;
  let j = 0;
  while (i < rows && j < cols) {
    if (a[i] === b[j]) {
      out.push(["same", a[i]]);
      i += 1;
      j += 1;
    } else if (table[i + 1][j] >= table[i][j + 1]) {
      out.push(["del", a[i]]);
      i += 1;
    } else {
      out.push(["add", b[j]]);
      j += 1;
    }
  }
  while (i < rows) out.push(["del", a[i++]]);
  while (j < cols) out.push(["add", b[j++]]);
  return out;
}

/** Collapses long runs of unchanged lines, keeping `context` around each edit. */
export function fold(rows: Row[], context = 3): (Row | { kind: "gap"; count: number })[] {
  const keep = new Array<boolean>(rows.length).fill(false);
  rows.forEach((row, index) => {
    if (row.kind === "same") return;
    for (let at = index - context; at <= index + context; at += 1) {
      if (at >= 0 && at < rows.length) keep[at] = true;
    }
  });

  const out: (Row | { kind: "gap"; count: number })[] = [];
  let skipped = 0;
  rows.forEach((row, index) => {
    if (keep[index]) {
      if (skipped) {
        out.push({ kind: "gap", count: skipped });
        skipped = 0;
      }
      out.push(row);
    } else {
      skipped += 1;
    }
  });
  if (skipped) out.push({ kind: "gap", count: skipped });
  return out;
}

/** A unified patch, as a ```diff fence or a tool that emits one. */
export function parsePatch(text: string): Row[] {
  const rows: Row[] = [];
  let before = 0;
  let after = 0;

  for (const line of text.replace(/\n$/, "").split("\n")) {
    const header = /^@@\s+-(\d+)(?:,\d+)?\s+\+(\d+)(?:,\d+)?\s+@@/.exec(line);
    if (header) {
      before = Number(header[1]);
      after = Number(header[2]);
      rows.push({ kind: "meta", text: line, before: null, after: null });
      continue;
    }
    if (/^(diff |index |--- |\+\+\+ |new file|deleted file|similarity )/.test(line)) {
      rows.push({ kind: "meta", text: line, before: null, after: null });
      continue;
    }
    if (line.startsWith("+")) {
      rows.push({ kind: "add", text: line.slice(1), before: null, after: after++ });
    } else if (line.startsWith("-")) {
      rows.push({ kind: "del", text: line.slice(1), before: before++, after: null });
    } else {
      const body = line.startsWith(" ") ? line.slice(1) : line;
      rows.push({ kind: "same", text: body, before: before++, after: after++ });
    }
  }
  return rows;
}

/** Reads whatever the calling tool used to describe its change. */
export function readChanges(input: unknown): Change[] {
  if (typeof input !== "object" || input === null) return [];
  const record = input as Record<string, unknown>;
  const path = firstString(record, ["file_path", "path", "notebook_path", "filePath"]);

  // `multi_edit` sends a list; each entry is the same shape as a single edit.
  const edits = record.edits;
  if (Array.isArray(edits)) {
    const changes = edits
      .map((edit) => single(edit as Record<string, unknown>, path))
      .filter((change): change is Change => change !== null);
    if (changes.length) return changes;
  }

  const one = single(record, path);
  return one ? [one] : [];
}

function single(record: Record<string, unknown>, path: string | null): Change | null {
  if (typeof record !== "object" || record === null) return null;

  const patch = firstString(record, ["patch", "diff", "unified_diff"]);
  if (patch) return { path, rows: parsePatch(patch) };

  const before = firstString(record, ["old_string", "old_str", "old"]);
  const after = firstString(record, ["new_string", "new_str", "new", "content", "text"]);
  if (after === null && before === null) return null;

  return { path, rows: diffLines(before ?? "", after ?? "") };
}

function firstString(record: Record<string, unknown>, keys: string[]): string | null {
  for (const key of keys) {
    const value = record[key];
    if (typeof value === "string") return value;
  }
  return null;
}

/** True when this input describes a change worth drawing as a diff. */
export function isEditShape(input: unknown): boolean {
  return readChanges(input).some((change) => change.rows.length > 0);
}

/** How many lines the change adds and removes, for a one-line summary. */
export function tally(rows: Row[]): { added: number; removed: number } {
  let added = 0;
  let removed = 0;
  for (const row of rows) {
    if (row.kind === "add") added += 1;
    if (row.kind === "del") removed += 1;
  }
  return { added, removed };
}

/** The syntax language for a path, for highlighting diff bodies. */
export function languageOf(path: string | null): string {
  if (!path) return "";
  const dot = path.lastIndexOf(".");
  return dot === -1 ? "" : path.slice(dot + 1).toLowerCase();
}
