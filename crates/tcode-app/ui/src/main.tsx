import { StrictMode } from "react";
import { createRoot } from "react-dom/client";

// Fonts are bundled, never fetched: the webview has no network entitlement for
// them and must not gain one (DESIGN.md § Typography).
import "@fontsource-variable/instrument-sans";
import "@fontsource/ibm-plex-mono/400.css";
import "@fontsource/ibm-plex-mono/500.css";

// Order matters: base.css declares the token contract with derived fallbacks,
// the theme assigns the real values on top, and components read tokens only.
// Swapping the theme import here swaps the entire look.
import "./theme/base.css";
import "./theme/porcelain.css";
import "./app.css";

import { App } from "./App";

const root = document.getElementById("root");
if (!root) throw new Error("index.html lost its #root");

createRoot(root).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
