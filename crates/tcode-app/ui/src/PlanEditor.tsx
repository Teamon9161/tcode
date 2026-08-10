import { useEffect, useRef, useState } from "react";

import {
  STATUS_MARK,
  addPhase,
  editAt,
  fromDraft,
  isEdited,
  movePhase,
  nextStatus,
  planChanges,
  removeAt,
  type DraftPhase,
  type PhaseField,
  type PhasePath,
  type Plan,
  type PlanChange,
  type PlanComment,
  type PlanDraft,
  type PlanPhase,
} from "./plan";
import { commentTarget, normalizeQuote, type BubbleAt } from "./selection";
import { SelectionBubble } from "./components/SelectionBubble";
import { GrowingText } from "./components/GrowingText";
import { TextDiff } from "./components/Diff";
import { ChevronDown, ChevronRight, CloseIcon, PlusIcon } from "./components/Icons";
import { ModelPicker } from "./Chips";
import { Prose } from "./Prose";

/**
 * The plan, as a document you can work on.
 *
 * There is no edit mode. Every title and every piece of prose is the text
 * itself, editable where it sits; the reviewer changes what they want changed,
 * comments on the passages they want to talk about, and submits once. That is
 * the whole difference from the terminal's review pane, which can only navigate
 * blocks and open `$EDITOR` — a compromise the terminal has to make and this
 * window does not.
 *
 * Two verbs, kept visibly apart because they mean different things to the model:
 *
 *  - **An edit** changes the plan that will be executed. It goes back as the
 *    breakdown, and core turns it into the plan body (`revise_plan_body`), which
 *    is also why a phase whose prose the reviewer never touched keeps it.
 *  - **A comment** is something to say *about* the plan. It goes back as an
 *    anchored note (`PlanNote`) with the passage quoted, whether the plan is
 *    approved or sent back for more work.
 *
 * One component, two mounts: the approval dock (`Approval.tsx`) and the plan
 * pane (`Inspector.tsx`). They share the draft in `SessionState`, so the same
 * edit is on screen in both — a second copy of this state was the obvious
 * alternative and would have let the two disagree about what is being approved.
 */
