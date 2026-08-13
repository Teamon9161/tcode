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
    build: {
      outDir: "dist",
      emptyOutDir: true,
      // Fonts stay files, however small.
      //
      // Vite inlines any asset under 4 kB as a `data:` URI, and the smaller
      // subsets of a variable font are under it — which puts them straight into
      // `@font-face` as `data:font/woff2;…` and straight into the app's CSP,
      // where `default-src 'self'` blocks them. The failure is silent in the
      // worst way: no missing file, no failed request, just a fallback face
      // rendering and a console message nobody reads. It was there before
      // Electron and would have stayed there.
      //
      // The alternative was `font-src 'self' data:`, and it is the wrong one.
      // `data:` appears in this policy exactly once, for `img-src`, because a
      // pasted screenshot has nowhere else to live (`paste.ts`); every other
      // byte the app loads is its own, and a build setting is the cheaper thing
      // to change than the sentence that says so.
      assetsInlineLimit: (file) => (file.endsWith(".woff2") || file.endsWith(".woff") ? false : undefined),
      // The desktop bundle intentionally ships a large self-contained app
      // shell, plus lazy Shiki grammar chunks. Keep the build warning threshold
      // above the expected app shell so `npm start` reports actionable issues
      // instead of repeating the same known size note.
      chunkSizeWarningLimit: 2048,
    },
    // `@ipc` is the whole seam (`src/ipc.ts`), so swapping that one specifier
    // is what puts the design preview on fixtures. It is a bare specifier
    // rather than a relative path precisely so it *can* be aliased: relative
    // imports resolve before an alias ever sees them, and the rule that
    // fixtures never reach the shipped bundle depends on this being the only
    // switch.
    //
    // **One entry, and that is the finished shape.** There used to be two more
    // for the window controls and the folder dialog, which reached their shells
    // directly; they are commands now, answered by whichever shell is hosting
    // the app, so there is nothing left that a second alias could stand in for.
    // A new one here means something bypassed the seam.
    resolve: {
      alias: { "@ipc": preview ? "/src/preview/mock-ipc.ts" : "/src/ipc.ts" },
    },
  };
});
