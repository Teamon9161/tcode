import { useMemo, useRef, type CSSProperties, type RefObject } from "react";

import type { Decision, SessionInfo, Status } from "./types";
import { SessionContext, type SessionState } from "./session";
import {
  canBack,
  canForward,
  inspectTitle,
  navBack,
  navForward,
  navValue,
  type Inspect,
} from "./inspect";
import { frames, type Leaf, type PlacedDivider, type Rect, type Tiling } from "./layout";
import { FolderMenu } from "./FolderMenu";
import { StatusDot } from "./components/Status";
import { BackIcon, CloseIcon, ForwardIcon, PanelIcon } from "./components/Icons";
import { Transcript } from "./Transcript";
import { Composer } from "./Composer";
import { InspectView } from "./Inspector";
import { Approval } from "./Approval";
import { ProgressStrip } from "./ProgressStrip";
import type { PlanDecision } from "./PlanEditor";
import { draftOf, isPlanReview, type PlanDraft } from "./plan";

/**
 * The tiling field: the pane tree, drawn.
 *
 * A split is a flex container holding two children and one divider, so the
 * recursive data structure in `layout.ts` renders as a recursive component and
 * nothing here has to know how deep it is. Ratios become `flex-grow`, which
 * means the browser does the arithmetic and a window resize needs no code at
 * all.
 *
 * Every pane wears the same frame — a thin header and a body — whatever is
 * inside it. That is what makes a diff and a conversation feel like two of the
 * same thing rather than a panel bolted to the side of the app.
 */

/** Everything a pane needs that is not its own shape. Passed as one object
 *  because it goes down every level of the recursion untouched. */
export type PaneContext = {
  sessions: SessionInfo[];
  stateOf: (session: string) => SessionState;
  statusOf: (session: string) => Status;
  focus: string;
  /** True once the window holds more than one pane; below that, "which pane is
   *  current" is not a question anybody is asking. */
  split: boolean;
  onFocus: (pane: string) => void;
  onClosePane: (pane: string) => void;
  onRatio: (divider: string, ratio: number) => void;
  onOpen: (pane: string, session: string, value: Inspect) => void;
  onNavigate: (pane: string, step: typeof navBack) => void;
  onToggleFiles: (pane: string, session: string) => void;
  onDraft: (session: string, value: string) => void;
  onAttach: (session: string, items: SessionState["attachments"]) => void;
  onDetach: (session: string, id: string) => void;
  onSend: (session: string) => void;
  onInterrupt: (session: string) => void;
  onAnswer: (session: string, decision: Decision, comment: string) => void;
  onDecidePlan: (session: string, choice: PlanDecision) => void;
  onPlanDraft: (session: string, draft: PlanDraft) => void;
  onSavePlan: (session: string) => void;
  onPlanOpen: (session: string, open: boolean) => void;
  onPlanFirst: (session: string, on: boolean) => void;
  /** Start a conversation in another folder, from this pane's folder chip. */
  onOpenFolder: (path: string) => Promise<void>;
};

/**
 * One flat layer of positioned panes, never a nest of containers.
 *
 * Every pane is a direct child of the field and keyed by its leaf id, so React
 * keeps the same component instance no matter how the tree around it changes.
 * That is what lets a conversation keep its scroll position when a panel opens
 * beside it — see `frames` in `layout.ts` for what nesting cost.
 */
export function Panes({ tiling, context }: { tiling: Tiling; context: PaneContext }) {
  const field = useRef<HTMLDivElement>(null);
  const { panes, dividers } = useMemo(() => frames(tiling), [tiling]);
  if (!tiling.root) return null;

  return (
    <div className={`panes${context.split ? " is-split" : ""}`}>
      <div className="panes-field" ref={field}>
        {panes.map(({ leaf, rect }) => (
          <div key={leaf.id} className="pane-slot" style={box(rect)}>
            <Pane leaf={leaf} context={context} />
          </div>
        ))}
        {dividers.map((divider) => (
          <Divider
            key={divider.id}
            divider={divider}
            field={field}
            onRatio={context.onRatio}
          />
        ))}
      </div>
    </div>
  );
}

/** A rect of the field as CSS percentages. */
function box(rect: Rect): CSSProperties {
  return {
    left: `${rect.left * 100}%`,
    top: `${rect.top * 100}%`,
    width: `${rect.width * 100}%`,
    height: `${rect.height * 100}%`,
  };
}

