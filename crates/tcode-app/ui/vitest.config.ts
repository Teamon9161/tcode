import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

// Tests exercise the rendering boundary (`src/rich.test.tsx`): hostile markdown
// in, plain text out. They need JSX and a DOM (KaTeX builds nodes), and nothing
// else — no Tauri, no network.
export default defineConfig({
  plugins: [react()],
  test: { environment: "jsdom", include: ["src/**/*.test.{ts,tsx}"] },
});
