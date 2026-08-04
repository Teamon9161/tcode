/**
 * Where a link in prose goes.
 *
 * The markdown renderer has always drawn links (`rich.tsx`), and clicking one
 * has always done nothing: an `<a target="_blank">` in this webview asks for a
 * window nobody opens. Meanwhile the window already holds a renderer for every
 * kind of thing those links point at — a browser pane for a page, and the
 * `show` table for a `.csv`, a `.png`, a report. So this is a router, not a
 * viewer: it turns an `href` into one of the two acts the window can already
 * perform, and refuses everything else.
 *
 * **A link is model output, so this decides nothing on its say-so.** A web
 * address goes to the browser pane, whose capability set is empty by
 * construction (rule 9h) — the same place a typed address goes. A path goes to
 * the shown viewer, which re-checks it against `is_viewable_path` in the
 * backend (rule 3) and renders `.html` through the loopback origin exactly as
 * `show` does. Neither route is new ground; both are reached by a *person
 * clicking*, which is the difference that matters, because nothing here opens
 * by itself.
 *
 * Pure and separately tested because the interesting half is the refusals, and
 * they are string work: a scheme this app has no answer for, a bare fragment, a
 * path with `..` in it.
 */
export type LinkTarget =
  /** A page, for the window's one browser pane. */
  | { kind: "web"; url: string }
  /** A file on disk, absolute, for the same viewer `show` opens into. */
  | { kind: "file"; path: string }
  /** Nothing this window can do. The click is swallowed rather than followed:
   *  the alternative is the app's own document navigating somewhere. */
  | { kind: "none" };

/** Schemes with a destination in this window. `mailto:` is deliberately absent
 *  — it is a link to another application, and handing model output to one is a
 *  larger decision than making a chart clickable. */
const WEB = ["http:", "https:"];

export function linkTarget(href: string, cwd: string): LinkTarget {
  const trimmed = href.trim();
  if (!trimmed) return { kind: "none" };
  // A fragment addresses a place inside a document. The transcript is not that
  // document, and following one would only change the app's own URL.
  if (trimmed.startsWith("#")) return { kind: "none" };

  const scheme = schemeOf(trimmed);
  if (scheme) {
    if (WEB.includes(scheme)) return { kind: "web", url: trimmed };
    // `file:` is a path spelled as a URL — a form models emit constantly, and
    // one this window can honour, because the destination is the same viewer.
    if (scheme === "file:") return fileAt(fromFileUrl(trimmed), cwd);
    return { kind: "none" };
  }

  return fileAt(trimmed, cwd);
}

function fileAt(path: string, cwd: string): LinkTarget {
  // A query or fragment on a path is addressing something inside the file, and
  // no viewer here takes an argument. Dropping it opens the file, which is the
  // useful half of what was asked for.
  const bare = decode(path.split(/[?#]/)[0]);
  if (!bare) return { kind: "none" };
  // A path is only openable once it is absolute — the boundary in the backend
  // is lexical containment, so a relative path is not "inside the folder", it
  // is unanswerable. Resolution happens here because this is the only side that
  // knows which conversation was clicked in.
  if (!cwd) return { kind: "none" };
  return { kind: "file", path: resolve(cwd, bare) };
}

function schemeOf(href: string): string | null {
  // Windows paths start `C:\`, which is a scheme to a naive parser and a drive
  // to everyone else. A scheme is at least two characters, which is exactly the
  // distinction, and it is the one the URL spec draws too.
  const match = /^([a-zA-Z][a-zA-Z\d+\-.]+):/.exec(href);
  return match ? `${match[1].toLowerCase()}:` : null;
}

/** `file:///home/me/out.csv` → `/home/me/out.csv`, `file:///C:/tmp/x` → `C:/tmp/x`. */
function fromFileUrl(href: string): string {
  const path = href.replace(/^file:\/\/(localhost)?/i, "");
  return /^\/[a-zA-Z]:/.test(path) ? path.slice(1) : path;
}

function decode(path: string): string {
  try {
    return decodeURIComponent(path);
  } catch {
    // A stray `%` is not an encoding, it is a character in a filename.
    return path;
  }
}

/**
 * A path against the conversation's folder, resolved without touching disk.
 *
 * The separator comes from `cwd` rather than from the platform: the tests run
 * on one machine and both shapes have to be answerable, and `cwd` is the one
 * piece of evidence in hand that says which kind of path this is.
 *
 * `..` past the root is dropped rather than escaping, matching `normalize` in
 * `tcode-tools`'s `show.rs`. The two sides agreeing is not decorative — the
 * backend re-checks containment, and a path that means one thing here and
 * another there is a rejection nobody can act on.
 */
export function resolve(cwd: string, path: string): string {
  const win = isWindows(cwd);
  const unc = win && /^\\\\/.test(cwd);
  const joined = absolute(path, win) ? path : `${cwd}/${path}`;

  const out: string[] = [];
  // On Windows the first segment is the drive (or the server, for a UNC path)
  // and is never something `..` may consume.
  const floor = win ? 1 : 0;
  for (const part of joined.split(/[\\/]+/)) {
    if (part === "" || part === ".") continue;
    if (part === "..") {
      if (out.length > floor) out.pop();
      continue;
    }
    out.push(part);
  }

  if (win) return `${unc ? "\\\\" : ""}${out.join("\\")}`;
  return `/${out.join("/")}`;
}

function isWindows(cwd: string): boolean {
  return /^[a-zA-Z]:[\\/]/.test(cwd) || cwd.startsWith("\\\\");
}

/** A leading `/` is only absolute where it names a root. On Windows it names
 *  nothing — no drive — so it is resolved against the folder instead, and the
 *  backend gets a path it can accept or refuse legibly either way. */
function absolute(path: string, win: boolean): boolean {
  if (win) return /^[a-zA-Z]:[\\/]/.test(path) || path.startsWith("\\\\");
  return path.startsWith("/");
}
