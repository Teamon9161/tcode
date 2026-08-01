import { defineConfig, type Plugin } from "vite";
import react from "@vitejs/plugin-react";

// Mode `preview` swaps the Tauri APIs for fixtures and serves `preview.html`,
// the design preview (`npm run preview:ui`). It is a build-time mode rather
// than a runtime flag so the shipped bundle cannot contain the mocks.
//
// Vite's own `--mode` rather than an environment variable, and that is not
// cosmetic: `PREVIEW=1 vite` is POSIX shell syntax for setting one, so the
// script carrying it did not run at all on Windows — in cmd and PowerShell it
// is a parse error, not a variable. A CLI flag is the same string on every
// platform.
/**
 * In preview mode, `/` *is* the design preview.
 *
 * Without this the root still serves `index.html`, the real app — which in this
 * mode has fixtures aliased in but no backend answering the commands the app
 * shell makes on startup, so it renders a blank window with a warning in the
 * console. That is indistinguishable from "the preview is broken", and it is
 * one wrong URL away at all times: any bookmark, any refresh that dropped the
 * path, anyone typing the host vite prints on startup.
 *
 * The query survives the rewrite because the scene lives there (`?scene=…`).
 */
function previewRoot(): Plugin {
  return {
    name: "tcode-preview-root",
    configureServer(server) {
      server.middlewares.use((request, _response, next) => {
        if (request.url === "/" || request.url?.startsWith("/?")) {
          request.url = `/preview.html${request.url.slice(1)}`;
        }
        next();
      });
    },
  };
}

export default defineConfig(({ mode }) => {
  const preview = mode === "preview";

  // Fixed port and no fallback: Tauri's `devUrl` points here, so a silently
  // relocated dev server would show an empty window rather than an error.
  return {
    plugins: [react(), ...(preview ? [previewRoot()] : [])],
    clearScreen: false,
    server: { port: preview ? 5174 : 5173, strictPort: true },
    // The artifact sandbox lives in `public/` and is built separately
    // (`vite.sandbox.config.ts`), so it is copied verbatim and never enters
    // this module graph. It has to be a real document rather than a `srcdoc`
    // string — a srcdoc frame inherits this page's CSP and, being an opaque
    // origin, then cannot run any script at all (see
    // `src/sandbox/protocol.ts`).
    build: { outDir: "dist", emptyOutDir: true },
    resolve: {
      alias: preview
        ? {
            "@tauri-apps/api/core": "/src/preview/mock-core.ts",
            "@tauri-apps/api/event": "/src/preview/mock-event.ts",
            "@tauri-apps/api/window": "/src/preview/mock-window.ts",
            "@tauri-apps/plugin-dialog": "/src/preview/mock-dialog.ts",
          }
        : {},
    },
  };
});
