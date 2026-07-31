/**
 * The parent ↔ sandbox contract.
 *
 * Rich output the model produces — charts, diagrams, a self-contained HTML
 * artifact — has to become DOM to be seen, and model output is data that
 * routinely carries file contents, fetched pages and MCP results inside it. In
 * this window a script that escapes into the main realm reaches
 * `window.__TAURI__` and therefore an arbitrary command on this machine, so
 * "parse it carefully" is not a sufficient answer.
 *
 * The answer is an execution boundary instead of a discipline: everything in
 * this directory runs inside an `<iframe sandbox="allow-scripts">` **without**
 * `allow-same-origin`. That makes the frame an opaque origin, so `parent.*`,
 * storage and cookies all throw for it (verified: `parent.document`,
 * `parent.__TAURI__` and `localStorage` each raise a DOMException) while its own
 * bundled chunks still load normally under the app's `default-src 'self'`. The
 * renderer inside may use `innerHTML` freely — that is the entire point of
 * putting it here — because the worst outcome it can reach is a spoiled iframe.
 *
 * Consequences worth knowing before changing anything here:
 *
 *  - Messages must be posted with `"*"` as the target origin, because an opaque
 *    origin cannot be named. Identity is therefore established by comparing
 *    `event.source` against the frame's `contentWindow`, never by `event.origin`
 *    (which arrives as `"null"`).
 *  - There is no hello from inside. The parent sends its request on the frame's
 *    `load` event, which is race-free because module scripts are deferred and
 *    have executed by then. An announcement from the frame would be sent exactly
 *    once, and any listener attached after it would wait forever.
 *  - The frame owns no theme. It cannot read the app's stylesheet, so the values
 *    it needs travel across as resolved token values in `theme`.
 *  - Nothing here may gain `allow-same-origin`, and the app's CSP must never
 *    gain `unsafe-eval`; either one collapses the boundary this file exists for.
 */

/** Loaded as a real document rather than `srcdoc`: a srcdoc frame inherits the
 *  parent's CSP, which — combined with the opaque origin — leaves it unable to
 *  run any script at all. */
export const SANDBOX_URL = "sandbox.html";

export type SandboxKind = "mermaid" | "echarts" | "html" | "svg";

/** Resolved token values, since the frame cannot read the app's stylesheet. */
export type SandboxTheme = Record<string, string>;

/** The tokens the renderers inside actually consult. Kept explicit so the
 *  message stays small and so adding one is a deliberate act. */
export const THEME_KEYS = [
  "--bg",
  "--ink",
  "--body",
  "--muted",
  "--faint",
  "--line",
  "--sunken",
  "--brand",
  "--amber",
  "--danger",
  "--font-ui",
  "--font-mono",
  "--text-xs",
  "--text-sm",
] as const;

export type ToSandbox = {
  tcode: "render";
  id: number;
  kind: SandboxKind;
  source: string;
  theme: SandboxTheme;
};

export type FromSandbox =
  | { tcode: "sized"; id: number; height: number }
  | { tcode: "failed"; id: number; message: string };

export function isFromSandbox(data: unknown): data is FromSandbox {
  if (typeof data !== "object" || data === null) return false;
  const message = data as { tcode?: unknown; id?: unknown };
  if (typeof message.id !== "number") return false;
  return message.tcode === "sized" || message.tcode === "failed";
}
