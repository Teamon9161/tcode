import { useMemo, useState } from "react";

import type { ApprovalMode, ApprovalRequest, Decision } from "./types";
import { Code } from "./components/Code";
import { Diff, isEditShape } from "./components/Diff";
import { Path } from "./components/Path";
import { StatusDot } from "./components/Status";
import { PlanEditor, type PlanDecision } from "./PlanEditor";
import { isPlanReview, type Plan, type PlanDraft } from "./plan";
import { callTarget, useToolName } from "./toolViews";

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
  onAnswer: (decision: Decision, comment: string, setMode?: ApprovalMode) => void;
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
  const call = useMemo(() => readCall(request), [request]);
  const toolName = useToolName();
  // A question and a plan review are not actions being authorized, so neither
  // the action title nor the tool chip applies to them.
  const consent = !questions && !review;

  return (
    <section
      className={`dock approval${questions ? " is-question" : ""}${review ? " is-plan" : ""}`}
      role="region"
      aria-label={review ? "A plan for you to review" : questions ? "A question for you" : "Approval needed"}
    >
      {/* The dot is the same "needs you" amber the rail uses for this session,
          so the two surfaces agree at a glance about which state this is.

          The header names the action exactly once. "Run this?" and "Change a
          file?" already are the tool's own verb — `shell`'s `display_name()` is
          literally "Run" — so a chip beside them printed the same word twice.
          It appears only where the title had to fall back to the generic
          question, which is precisely when the tool's name is the whole
          identity of the call: a web fetch, an MCP server's tool. */}
      <header className="approval-head">
        <StatusDot status="waiting" />
        <h2>{title(request, questions, review, call)}</h2>
        {consent && !named(request, call) && (
          <span className="approval-tool">{toolName(request.tool)}</span>
        )}
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
        <Consent key={request.id} request={request} call={call} onAnswer={onAnswer} />
      )}
    </section>
  );
}

/**
 * What this call is, taken from the call itself.
 *
 * Core hands over two strings that say the same thing in different punctuation:
 * `descriptor` is the rule a lasting yes would save (`run(git status)`), and
 * `summary` is its sentence form (`run: git status`). Stacked, they made the
 * reader compare two lines to discover they were equal, and the parentheses are
 * permission-rule syntax rather than anything a person reads. So neither is the
 * display any more. The command comes out of the call's own input — whole, with
 * its line breaks intact, which is the one thing a one-line summary could never
 * carry — and the descriptor moves to where it is actually a fact, beside the
 * buttons that would persist it.
 */
type Call = {
  /** A command, whole. Recognized by the shape of the input like every other
   *  branch in this file: `command` is what a run tool takes, whatever the tool
   *  happens to be called. */
  command: string | null;
  /** Otherwise what the call is about: a path, a URL, a pattern. */
  target: string;
  /** Where the command was told to run, when it named somewhere. Absent means
   *  the session's own folder, which the pane header already says. */
  cwd: string | null;
};

export function readCall(request: ApprovalRequest): Call {
  const input =
    typeof request.input === "object" && request.input !== null
      ? (request.input as Record<string, unknown>)
      : {};
  const command = typeof input.command === "string" ? input.command.trim() : "";
  const cwd = typeof input.cwd === "string" ? input.cwd.trim() : "";
  return {
    command: command || null,
    // Core's summary is the last resort rather than the first: for a tool this
    // app has never heard of it is the only sentence anyone wrote about the
    // call, and a bare descriptor is better than an empty line.
    target: callTarget(request.tool, request.input) || request.summary || request.descriptor,
    cwd: cwd || null,
  };
}

/** Whether the title already names what the tool does. */
export function named(request: ApprovalRequest, call: Call): boolean {
  return request.is_edit || call.command !== null;
}

export function title(
  request: ApprovalRequest,
  questions: Question[] | null,
  review: boolean,
  call: Call,
): string {
  if (review) return "Review this plan";
  if (questions) return questions.length === 1 ? questions[0].question : "A few questions";
  if (request.is_edit) return "Change a file?";
  if (call.command) return "Run this?";
  // Everything else is a tool whose verb the header cannot guess. The chip
  // beside this says which one, and the target below says on what.
  return "Allow this?";
}

/** The consent surface: the call, then the four answers. It shows the command
 *  or target, the diff, and the raw input behind a disclosure, because
 *  approving something you were not shown is not consent. */
function Consent({
  request,
  call,
  onAnswer,
}: {
  request: ApprovalRequest;
  call: Call;
  onAnswer: (decision: Decision, comment: string, setMode?: ApprovalMode) => void;
}) {
  const [comment, setComment] = useState("");
  const [showInput, setShowInput] = useState(false);
  const diffable = isEditShape(request.input);

  return (
    <>
      <div className="approval-body">
        {/* A command is code, so it is drawn by the thing that draws code —
            the same `Code` block a ```sh fence in the conversation gets, with
            the same highlighting and the same copy button. A flat mono slab on
            a grey rectangle was a second, worse code affordance invented for
            the one place in the app where reading code exactly is the whole
            task: an unlit wall of text is where a `rm` hides in the middle of a
            pipeline. Highlighting is not decoration here, it is the structure
            of what you are agreeing to. */}
        {call.command ? (
          <div className="approval-command">
            <Code source={call.command} language="sh" />
          </div>
        ) : (
          <p className="approval-target">{call.target}</p>
        )}
        {/* Only when the call named a directory of its own. Silence means the
            session's folder, which the pane header is already showing. */}
        {call.cwd && (
          <p className="approval-where">
            in <Path path={call.cwd} keep={2} className="approval-path" />
          </p>
        )}

        {diffable && <Diff input={request.input} />}

        <button
          className="link-btn approval-toggle"
          onClick={() => setShowInput((was) => !was)}
          aria-expanded={showInput}
        >
          {showInput ? "hide the exact call" : "show the exact call"}
        </button>
        {showInput && (
          <div className="approval-exact">
            {request.allows_project && (
              <p className="approval-rule">
                a yes beyond this once saves the rule <code>{request.descriptor}</code>
              </p>
            )}
            <pre className="approval-input">{JSON.stringify(request.input, null, 2)}</pre>
          </div>
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
          {request.is_edit ? (
            <button
              className="btn btn-ghost"
              onClick={() => onAnswer("yes", comment, "accept-edits")}
              title="Allow file edits for the rest of this session"
            >
              Yes, allow all edits
            </button>
          ) : (
            <>
              {request.allows_project && (
                <button
                  className="btn btn-ghost"
                  onClick={() => onAnswer("yes-project", comment)}
                  // The rule itself, not a description of one: this is the
                  // string that lands in the file, and the reader has to be
                  // able to tell how wide it is before granting it.
                  title={`Saves the rule ${request.descriptor} in this project's .tcode/config.toml`}
                >
                  Yes, allow in this project
                </button>
              )}
              <button
                className="btn btn-ghost"
                onClick={() => onAnswer("yes-session", comment)}
                title={`Allows ${request.descriptor} for the rest of this session`}
              >
                Yes, for this session
              </button>
            </>
          )}
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
  onAnswer: (decision: Decision, comment: string, setMode?: ApprovalMode) => void;
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
