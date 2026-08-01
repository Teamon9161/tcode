import { readdirSync, readFileSync, statSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";

/**
 * The structural half of the rendering boundary.
 *
 * `rich.test.tsx` proves the renderer escapes hostile input. This proves nobody
 * added a second path around it. The rule is easy to state and easy to break by
 * accident — "model output never reaches innerHTML" — so it is checked
 * mechanically rather than remembered.
 *
 * Two files are allowed to do it, each for a reason written down at the site:
 *
 *  - `math.tsx`, because KaTeX's entire output is a markup string and the
 *    boundary is drawn around KaTeX's own options instead (`trust: false`).
 *  - `sandbox/`, because everything in there runs in an opaque origin that
 *    cannot reach this realm — building DOM freely is what it is for.
 */

const SRC = dirname(fileURLToPath(import.meta.url));

const BANNED = [
  { pattern: /dangerouslySetInnerHTML/, name: "dangerouslySetInnerHTML" },
  { pattern: /\.innerHTML\s*=/, name: "innerHTML assignment" },
  { pattern: /\beval\s*\(/, name: "eval" },
  { pattern: /new\s+Function\s*\(/, name: "new Function" },
];

/** Paths whose exemption is documented in the file itself. */
const EXEMPT = [/[\\/]math\.tsx$/, /[\\/]sandbox[\\/]/, /\.test\.tsx?$/];

function sources(dir: string): string[] {
  return readdirSync(dir).flatMap((entry) => {
    const path = join(dir, entry);
    if (statSync(path).isDirectory()) return sources(path);
    return /\.tsx?$/.test(path) ? [path] : [];
  });
}

describe("no second path from model output to markup", () => {
  const files = sources(SRC).filter((path) => !EXEMPT.some((rule) => rule.test(path)));

  it("finds source files to check", () => {
    expect(files.length).toBeGreaterThan(10);
  });

  for (const { pattern, name } of BANNED) {
    it(`does not use ${name}`, () => {
      const offenders = files.filter((path) => pattern.test(readFileSync(path, "utf8")));
      expect(offenders.map((path) => path.slice(SRC.length + 1))).toEqual([]);
    });
  }
});

/**
 * The other structural half: how many frames there are, and which.
 *
 * This app has exactly two, isolated by two different mechanisms, and each
 * mechanism is undone by the attribute the other one needs:
 *
 *  - `Sandbox.tsx` has **no origin**. `allow-same-origin` would give it the
 *    app's, and the app's realm holds `window.__TAURI__`.
 *  - `Framed.tsx` has **its own origin** (loopback), and needs
 *    `allow-same-origin` for a report to fetch its own data — which is safe
 *    precisely because that origin is not this one.
 *
 * A third frame written by someone who copied whichever was nearest is the
 * failure this guards: pasting `Framed`'s attributes onto a frame loading from
 * `'self'` collapses the boundary silently, and nothing about the diff would
 * look wrong. Adding a frame here should require reading this comment first.
 */
describe("frames are only where the boundary is documented", () => {
  const FRAME_FILES = [/[\\/]Sandbox\.tsx$/, /[\\/]Framed\.tsx$/];

  /** Frames as *elements*, not as the prose describing them: both files above
   *  quote their own markup at length in the comments explaining it, and a
   *  check that cannot tell those apart would be one that has to be relaxed. */
  const CREATES_FRAME = [/<iframe[\s/>]/, /createElement\(\s*["'`]iframe/];

  it("has no third frame", () => {
    const offenders = sources(SRC)
      .filter((path) => !/\.test\.tsx?$/.test(path))
      .filter((path) => !FRAME_FILES.some((rule) => rule.test(path)))
      .filter((path) => {
        const source = readFileSync(path, "utf8");
        return CREATES_FRAME.some((rule) => rule.test(stripComments(source)));
      });
    expect(offenders.map((path) => path.slice(SRC.length + 1))).toEqual([]);
  });

  it("keeps the opaque-origin frame opaque", () => {
    // Read off the attribute, so the sentence in the comment above it saying
    // `allow-same-origin` must never appear here does not itself trip this.
    expect(sandboxAttributes(readFileSync(join(SRC, "Sandbox.tsx"), "utf8"))).toEqual([
      "allow-scripts",
    ]);
  });

  /** The served frame's own list is pinned by value in `FileBody.test.tsx`;
   *  here it only has to be a named constant, so there is one place to pin. */
  it("gives the served frame its capabilities from one named list", () => {
    expect(sandboxAttributes(readFileSync(join(SRC, "Framed.tsx"), "utf8"))).toEqual([
      "FRAME_SANDBOX",
    ]);
  });
});

/** Every `sandbox=` attribute's value: the string literal, or the name of the
 *  expression it was given. */
function sandboxAttributes(source: string): string[] {
  return [...stripComments(source).matchAll(/sandbox=(?:"([^"]*)"|\{([A-Za-z_$][\w$]*)\})/g)].map(
    (found) => found[1] ?? found[2],
  );
}

function stripComments(source: string): string {
  return source.replace(/\/\*[\s\S]*?\*\//g, "").replace(/\/\/[^\n]*/g, "");
}
