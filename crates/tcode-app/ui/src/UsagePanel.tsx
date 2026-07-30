import { useEffect, useRef, useState, type CSSProperties } from "react";
import { createPortal } from "react-dom";

import { useLimits } from "./session";
import { useSeat } from "./seat";
import {
  cacheShare,
  contextLevel,
  limitLevel,
  percent,
  resetIn,
  tokens,
  totalInput,
  windowLabel,
  type Level,
  type Limit,
  type Meter,
} from "./usage";

/**
 * What the conversation is spending, behind one ring.
 *
 * Two different budgets meet here and the design keeps them apart rather than
 * averaging them into one "usage" number: the **context window** is per
 * conversation and refillable (a compaction empties it), while a **subscription
 * window** is per account and refills only with the clock. Confusing them is
 * how a meter ends up saying you are fine at the moment you are out of hours.
 *
 * The ring answers the glance and the panel answers the question, which is the
 * split the strip needs — the number is checked constantly and read carefully
 * about twice a day.
 *
 * It is achromatic while the numbers are ordinary. That is this app's palette
 * rule, not the terminal's: chroma means state, and "34% of a window used" is
 * not a state, it is the number being unremarkable. Amber and red arrive at the
 * same thresholds the TUI warns at, so the two frontends never disagree about
 * when something is worth looking at.
 */
export function UsagePanel({ meter, window: capacity }: { meter: Meter; window: number }) {
  const limits = useLimits();
  const [open, setOpen] = useState(false);
  const trigger = useRef<HTMLButtonElement>(null);
  const box = useRef<HTMLDivElement>(null);

  const close = () => {
    setOpen(false);
    // Back to the chip: Escape leaves the hand where it started rather than
    // stranding focus on a panel that no longer exists.
    trigger.current?.focus();
  };
  useSeat({ open, trigger, box, onEscape: close, onOutside: () => setOpen(false) });

  const used = percent(meter.context, capacity);
  const level = contextLevel(used);
  // The tightest subscription window, and only once it is worth interrupting a
  // glance for: a 5-hour budget running out is the one fact on this strip you
  // would rather not have had to open a panel to learn.
  const pressing = [limits?.primary, limits?.secondary ?? null]
    .filter((limit): limit is Limit => limit !== null && limit !== undefined)
    .filter((limit) => limitLevel(limit.used_percent) !== "calm")
    .sort((a, b) => b.used_percent - a.used_percent)[0];

  return (
    <div className="chip-box">
      <button
        ref={trigger}
        type="button"
        className="chip is-usage"
        aria-expanded={open}
        onClick={() => setOpen((was) => !was)}
        title="What this conversation is spending"
      >
        <Ring pct={used} level={level} />
        <span className={`chip-usage-figure is-${level}`}>
          {meter.estimated && <span aria-hidden="true">≈</span>}
          {Math.round(used)}%
        </span>
        {pressing && (
          <span className={`chip-usage-alarm is-${limitLevel(pressing.used_percent)}`}>
            {windowLabel(pressing.window_minutes)} {Math.round(pressing.used_percent)}%
          </span>
        )}
      </button>

      {open &&
        createPortal(
          <div className="seated upanel" ref={box} role="dialog" aria-label="Usage">
            <section className="upanel-band">
              <Gauge
                label="context"
                figure={`${tokens(meter.context)} / ${tokens(capacity)}`}
                pct={used}
                level={level}
                estimated={meter.estimated}
              />
              <Receipt meter={meter} />
            </section>

            <section className="upanel-band is-limits">
              <h3 className="upanel-group">subscription</h3>
              {limits ? (
                <div className="upanel-windows">
                  <Window limit={limits.primary} />
                  {limits.secondary && <Window limit={limits.secondary} />}
                </div>
              ) : (
                <p className="upanel-note">
                  This provider reports no subscription limits — usage is billed
                  per token, and the receipt above is the whole story.
                </p>
              )}
            </section>
          </div>,
          document.body,
        )}
    </div>
  );
}

/**
 * The glanceable form. A ring rather than a bar because the strip has one line
 * and no width to spare, and because a circle reads as a proportion at 13px
 * where a 12px bar reads as a smudge.
 */