export function PlanEditor({
  plan,
  draft,
  mode,
  busy,
  onDraft,
  onDecide,
  onSave,
}: {
  plan: Plan;
  draft: PlanDraft;
  /** `review` submits through the approval that is waiting; `edit` writes the
   *  file directly, which is legal at any time — the file is the user's. */
  mode: "review" | "edit";
  busy?: boolean;
  onDraft: (next: PlanDraft) => void;
  onDecide?: (choice: PlanDecision) => void;
  onSave?: () => void;
}) {
  const [bubble, setBubble] = useState<Anchored | null>(null);
  const [composing, setComposing] = useState<Anchored | null>(null);
  const [showChanges, setShowChanges] = useState(false);
  const [note, setNote] = useState("");
  const edited = isEdited(draft);
  const changes = edited ? planChanges(draft.base, draft.phases) : [];

  const setPhases = (phases: DraftPhase[]) => onDraft({ ...draft, phases });

  const comment = (anchored: Anchored, text: string) => {
    const entry: PlanComment = {
      id: `c${Date.now()}-${draft.comments.length}`,
      path: anchored.path,
      field: anchored.field,
      quote: anchored.quote,
      text,
    };
    onDraft({ ...draft, comments: [...draft.comments, entry] });
  };

  // Selection → offer. Three triggers, one path: drag a passage, right-click it,
  // or select it from the keyboard (below).
  const offer = (event: React.MouseEvent) => {
    const anchored = anchorOf(event.target);
    const found = commentTarget(event.nativeEvent);
    if (!anchored || !found) {
      setBubble(null);
      return;
    }
    setBubble({ ...anchored, quote: found.quote, at: found.at });
  };

  return (
    <section
      className={`plan-editor is-${mode}`}
      aria-label="The plan"
      onMouseUp={offer}
      onContextMenu={(event) => {
        // Suppressed only inside the editor, and only to put the same comment
        // affordance under the pointer the user just used.
        const found = commentTarget(event.nativeEvent);
        if (!found || !anchorOf(event.target)) return;
        event.preventDefault();
        offer(event);
      }}
      onKeyDown={(event) => {
        if (!(event.metaKey || event.ctrlKey) || !event.shiftKey) return;
        if (event.key.toLowerCase() !== "m") return;
        const anchored = keyboardAnchor();
        if (!anchored) return;
        event.preventDefault();
        setComposing(anchored);
        setBubble(null);
      }}
    >
      <header className="plan-head">
        <h3 className="plan-title">{plan.title}</h3>
        <span className={`plan-state is-${plan.state}`}>{plan.state}</span>
        <span className="plan-count">
          {plan.done}/{plan.total} phases
        </span>
        {plan.description && <p className="plan-description">{plan.description}</p>}
      </header>

      {/* The part of the plan that belongs to no phase — most of what the
          reviewer is actually deciding about. Read-only here: this editor works
          in phases, and prose that came back through it would have to be
          re-parsed out of a structure that has nowhere to put it. Changing it
          goes through a comment, or through the file itself. */}
      {plan.background && <Prose className="plan-background" text={plan.background} />}

      <ol className="plan-phases">
        {draft.phases.map((phase, index) => (
          <PhaseRowView
            key={phase.id}
            phase={phase}
            path={[index]}
            siblings={draft.phases.length}
            comments={draft.comments}
            onEdit={(change) => setPhases(editAt(draft.phases, [index], change))}
            onEditAt={(path, change) => setPhases(editAt(draft.phases, path, change))}
            onRemoveAt={(path) => setPhases(removeAt(draft.phases, path))}
            onMoveAt={(path, to) => setPhases(movePhase(draft.phases, path, to))}
            onAddSub={(path) => setPhases(addPhase(draft.phases, path))}
            onUncomment={(id) =>
              onDraft({ ...draft, comments: draft.comments.filter((entry) => entry.id !== id) })
            }
          />
        ))}
      </ol>

      <button
        type="button"
        className="plan-add"
        onClick={() => setPhases(addPhase(draft.phases, null))}
      >
        <PlusIcon size={12} /> add a phase
      </button>

      {edited && (
        <div className="plan-changes">
          <button
            type="button"
            className="link-btn"
            onClick={() => setShowChanges((was) => !was)}
            aria-expanded={showChanges}
          >
            {showChanges ? <ChevronDown size={12} /> : <ChevronRight size={12} />}
            edited · {changes.length} {changes.length === 1 ? "change" : "changes"}
          </button>
          {/* Approving a plan you rewrote without having seen what you changed is
              not informed consent, so the diff is one click away rather than
              somewhere else entirely. */}
          {showChanges && <ChangeList changes={changes} />}
        </div>
      )}

      {composing && (
        <CommentComposer
          anchored={composing}
          onCancel={() => setComposing(null)}
          onSave={(text) => {
            comment(composing, text);
            setComposing(null);
          }}
        />
      )}

      {bubble && (
        <SelectionBubble
          at={bubble.at}
          onDismiss={() => setBubble(null)}
          onComment={() => {
            setComposing(bubble);
            setBubble(null);
          }}
        />
      )}

      {mode === "review" ? (
        <div className="plan-actions">
          <input
            className="approval-comment"
            value={note}
            placeholder="Anything else to say about this plan"
            onChange={(event) => setNote(event.target.value)}
          />
          <div className="plan-execution-model">
            <span className="plan-execution-label">Execution model</span>
            <ModelPicker />
            <span className="plan-execution-note">Applies to every session in this app.</span>
          </div>
          <div className="plan-buttons">
            <button
              type="button"
              className="btn btn-secondary"
              disabled={busy}
              onClick={() => onDecide?.({ decision: "no", fresh: false, note, ...payload(draft) })}
            >
              Keep planning
            </button>
            {/* A planning conversation is full of the exploration that produced
                the plan. Executing it in a pane of its own is what this window is
                for; see `execute_plan_elsewhere`. */}
            <button
              type="button"
              className="btn btn-ghost"
              disabled={busy}
              onClick={() => onDecide?.({ decision: "yes", fresh: true, note, ...payload(draft) })}
            >
              Execute in a new session
            </button>
            <button
              type="button"
              className="btn btn-primary"
              disabled={busy}
              onClick={() => onDecide?.({ decision: "yes", fresh: false, note, ...payload(draft) })}
            >
              {edited ? "Approve my version" : "Execute here"}
            </button>
          </div>
        </div>
      ) : (
        <div className="plan-actions">
          <div className="plan-buttons">
            <button
              type="button"
              className="btn btn-secondary"
              disabled={!edited || busy}
              onClick={() => onDraft({ ...draft, phases: draft.base })}
            >
              Revert
            </button>
            <button
              type="button"
              className="btn btn-primary"
              disabled={!edited || busy}
              onClick={onSave}
            >
              Save the plan
            </button>
          </div>
        </div>
      )}
    </section>
  );
}

/** What one review decision carries back. `fresh` is the third option rather
 *  than a fourth decision: it approves *and* hands execution to a new session. */
