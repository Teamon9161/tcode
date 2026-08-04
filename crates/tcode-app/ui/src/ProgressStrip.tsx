import { STATUS_MARK, currentPhase, phaseRows, type Plan } from "./plan";
import { ChevronDown, ChevronRight, PanelIcon } from "./components/Icons";
import { rich } from "./rich";

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

  return (
    <section className={`progress-strip${expanded ? " is-open" : ""}`} aria-label="Plan progress">
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
            {rows.map((row) => (
              <li
                key={row.path.join(".")}
                className={`strip-phase is-${row.status}${row.depth > 0 ? " is-nested" : ""}`}
              >
                <span className={`strip-mark is-${row.status}`}>{STATUS_MARK[row.status]}</span>
                <span className="strip-phase-name">{row.phase}</span>
                {/* Only the running phase's prose, which is the same budget the
                    terminal's pane keeps: what a finished or not-yet-started
                    phase breaks down into is not read at a glance. */}
                {row.status === "in_progress" && row.detail && (
                  <div className="strip-detail">{rich(row.detail)}</div>
                )}
              </li>
            ))}
          </ol>
        )}
      </div>
    </section>
  );
}
