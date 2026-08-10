import { useEffect, useRef } from "react";
import { createPortal } from "react-dom";

import { ChevronRight, CloseIcon } from "./components/Icons";

export type GalleryImage = {
  url: string;
  label: string;
};

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
  const current = index === null ? null : images[index] ?? null;
  const canMove = images.length > 1;
  const open = index !== null;

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
  }, [canMove, images.length, index, onClose, onIndex]);

  if (!current || index === null) return null;

  const move = (by: number) => onIndex((index + by + images.length) % images.length);

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

        <div className="image-viewer-stage">
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
          <img src={current.url} alt={current.label} />
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
