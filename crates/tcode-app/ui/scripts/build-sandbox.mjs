// Builds the artifact sandbox: one classic IIFE per entry, into `public/`.
// See vite.sandbox.config.ts for why they cannot be modules or one bundle.
import { execFileSync } from "node:child_process";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const vite = join(root, "node_modules", "vite", "bin", "vite.js");

for (const entry of ["sandbox", "sandbox-echarts", "sandbox-mermaid"]) {
  execFileSync(process.execPath, [vite, "build", "--config", "vite.sandbox.config.ts"], {
    cwd: root,
    stdio: "inherit",
    env: { ...process.env, SANDBOX_ENTRY: entry },
  });
}
