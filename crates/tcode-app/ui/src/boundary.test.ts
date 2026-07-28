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