export type PlanDecision = {
  decision: "yes" | "no";
  fresh: boolean;
  /** Free-form remark, sent alongside the anchored comments. */
  note: string;
  /** The edited breakdown, in the shape the backend validates. */
  phases: PlanPhase[];
  comments: PlanComment[];
};

function payload(draft: PlanDraft): { phases: PlanPhase[]; comments: PlanComment[] } {
  return { phases: fromDraft(draft.phases), comments: draft.comments };
}

/** A selection, plus which text it was in. */
type Anchored = { path: PhasePath; field: PhaseField; quote: string; at: BubbleAt };

/** Which phase and field an event came from, read off the field itself. The
 *  alternative — threading a callback through every row — would put the same
 *  parameter on every level of this tree for the benefit of one gesture. */
function anchorOf(target: EventTarget | null): { path: PhasePath; field: PhaseField } | null {
  if (!(target instanceof HTMLElement)) return null;
  const field = target.closest<HTMLElement>("[data-phase-path]");
  const path = field?.dataset.phasePath;
  const which = field?.dataset.phaseField;
  if (!path || (which !== "phase" && which !== "detail")) return null;
  return { path: path.split(".").map(Number), field: which };
}

/** The same anchor for a keyboard selection, whose "pointer" is the caret: the
 *  bubble is skipped entirely and the composer opens directly. */
function keyboardAnchor(): Anchored | null {
  const active = document.activeElement;
  if (!(active instanceof HTMLTextAreaElement)) return null;
  const anchored = anchorOf(active);
  const { selectionStart, selectionEnd, value } = active;
  if (!anchored || selectionEnd <= selectionStart) return null;
  const box = active.getBoundingClientRect();
  return {
    ...anchored,
    quote: normalizeQuote(value.slice(selectionStart, selectionEnd)),
    at: { x: box.left + 16, y: box.bottom },
  };
}

function PhaseRowView({
  phase,
  path,
  siblings,
  comments,
  onEdit,
  onEditAt,
  onRemoveAt,
  onMoveAt,
  onAddSub,
  onUncomment,
}: {
  phase: DraftPhase;
  path: PhasePath;
  siblings: number;
  comments: PlanComment[];
  onEdit: (change: (phase: DraftPhase) => DraftPhase) => void;
  onEditAt: (path: PhasePath, change: (phase: DraftPhase) => DraftPhase) => void;
  onRemoveAt: (path: PhasePath) => void;
  onMoveAt: (path: PhasePath, to: number) => void;
  onAddSub: (path: PhasePath) => void;
  onUncomment: (id: string) => void;
}) {
  const at = path[path.length - 1];
  const nested = path.length > 1;
  const key = path.join(".");

  return (
    <li
      className={`phase-row${nested ? " is-nested" : ""}`}
      // Alt is what keeps this from competing with typing: the hand is in a text
      // field nearly all the time, so a bare arrow key is a caret move.
      onKeyDown={(event) => {
        if (!event.altKey || (event.key !== "ArrowUp" && event.key !== "ArrowDown")) return;
        event.preventDefault();
        event.stopPropagation();
        onMoveAt(path, event.key === "ArrowUp" ? at - 1 : at + 1);
      }}
    >
      <div className="phase-line">
        <button
          type="button"
          className={`phase-box is-${phase.status}`}
          title={`${phase.status} — click to change`}
          aria-label={`Status: ${phase.status}`}
          onClick={() => onEdit((was) => ({ ...was, status: nextStatus(was.status) }))}
        >
          {STATUS_MARK[phase.status]}
        </button>

        <GrowingText
          className="phase-name"
          value={phase.phase}
          placeholder="what this phase does"
          rows={1}
          data-phase-path={key}
          data-phase-field="phase"
          onChange={(value) => onEdit((was) => ({ ...was, phase: value }))}
        />

        <span className="phase-tools">
          {/* Reordering is keyboard-first and pointer-second: the hand is in a
              text field, and Alt+arrow does not compete with typing. */}
          <button
            type="button"
            className="icon-btn"
            title="Move up (Alt+↑)"
            aria-label="Move this phase up"
            disabled={at === 0}
            onClick={() => onMoveAt(path, at - 1)}
          >
            ↑
          </button>
          <button
            type="button"
            className="icon-btn"
            title="Move down (Alt+↓)"
            aria-label="Move this phase down"
            disabled={at === siblings - 1}
            onClick={() => onMoveAt(path, at + 1)}
          >
            ↓
          </button>
          {!nested && (
            <button
              type="button"
              className="icon-btn"
              title="Add a sub-phase"
              aria-label="Add a sub-phase"
              onClick={() => onAddSub(path)}
            >
              <PlusIcon size={12} />
            </button>
          )}
          <button
            type="button"
            className="icon-btn"
            title="Remove this phase"
            aria-label="Remove this phase"
            onClick={() => onRemoveAt(path)}
          >
            <CloseIcon size={12} />
          </button>
        </span>
      </div>

      <GrowingText
        className="phase-detail"
        value={phase.detail}
        placeholder="what it changes, which files, what could break"
        rows={2}
        data-phase-path={key}
        data-phase-field="detail"
        onChange={(value) => onEdit((was) => ({ ...was, detail: value }))}
      />

      <CommentList
        comments={comments.filter((entry) => entry.path.join(".") === key)}
        onRemove={onUncomment}
      />

      {phase.phases.length > 0 && (
        <ol className="phase-children">
          {phase.phases.map((child, index) => (
            <PhaseRowView
              key={child.id}
              phase={child}
              path={[...path, index]}
              siblings={phase.phases.length}
              comments={comments}
              onEdit={(change) => onEditAt([...path, index], change)}
              onEditAt={onEditAt}
              onRemoveAt={onRemoveAt}
              onMoveAt={onMoveAt}
              onAddSub={onAddSub}
              onUncomment={onUncomment}
            />
          ))}
        </ol>
      )}
    </li>
  );
}

