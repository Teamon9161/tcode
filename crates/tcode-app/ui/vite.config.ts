import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// `PREVIEW=1` swaps the Tauri APIs for fixtures and serves `preview.html`, the
// design preview (`npm run preview:ui`). It is a separate mode rather than a
// runtime flag so the shipped bundle cannot contain the mocks.
const preview = process.env.PREVIEW === "1";

// Fixed port and no fallback: Tauri's `devUrl` points here, so a silently
// relocated dev server would show an empty window rather than an error.
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: { port: preview ? 5174 : 5173, strictPort: true },
  build: { outDir: "dist", emptyOutDir: true },
  resolve: {
    alias: preview
      ? {
          "@tauri-apps/api/core": "/src/preview/mock-core.ts",
          "@tauri-apps/api/event": "/src/preview/mock-event.ts",
          "@tauri-apps/plugin-dialog": "/src/preview/mock-dialog.ts",
        }
      : {},
  },
});