/**
 * The handle between two panes.
 *
 * The drag reads the pointer's absolute position on the field and converts it
 * back into this split's own ratio, rather than tracking a delta from where the
 * pointer went down. That way the divider stays exactly under the cursor even
 * after the clamp in `setRatio` has refused to go any further — a delta would
 * accumulate the refused movement and leave the handle behind the pointer.
 */
function Divider({
  divider,
  field,
  onRatio,
}: {
  divider: PlacedDivider;
  field: RefObject<HTMLDivElement | null>;
  onRatio: (divider: string, ratio: number) => void;
}) {
  const { id, dir, ratio, within } = divider;
  const row = dir === "row";

  const drag = (event: React.PointerEvent<HTMLDivElement>) => {
    const area = field.current?.getBoundingClientRect();
    if (!area) return;
    event.currentTarget.setPointerCapture(event.pointerId);
    const move = (moved: PointerEvent) => {
      const onField = row
        ? (moved.clientX - area.left) / area.width
        : (moved.clientY - area.top) / area.height;
      const span = row ? within.width : within.height;
      const start = row ? within.left : within.top;
      if (span > 0) onRatio(id, (onField - start) / span);
    };
    const stop = () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", stop);
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", stop);
  };

  const place: CSSProperties = row
    ? {
        left: `${(within.left + within.width * ratio) * 100}%`,
        top: `${within.top * 100}%`,
        height: `${within.height * 100}%`,
      }
    : {
        top: `${(within.top + within.height * ratio) * 100}%`,
        left: `${within.left * 100}%`,
        width: `${within.width * 100}%`,
      };

  return (
    <div
      className={`divider is-${dir}`}
      style={place}
      role="separator"
      aria-orientation={row ? "vertical" : "horizontal"}
      aria-label="Resize these panes"
      aria-valuenow={Math.round(ratio * 100)}
      tabIndex={0}
      onPointerDown={drag}
      onKeyDown={(event) => {
        const less = row ? "ArrowLeft" : "ArrowUp";
        const more = row ? "ArrowRight" : "ArrowDown";
        if (event.key === less) onRatio(id, ratio - 0.02);
        if (event.key === more) onRatio(id, ratio + 0.02);
      }}
    />
  );
}

/** The frame every pane wears. Focus follows the pointer down rather than a
 *  click, so dragging a divider or selecting text in a pane also makes it the
 *  current one — the same moment a window manager would have taken focus. */
function Pane({ leaf, context }: { leaf: Leaf; context: PaneContext }) {
  const current = context.split && leaf.id === context.focus;
  return (
    <section
      className={`pane${current ? " is-focused" : ""}`}
      // Read back by the directional focus keys, which need boxes rather than
      // the tree to answer "what is to the left of here" (see `focus.ts`).
      data-pane={leaf.id}
      onPointerDownCapture={() => context.onFocus(leaf.id)}
    >
      {/* A pane is where "which conversation" is answered, so it is where the
          answer is published. Leaves that need it — a `show` artifact loading
          its file — read it from here instead of having it threaded through
          every layer of the transcript. */}
      <SessionContext.Provider value={leaf.pane.session}>
        {leaf.pane.kind === "session" ? (
          <SessionPane leaf={leaf} session={leaf.pane.session} context={context} />
        ) : (
          <InspectPane leaf={leaf} context={context} />
        )}
      </SessionContext.Provider>
    </section>
  );
}

