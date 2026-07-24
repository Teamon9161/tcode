/**
 * The tcode mark: a crop frame with a diamond held at its centre.
 *
 * The frame is the graphic language of a drafting table — four corner marks
 * that say "this is the thing under attention" — and the diamond is the work
 * inside it. That makes the mark do a job rather than just sit in the corner:
 * the diamond is tinted by `state`, so the title bar's logo *is* the status
 * light for the session in view.
 *
 * Drawn on a 24-unit grid with a 2-unit stroke so it stays crisp at 16px,
 * where it spends most of its life.
 */
export function Mark({
  size = 20,
  state = "idle",
  className,
}: {
  size?: number;
  /** Tints the centre diamond; the frame never changes color. */
  state?: "idle" | "running" | "waiting" | "failed";
  className?: string;
}) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      className={className}
      aria-hidden="true"
      focusable="false"
    >
      <g
        stroke="currentColor"
        strokeWidth="2.2"
        strokeLinecap="round"
        strokeLinejoin="round"
      >
        <path d="M3 7V3.1h3.9" />
        <path d="M17.1 3.1H21V7" />
        <path d="M21 17v3.9h-3.9" />
        <path d="M6.9 20.9H3V17" />
      </g>
      <path
        d="M12 8.7 15.3 12 12 15.3 8.7 12Z"
        className={`mark-core mark-core-${state}`}
      />
    </svg>
  );
}

/** The mark plus the wordmark, for the title bar and the launchpad header. */
export function Wordmark({ state }: { state?: "idle" | "running" | "waiting" | "failed" }) {
  return (
    <span className="wordmark">
      <Mark size={18} state={state} />
      <span className="wordmark-text">tcode</span>
    </span>
  );
}
