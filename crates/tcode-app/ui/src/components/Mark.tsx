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

/**
 * The product's name in the title bar.
 *
 * Word only. The mark sat here until it was looked at in place: at 18px in a
 * bar that is already the thinnest band in the window, the crop frame reads as
 * four detached ticks around a dot, and it was the third thing in a row saying
 * "tcode" — after the window itself and the word beside it. The mark still has
 * a job at larger sizes (the failure screen), and the status it used to carry
 * is the rail's, where it sits next to the session it describes.
 */
export function Wordmark() {
  return <span className="wordmark-text">tcode</span>;
}