function Ring({ pct, level }: { pct: number; level: Level }) {
  const r = 5.5;
  const circumference = 2 * Math.PI * r;
  return (
    <svg
      className={`usage-ring is-${level}`}
      width="14"
      height="14"
      viewBox="0 0 14 14"
      aria-hidden="true"
    >
      <circle className="usage-ring-track" cx="7" cy="7" r={r} />
      <circle
        className="usage-ring-arc"
        cx="7"
        cy="7"
        r={r}
        strokeDasharray={`${(pct / 100) * circumference} ${circumference}`}
        // From twelve o'clock, like every dial anyone has read before.
        transform="rotate(-90 7 7)"
      />
    </svg>
  );
}

/** One labelled meter: the name, the figures, and the bar under both. */
function Gauge({
  label,
  figure,
  note,
  pct,
  level,
  estimated,
}: {
  label: string;
  /** The absolute counts, where there are any to show. A subscription window
   *  reports a percentage and nothing else, so it goes without. */
  figure?: string;
  note?: string;
  pct: number;
  level: Level;
  estimated?: boolean;
}) {
  return (
    <div className="upanel-gauge">
      <div className="upanel-gauge-head">
        <span className="upanel-gauge-label">{label}</span>
        {figure && <span className="upanel-gauge-figure">{figure}</span>}
        <span className={`upanel-gauge-pct is-${level}`}>
          {estimated && <span aria-hidden="true">≈</span>}
          {Math.round(pct)}%
        </span>
      </div>
      <div
        className={`upanel-bar is-${level}`}
        role="progressbar"
        aria-label={label}
        aria-valuenow={Math.round(pct)}
      >
        {/* Scaled rather than widened: a bar that animates its own width makes
            the browser lay the row out again on every frame, and this one moves
            while a turn streams. */}
        <span className="upanel-bar-fill" style={{ "--fill": pct / 100 } as CSSProperties} />
      </div>
      {note && <p className="upanel-gauge-note">{note}</p>}
    </div>
  );
}

/** One subscription window. The label is derived from the minutes the provider
 *  reported, so a plan with a three-hour window says "3h" instead of the "5h"
 *  today's Codex plan happens to use. */
function Window({ limit }: { limit: Limit }) {
  const [now, setNow] = useState(() => Math.floor(Date.now() / 1000));
  // Only while the panel is open, and only every half minute: the countdown is
  // read in units of minutes, and a per-second timer would be a re-render a
  // second for a digit that changes sixty times less often.
  useEffect(() => {
    const tick = setInterval(() => setNow(Math.floor(Date.now() / 1000)), 30_000);
    return () => clearInterval(tick);
  }, []);

  const left = resetIn(limit.resets_at, now);
  return (
    <Gauge
      label={windowLabel(limit.window_minutes)}
      note={left ? `resets in ${left}` : undefined}
      pct={Math.min(100, Math.max(0, limit.used_percent))}
      level={limitLevel(limit.used_percent)}
    />
  );
}

/**
 * The last turn's receipt: what was paid for, what came back, and how much of
 * the prompt was served from cache.
 *
 * Uncached input only, deliberately — it is the figure that cost money. The
 * cache share beside it is what says the append-only ledger is doing its job;
 * a number that falls off a cliff between turns means something rewrote the
 * prefix, which is worth noticing here rather than on a monthly bill.
 */
function Receipt({ meter }: { meter: Meter }) {
  const share = cacheShare(meter.turn);
  if (totalInput(meter.turn) === 0 && meter.turn.output_tokens === 0) {
    return <p className="upanel-note">No turn has run in this conversation yet.</p>;
  }
  return (
    <dl className="upanel-receipt">
      <div>
        <dt>paid input</dt>
        <dd>{tokens(meter.turn.input_tokens)}</dd>
      </div>
      <div>
        <dt>output</dt>
        <dd>{tokens(meter.turn.output_tokens)}</dd>
      </div>
      <div>
        <dt>from cache</dt>
        <dd>{share === null ? "—" : `${Math.round(share * 100)}%`}</dd>
      </div>
    </dl>
  );
}
