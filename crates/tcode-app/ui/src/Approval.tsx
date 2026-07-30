import { useMemo, useState } from "react";

import type { ApprovalRequest, Decision } from "./types";
import { Diff, isEditShape } from "./components/Diff";
import { StatusDot } from "./components/Status";
import { PlanEditor, type PlanDecision } from "./PlanEditor";
import { isPlanReview, type Plan, type PlanDraft } from "./plan";

/**
 * What the agent is parked on, docked above the composer.
 *
 * It is deliberately **not** a modal. The reason this app exists is several
 * conversations at once, and a scrim over the whole window makes one paused
 * session hold the other three hostage: you cannot read another transcript,
 * open a folder, or even see what else is waiting until you have answered. The
 * dock keeps the question in the one place you are already looking — between
 * the conversation and the box you would type in — while the rest of the app
 * stays live.
 *
 * Nothing here takes focus. The old dialog focused its deny button so a stray
 * Enter could not approve anything; a dock that stole focus would fight the
 * composer for every keystroke instead. The same safety property now comes from
 * a different place: no key answers this, only a click on a named button.
 *
 * The panel cannot be dismissed, which is what a modal was really buying. An
 * unanswered approval is a parked turn, and a card you can close is a turn with
 * no way back to it.
 */
export function Approval({
  request,
  plan,
  draft,
  onAnswer,
  onDecidePlan,
  onPlanDraft,
}: {
  request: ApprovalRequest;
  plan: Plan | null;
  draft: PlanDraft | null;
  onAnswer: (decision: Decision, comment: string) => void;
  onDecidePlan: (choice: PlanDecision) => void;
  onPlanDraft: (draft: PlanDraft) => void;
}) {
  // Question forms are recognized by the shape of the call, not by the tool's
  // name: `ask_user` is the tool that produces this input, and anything else
  // that ever produces the same `questions` array means the same thing by it.
  const questions = useMemo(() => readQuestions(request.input), [request.input]);
  // A plan review, recognized the same way: core attaches the saved plan body to
  // the review copy of the call, and that body is what makes this a document to
  // read rather than an action to authorize. The editor works from the *file*
  // (`plan`), because that has every phase's prose — the call carries only what
  // the model happened to resend.
  const review = useMemo(() => isPlanReview(request.input), [request.input]);

  return (
    <section
      className={`approval${questions ? " is-question" : ""}${review ? " is-plan" : ""}`}
      role="region"
      aria-label={review ? "A plan for you to review" : questions ? "A question for you" : "Approval needed"}
    >
      {/* The dot is the same "needs you" amber the rail uses for this session,
          so the two surfaces agree at a glance about which state this is. */}
      <header className="approval-head">
        <StatusDot status="waiting" />
        <h2>{title(request, questions, review)}</h2>
        {!questions && !review && <span className="approval-tool">{request.tool}</span>}
      </header>
      {review && plan && draft ? (
        <PlanEditor
          key={request.id}
          plan={plan}
          draft={draft}
          mode="review"
          onDraft={onPlanDraft}
          onDecide={onDecidePlan}
        />
      ) : questions ? (
        <QuestionForm key={request.id} questions={questions} onAnswer={onAnswer} />
      ) : (
        <Consent key={request.id} request={request} onAnswer={onAnswer} />
      )}
    </section>
  );
}

function title(
  request: ApprovalRequest,
  questions: Question[] | null,
  review: boolean,
): string {
  if (review) return "Review this plan";
  if (!questions) return request.is_edit ? "Change a file?" : "Run this?";
  return questions.length === 1 ? questions[0].question : "A few questions";
}

/** The consent surface: the exact call, then the four answers. It shows tool,
 *  target and raw input because approving something you were not shown is not
 *  consent. */
