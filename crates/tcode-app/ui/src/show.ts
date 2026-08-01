import type { SandboxKind } from "./sandbox/protocol";

/**
 * How a file the model asked to `show` is drawn.
 *
 * This is `fences.tsx`'s table with a second entrance. A fenced block and a file
 * are the same question asked two ways — "what kind of thing is this and how
 * should it be drawn" — and the answers overlap almost completely (mermaid,
 * html, svg, markdown). Keeping two tables would mean a diagram rendering one
 * way when the model writes it inline and another way when it writes it to a
 * file, which is the drift the tool exists to avoid: the whole point of `show`
 * is that putting the bytes on disk instead of in the reply changes only the
 * cost, not the result.
 *
 * The key is the extension, because that is what the backend can validate and
 * what the model already chose when it wrote the file. Anything unrecognised is
 * text — a fallback that is always right, which is what lets this list stay
 * short.
 */
export type Shown =
  /** Drawn behind the execution boundary (`Sandbox.tsx`). */
  | { as: "sandbox"; sandbox: SandboxKind }
  /**
   * Loaded by a frame from the app's loopback origin (`Framed.tsx`), as a page
   * rather than as a string of markup.
   *
   * This is where a generated report goes, and the reason is that a report is
   * not markup — it is a document with parts. A plotly file runs a script to
   * draw itself, a quarto page pulls in a stylesheet, a notebook export asks
   * for `./fig1.png`, and half of them fetch their own data. None of those are
   * things the sandbox frame withholds by policy; they are things an opaque
   * origin cannot do at all, and `innerHTML` does not run `<script>` in any
   * origin. Served over an origin, all of it simply works, and the frame is
   * cross-origin to the app — a stronger separation than the sandbox attribute,
   * because it does not depend on an attribute being spelled right.
   */
  | { as: "framed" }
  /** Delimited rows, drawn as a table with a bounded number of them on screen. */
  | { as: "table"; separator: string }
  | { as: "image" }
  /** Prose, through the same restricted markdown renderer the transcript uses. */
  | { as: "doc" }
  | { as: "text" };

/**
 * How the bytes reach the view — the question a loader must answer before it
 * has anything to look at, which is why it lives on the path and not on `Shown`.
 */
export type Load =
  /** Read as text. */
  | "text"
  /** Read as a `data:` URL, because it is not text. */
  | "bytes"
  /** Not read at all: the frame requests it from the origin itself. */
  | "served";

type View = {
  ext: string[];
  load?: Load;
  /**
   * A constant, except where the extension genuinely does not settle it. `.json`
   * is a container, not a kind of thing: an echarts option and a config file
   * share it, so that one entry looks at the body. Everything else is decided
   * before a byte is loaded.
   */
  as: Shown | ((body: string) => Shown);
};

const VIEWS: View[] = [
  { ext: ["html", "htm"], load: "served", as: { as: "framed" } },
  { ext: ["svg"], as: { as: "sandbox", sandbox: "svg" } },
  { ext: ["mmd", "mermaid"], as: { as: "sandbox", sandbox: "mermaid" } },
  { ext: ["csv"], as: { as: "table", separator: "," } },
  { ext: ["tsv"], as: { as: "table", separator: "\t" } },
  {
    ext: ["png", "jpg", "jpeg", "gif", "webp", "avif", "bmp", "ico"],
    load: "bytes",
    as: { as: "image" },
  },
  { ext: ["md", "markdown"], as: { as: "doc" } },
  {
    ext: ["json"],
    as: (body) => (isChartOption(body) ? { as: "sandbox", sandbox: "echarts" } : { as: "text" }),
  },
];

const LOOKUP = new Map<string, View>();
for (const view of VIEWS) {
  for (const ext of view.ext) LOOKUP.set(ext, view);
}

export function extensionOf(path: string): string {
  const name = basename(path);
  const dot = name.lastIndexOf(".");
  return dot <= 0 ? "" : name.slice(dot + 1).toLowerCase();
}

/** How this file's bytes reach the view. Answerable from the path alone, which
 *  is what the loader needs before it has anything to look at. */
export function loadOf(path: string): Load {
  return LOOKUP.get(extensionOf(path))?.load ?? "text";
}

/** Whether this file has to arrive as a `data:` URL. */
export function isBinary(path: string): boolean {
  return loadOf(path) === "bytes";
}

/** Whether nothing needs loading: the frame fetches the file itself, so a
 *  reader that pre-reads it would only be paying for bytes it discards — and,
 *  for a large report, truncating them on the way. */
export function isServed(path: string): boolean {
  return loadOf(path) === "served";
}

/** How to draw it, once the body is in hand. */
export function shownAs(path: string, body: string): Shown {
  const view = LOOKUP.get(extensionOf(path));
  if (!view) return { as: "text" };
  return typeof view.as === "function" ? view.as(body) : view.as;
}

/** The pane's caption when the call did not supply one. */
export function basename(path: string): string {
  const cut = Math.max(path.lastIndexOf("/"), path.lastIndexOf("\\"));
  return cut === -1 ? path : path.slice(cut + 1);
}

/** An echarts spec, as opposed to any other JSON. Recognised by the two fields
 *  every chart has to have something in — no chart is drawn from a config file
 *  by accident, and a spec that is not recognised still shows as its own text. */
function isChartOption(body: string): boolean {
  let parsed: unknown;
  try {
    parsed = JSON.parse(body);
  } catch {
    return false;
  }
  if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) return false;
  const option = parsed as Record<string, unknown>;
  return "series" in option || "dataset" in option;
}

/**
 * A delimited file, parsed far enough to draw.
 *
 * Quoted fields are handled because real exports have them — a comma inside a
 * bond name would otherwise shift every column after it, and a table that is
 * silently misaligned is worse than no table. Everything past that (type
 * inference, alignment by column type) is the renderer's, not this.
 */
export function parseRows(text: string, separator: string): string[][] {
  const rows: string[][] = [];
  let row: string[] = [];
  let field = "";
  let quoted = false;

  for (let at = 0; at < text.length; at += 1) {
    const char = text[at];
    if (quoted) {
      if (char !== '"') {
        field += char;
      } else if (text[at + 1] === '"') {
        field += '"';
        at += 1;
      } else {
        quoted = false;
      }
      continue;
    }
    if (char === '"' && field === "") {
      quoted = true;
    } else if (char === separator) {
      row.push(field);
      field = "";
    } else if (char === "\n" || char === "\r") {
      // A trailing newline ends the last row rather than opening an empty one.
      if (char === "\r" && text[at + 1] === "\n") at += 1;
      row.push(field);
      rows.push(row);
      row = [];
      field = "";
    } else {
      field += char;
    }
  }
  if (field !== "" || row.length > 0) {
    row.push(field);
    rows.push(row);
  }
  return rows;
}
