import { StrictMode } from "react";
import { createRoot } from "react-dom/client";

import "@fontsource-variable/instrument-sans";
import "@fontsource/ibm-plex-mono/400.css";
import "@fontsource/ibm-plex-mono/500.css";
import "../theme/fonts.css";
import "../theme/base.css";
import "../theme/porcelain.css";
import "../theme/code-themes.css";
import "@xterm/xterm/css/xterm.css";
import "../app.css";
import "./preview.css";

import { loadCodeTheme, setCodeTheme } from "../codeTheme";
import { Preview } from "./Preview";

setCodeTheme(loadCodeTheme());

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <Preview />
  </StrictMode>,
);
