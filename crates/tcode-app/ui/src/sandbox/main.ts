/**
 * The sandbox bootstrap.
 *
 * Runs in an opaque origin (see `protocol.ts`), so it may build DOM from model
 * output without that being a path to the app's IPC. It renders one artifact
 * per frame, reports its height back, and never navigates or fetches.
 *
 * It stays deliberately tiny. Renderers that need a library — charts,
 * diagrams — are separate classic scripts injected on first use, so a frame
 * showing a plain HTML artifact never pays for echarts or mermaid, and a
 * conversation with neither never loads either.
 */

import type { FromSandbox, SandboxTheme, ToSandbox } from "./protocol";
import "./runtime";

const stage = document.getElementById("stage") as HTMLDivElement;

function send(message: FromSandbox) {
  parent.postMessage(message, "*");
}

/** Height is reported rather than guessed: the parent has no way to measure
 *  across an opaque origin, and a fixed-height box is what makes embedded
 *  output feel like a foreign body. */
function report(id: number) {
  send({ tcode: "sized", id, height: Math.max(Math.ceil(stage.getBoundingClientRect().height), 24) });
}

function applyTheme(theme: SandboxTheme) {
  for (const [name, value] of Object.entries(theme)) {
    document.documentElement.style.setProperty(name, value);
  }
}

const token = (name: string) =>
  getComputedStyle(document.documentElement).getPropertyValue(name).trim();

/** Injects a renderer bundle once. Classic, not `import()`: a module fetch from
 *  this opaque origin needs a CORS grant that neither the dev server nor the
 *  app's asset protocol is guaranteed to give. */
const loading = new Map<string, Promise<void>>();

function load(kind: string): Promise<void> {
  const already = loading.get(kind);
  if (already) return already;

  const pending = new Promise<void>((resolve, reject) => {
    const script = document.createElement("script");
    script.src = `./sandbox-${kind}.js`;
    script.onload = () => resolve();
    script.onerror = () => reject(new Error(`cannot load the ${kind} renderer`));
    document.head.appendChild(script);
  });
  loading.set(kind, pending);
  return pending;
}

async function render(message: ToSandbox) {
  applyTheme(message.theme);
  stage.replaceChildren();

  if (message.kind === "html") {
    // The whole point of this frame: markup from the model becomes markup here,
    // where the worst it can reach is this document.
    stage.innerHTML = message.source;
    return;
  }

  if (message.kind === "svg") {
    // XML SVG is not HTML. Parsing it in its own namespace handles XML
    // declarations and keeps the root's intrinsic geometry intact before it is
    // imported into this opaque-origin document.
    const image = new DOMParser().parseFromString(message.source, "image/svg+xml");
    const svg = image.documentElement;
    if (svg.localName !== "svg" || image.getElementsByTagName("parsererror").length > 0) {
      throw new Error("the SVG file is not well-formed");
    }
    stage.append(document.importNode(svg, true));
    return;
  }

  await load(message.kind);
  const renderer = window.__tcodeRenderers?.[message.kind];
  if (!renderer) throw new Error(`no renderer registered for ${message.kind}`);
  await renderer(stage, message.source, token);
}

window.addEventListener("message", (event: MessageEvent) => {
  const message = event.data as ToSandbox;
  if (typeof message !== "object" || message === null || message.tcode !== "render") return;

  render(message)
    .then(() => {
      report(message.id);
      // Late layout — a font settling, a chart animating in — would otherwise
      // leave the frame the wrong height for the rest of its life.
      new ResizeObserver(() => report(message.id)).observe(stage);
    })
    .catch((error: unknown) => {
      send({
        tcode: "failed",
        id: message.id,
        message: error instanceof Error ? error.message : String(error),
      });
    });
});
