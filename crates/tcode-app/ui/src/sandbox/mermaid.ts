import mermaid from "mermaid";

import { register } from "./runtime";

/** Diagrams. Loaded only when a conversation actually produces one. */
register("mermaid", async (stage, source, token) => {
  mermaid.initialize({
    startOnLoad: false,
    // mermaid's own label sanitiser. Kept strict even though the frame already
    // contains the damage — two cheap layers, and strict is the setting mermaid
    // tests hardest.
    securityLevel: "strict",
    theme: "base",
    fontFamily: token("--font-ui"),
    themeVariables: {
      background: "transparent",
      primaryColor: token("--sunken"),
      primaryTextColor: token("--ink"),
      primaryBorderColor: token("--line"),
      lineColor: token("--muted"),
      secondaryColor: token("--bg"),
      tertiaryColor: token("--sunken"),
    },
  });

  const { svg } = await mermaid.render(`m${Date.now()}`, source);
  stage.innerHTML = svg;
  const node = stage.querySelector("svg");
  if (node) {
    node.removeAttribute("height");
    node.style.maxWidth = "100%";
  }
});