/**
 * A comment, drawn under the text it is about, quoting the passage.
 *
 * The quote *is* the anchor — there is no highlight inside the text. Two reasons,
 * and neither is laziness: a highlight inside a textarea needs the field mirrored
 * into a hidden overlay to survive scrolling and wrapping, and a highlight
 * anchored to character offsets stops meaning anything the moment the reviewer
 * edits that very paragraph, which is what they are here to do. The quote
 * survives every edit, and it is exactly what the model is sent.
 */
function CommentList({
  comments,
  onRemove,
}: {
  comments: PlanComment[];
  onRemove: (id: string) => void;
}) {
  if (comments.length === 0) return null;
  return (
    <ul className="phase-comments">
      {comments.map((entry) => (
        <li key={entry.id} className="phase-comment">
          <p className="phase-quote">{entry.quote}</p>
          <p className="phase-note">{entry.text}</p>
          <button
            type="button"
            className="icon-btn"
            onClick={() => onRemove(entry.id)}
            title="Remove this comment"
            aria-label="Remove this comment"
          >
            <CloseIcon size={11} />
          </button>
        </li>
      ))}
    </ul>
  );
}

function CommentComposer({
  anchored,
  onSave,
  onCancel,
}: {
  anchored: Anchored;
  onSave: (text: string) => void;
  onCancel: () => void;
}) {
  const [text, setText] = useState("");
  const field = useRef<HTMLTextAreaElement>(null);
  useEffect(() => field.current?.focus(), []);

  return (
    <div className="comment-composer">
      <p className="phase-quote">{anchored.quote}</p>
      <textarea
        ref={field}
        className="comment-text"
        value={text}
        rows={2}
        placeholder="a note for the model about this passage"
        onChange={(event) => setText(event.target.value)}
        onKeyDown={(event) => {
          if (event.key === "Escape") onCancel();
          if (event.key === "Enter" && !event.shiftKey && text.trim()) {
            event.preventDefault();
            onSave(text.trim());
          }
        }}
      />
      <div className="comment-actions">
        <button type="button" className="btn btn-ghost" onClick={onCancel}>
          Cancel
        </button>
        <button
          type="button"
          className="btn btn-secondary"
          disabled={!text.trim()}
          onClick={() => onSave(text.trim())}
        >
          Comment
        </button>
      </div>
    </div>
  );
}

function ChangeList({ changes }: { changes: PlanChange[] }) {
  return (
    <ul className="change-list">
      {changes.map((change, index) => (
        <li key={index} className={`change is-${change.kind}`}>
          <span className="change-kind">{label(change)}</span>
          <span className="change-title">{change.title}</span>
          {change.kind === "detail" && (
            <div className="change-diff">
              <TextDiff before={change.from} after={change.to} />
            </div>
          )}
        </li>
      ))}
    </ul>
  );
}

function label(change: PlanChange): string {
  switch (change.kind) {
    case "added":
      return "new phase";
    case "removed":
      return "removed";
    case "renamed":
      return `renamed from “${change.from}”`;
    case "moved":
      return "moved";
    case "status":
      return `${change.from} → ${change.to}`;
    case "detail":
      return "detail";
  }
}
