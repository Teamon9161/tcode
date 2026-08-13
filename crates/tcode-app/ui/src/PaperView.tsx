import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import { invoke } from "@ipc";
import { getDocument, GlobalWorkerOptions, TextLayer, type PDFDocumentProxy } from "pdfjs-dist";
import workerUrl from "pdfjs-dist/build/pdf.worker.mjs?url";

import { useSession } from "./session";
import { basename } from "./show";
import { paperPrompt, summarizePaperPrompt, type PaperAction } from "./paperPrompt";
import { ChevronRight, ChevronDown, SparkleIcon, ListTreeIcon, ZoomInIcon, ZoomOutIcon, HighlighterIcon } from "./components/Icons";

GlobalWorkerOptions.workerSrc = workerUrl;

const MIN_SCALE = 0.6;
const MAX_SCALE = 2.4;
const SCALE_STEP = 0.2;
const PAGE_GAP = 12;
const RENDER_BUFFER = 1;

const HIGHLIGHT_COLORS = ["#facc15", "#86efac", "#93c5fd", "#fca5a5", "#d8b4fe"] as const;

type Status =
  | { kind: "loading"; detail: string }
  | { kind: "ready" }
  | { kind: "failed"; detail: string };

type SelectionMenu = {
  text: string;
  page: number;
  top: number;
  left: number;
};

type OutlineItem = {
  title: string;
  dest: unknown;
  items: OutlineItem[];
};

type PageDimensions = {
  width: number;
  height: number;
};

