import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import { invoke } from "@ipc";
import { getDocument, GlobalWorkerOptions, TextLayer, type PDFDocumentProxy } from "pdfjs-dist";
import workerUrl from "pdfjs-dist/build/pdf.worker.mjs?url";

import { useSession } from "./session";
import { basename } from "./show";
import { paperPrompt, summarizePaperPrompt, type PaperAction } from "./paperPrompt";
import { ChevronRight, ChevronDown, SparkleIcon, ListTreeIcon, ZoomInIcon, ZoomOutIcon } from "./components/Icons";

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

type OutlineItem = {
  title: string;
  dest: unknown;
  items: OutlineItem[];
};

async function resolveOutlinePage(doc: PDFDocumentProxy, dest: unknown): Promise<number | null> {
  try {
    let ref = dest;
    if (typeof dest === "string") ref = await doc.getDestination(dest);
    if (!Array.isArray(ref) || ref.length === 0) return null;
    const index = await doc.getPageIndex(ref[0]);
    return index + 1;
  } catch {
    return null;
  }
}

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
  const shell = useRef<HTMLDivElement>(null);
  const [url, setUrl] = useState<string | null>(null);
  const [document, setDocument] = useState<PDFDocumentProxy | null>(null);
  const [pageNumber, setPageNumber] = useState(1);
  const [scale, setScale] = useState(1);
  const [menu, setMenu] = useState<SelectionMenu | null>(null);
  const [status, setStatus] = useState<Status>({ kind: "loading", detail: "preparing PDF" });
  const [outline, setOutline] = useState<OutlineItem[] | null>(null);
  const [outlineOpen, setOutlineOpen] = useState(false);

  useEffect(() => {
    let live = true;
    setUrl(null);
    setDocument(null);
    setPageNumber(1);
    setMenu(null);
    setOutline(null);
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
        loaded.getOutline().then((items) => {
          if (live && items && items.length > 0) setOutline(items as OutlineItem[]);
        });
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

    const shellEl = shell.current;
    if (shellEl) shellEl.style.setProperty("--total-scale-factor", String(scale));

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
        if (live) {
          setStatus({ kind: "ready" });
          stage.current?.scrollTo(0, 0);
        }
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

  const summarizePaper = useCallback(() => {
    if (!onPrompt) return;
    onPrompt(summarizePaperPrompt(path));
  }, [onPrompt, path]);

  const goToPage = useCallback(
    (page: number) => {
      const total = document?.numPages ?? 0;
      if (total === 0) return;
      setPageNumber(Math.max(1, Math.min(total, page)));
    },
    [document],
  );

  const jumpToOutlineDest = useCallback(
    async (dest: unknown) => {
      if (!document) return;
      const page = await resolveOutlinePage(document, dest);
      if (page !== null) goToPage(page);
    },
    [document, goToPage],
  );

  useEffect(() => {
    const layer = textLayer.current;
    if (!layer) return;
    const onDown = (event: MouseEvent) => {
      if (event.button !== 0) return;
      const rect = layer.getBoundingClientRect();
      if (rect.width === 0) return;
      const clickedRight = (event.clientX - rect.left) > rect.width / 2;

      const muted: HTMLElement[] = [];
      for (const span of layer.querySelectorAll<HTMLElement>("span")) {
        const left = parseFloat(span.style.left);
        if (Number.isNaN(left)) continue;
        if ((left >= 50) !== clickedRight) {
          span.style.userSelect = "none";
          muted.push(span);
        }
      }
      if (muted.length === 0) return;

      const restore = () => {
        for (const el of muted) el.style.userSelect = "";
        window.removeEventListener("mouseup", restore, true);
      };
      window.addEventListener("mouseup", restore, true);
    };
    layer.addEventListener("mousedown", onDown);
    return () => layer.removeEventListener("mousedown", onDown);
  }, []);

  const zoomIn = useCallback(() => setScale((at) => Math.min(MAX_SCALE, roundScale(at + SCALE_STEP))), []);
  const zoomOut = useCallback(() => setScale((at) => Math.max(MIN_SCALE, roundScale(at - SCALE_STEP))), []);

  useEffect(() => {
    const el = stage.current;
    if (!el) return;
    const total = document?.numPages ?? 0;

    const onKey = (event: KeyboardEvent) => {
      const mod = event.ctrlKey || event.metaKey;
      if (mod && !event.altKey) {
        if (event.key === "=" || event.key === "+") {
          event.preventDefault();
          zoomIn();
        } else if (event.key === "-") {
          event.preventDefault();
          zoomOut();
        } else if (event.key === "0") {
          event.preventDefault();
          setScale(1);
        }
        return;
      }
      if (event.altKey || mod) return;
      switch (event.key) {
        case "ArrowLeft":
        case "PageUp":
          event.preventDefault();
          setPageNumber((at) => Math.max(1, at - 1));
          break;
        case "ArrowRight":
        case "PageDown":
          event.preventDefault();
          setPageNumber((at) => Math.min(total || at, at + 1));
          break;
        case "Home":
          event.preventDefault();
          setPageNumber(1);
          break;
        case "End":
          if (total > 0) {
            event.preventDefault();
            setPageNumber(total);
          }
          break;
      }
    };

    const onWheel = (event: WheelEvent) => {
      if (event.ctrlKey || event.metaKey) {
        event.preventDefault();
        if (event.deltaY < 0) zoomIn();
        else if (event.deltaY > 0) zoomOut();
        return;
      }
      const threshold = 30;
      if (event.deltaY > 0 && el.scrollTop + el.clientHeight >= el.scrollHeight - threshold) {
        if (total > 0) setPageNumber((at) => Math.min(total, at + 1));
      } else if (event.deltaY < 0 && el.scrollTop <= threshold) {
        setPageNumber((at) => Math.max(1, at - 1));
      }
    };

    el.addEventListener("keydown", onKey);
    el.addEventListener("wheel", onWheel, { passive: false });
    return () => {
      el.removeEventListener("keydown", onKey);
      el.removeEventListener("wheel", onWheel);
    };
  }, [zoomIn, zoomOut, document]);

  const pages = document?.numPages ?? 0;
  const canPrev = pageNumber > 1;
  const canNext = pages > 0 && pageNumber < pages;
  const ready = status.kind === "ready";

  return (
    <div className="paper-view">
      <div className="paper-bar">
        <p className="paper-title" title={path}>{basename(path)}</p>
        <div className="paper-controls" aria-label="PDF controls">
          {outline && (
            <button
              type="button"
              className="paper-btn"
              aria-expanded={outlineOpen}
              aria-label="Toggle outline"
              title="Outline"
              onClick={() => setOutlineOpen((v) => !v)}
            >
              <ListTreeIcon size={15} />
            </button>
          )}
          {onPrompt && (
            <button
              type="button"
              className="paper-btn"
              onClick={summarizePaper}
              disabled={!ready}
              aria-label="Summarize paper"
              title="Summarize this paper"
            >
              <SparkleIcon size={15} />
            </button>
          )}
          <span className="paper-sep" />
          <button type="button" className="paper-btn" onClick={() => setPageNumber((at) => Math.max(1, at - 1))} disabled={!canPrev} aria-label="Previous page" title="Previous page">
            <ChevronRight size={15} className="paper-icon-flip" />
          </button>
          <span className="paper-page">
            {pageNumber}{pages ? ` / ${pages}` : ""}
          </span>
          <button type="button" className="paper-btn" onClick={() => setPageNumber((at) => Math.min(pages || at, at + 1))} disabled={!canNext} aria-label="Next page" title="Next page">
            <ChevronRight size={15} />
          </button>
          <span className="paper-sep" />
          <button type="button" className="paper-btn" onClick={zoomOut} disabled={scale <= MIN_SCALE} aria-label="Zoom out" title="Zoom out">
            <ZoomOutIcon size={15} />
          </button>
          <span className="paper-page">{Math.round(scale * 100)}%</span>
          <button type="button" className="paper-btn" onClick={zoomIn} disabled={scale >= MAX_SCALE} aria-label="Zoom in" title="Zoom in">
            <ZoomInIcon size={15} />
          </button>
        </div>
      </div>

      {status.kind === "failed" && <p className="inspect-empty">{status.detail}</p>}
      <div className="paper-body">
        {outlineOpen && outline && (
          <nav className="paper-outline" aria-label="Document outline">
            <OutlineTree items={outline} onJump={jumpToOutlineDest} currentPage={pageNumber} doc={document} />
          </nav>
        )}
        <div
          ref={stage}
          className="paper-stage"
          tabIndex={-1}
          aria-busy={status.kind === "loading"}
          onMouseUp={refreshSelection}
          onKeyUp={refreshSelection}
          onScroll={() => setMenu(null)}
        >
          {status.kind === "loading" && <p className="paper-status">{status.detail}…</p>}
          <div ref={shell} className="paper-page-shell">
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
    </div>
  );
}

function OutlineTree({
  items,
  onJump,
  currentPage,
  doc,
  depth = 0,
}: {
  items: OutlineItem[];
  onJump: (dest: unknown) => void;
  currentPage: number;
  doc: PDFDocumentProxy | null;
  depth?: number;
}) {
  return (
    <ul className="paper-outline-list" style={depth > 0 ? { paddingInlineStart: "var(--s-3)" } : undefined}>
      {items.map((item, i) => (
        <OutlineNode key={i} item={item} onJump={onJump} currentPage={currentPage} doc={doc} depth={depth} />
      ))}
    </ul>
  );
}

function OutlineNode({
  item,
  onJump,
  currentPage,
  doc,
  depth,
}: {
  item: OutlineItem;
  onJump: (dest: unknown) => void;
  currentPage: number;
  doc: PDFDocumentProxy | null;
  depth: number;
}) {
  const [expanded, setExpanded] = useState(depth < 1);
  const [resolvedPage, setResolvedPage] = useState<number | null>(null);
  const hasChildren = item.items && item.items.length > 0;

  useEffect(() => {
    if (!doc || !item.dest) return;
    let live = true;
    resolveOutlinePage(doc, item.dest).then((p) => {
      if (live) setResolvedPage(p);
    });
    return () => { live = false; };
  }, [doc, item.dest]);

  const isCurrent = resolvedPage !== null && resolvedPage === currentPage;

  return (
    <li className="paper-outline-item">
      <div className="paper-outline-row">
        {hasChildren ? (
          <button
            type="button"
            className="paper-outline-toggle"
            onClick={() => setExpanded((v) => !v)}
            aria-expanded={expanded}
          >
            {expanded ? <ChevronDown size={12} /> : <ChevronRight size={12} />}
          </button>
        ) : (
          <span className="paper-outline-toggle-spacer" />
        )}
        <button
          type="button"
          className={`paper-outline-link${isCurrent ? " is-current" : ""}`}
          onClick={() => onJump(item.dest)}
          title={item.title}
        >
          {item.title}
        </button>
      </div>
      {hasChildren && expanded && (
        <OutlineTree items={item.items} onJump={onJump} currentPage={currentPage} doc={doc} depth={depth + 1} />
      )}
    </li>
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
