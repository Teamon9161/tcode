import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Fixed port and no fallback: Tauri's `devUrl` points here, so a silently
// relocated dev server would show an empty window rather than an error.
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: { port: 5173, strictPort: true },
  build: { outDir: "dist", emptyOutDir: true },
});
