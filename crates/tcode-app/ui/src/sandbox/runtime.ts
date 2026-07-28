/**
 * The contract between the sandbox bootstrap and a renderer bundle.
 *
 * Renderers are separate classic scripts, injected on demand (see
 * `vite.sandbox.config.ts`). They cannot be `import`ed, because a module fetch
 * from this frame's opaque origin needs CORS the server may not grant — so the
 * link between them is this global, registered on load.
 */
export type Renderer = (
  stage: HTMLElement,
  source: string,
  /** Resolves a theme token the parent sent across. */
  token: (name: string) => string,
) => void | Promise<void>;

declare global {
  interface Window {
    __tcodeRenderers?: Record<string, Renderer>;
  }
}

export function register(kind: string, renderer: Renderer) {
  window.__tcodeRenderers = { ...window.__tcodeRenderers, [kind]: renderer };
}
