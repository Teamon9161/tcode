import { defineConfig } from "vite";

/**
 * The sandbox document's scripts, built as **classic** scripts.
 *
 * This is not a style preference. The artifact frame is deliberately an opaque
 * origin (`sandbox="allow-scripts"` with no `allow-same-origin`), and module
 * scripts are always fetched in CORS mode — from an opaque origin that means an
 * `Origin: null` request the server must explicitly allow. Measured in a
 * sandboxed frame under `default-src 'self'`:
 *
 *   classic <script src>          → runs
 *   <script type="module">        → does NOT run
 *   <script type="module"> + ACAO → runs
 *
 * Both Vite's dev server and Tauri's asset protocol would have to send
 * `Access-Control-Allow-Origin` for the module form to work, and a mistake
 * there fails in the shipped app rather than in development. A classic script
 * is a no-cors fetch and depends on nothing.
 *
 * An IIFE cannot code-split, so instead of one bundle there is one per artifact
 * kind, injected on first use by the bootstrap. That keeps the frame's fixed
 * cost small — a conversation that never draws a chart never loads echarts —
 * where a single bundle would have been 4.6 MB parsed in every frame.
 *
 * Output lands in `public/`, which the main build copies verbatim, so the
 * sandbox never enters the app's module graph.
 *
 * Run once per entry: `SANDBOX_ENTRY=<name> vite build --config …`.
 */
const ENTRIES: Record<string, string> = {
  // The bootstrap. Handles plain HTML artifacts by itself and injects the rest.
  sandbox: "src/sandbox/main.ts",
  "sandbox-echarts": "src/sandbox/echarts.ts",
  "sandbox-mermaid": "src/sandbox/mermaid.ts",
};

const name = process.env.SANDBOX_ENTRY ?? "sandbox";
const entry = ENTRIES[name];
if (!entry) {
  throw new Error(`unknown SANDBOX_ENTRY '${name}'; expected one of ${Object.keys(ENTRIES).join(", ")}`);
}

export default defineConfig({
  // `public/` is the output here, so copying it into itself must be off.
  publicDir: false,
  // Library builds get no environment shim, and echarts reads
  // `process.env.NODE_ENV` at module scope — without this it throws
  // `process is not defined` before it can register anything, which surfaces as
  // a renderer that loaded but does not exist.
  define: { "process.env.NODE_ENV": JSON.stringify("production") },
  build: {
    outDir: "public",
    emptyOutDir: false,
    cssCodeSplit: false,
    lib: {
      entry,
      name: name.replace(/-/g, "_"),
      formats: ["iife"],
      fileName: () => `${name}.js`,
    },
    rollupOptions: {
      // Required for `iife`: there is nowhere to split a chunk out to.
      output: { inlineDynamicImports: true, assetFileNames: `${name}.[ext]` },
    },
  },
});
