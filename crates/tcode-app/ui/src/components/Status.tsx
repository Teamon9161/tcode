import { STATUS_LABEL, type Status } from "../types";

/**
 * Status is never carried by hue alone (PRODUCT.md § Accessibility): each
 * state has its own glyph — a pulsing filled diamond, a hollow ring, a cross —
 * so it survives both colour blindness and reduced motion.
 */
export function StatusDot({ status, size = 8 }: { status: Status; size?: number }) {
  if (status === "failed") {
    return (
      <svg
        width={size + 3}
        height={size + 3}
        viewBox="0 0 12 12"
        className="dot dot-failed"
        aria-hidden="true"
      >
        <path
          d="M3 3 9 9M9 3 3 9"
          stroke="currentColor"
          strokeWidth="2.4"
          strokeLinecap="round"
        />
      </svg>
    );
  }
  if (status === "waiting") {
    return (
      <svg
        width={size + 3}
        height={size + 3}
        viewBox="0 0 12 12"
        className="dot dot-waiting"
        aria-hidden="true"
      >
        <circle cx="6" cy="6" r="3.6" fill="none" stroke="currentColor" strokeWidth="2.4" />
      </svg>
    );
  }
  // Running and idle share the filled diamond of the mark; only running moves.
  return (
    <svg
      width={size + 3}
      height={size + 3}
      viewBox="0 0 12 12"
      className={`dot dot-${status}`}
      aria-hidden="true"
    >
      <path d="M6 1.6 10.4 6 6 10.4 1.6 6Z" fill="currentColor" />
    </svg>
  );
}

/** Dot plus word. The only place a state colour appears as a fill. */
export function StatusPill({ status }: { status: Status }) {
  return (
    <span className={`pill pill-${status}`}>
      <StatusDot status={status} />
      {STATUS_LABEL[status]}
    </span>
  );
}