function Consent({
  request,
  onAnswer,
}: {
  request: ApprovalRequest;
  onAnswer: (decision: Decision, comment: string) => void;
}) {
  const [comment, setComment] = useState("");
  const [showInput, setShowInput] = useState(false);
  const diffable = isEditShape(request.input);

  return (
    <>
      <div className="approval-body">
        <p className="approval-target">{request.descriptor}</p>
        {request.summary && <p className="approval-summary">{request.summary}</p>}

        {diffable && <Diff input={request.input} />}

        <button
          className="link-btn approval-toggle"
          onClick={() => setShowInput((was) => !was)}
          aria-expanded={showInput}
        >
          {showInput ? "hide the raw call" : diffable ? "show the raw call" : "show the exact call"}
        </button>
        {showInput && (
          <pre className="approval-input">{JSON.stringify(request.input, null, 2)}</pre>
        )}
      </div>

      <input
        className="approval-comment"
        value={comment}
        placeholder="Optional note — guidance for a yes, a reason for a no"
        onChange={(event) => setComment(event.target.value)}
      />

      <div className="approval-actions">
        <button className="btn btn-secondary" onClick={() => onAnswer("no", comment)}>
          No
        </button>
        <div className="approval-yes">
          {request.allows_project && (
            <button className="btn btn-ghost" onClick={() => onAnswer("yes-project", comment)}>
              Always here
            </button>
          )}
          <button className="btn btn-ghost" onClick={() => onAnswer("yes-session", comment)}>
            This session
          </button>
          <button className="btn btn-primary" onClick={() => onAnswer("yes", comment)}>
            Yes, once
          </button>
        </div>
      </div>
    </>
  );
}

/* ------------------------------------------------------------------ questions */

export type Choice = {
  label: string;
  description: string;
  /** The artifact this option produces, shown beside the list. */
  preview: string | null;
};

export type Question = {
  question: string;
  options: Choice[];
  multi: boolean;
};

/** The escape hatch the harness adds to every question. The model never writes
 *  it: a menu it wrote cannot contain the answer it failed to think of. */
const OTHER = "Something else";

/**
 * The `ask_user` form.
 *
 * The TUI pages through questions because a terminal has one screenful; here
 * they are all on screen, which is the entire reason for a window. What must
 * stay identical is the *answer*, because the harness turns it into one note
 * the model reads: one question answers with its own text, several answer as
 * `N. question → answer` lines.
 */
function QuestionForm({
  questions,
  onAnswer,
}: {
  questions: Question[];
  onAnswer: (decision: Decision, comment: string) => void;
}) {
  // One entry per question: which options are ticked, plus its note. Single
  // select keeps at most one index, so the two modes share one state shape.
  const [picked, setPicked] = useState<number[][]>(() => questions.map(() => []));
  const [notes, setNotes] = useState<string[]>(() => questions.map(() => ""));
  const [focused, setFocused] = useState<number[]>(() => questions.map(() => 0));

  const choose = (q: number, option: number) =>
    setPicked((was) =>
      was.map((chosen, index) => {
        if (index !== q) return chosen;
        if (!questions[q].multi) return [option];
        return chosen.includes(option)
          ? chosen.filter((i) => i !== option)
          : [...chosen, option].sort((a, b) => a - b);
      }),
    );

  const answers = questions.map((question, index) =>
    answerOf(question, picked[index], notes[index]),
  );
  // An unanswered question, or "Something else" with nothing typed, is not an
  // answer: sending it would tell the model the user chose something they
  // explicitly did not choose.
  const ready = questions.every((question, index) => {
    if (picked[index].length === 0) return false;
    return !picksOther(question, picked[index]) || notes[index].trim().length > 0;
  });

  return (
    <>
      <div className="approval-body">
        {questions.map((question, index) => (
          <fieldset className="question" key={index}>
            {questions.length > 1 && <legend>{question.question}</legend>}
            <div className="question-split">
              <ul className="options">
                {question.options.map((option, optionIndex) => (
                  <li key={optionIndex}>
                    <button
                      className={`option${picked[index].includes(optionIndex) ? " is-picked" : ""}${
                        focused[index] === optionIndex ? " is-focused" : ""
                      }`}
                      aria-pressed={picked[index].includes(optionIndex)}
                      onClick={() => {
                        choose(index, optionIndex);
                        setFocused((was) =>
                          was.map((at, i) => (i === index ? optionIndex : at)),
                        );
                      }}
                      onMouseEnter={() =>
                        setFocused((was) => was.map((at, i) => (i === index ? optionIndex : at)))
                      }
                    >
                      <span className="option-label">{option.label}</span>
                      {option.description && (
                        <span className="option-description">{option.description}</span>
                      )}
                    </button>
                  </li>
                ))}
              </ul>
              {/* Re-rendered as the pointer moves between options, which is what
                  the tool's own description promises of a preview. */}
              {question.options[focused[index]]?.preview && (
                <pre className="option-preview">{question.options[focused[index]].preview}</pre>
              )}
            </div>
            <input
              className="approval-comment"
              value={notes[index]}
              placeholder={
                picksOther(question, picked[index])
                  ? "Your answer — this replaces the options"
                  : "Optional note"
              }
              onChange={(event) =>
                setNotes((was) =>
                  was.map((note, i) => (i === index ? event.target.value : note)),
                )
              }
            />
          </fieldset>
        ))}
      </div>

      <div className="approval-actions">
        <button className="btn btn-secondary" onClick={() => onAnswer("no", "")}>
          Don&rsquo;t answer
        </button>
        <div className="approval-yes">
          <button
            className="btn btn-primary"
            disabled={!ready}
            onClick={() => onAnswer("yes", aggregate(questions, answers))}
          >
            Send
          </button>
        </div>
      </div>
    </>
  );
}

