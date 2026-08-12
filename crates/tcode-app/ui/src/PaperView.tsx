import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import { invoke } from "@ipc";
import { getDocument, GlobalWorkerOptions, TextLayer, type PDFDocumentProxy } from "pdfjs-dist";
import workerUrl from "pdfjs-dist/build/pdf.worker.mjs?url";

import { useSession } from "./session";
import { basename } from "./show";
import { paperPrompt, type PaperAction } from "./paperPrompt";

GlobalWorkerOptions.workerSrc = workerUrl;

const MIN_SCALE = 0.6;
const MAX_SCALE = 2.4;
const SCALE_STEP = 0.2;

type Status =
  | { kind: "loading"; detail: string }
  | { kind: "ready" }
  | { kind: "failed"; detail: string };

type SelectionMenu = {
  text: string;
  top: number;
  left: number;
};

export function PaperView({
  path,
  onPrompt,
}: {
  path: string;
  onPrompt?: (prompt: string) => void;
}) {
  const session = useSession();
  const canvas = useRef<HTMLCanvasElement>(null);
  const textLayer = useRef<HTMLDivElement>(null);
  const stage = useRef<HTMLDivElement>(null);
  const [url, setUrl] = useState<string | null>(null);
  const [document, setDocument] = useState<PDFDocumentProxy | null>(null);
  const [pageNumber, setPageNumber] = useState(1);
  const [scale, setScale] = useState(1);
  const [menu, setMenu] = useState<SelectionMenu | null>(null);
  const [status, setStatus] = useState<Status>({ kind: "loading", detail: "preparing PDF" });

  useEffect(() => {
    let live = true;
    setUrl(null);
    setDocument(null);
    setPageNumber(1);
    setMenu(null);
    setStatus({ kind: "loading", detail: "preparing PDF" });

    invoke<string>("serve_url", { session, path })
      .then((served) => {
        if (!live) return;
        setUrl(served);
        setStatus({ kind: "loading", detail: "loading PDF" });
      })
      .catch((error) => {
        if (live) setStatus({ kind: "failed", detail: String(error) });
      });

    return () => {
      live = false;
    };
  }, [path, session]);

  useEffect(() => {
    if (!url) return;
    let live = true;
    const task = getDocument({ url });

    task.promise
      .then((loaded) => {
        if (!live) {
          void loaded.cleanup();
          return;
        }
        setDocument(loaded);
        setStatus({ kind: "ready" });
      })
      .catch((error) => {
        if (live) setStatus({ kind: "failed", detail: String(error) });
      });

    return () => {
      live = false;
      task.destroy();
    };
  }, [url]);

  useEffect(() => {
    return () => {
      if (document) void document.cleanup();
    };
  }, [document]);

  useLayoutEffect(() => {
    if (!document || !canvas.current || !textLayer.current) return;
    let live = true;
    const targetCanvas = canvas.current;
    const targetTextLayer = textLayer.current;
    const context = targetCanvas.getContext("2d");
    if (!context) {
      setStatus({ kind: "failed", detail: "this system cannot create a PDF canvas" });
      return;
    }

    setMenu(null);
    targetTextLayer.replaceChildren();
    setStatus({ kind: "loading", detail: `rendering page ${pageNumber}` });

    let cancelTextLayer: (() => void) | null = null;
    let renderTask: { cancel: () => void; promise: Promise<unknown> } | null = null;

    document
      .getPage(pageNumber)
      .then((page) => {
        if (!live) return undefined;
        const viewport = page.getViewport({ scale });
        const pixelRatio = window.devicePixelRatio || 1;
        targetCanvas.width = Math.floor(viewport.width * pixelRatio);
        targetCanvas.height = Math.floor(viewport.height * pixelRatio);
        targetCanvas.style.width = `${viewport.width}px`;
        targetCanvas.style.height = `${viewport.height}px`;
        targetTextLayer.style.width = `${viewport.width}px`;
        targetTextLayer.style.height = `${viewport.height}px`;
        context.setTransform(pixelRatio, 0, 0, pixelRatio, 0, 0);
        context.clearRect(0, 0, viewport.width, viewport.height);

        renderTask = page.render({ canvas: targetCanvas, canvasContext: context, viewport });
        const text = new TextLayer({
          textContentSource: page.streamTextContent(),
          container: targetTextLayer,
          viewport,
        });
        cancelTextLayer = () => text.cancel();

        return Promise.all([renderTask.promise, text.render()]);
      })
      .then(() => {
        if (live) setStatus({ kind: "ready" });
      })
      .catch((error) => {
        if (!live || isRenderingCancel(error)) return;
        setStatus({ kind: "failed", detail: String(error) });
      });

    return () => {
      live = false;
      renderTask?.cancel();
      cancelTextLayer?.();
    };
  }, [document, pageNumber, scale]);

  const refreshSelection = useCallback(() => {
    const layer = textLayer.current;
    const scroller = stage.current;
    const selection = window.getSelection();
    if (!layer || !scroller || !selection || selection.rangeCount === 0 || selection.isCollapsed) {
      setMenu(null);
      return;
    }
    if (!inside(layer, selection.anchorNode) || !inside(layer, selection.focusNode)) {
      setMenu(null);
      return;
    }

    const text = selection.toString().trim();
    const range = selection.getRangeAt(0);
    const rect = firstUsableRect(range);
    if (!text || !rect) {
      setMenu(null);
      return;
    }

    const stageRect = scroller.getBoundingClientRect();
    setMenu({
      text,
      left: rect.left - stageRect.left + scroller.scrollLeft + rect.width / 2,
      top: rect.top - stageRect.top + scroller.scrollTop,
    });
  }, []);

  const sendPrompt = useCallback(
    (action: PaperAction) => {
      if (!menu || !onPrompt) return;
      onPrompt(paperPrompt(action, path, pageNumber, menu.text));
      setMenu(null);
      window.getSelection()?.removeAllRanges();
    },
    [menu, onPrompt, pageNumber, path],
  );

  const pages = document?.numPages ?? 0;
  const canPrev = pageNumber > 1;
  const canNext = pages > 0 && pageNumber < pages;

  return (
    <div className="paper-view">
      <div className="paper-bar">
        <p className="paper-title" title={path}>{basename(path)}</p>
        <div className="paper-controls" aria-label="PDF controls">
          <button type="button" className="chip" onClick={() => setPageNumber((at) => Math.max(1, at - 1))} disabled={!canPrev}>
            Previous
          </button>
          <span className="paper-page">Page {pageNumber}{pages ? ` of ${pages}` : ""}</span>
          <button type="button" className="chip" onClick={() => setPageNumber((at) => Math.min(pages || at, at + 1))} disabled={!canNext}>
            Next
          </button>
          <button type="button" className="chip" onClick={() => setScale((at) => Math.max(MIN_SCALE, roundScale(at - SCALE_STEP)))} disabled={scale <= MIN_SCALE}>
            −
          </button>
          <span className="paper-page">{Math.round(scale * 100)}%</span>
          <button type="button" className="chip" onClick={() => setScale((at) => Math.min(MAX_SCALE, roundScale(at + SCALE_STEP)))} disabled={scale >= MAX_SCALE}>
            +
          </button>
        </div>
      </div>

      {status.kind === "failed" && <p className="inspect-empty">{status.detail}</p>}
      <div
        ref={stage}
        className="paper-stage"
        aria-busy={status.kind === "loading"}
        onMouseUp={refreshSelection}
        onKeyUp={refreshSelection}
        onScroll={() => setMenu(null)}
      >
        {status.kind === "loading" && <p className="paper-status">{status.detail}…</p>}
        <div className="paper-page-shell">
          <canvas ref={canvas} className="paper-canvas" />
          <div ref={textLayer} className="paper-text-layer" />
        </div>
        {menu && onPrompt && (
          <div
            className="paper-selection-menu"
            style={{ left: menu.left, top: menu.top }}
            onMouseDown={(event) => event.preventDefault()}
          >
            <button type="button" className="chip" onClick={() => sendPrompt("translate")}>
              Translate
            </button>
            <button type="button" className="chip" onClick={() => sendPrompt("explain")}>
              Explain
            </button>
            <button type="button" className="chip" onClick={() => sendPrompt("ask")}>
              Ask
            </button>
          </div>
        )}
      </div>
    </div>
  );
}

function roundScale(value: number): number {
  return Math.round(value * 10) / 10;
}

function isRenderingCancel(error: unknown): boolean {
  return error instanceof Error && /cancel/i.test(error.name);
}

function inside(container: HTMLElement, node: Node | null): boolean {
  return !!node && (node === container || container.contains(node));
}

function firstUsableRect(range: Range): DOMRect | null {
  const rects = Array.from(range.getClientRects()).filter((rect) => rect.width > 0 && rect.height > 0);
  return rects[0] ?? null;
}