export type PaperHighlight = {
  id: string;
  pageNumber: number;
  rects: Array<[number, number, number, number]>;
  selectedText: string;
  color: string;
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

function captureSelectionRects(pageShell: HTMLElement, scale: number): Array<[number, number, number, number]> {
  const selection = window.getSelection();
  if (!selection || selection.rangeCount === 0) return [];

  const range = selection.getRangeAt(0);
  const clientRects = Array.from(range.getClientRects()).filter((r) => r.width > 0 && r.height > 0);
  if (clientRects.length === 0) return [];

  const shellRect = pageShell.getBoundingClientRect();
  const pdfRects: Array<[number, number, number, number]> = [];

  for (const cr of clientRects) {
    const x = (cr.left - shellRect.left) / scale;
    const y = (cr.top - shellRect.top) / scale;
    const w = cr.width / scale;
    const h = cr.height / scale;
    pdfRects.push([x, y, w, h]);
  }

  return mergeOverlappingRects(pdfRects);
}

function mergeOverlappingRects(rects: Array<[number, number, number, number]>): Array<[number, number, number, number]> {
  if (rects.length <= 1) return rects;
  const sorted = [...rects].sort((a, b) => a[1] - b[1] || a[0] - b[0]);
  const merged: Array<[number, number, number, number]> = [sorted[0]];
  for (let i = 1; i < sorted.length; i++) {
    const [x, y, w, h] = sorted[i];
    const last = merged[merged.length - 1];
    const [lx, ly, lw, lh] = last;
    if (Math.abs(y - ly) < 2 && x <= lx + lw + 2) {
      const nx = Math.min(lx, x);
      const ny = Math.min(ly, y);
      const nr = Math.max(lx + lw, x + w);
      const nb = Math.max(ly + lh, y + h);
      merged[merged.length - 1] = [nx, ny, nr - nx, nb - ny];
    } else {
      merged.push([x, y, w, h]);
    }
  }
  return merged;
}

let highlightIdCounter = 0;
function nextHighlightId(): string {
  return `hl_${Date.now()}_${highlightIdCounter++}`;
}

export function PaperView({
  path,
  onPrompt,
}: {
  path: string;
  onPrompt?: (prompt: string) => void;
}) {
  const session = useSession();
  const stage = useRef<HTMLDivElement>(null);
  const pageRefs = useRef<Map<number, HTMLDivElement>>(new Map());
  const [url, setUrl] = useState<string | null>(null);
  const [document, setDocument] = useState<PDFDocumentProxy | null>(null);
  const [pageDims, setPageDims] = useState<PageDimensions[]>([]);
  const [pageNumber, setPageNumber] = useState(1);
  const [scale, setScale] = useState(1);
  const [menu, setMenu] = useState<SelectionMenu | null>(null);
  const [status, setStatus] = useState<Status>({ kind: "loading", detail: "preparing PDF" });
  const [outline, setOutline] = useState<OutlineItem[] | null>(null);
  const [outlineOpen, setOutlineOpen] = useState(false);
  const [visiblePages, setVisiblePages] = useState<Set<number>>(new Set([1]));
  const scrollingToPage = useRef(false);
  const [highlights, setHighlights] = useState<PaperHighlight[]>([]);
  const [highlightColor, setHighlightColor] = useState(HIGHLIGHT_COLORS[0]);

  useEffect(() => {
    let live = true;
    setUrl(null);
    setDocument(null);
    setPageDims([]);
    setPageNumber(1);
    setMenu(null);
    setOutline(null);
    setVisiblePages(new Set([1]));
    setHighlights([]);
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

    invoke<PaperHighlight[]>("paper_highlights_load", { session, path })
      .then((loaded) => {
        if (live && loaded) setHighlights(loaded);
      })
      .catch(() => {});

    return () => {
      live = false;
    };
  }, [path, session]);

  useEffect(() => {
    if (!url) return;
    let live = true;
    const task = getDocument({ url });

    task.promise
      .then(async (loaded) => {
        if (!live) {
          void loaded.cleanup();
          return;
        }
        const dims: PageDimensions[] = [];
        for (let i = 1; i <= loaded.numPages; i++) {
          const page = await loaded.getPage(i);
          const vp = page.getViewport({ scale: 1 });
          dims.push({ width: vp.width, height: vp.height });
        }
        if (!live) {
          void loaded.cleanup();
          return;
        }
        setDocument(loaded);
        setPageDims(dims);
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

  // IntersectionObserver to track visible pages
  useEffect(() => {
    const stageEl = stage.current;
    if (!stageEl || pageDims.length === 0) return;

    const ratios = new Map<number, number>();
    const observer = new IntersectionObserver(
      (entries) => {
        for (const entry of entries) {
          const pageNum = Number(entry.target.getAttribute("data-page"));
          if (!pageNum) continue;
          ratios.set(pageNum, entry.intersectionRatio);
          if (entry.intersectionRatio <= 0) ratios.delete(pageNum);
        }

        const visible = new Set<number>();
        let bestPage = 1;
        let bestRatio = 0;
        for (const [num, ratio] of ratios) {
          visible.add(num);
          if (ratio > bestRatio) {
            bestRatio = ratio;
            bestPage = num;
          }
        }

        if (visible.size > 0) {
          setVisiblePages(visible);
          if (!scrollingToPage.current) {
            setPageNumber(bestPage);
          }
        }
      },
      {
        root: stageEl,
        threshold: [0, 0.1, 0.25, 0.5, 0.75, 1],
      },
    );

    for (const [, el] of pageRefs.current) {
      observer.observe(el);
    }

    return () => observer.disconnect();
  }, [pageDims, scale]);

  const refreshSelection = useCallback(() => {
    const scroller = stage.current;
    const selection = window.getSelection();
    if (!scroller || !selection || selection.rangeCount === 0 || selection.isCollapsed) {
      setMenu(null);
      return;
    }

    const anchor = selection.anchorNode;
    if (!anchor) {
      setMenu(null);
      return;
    }

    let selectionPage = 0;
    const anchorEl = anchor instanceof HTMLElement ? anchor : anchor.parentElement;
    if (anchorEl) {
      const shell = anchorEl.closest("[data-page]");
      if (shell) selectionPage = Number(shell.getAttribute("data-page"));
    }
    if (!selectionPage) {
      setMenu(null);
      return;
    }

    const textLayerEl = anchorEl?.closest(".paper-text-layer") ?? anchorEl?.querySelector(".paper-text-layer");
    if (!textLayerEl) {
      setMenu(null);
      return;
    }
    const focusEl = selection.focusNode instanceof HTMLElement ? selection.focusNode : selection.focusNode?.parentElement;
    if (!focusEl?.closest(".paper-text-layer")) {
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
      page: selectionPage,
      left: rect.left - stageRect.left + scroller.scrollLeft + rect.width / 2,
      top: rect.top - stageRect.top + scroller.scrollTop,
    });
  }, []);

  const sendPrompt = useCallback(
    (action: PaperAction) => {
      if (!menu || !onPrompt) return;
      onPrompt(paperPrompt(action, path, menu.page, menu.text));
      setMenu(null);
      window.getSelection()?.removeAllRanges();
    },
    [menu, onPrompt, path],
  );

  const addHighlight = useCallback(() => {
    if (!menu) return;

    const pageShell = pageRefs.current.get(menu.page);
    if (!pageShell) return;

    const rects = captureSelectionRects(pageShell, scale);
    if (rects.length === 0) return;

    const hl: PaperHighlight = {
      id: nextHighlightId(),
      pageNumber: menu.page,
      rects,
      selectedText: menu.text,
      color: highlightColor,
    };

    setHighlights((prev) => {
      const next = [...prev, hl];
      invoke("paper_highlights_save", { session, path, highlights: next }).catch(() => {});
      return next;
    });

    setMenu(null);
    window.getSelection()?.removeAllRanges();
  }, [menu, scale, highlightColor, session, path]);

  const removeHighlight = useCallback(
    (id: string) => {
      setHighlights((prev) => {
        const next = prev.filter((h) => h.id !== id);
        invoke("paper_highlights_save", { session, path, highlights: next }).catch(() => {});
        return next;
      });
    },
    [session, path],
  );

  const summarizePaper = useCallback(() => {
    if (!onPrompt) return;
    onPrompt(summarizePaperPrompt(path));
  }, [onPrompt, path]);

  const goToPage = useCallback(
    (page: number) => {
      const total = pageDims.length;
      if (total === 0) return;
      const target = Math.max(1, Math.min(total, page));
      setPageNumber(target);

      const el = pageRefs.current.get(target);
      if (el && stage.current) {
        scrollingToPage.current = true;
        el.scrollIntoView({ behavior: "smooth", block: "start" });
        setTimeout(() => {
          scrollingToPage.current = false;
        }, 600);
      }
    },
    [pageDims.length],
  );

  const jumpToOutlineDest = useCallback(
    async (dest: unknown) => {
      if (!document) return;
      const page = await resolveOutlinePage(document, dest);
      if (page !== null) goToPage(page);
    },
    [document, goToPage],
  );

  const zoomIn = useCallback(() => setScale((at) => Math.min(MAX_SCALE, roundScale(at + SCALE_STEP))), []);
  const zoomOut = useCallback(() => setScale((at) => Math.max(MIN_SCALE, roundScale(at - SCALE_STEP))), []);

  useEffect(() => {
    const el = stage.current;
    if (!el) return;
    const total = pageDims.length;

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
        case "Home":
          event.preventDefault();
          goToPage(1);
          break;
        case "End":
          if (total > 0) {
            event.preventDefault();
            goToPage(total);
          }
          break;
      }
    };

    const onWheel = (event: WheelEvent) => {
      if (event.ctrlKey || event.metaKey) {
        event.preventDefault();
        if (event.deltaY < 0) zoomIn();
        else if (event.deltaY > 0) zoomOut();
      }
    };

    el.addEventListener("keydown", onKey);
    el.addEventListener("wheel", onWheel, { passive: false });
    return () => {
      el.removeEventListener("keydown", onKey);
      el.removeEventListener("wheel", onWheel);
    };
  }, [zoomIn, zoomOut, pageDims.length, goToPage]);

  const setPageRef = useCallback((pageNum: number, el: HTMLDivElement | null) => {
    if (el) pageRefs.current.set(pageNum, el);
    else pageRefs.current.delete(pageNum);
  }, []);

  const pages = pageDims.length;
  const canPrev = pageNumber > 1;
  const canNext = pages > 0 && pageNumber < pages;
  const ready = status.kind === "ready";

  const renderBuffer = new Set<number>();
  for (const v of visiblePages) {
    for (let i = v - RENDER_BUFFER; i <= v + RENDER_BUFFER; i++) {
      if (i >= 1 && i <= pages) renderBuffer.add(i);
    }
  }

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
          <button type="button" className="paper-btn" onClick={() => goToPage(pageNumber - 1)} disabled={!canPrev} aria-label="Previous page" title="Previous page">
            <ChevronRight size={15} className="paper-icon-flip" />
          </button>
          <span className="paper-page">
            {pageNumber}{pages ? ` / ${pages}` : ""}
          </span>
          <button type="button" className="paper-btn" onClick={() => goToPage(pageNumber + 1)} disabled={!canNext} aria-label="Next page" title="Next page">
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
          <div className="paper-pages">
            {pageDims.map((dims, i) => {
              const num = i + 1;
              const shouldRender = renderBuffer.has(num);
              const pageHighlights = highlights.filter((h) => h.pageNumber === num);
              return (
                <PageSlot
                  key={num}
                  pageNum={num}
                  dims={dims}
                  scale={scale}
                  doc={shouldRender ? document : null}
                  setRef={setPageRef}
                  highlights={pageHighlights}
                  onRemoveHighlight={removeHighlight}
                />
              );
            })}
          </div>
          {menu && (
            <div
              className="paper-selection-menu"
              style={{ left: menu.left, top: menu.top }}
              onMouseDown={(event) => event.preventDefault()}
            >
              <button type="button" className="chip" onClick={addHighlight} title="Highlight selected text">
                <HighlighterIcon size={14} />
              </button>
              <span className="paper-color-picks">
                {HIGHLIGHT_COLORS.map((c) => (
                  <button
                    key={c}
                    type="button"
                    className={`paper-color-dot${c === highlightColor ? " is-active" : ""}`}
                    style={{ background: c }}
                    onClick={() => setHighlightColor(c)}
                    aria-label={`Highlight color ${c}`}
                  />
                ))}
              </span>
              {onPrompt && (
                <>
                  <span className="paper-menu-sep" />
                  <button type="button" className="chip" onClick={() => sendPrompt("translate")}>
                    Translate
                  </button>
                  <button type="button" className="chip" onClick={() => sendPrompt("explain")}>
                    Explain
                  </button>
                  <button type="button" className="chip" onClick={() => sendPrompt("ask")}>
                    Ask
                  </button>
                </>
              )}
            </div>
          )}
        </div>
      </div>
    </div>
  );
}

function PageSlot({
  pageNum,
  dims,
  scale,
  doc,
  setRef,
  highlights,
  onRemoveHighlight,
}: {
  pageNum: number;
  dims: PageDimensions;
  scale: number;
  doc: PDFDocumentProxy | null;
  setRef: (pageNum: number, el: HTMLDivElement | null) => void;
  highlights: PaperHighlight[];
  onRemoveHighlight: (id: string) => void;
}) {
  const shellRef = useRef<HTMLDivElement>(null);
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const textLayerRef = useRef<HTMLDivElement>(null);
  const [rendered, setRendered] = useState(false);

  const scaledWidth = dims.width * scale;
  const scaledHeight = dims.height * scale;

  useEffect(() => {
    const el = shellRef.current;
    if (!el) return;
    setRef(pageNum, el);
    return () => setRef(pageNum, null);
  }, [pageNum, setRef]);

  useLayoutEffect(() => {
    if (!doc || !canvasRef.current || !textLayerRef.current) {
      setRendered(false);
      return;
    }

    let live = true;
    const targetCanvas = canvasRef.current;
    const targetTextLayer = textLayerRef.current;
    const context = targetCanvas.getContext("2d");
    if (!context) return;

    targetTextLayer.replaceChildren();

    const shellEl = shellRef.current;
    if (shellEl) shellEl.style.setProperty("--total-scale-factor", String(scale));

    let cancelTextLayer: (() => void) | null = null;
    let renderTask: { cancel: () => void; promise: Promise<unknown> } | null = null;

    doc
      .getPage(pageNum)
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
        if (live) setRendered(true);
      })
      .catch((error) => {
        if (!live || isRenderingCancel(error)) return;
      });

    return () => {
      live = false;
      renderTask?.cancel();
      cancelTextLayer?.();
    };
  }, [doc, pageNum, scale]);

  // Dual-column selection limiter
  useEffect(() => {
    const layer = textLayerRef.current;
    if (!layer || !doc) return;
    const onDown = (event: MouseEvent) => {
      if (event.button !== 0) return;
      const rect = layer.getBoundingClientRect();
      if (rect.width === 0) return;
      const clickedRight = (event.clientX - rect.left) > rect.width / 2;

      const allSpans = layer.querySelectorAll<HTMLElement>("span");
      const muted: HTMLElement[] = [];
      for (const span of allSpans) {
        const left = parseFloat(span.style.left);
        if (Number.isNaN(left)) continue;
        if ((left >= 50) !== clickedRight) {
          muted.push(span);
        }
      }
      if (muted.length === 0 || muted.length === allSpans.length) return;
      for (const el of muted) el.style.userSelect = "none";

      const restore = () => {
        for (const el of muted) el.style.userSelect = "";
        window.removeEventListener("mouseup", restore, true);
      };
      window.addEventListener("mouseup", restore, true);
    };
    layer.addEventListener("mousedown", onDown);
    return () => layer.removeEventListener("mousedown", onDown);
  }, [doc]);

  return (
    <div
      ref={shellRef}
      className="paper-page-shell"
      data-page={pageNum}
      style={{ width: scaledWidth, height: scaledHeight }}
    >
      {doc && (
        <>
          <canvas ref={canvasRef} className="paper-canvas" />
          {highlights.length > 0 && (
            <div className="paper-highlight-layer">
              {highlights.map((hl) => (
                <div key={hl.id} className="paper-highlight-group">
                  {hl.rects.map(([x, y, w, h], i) => (
                    <div
                      key={i}
                      className="paper-highlight-rect"
                      style={{
                        left: x * scale,
                        top: y * scale,
                        width: w * scale,
                        height: h * scale,
                        background: hl.color,
                      }}
                      title={hl.selectedText}
                      onDoubleClick={() => onRemoveHighlight(hl.id)}
                    />
                  ))}
                </div>
              ))}
            </div>
          )}
          <div ref={textLayerRef} className="paper-text-layer" />
        </>
      )}
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
