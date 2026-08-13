import { useCallback, useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";

import { ChevronRight, CloseIcon, FitIcon, ZoomInIcon, ZoomOutIcon } from "./components/Icons";

export type GalleryImage = {
  url: string;
  label: string;
};

const MIN_ZOOM = 0.1;
const MAX_ZOOM = 10;
const ZOOM_STEP = 1.2;

function clampZoom(z: number) {
  return Math.min(MAX_ZOOM, Math.max(MIN_ZOOM, z));
}

/**
 * One focussed, full-screen reading surface for a related set of images.
 *
 * A thumbnail still belongs in the conversation or composer that introduced
 * it; this only takes over when somebody asks to inspect its actual pixels.
 * Keeping the collection with that source makes adjacent images available with
 * the same left/right vocabulary as a normal desktop image viewer.
 */
export function ImageViewer({
  images,
  index,
  onIndex,
  onClose,
}: {
  images: GalleryImage[];
  index: number | null;
  onIndex: (next: number) => void;
  onClose: () => void;
}) {
  const close = useRef<HTMLButtonElement>(null);
  const restore = useRef<HTMLElement | null>(null);
  const stageRef = useRef<HTMLDivElement>(null);
  const current = index === null ? null : images[index] ?? null;
  const canMove = images.length > 1;
  const open = index !== null;

  const [zoom, setZoom] = useState(1);
  const [pan, setPan] = useState({ x: 0, y: 0 });
  const dragging = useRef(false);
  const dragStart = useRef({ x: 0, y: 0 });
  const panStart = useRef({ x: 0, y: 0 });

  const resetView = useCallback(() => {
    setZoom(1);
    setPan({ x: 0, y: 0 });
  }, []);

  const zoomBy = useCallback((factor: number) => {
    setZoom((prev) => clampZoom(prev * factor));
  }, []);

  // Reset view when switching images
  useEffect(() => {
    resetView();
  }, [index, resetView]);

  useEffect(() => {
    if (!open) return;
    restore.current = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    close.current?.focus();
    return () => {
      restore.current?.focus();
      restore.current = null;
    };
  }, [open]);

  useEffect(() => {
    if (!open) return;
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.preventDefault();
        event.stopPropagation();
        onClose();
        return;
      }

      // Ctrl+= / Ctrl+- for zoom
      if (event.ctrlKey || event.metaKey) {
        if (event.key === "=" || event.key === "+") {
          event.preventDefault();
          zoomBy(ZOOM_STEP);
          return;
        }
        if (event.key === "-") {
          event.preventDefault();
          zoomBy(1 / ZOOM_STEP);
          return;
        }
        if (event.key === "0") {
          event.preventDefault();
          resetView();
          return;
        }
      }

      if (!canMove || index === null) return;
      if (event.key === "ArrowLeft") {
        event.preventDefault();
        onIndex((index - 1 + images.length) % images.length);
      } else if (event.key === "ArrowRight") {
        event.preventDefault();
        onIndex((index + 1) % images.length);
      }
    };
    window.addEventListener("keydown", onKeyDown, true);
    return () => {
      window.removeEventListener("keydown", onKeyDown, true);
    };
  }, [canMove, images.length, index, onClose, onIndex, zoomBy, resetView]);

  // Wheel zoom on the stage
  useEffect(() => {
    if (!open) return;
    const stage = stageRef.current;
    if (!stage) return;
    const onWheel = (event: WheelEvent) => {
      if (event.ctrlKey || event.metaKey) {
        event.preventDefault();
        const factor = event.deltaY < 0 ? ZOOM_STEP : 1 / ZOOM_STEP;
        setZoom((prev) => clampZoom(prev * factor));
      }
    };
    stage.addEventListener("wheel", onWheel, { passive: false });
    return () => stage.removeEventListener("wheel", onWheel);
  }, [open]);

  // Drag-to-pan
  useEffect(() => {
    if (!open) return;
    const onMove = (event: MouseEvent) => {
      if (!dragging.current) return;
      setPan({
        x: panStart.current.x + (event.clientX - dragStart.current.x),
        y: panStart.current.y + (event.clientY - dragStart.current.y),
      });
    };
    const onUp = () => {
      dragging.current = false;
    };
    window.addEventListener("mousemove", onMove);
    window.addEventListener("mouseup", onUp);
    return () => {
      window.removeEventListener("mousemove", onMove);
      window.removeEventListener("mouseup", onUp);
    };
  }, [open]);

  if (!current || index === null) return null;

  const move = (by: number) => onIndex((index + by + images.length) % images.length);

  const onStageMouseDown = (event: React.MouseEvent) => {
    if (event.button !== 0) return;
    // Only start drag on the image area, not on step buttons
    if ((event.target as HTMLElement).closest("button")) return;
    event.preventDefault();
    dragging.current = true;
    dragStart.current = { x: event.clientX, y: event.clientY };
    panStart.current = { ...pan };
  };

  const isZoomed = zoom !== 1;
  const zoomPercent = Math.round(zoom * 100);

  return createPortal(
    <div
      className="image-viewer-backdrop"
      onMouseDown={(event) => {
        if (event.target === event.currentTarget) onClose();
      }}
    >
      <section
        className="image-viewer"
        role="dialog"
        aria-modal="true"
        aria-label={`Image viewer: ${current.label}`}
      >
        <header className="image-viewer-head">
          <p className="image-viewer-label">
            {current.label}
            {canMove && <span>{index + 1} of {images.length}</span>}
          </p>
          <div className="image-viewer-controls">
            <button
              type="button"
              className="image-viewer-btn"
              onClick={() => zoomBy(1 / ZOOM_STEP)}
              aria-label="Zoom out"
              title="Zoom out (Ctrl+-)"
            >
              <ZoomOutIcon size={16} />
            </button>
            <span className="image-viewer-zoom-level">{zoomPercent}%</span>
            <button
              type="button"
              className="image-viewer-btn"
              onClick={() => zoomBy(ZOOM_STEP)}
              aria-label="Zoom in"
              title="Zoom in (Ctrl++)"
            >
              <ZoomInIcon size={16} />
            </button>
            <button
              type="button"
              className="image-viewer-btn"
              onClick={resetView}
              aria-label="Fit to screen"
              title="Fit to screen (Ctrl+0)"
              disabled={!isZoomed && pan.x === 0 && pan.y === 0}
            >
              <FitIcon size={16} />
            </button>
          </div>
          <button
            ref={close}
            type="button"
            className="image-viewer-close"
            onClick={onClose}
            aria-label="Close image viewer"
            title="Close (Esc)"
          >
            <CloseIcon size={18} />
          </button>
        </header>

        <div
          ref={stageRef}
          className="image-viewer-stage"
          style={{ cursor: isZoomed ? "grab" : undefined }}
          onMouseDown={onStageMouseDown}
          onDoubleClick={resetView}
        >
          {canMove && (
            <button
              type="button"
              className="image-viewer-step is-previous"
              onClick={() => move(-1)}
              aria-label="Previous image"
              title="Previous image (Left arrow)"
            >
              <ChevronRight size={22} />
            </button>
          )}
          <div className="image-viewer-canvas">
            <img
              src={current.url}
              alt={current.label}
              draggable={false}
              style={{
                transform: `translate(${pan.x}px, ${pan.y}px) scale(${zoom})`,
              }}
            />
          </div>
          {canMove && (
            <button
              type="button"
              className="image-viewer-step"
              onClick={() => move(1)}
              aria-label="Next image"
              title="Next image (Right arrow)"
            >
              <ChevronRight size={22} />
            </button>
          )}
        </div>
      </section>
    </div>,
    document.body,
  );
}