function SessionPane({
  leaf,
  session,
  context,
}: {
  leaf: Leaf;
  session: string;
  context: PaneContext;
}) {
  const info = context.sessions.find((open) => open.id === session);
  const state = context.stateOf(session);
  // A draft is built once per plan, not per render: `draftOf` mints the row ids
  // the editor keys on, so rebuilding it while someone is typing would remount
  // every field and take the caret with it.
  const draft = useMemo(
    () => state.planDraft ?? (state.plan ? draftOf(state.plan) : null),
    [state.planDraft, state.plan],
  );
  if (!info) return <p className="pane-empty">this conversation was closed</p>;

  return (
    <>
      <header className="pane-head">
        <StatusDot status={context.statusOf(session)} />
        {/* The name and the path were the same fact twice — a conversation is
            named after its folder — so they are one control now. It was already
            the answer to "which folder"; making it the control for it too is
            what let the rail stop carrying a button that started a different
            kind of thing than everything else in it. */}
        <FolderMenu
          name={info.name}
          cwd={info.cwd}
          home={info.home}
          onOpenFolder={context.onOpenFolder}
        />
        <button
          className="icon-btn"
          onClick={() => context.onToggleFiles(leaf.id, session)}
          aria-label={`Files ${info.name} touched`}
          title={`Files ${info.name} touched`}
        >
          <PanelIcon size={14} />
        </button>
        <button
          className="icon-btn"
          onClick={() => context.onClosePane(leaf.id)}
          aria-label={`Hide ${info.name}`}
          title="Hide this pane — the conversation keeps running"
        >
          <CloseIcon size={14} />
        </button>
      </header>

      {/* One surface from here down: the transcript, the approval dock and the
          composer are the same sheet, not three panels with rules between. */}
      <div className="pane-body is-stage">
        <Transcript
          blocks={state.blocks}
          running={state.running}
          onOpen={(value) => context.onOpen(leaf.id, session, value)}
        />

        {/* Docked, not modal: every other pane stays readable while this one
            waits. See `Approval.tsx`. */}
        {state.approval && (
          <Approval
            request={state.approval}
            plan={state.plan}
            draft={draft}
            onAnswer={(decision, comment) => context.onAnswer(session, decision, comment)}
            onDecidePlan={(choice) => context.onDecidePlan(session, choice)}
            onPlanDraft={(draft) => context.onPlanDraft(session, draft)}
          />
        )}

        {/* Above the composer, below whatever is being asked: the plan is
            context for what you are about to type, and it must not push an
            unanswered question further from the answer. Absent entirely when
            there is no plan — an empty strip over every composer is furniture.
            While a review is open it stays out of the way: the panel above *is*
            the plan, in full, and two of them would be two answers to "which
            plan is this". */}
        {state.plan && !(state.approval && isPlanReview(state.approval.input)) && (
          <ProgressStrip
            plan={state.plan}
            expanded={state.planOpen}
            running={state.running}
            onToggle={() => context.onPlanOpen(session, !state.planOpen)}
            onOpen={() => context.onOpen(leaf.id, session, { kind: "plan" })}
          />
        )}

        <Composer
          value={state.draft}
          running={state.running}
          disabled={false}
          attachments={state.attachments}
          meter={state.meter}
          planFirst={state.planFirst}
          onPlanFirst={(on) => context.onPlanFirst(session, on)}
          onChange={(value) => context.onDraft(session, value)}
          onAttach={(items) => context.onAttach(session, items)}
          onDetach={(id) => context.onDetach(session, id)}
          onSubmit={() => context.onSend(session)}
          onInterrupt={() => context.onInterrupt(session)}
        />
      </div>
    </>
  );
}

function InspectPane({ leaf, context }: { leaf: Leaf; context: PaneContext }) {
  const pane = leaf.pane.kind === "inspect" ? leaf.pane : null;
  const state = context.stateOf(pane?.session ?? "");
  // Same reason as the session pane: one draft per plan, not one per render.
  const draft = useMemo(
    () => state.planDraft ?? (state.plan ? draftOf(state.plan) : null),
    [state.planDraft, state.plan],
  );
  if (!pane) return null;
  const { session, nav } = pane;
  const value = navValue(nav);
  const info = context.sessions.find((open) => open.id === session);

  return (
    <>
      <header className="pane-head">
        <div className="pane-history">
          <button
            className="icon-btn"
            onClick={() => context.onNavigate(leaf.id, navBack)}
            disabled={!canBack(nav)}
            aria-label="Back"
          >
            <BackIcon size={14} />
          </button>
          <button
            className="icon-btn"
            onClick={() => context.onNavigate(leaf.id, navForward)}
            disabled={!canForward(nav)}
            aria-label="Forward"
          >
            <ForwardIcon size={14} />
          </button>
        </div>
        <span className="pane-name">{inspectTitle(value)}</span>
        <button
          className="icon-btn"
          onClick={() => context.onClosePane(leaf.id)}
          aria-label="Close this pane"
        >
          <CloseIcon size={14} />
        </button>
      </header>

      <div className="pane-body is-inspect">
        <InspectView
          value={value}
          blocks={state.blocks}
          files={state.files}
          cwd={info?.cwd ?? ""}
          plan={state.plan}
          planDraft={draft}
          onOpen={(next) => context.onOpen(leaf.id, session, next)}
          onPlanDraft={(draft) => context.onPlanDraft(session, draft)}
          onSavePlan={() => context.onSavePlan(session)}
        />
      </div>
    </>
  );
}
