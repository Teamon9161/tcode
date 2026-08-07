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
// The emulator's structural CSS — cell geometry and the viewport, not colour;
// its palette comes from `--term-*` through `termHost.ts`. Ahead of `app.css`
// so this app's rules for that pane win (`.pane-body.is-term`).
import "@xterm/xterm/css/xterm.css";
import "./app.css";

import { SHELL } from "@ipc";
import { App } from "./App";
import { Boundary } from "./Boundary";

// The title bar is app-drawn (rule 9c). Electron makes a region draggable by
// reading `-webkit-app-region` off the computed style, so the drag surface is
// an attribute (`components/drag.ts`) that `app.css` matches. Set before the
// first paint; the design preview answers "preview" instead, so a preview never
// inherits a drag region it has no window for.
document.documentElement.dataset.shell = SHELL;

const root = document.getElementById("root");
if (!root) throw new Error("index.html lost its #root");

createRoot(root).render(
  <StrictMode>
    {/* Outside `App`, because the errors worth catching include the ones thrown
        while `App` itself is rendering. */}
    <Boundary>
      <App />
    </Boundary>
  </StrictMode>,
);