function picksOther(question: Question, picked: number[]): boolean {
  return picked.some((index) => question.options[index]?.label === OTHER);
}

/**
 * One question's answer: the chosen labels plus any note.
 *
 * "Something else" is the exception — there the typed text *is* the answer and
 * no label may be reported beside it, or the model reads the rejected menu item
 * as the user's choice. This mirrors `QuestionPage::answer` in the TUI, which
 * is the definition the model has been trained against by every other session.
 */
export function answerOf(question: Question, picked: number[], note: string): string {
  const text = note.trim();
  const labels = picked
    .map((index) => question.options[index]?.label ?? "")
    .filter((label) => label && label !== OTHER);
  if (picksOther(question, picked)) return [...labels, text].join(", ");
  const joined = labels.join(", ");
  return text ? `${joined} — ${text}` : joined;
}

export function aggregate(questions: Question[], answers: string[]): string {
  if (questions.length === 1) return answers[0];
  return questions
    .map((question, index) => `${index + 1}. ${question.question} → ${answers[index]}`)
    .join("\n");
}

/**
 * The `questions` array out of an `ask_user` call, or null if this call is not
 * one. Tolerates the legacy single `question` + `options` shape and bare string
 * options, exactly as the TUI does — old sessions replay through here too.
 */
export function readQuestions(input: unknown): Question[] | null {
  if (typeof input !== "object" || input === null) return null;
  const record = input as Record<string, unknown>;
  const raw = Array.isArray(record.questions)
    ? record.questions
    : record.question !== undefined
      ? [record]
      : null;
  if (!raw || raw.length === 0) return null;

  const questions: Question[] = [];
  for (const entry of raw) {
    if (typeof entry !== "object" || entry === null) return null;
    const item = entry as Record<string, unknown>;
    if (typeof item.question !== "string" || !Array.isArray(item.options)) return null;
    const options = item.options.map(readChoice);
    questions.push({
      question: item.question,
      // The harness always offers a way out of a menu that missed the point.
      options: [...options, { label: OTHER, description: "none of these — type your own answer", preview: null }],
      multi: item.multiSelect === true,
    });
  }
  return questions;
}

function readChoice(value: unknown): Choice {
  if (typeof value === "string") return { label: value, description: "", preview: null };
  const record = (value ?? {}) as Record<string, unknown>;
  const preview = typeof record.preview === "string" ? record.preview.trim() : "";
  return {
    label: typeof record.label === "string" ? record.label : "",
    description: typeof record.description === "string" ? record.description : "",
    preview: preview.length > 0 ? preview : null,
  };
}
