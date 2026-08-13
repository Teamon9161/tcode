// Build what `npm start` is about to run, when it is stale — so launching the
// app never serves a `ui/dist` that no longer matches `ui/src`.
//
// The Electron shell loads `ui/dist` as static assets (AGENTS.md: "Electron 只
// 加载 ui/dist"), so editing the frontend and pressing start used to show the
// previous build, and the only way to notice was to remember the manual
// `npm --prefix ui run build` step. This check removes the remembering, not
// the building.
//
// The frontend is checked, not unconditionally rebuilt: tsc + vite cost
// seconds, and a dev starts the shell many times a day for runs that changed
// nothing. The sidecar is always passed to `cargo`, whose own fingerprints
// decide — it is ~0.1s when clean and cannot be fooled by timestamps the way
// an mtime check can, which is exactly why the frontend check exists in the
// first place.
"use strict";

const { execFileSync } = require("node:child_process");
const { existsSync, readdirSync, statSync } = require("node:fs");
const { join } = require("node:path");

const root = join(__dirname, "..");

/** Newest mtime under `dir`, skipping build outputs and caches. */
function newestMtime(dir) {
  let newest = 0;
  const walk = (current) => {
    for (const entry of readdirSync(current, { withFileTypes: true })) {
      if (entry.isDirectory()) {
        if (entry.name === "node_modules" || entry.name === "dist" || entry.name === ".git") {
          continue;
        }
        walk(join(current, entry.name));
      } else {
        newest = Math.max(newest, statSync(join(current, entry.name)).mtimeMs);
      }
    }
  };
  walk(dir);
  return newest;
}

const distIndex = join(root, "ui", "dist", "index.html");
const frontendStale =
  !existsSync(distIndex) || statSync(distIndex).mtimeMs < newestMtime(join(root, "ui"));

if (frontendStale) {
  console.log("ui/dist is stale — building the frontend…");
  execFileSync("npm", ["--silent", "--prefix", "ui", "run", "build"], { cwd: root, stdio: "inherit", shell: true });
} else {
  console.log("ui/dist is up to date.");
}

execFileSync("cargo", ["build", "--bin", "tcode-sidecar"], {
  cwd: root,
  stdio: "inherit",
  shell: true,
});
