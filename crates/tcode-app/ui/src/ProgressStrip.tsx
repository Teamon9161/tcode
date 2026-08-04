import { useState } from "react";

import { STATUS_MARK, currentPhase, phaseRows, type Plan } from "./plan";
import { ChevronDown, ChevronRight, PanelIcon } from "./components/Icons";
import { Prose } from "./Prose";

/**
 * Where the plan lives while work is happening: one line above the composer.
 *
 * The question this app answers first is "what is running, and what needs me?"
 * — and for a conversation working through a multi-phase plan, "which phase" is
 * part of that answer. It used to be nowhere: a `progress` call rendered as an
 * ordinary tool card that scrolled away with everything else, so the only way to
 * find out where a task stood was to read back up the transcript.
 *
 * It is a line, not a panel. Collapsed by default because the phase you are on
 * is the whole answer most of the time; the list is one click away, and the plan
 * with all of its reasoning is one more (`⧉`, the same pop-out every other
 * inspectable thing in the transcript uses — not a second way to open things).
 *
 * Three things about its shape were learned the hard way.
 *
 * **One element owns the conversation's axis** (`.strip-body`), and everything
 * sits inside it. Every child used to claim `--measure` for itself, and the phase
 * list silently lost it to the `margin: 0` written for the `<ol>`'s own default —
 * so expanding the strip threw its contents against the left edge of the pane
 * while the line above them stayed on the composer's axis. The wider the pane
 * (the rail folded away, say) the further apart the two axes drifted.
 *
 * **It says what it is.** A title, a phase and a fraction with no word naming the
 * thing they belong to is legible only to somebody who already knows this app has
 * plans in it.
 */
export function ProgressStrip({
  plan,
  expanded,
  onToggle,
  onOpen,
}: {
  plan: Plan;
  expanded: boolean;
  onToggle: () => void;
  onOpen: () => void;
}) {
  const current = currentPhase(plan.phases);
  const rows = expanded ? phaseRows(plan.phases) : [];
  /** Which phases the reader has opened or closed by hand, keyed by path.
   *  Absent means "whatever this phase's status implies". */
  const [shown, setShown] = useState<Record<string, boolean>>({});

  return (
    <section className={`dock progress-strip${expanded ? " is-open" : ""}`} aria-label="Plan progress">
      <div className="strip-body">
        <div className="strip-line">
          <button
            type="button"
            className="strip-toggle"
            onClick={onToggle}
            aria-expanded={expanded}
            title={expanded ? "Hide the phases" : "Show the phases"}
          >
            {expanded ? <ChevronDown size={12} /> : <ChevronRight size={12} />}
            <span className="strip-kind">plan</span>
            <span className="strip-title">{plan.title}</span>
          </button>

          {/* A draft has not been approved, and saying so is the difference
              between "this is the plan" and "this is a proposal". It is a word,
              not a colour: amber in this app means a session is parked waiting
              for a human, and a draft nobody has been asked about is not that. */}
          {plan.state !== "active" && (
            <span className={`strip-state is-${plan.state}`}>{plan.state}</span>
          )}

          {current && (
            <span className="strip-current" title={current.phase}>
              <span className={`strip-mark is-${current.status}`}>
                {STATUS_MARK[current.status]}
              </span>
              {current.phase}
            </span>
          )}

          <span className="strip-count" title={`${plan.done} of ${plan.total} phases done`}>
            {plan.done}/{plan.total}
          </span>

          <button
            type="button"
            className="pop-out"
            onClick={onOpen}
            title="Open the plan in its own pane"
            aria-label="Open the plan in its own pane"
          >
            <PanelIcon size={12} />
          </button>
        </div>

        {expanded && (
          <ol className="strip-phases">
            {rows.map((row) => {
              const key = row.path.join(".");
              // The running phase's prose is open because that is the one being
              // read at a glance; the rest are one click, not nowhere. The
              // detail is the plan — a phase title is only its index (see the
              // `progress` tool) — so "what does this phase actually mean" had
              // no answer here at all until the phase happened to be running.
              const open = shown[key] ?? row.status === "in_progress";
              const detail = row.detail.trim();
              return (
                <li
                  key={key}
                  className={`strip-phase is-${row.status}${row.depth > 0 ? " is-nested" : ""}`}
                >
                  {detail ? (
                    <button
                      type="button"
                      className="strip-phase-line"
                      onClick={() => setShown((was) => ({ ...was, [key]: !open }))}
                      aria-expanded={open}
                      title={open ? "Hide what this phase covers" : "Show what this phase covers"}
                    >
                      <span className={`strip-mark is-${row.status}`}>{STATUS_MARK[row.status]}</span>
                      <span className="strip-phase-name">{row.phase}</span>
                      <span className="strip-phase-chevron" aria-hidden="true">
                        {open ? <ChevronDown size={11} /> : <ChevronRight size={11} />}
                      </span>
                    </button>
                  ) : (
                    // Nothing to open, so nothing that looks openable: a phase
                    // with no prose gets no control rather than a dead one.
                    <span className="strip-phase-line is-plain">
                      <span className={`strip-mark is-${row.status}`}>{STATUS_MARK[row.status]}</span>
                      <span className="strip-phase-name">{row.phase}</span>
                    </span>
                  )}
                  {open && detail && <Prose className="strip-detail" text={detail} />}
                </li>
              );
            })}
          </ol>
        )}
      </div>
    </section>
  );
}
