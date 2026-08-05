import { useEffect, useMemo, useRef, useState } from "react";

import {
  SANDBOX_URL,
  THEME_KEYS,
  isFromSandbox,
  type SandboxKind,
  type SandboxTheme,
} from "./sandbox/protocol";
import { asColor } from "./color";

/**
 * A model-authored artifact, rendered behind an execution boundary.
 *
 * The frame is deliberately opaque-origin (`allow-scripts` and nothing else),
 * which is what lets the renderer inside build DOM from model output without
 * that being a route to the app's IPC. See `sandbox/protocol.ts` for why this
 * shape and not `srcdoc`, an inlined bundle, or a sanitiser.
 *
 * Everything the parent knows about the frame arrives by message, including its
 * height — an opaque origin cannot be measured from outside.
 */
export function Sandbox({
  kind,
  source,
  label,
}: {
  kind: SandboxKind;
  source: string;
  label: string;
}) {
  const frame = useRef<HTMLIFrameElement>(null);
  const [height, setHeight] = useState(0);
  const [failure, setFailure] = useState<string | null>(null);

  // One id per request, so a stale frame's late reply cannot resize or fail the
  // frame that replaced it.
  const id = useMemo(() => nextId(), [kind, source]);

  useEffect(() => {
    setFailure(null);
    setHeight(0);

    const onMessage = (event: MessageEvent) => {
      // `event.origin` is `"null"` for an opaque origin and identifies nothing.
      // The window identity is the real check.
      if (event.source !== frame.current?.contentWindow) return;
      if (!isFromSandbox(event.data)) return;

      const message = event.data;
      if (message.id !== id) return;
      if (message.tcode === "sized") setHeight(message.height);
      if (message.tcode === "failed") setFailure(message.message);
    };

    window.addEventListener("message", onMessage);
    return () => window.removeEventListener("message", onMessage);
  }, [id]);

  // The request is sent on `load` rather than on a hello from inside.
  //
  // A handshake looks tidier and is a race: the frame announces itself exactly
  // once, so any listener attached after that announcement waits forever — and
  // under StrictMode's mount/unmount/mount the listener genuinely can miss it.
  // `load` has no such window, because module scripts are deferred and have all
  // executed by the time it fires.
  const send = () => {
    frame.current?.contentWindow?.postMessage(
      { tcode: "render", id, kind, source, theme: readTheme() },
      // An opaque origin cannot be named, so this is the only possible target.
      "*",
    );
  };

  if (failure) {
    return (
      <div className="artifact is-failed">
        <span className="artifact-label">{label}</span>
        <p className="artifact-error">{failure}</p>
        <pre className="artifact-source">{source}</pre>
      </div>
    );
  }

  return (
    <figure className="artifact">
      <figcaption className="artifact-label">{label}</figcaption>
      <iframe
        ref={frame}
        className="artifact-frame"
        // No `allow-same-origin`. Nothing else may be added here.
        sandbox="allow-scripts"
        src={SANDBOX_URL}
        onLoad={send}
        title={label}
        style={height ? { height } : undefined}
        // The frame paints its own content; it is never a tab stop.
        tabIndex={-1}
      />
      {height === 0 && <div className="artifact-loading" aria-hidden />}
    </figure>
  );
}

let counter = 0;
const nextId = () => ++counter;

/** The frame has no stylesheet of its own, so the tokens it draws with are
 *  resolved here and sent across. Values, not names: it cannot resolve a
 *  `var()` it never received.
 *
 *  Colours are normalised to sRGB on the way out. The theme is authored in
 *  OKLCH, and the libraries living in the sandbox parse colours themselves with
 *  their own helpers — mermaid's rejects `oklch(...)` outright. Converting here
 *  keeps the values coming from the theme (the token contract holds) while
 *  handing the frame something every library can read. */
function readTheme(): SandboxTheme {
  const style = getComputedStyle(document.documentElement);
  const theme: SandboxTheme = {};
  for (const key of THEME_KEYS) {
    const value = style.getPropertyValue(key).trim();
    if (value) theme[key] = asColor(value) ?? value;
  }
  return theme;
}

