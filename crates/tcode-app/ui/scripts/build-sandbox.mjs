// Builds the artifact sandbox: one classic IIFE per entry, into `public/`.
// See vite.sandbox.config.ts for why they cannot be modules or one bundle.
import { execFileSync } from "node:child_process";

for (const entry of ["sandbox", "sandbox-echarts", "sandbox-mermaid"]) {
  execFileSync("npx", ["vite", "build", "--config", "vite.sandbox.config.ts"], {
    stdio: "inherit",
    shell: process.platform === "win32",
    env: { ...process.env, SANDBOX_ENTRY: entry },
  });
}
