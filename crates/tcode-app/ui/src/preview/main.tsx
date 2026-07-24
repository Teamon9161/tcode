import { StrictMode } from "react";
import { createRoot } from "react-dom/client";

import "@fontsource-variable/instrument-sans";
import "@fontsource/ibm-plex-mono/400.css";
import "@fontsource/ibm-plex-mono/500.css";
import "../theme/base.css";
import "../theme/porcelain.css";
import "../app.css";
import "./preview.css";

import { Preview } from "./Preview";

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <Preview />
  </StrictMode>,
);
