import {
  memo,
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type CSSProperties,
  type RefObject,
} from "react";

import type { ApprovalMode, Decision, SessionInfo, Status } from "./types";
import { BLANK, SessionContext, type SessionState } from "./session";
import {
  canBack,
  canForward,
  inspectTitle,
  navBack,
  navForward,
  navValue,
  type Inspect,
} from "./inspect";
import {
  frames,
  paneSession,
  type Leaf,
  type PlacedDivider,
  type Rect,
  type Tiling,
} from "./layout";
import { linkTarget } from "./links";
import { LinkContext, type Follow } from "./Prose";
import { basename } from "./show";
import {
  browserPlacementHeld,
  registerBrowserPlacement,
  yieldBrowser,
} from "./browserYield";
import * as browserHost from "./webHost";
import { FolderMenu } from "./FolderMenu";
import { statusLabel } from "./activity";
import { sessionTitle } from "./railData";
import { StatusDot } from "./components/Status";
import {
  BackIcon,
  CloseIcon,
  CollapseIcon,
  ColumnsIcon,
  ExpandIcon,
  FileIcon,
  FolderIcon,
  ForwardIcon,
  PencilIcon,
  RefreshIcon,
  RowsIcon,
  SwapIcon,
} from "./components/Icons";
import { Transcript } from "./Transcript";
import { Composer } from "./Composer";
import { InspectView } from "./Inspector";
import { WebPane } from "./WebPane";
import { TermPane } from "./TermPane";
import { Approval } from "./Approval";
import { ProgressStrip } from "./ProgressStrip";
import { QueueStrip } from "./QueueStrip";
import { RewindBar } from "./RewindBar";
import type { RewindTarget } from "./rewind";
import type { PlanDecision } from "./PlanEditor";
import { draftOf, isPlanReview, type PlanDraft } from "./plan";
import { MOD } from "./keys";
import {
  matchesWorkspaceFileControls,
  WorkspaceFileControlsContext,
  type WorkspaceFileControls,
} from "./workspaceFileControls";

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
  focus: string;
  /** True once the window holds more than one pane; below that, "which pane is
   *  current" is not a question anybody is asking. */
  split: boolean;
  onFocus: (pane: string) => void;
  onClosePane: (pane: string) => void;
  /** Turn a seam: the two panes it separates swap between beside and stacked. */
  onRotate: (divider: string) => void;
  /** Exchange the two subtrees the seam separates, preserving their pane ids. */
  onSwap: (divider: string) => void;
  onRatio: (divider: string, ratio: number) => void;
  onOpen: (pane: string, session: string, value: Inspect) => void;
  /** Show it *as well*, in a pane of its own — the deliberate "keep this one and
   *  add another" act, which is why it is a second callback rather than a flag. */
  onOpenAside: (pane: string, session: string, value: Inspect) => void;
  /** Put `@path` in this conversation's composer — what the tree's "mention"
   *  does. It writes a draft rather than sending, because naming a file is the
   *  start of a sentence somebody is still writing. */
  onMention: (session: string, path: string) => void;
  onNavigate: (pane: string, step: typeof navBack) => void;
  onToggleFiles: (pane: string, session: string) => void;
  onToggleWorkspace: (pane: string, session: string) => void;
  /** Open the window-owned browser from the local file-navigation tool group. */
  onOpenBrowser: () => void;
  /** Hand a browser tab to the conversation being worked in — the only way a
   *  model learns about a tab it did not open (`../AGENT-BROWSER.md`). Like
   *  `onMention` it writes a draft; unlike it, the pane has no session of its
   *  own, so which conversation is read off the focused pane. */
  onHandOverTab: (tab: string) => void;
  /** Reveal exactly the native Browser tab associated with a transcript group.
   *  Unknown or closed capabilities do nothing; they never fall back to the
   *  tab that happens to be current. */
  onRevealBrowserTab: (tab: string) => void;
  /** Show, focus or hide the window's terminals — the three states `Mod+J`
   *  steps through (`toggleTerminal` in `layout.ts`). */
  onToggleTerminal: () => void;
  /** Where a new terminal tab starts. The focused conversation's folder, since
   *  with the window split "the current folder" is otherwise a guess — the same
   *  reason `Mod+N` reads it off the focused pane (AGENTS.md rule 9c). */
  terminalCwd: string;
  /** Send the window's browser to a page — what following a link in prose does.
   *  Distinct from `onOpenBrowser`, which is a toggle and would put the browser
   *  away exactly when somebody asked it for something. */
  onOpenUrl: (url: string) => void;
  /** The address the window has been asked to visit, and a serial so asking for
   *  the same page twice is two requests. It travels to the pane rather than
   *  going straight to the backend because the native webview does not exist
   *  until `WebPane` mounts and opens it — see the note there. */
  webRequest: { url: string; at: number } | null;
  /** The currently focused content takes the full pane field without changing
   *  the tiling tree, so leaving focus restores every pane in place. */
  expanded: string | null;
  onToggleExpanded: (pane: string) => void;
  onDraft: (session: string, value: string) => void;
  onAttach: (session: string, items: SessionState["attachments"]) => void;
  onDetach: (session: string, id: string) => void;
  /** The text comes from the composer rather than from this session's state:
   *  the draft is published on an idle, and a prompt typed and sent inside one
   *  would otherwise send the previous contents (`Composer.tsx`). */
  onSend: (session: string, text: string) => void;
  onInterrupt: (session: string) => void;
  /** Take one queued prompt back. Index *and* text: the backend refuses a pair
   *  that disagrees rather than dropping whatever now sits at that position. */
  onWithdrawQueued: (session: string, index: number, text: string) => void;
  /** Stop the turn that owns this queue and send it straight away. */
  onSendQueuedNow: (session: string, turn: number) => void;
  /** Ask what going back to this point would cost; `null` withdraws the ask. */
  onAskRewind: (session: string, target: RewindTarget | null) => void;
  onRewind: (session: string, restoreFiles: boolean) => void;
  onAnswer: (
    session: string,
    decision: Decision,
    comment: string,
    setMode?: ApprovalMode,
  ) => void;
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
export function Panes({
  tiling,
  context,
  stateOf,
  statusOf,
}: {
  tiling: Tiling;
  context: PaneContext;
  stateOf: (session: string) => SessionState;
  statusOf: (session: string) => Status;
}) {
  const field = useRef<HTMLDivElement>(null);
  const { panes, dividers } = useMemo(() => frames(tiling), [tiling]);
  const expanded = panes.some(({ leaf }) => leaf.id === context.expanded)
    ? context.expanded
    : null;
  if (!tiling.root) return null;

  return (
    <div className={`panes${context.split ? " is-split" : ""}`}>
      <div className="panes-field" ref={field}>
        {panes.map(({ leaf, rect }) => {
          const visible = !expanded || leaf.id === expanded;
          return (
            <PaneSlot
              key={leaf.id}
              leaf={leaf}
              rect={visible && expanded ? WHOLE : rect}
              context={context}
              stateOf={stateOf}
              statusOf={statusOf}
              expanded={expanded === leaf.id}
              hidden={!visible}
            />
          );
        })}
        {!expanded &&
          dividers.map((divider) => (
            <Divider
              key={divider.id}
              divider={divider}
              field={field}
              onRatio={context.onRatio}
              onRotate={context.onRotate}
              onSwap={context.onSwap}
            />
          ))}
      </div>
    </div>
  );
}

const WHOLE: Rect = { left: 0, top: 0, width: 1, height: 1 };

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
 * The cheap geometry layer around a memoized pane.
 *
 * Slot rectangles change on every divider frame and on structural moves such
 * as swapping two equally sized panes. The pane subtree should not redraw for
 * either, but a native browser page still has to follow both resize and pure
 * translation. Keeping the body ref and measurement here lets those two facts
 * coexist: this wrapper re-renders with the slot, while the pane beneath it can
 * keep its transcript, composer and browser chrome untouched.
 */
function PaneSlot({
  leaf,
  rect,
  context,
  stateOf,
  statusOf,
  expanded,
  hidden,
}: {
  leaf: Leaf;
  rect: Rect;
  context: PaneContext;
  stateOf: (session: string) => SessionState;
  statusOf: (session: string) => Status;
  expanded: boolean;
  hidden: boolean;
}) {
  const webBody = useRef<HTMLDivElement>(null);
  const placed = useRef("");
  const booked = useRef(0);
  const web = leaf.pane.kind === "web";
  const session = paneSession(leaf.pane);
  const state = session ? stateOf(session) : undefined;
  const status = session ? statusOf(session) : undefined;
  const nameOf = useCallback(
    (owner: string) =>
      sessionTitle(stateOf(owner).blocks) ??
      context.sessions.find((open) => open.id === owner)?.name ??
      "a conversation",
    [context.sessions, stateOf],
  );
  const webNameOf = web ? nameOf : undefined;

  const placeWeb = useCallback((first: boolean) => {
    const measured = webBody.current?.getBoundingClientRect();
    if (!measured) return;
    const bounds = {
      x: measured.left,
      y: measured.top,
      width: measured.width,
      height: measured.height,
    };
    const key = `${bounds.x},${bounds.y},${bounds.width},${bounds.height}`;
    if (!first && key === placed.current) return;
    placed.current = key;
    if (first) {
      browserHost.mount(bounds);
      return;
    }
    return browserHost.moved(bounds);
  }, []);

  const scheduleWeb = useCallback(
    (first: boolean) => {
      // A continuous divider/window resize owns the final placement. Reading a
      // dirty layout and crossing IPC for intermediate frames is pure overhead,
      // even while Electron correctly keeps the native page hidden.
      if (booked.current || browserPlacementHeld()) return;
      booked.current = requestAnimationFrame(() => {
        booked.current = 0;
        if (!browserPlacementHeld()) void placeWeb(first);
      });
    },
    [placeWeb],
  );

  useLayoutEffect(() => {
    if (!web) return;
    return registerBrowserPlacement(() => placeWeb(placed.current === ""));
  }, [placeWeb, web]);

  // Native window resizing has the same cost profile as dragging an internal
  // seam: Chromium emits many geometry samples for one visible outcome. Keep
  // the page out of the native tree until the resize goes quiet, then the
  // registered placement above reads and sends exactly the final rectangle.
  useEffect(() => {
    if (!web) return;
    let release: (() => void) | null = null;
    let finish = 0;
    const resized = () => {
      release ??= yieldBrowser();
      window.clearTimeout(finish);
      finish = window.setTimeout(() => {
        const done = release;
        release = null;
        done?.();
      }, 80);
    };
    window.addEventListener("resize", resized);
    return () => {
      window.removeEventListener("resize", resized);
      window.clearTimeout(finish);
      release?.();
    };
  }, [web]);

  // Deliberately no dependency array: the slot can move without resizing, and
  // ResizeObserver does not report that case. The actual DOM read waits until
  // the next frame, after the browser has already laid out the changed slot.
  useLayoutEffect(() => {
    if (web) scheduleWeb(placed.current === "");
    else placed.current = "";
  });

  useLayoutEffect(() => {
    if (!web || !webBody.current) return;
    const watch = new ResizeObserver(() => scheduleWeb(false));
    watch.observe(webBody.current);
    return () => watch.disconnect();
  }, [scheduleWeb, web]);

  useEffect(
    () => () => {
      if (booked.current) cancelAnimationFrame(booked.current);
    },
    [],
  );

  return (
    <div
      className={`pane-slot${hidden ? " is-hidden" : ""}`}
      style={box(rect)}
    >
      <Pane
        leaf={leaf}
        context={context}
        state={state}
        status={status}
        webBody={webBody}
        webNameOf={webNameOf}
        expanded={expanded}
        hidden={hidden}
      />
    </div>
  );
}

/**
 * The handle between two panes.
 *
 * The drag reads the pointer's absolute position on the field and converts it
 * back into this split's own ratio, rather than tracking a delta from where the
 * pointer went down. That way the divider stays exactly under the cursor even
 * after the clamp in `setRatio` has refused to go any further — a delta would
 * accumulate the refused movement and leave the handle behind the pointer.
 *
 * Two things keep it at the pointer's speed rather than the layout's:
 *
 *  - **One ratio per frame.** A pointer reports at the device's rate, which is
 *    well over 100Hz on current hardware, and every sample used to commit a new
 *    tiling and re-render the window. Frames are the rate at which any of that
 *    can be seen, so the samples in between are coalesced and the last one
 *    wins — dropping a sample costs nothing, since the next one carries the
 *    absolute position anyway.
 *  - **The browser stands down.** Following a pane means moving and resizing a
 *    native webview, which on this platform re-lays-out a whole page in another
 *    process. It is hidden for the length of the drag and shown again on
 *    release, which is what `browser.rs` has always said should happen here.
 *
 * It also carries the one control that *asks* for a direction. `dirFor` picks
 * one when a pane opens; turning it afterwards acts on the split, not on either
 * pane, so the button belongs here rather than in a pane header — where it
 * would be a sixth icon competing for room in exactly the narrow panes this
 * change exists to widen. It appears on hover, over the seam it turns.
 */
function Divider({
  divider,
  field,
  onRatio,
  onRotate,
  onSwap,
}: {
  divider: PlacedDivider;
  field: RefObject<HTMLDivElement | null>;
  onRatio: (divider: string, ratio: number) => void;
  onRotate: (divider: string) => void;
  onSwap: (divider: string) => void;
}) {
  const { id, dir, ratio, within } = divider;
  const row = dir === "row";

  const drag = (event: React.PointerEvent<HTMLDivElement>) => {
    const area = field.current?.getBoundingClientRect();
    if (!area) return;
    // A pointer going down on the seam must not also start a text selection.
    // Without this the default action runs, and a drag that passes over a
    // transcript leaves it highlighted from wherever the pointer entered — the
    // "it selected things while I resized" that made resizing feel unsafe.
    // Suppressing the default takes focus with it, so the handle asks for it
    // back; the ring is `:focus-visible`, so nothing appears for the pointer.
    event.preventDefault();
    event.currentTarget.focus({ preventScroll: true });
    event.currentTarget.setPointerCapture(event.pointerId);
    // The default action is only the *start* of a selection. A selection that
    // was already on screen extends under a drag in some engines, so the whole
    // window stops being selectable for the length of it — and anything
    // already highlighted is dropped, because that highlight is what would
    // otherwise grow.
    document.body.classList.add("is-resizing");
    window.getSelection()?.removeAllRanges();
    const restore = yieldBrowser();
    let frame = 0;
    let wanted: number | null = null;

    const commit = () => {
      frame = 0;
      if (wanted !== null) onRatio(id, wanted);
      wanted = null;
    };
    const move = (moved: PointerEvent) => {
      const onField = row
        ? (moved.clientX - area.left) / area.width
        : (moved.clientY - area.top) / area.height;
      const span = row ? within.width : within.height;
      const start = row ? within.left : within.top;
      if (span <= 0) return;
      wanted = (onField - start) / span;
      if (!frame) frame = requestAnimationFrame(commit);
    };
    const stop = () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", stop);
      window.removeEventListener("pointercancel", stop);
      // The last sample must land even if the pointer went up before its frame
      // did, or the divider settles one frame behind where it was let go.
      if (frame) cancelAnimationFrame(frame);
      commit();
      document.body.classList.remove("is-resizing");
      restore();
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", stop);
    // A drag the platform takes away — a window switch, a touch cancelled —
    // still has to give the browser back.
    window.addEventListener("pointercancel", stop);
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

  const turn = row ? "Stack these panes" : "Put these panes side by side";
  const exchange = "Swap these panes";

  return (
    <div className={`seam is-${dir}`} style={place}>
      <div
        className="divider"
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
      <button
        type="button"
        className="seam-turn"
        // The seam under it starts a drag on pointer-down, and a click is a
        // pointer-down first: without this, pressing the button resizes the
        // panes by however far the pointer drifted before it came back up.
        onPointerDown={(event) => event.stopPropagation()}
        onClick={() => onRotate(id)}
        aria-label={turn}
        title={turn}
      >
        {row ? <RowsIcon size={13} /> : <ColumnsIcon size={13} />}
      </button>
      <button
        type="button"
        className="seam-swap"
        onPointerDown={(event) => event.stopPropagation()}
        onClick={() => onSwap(id)}
        aria-label={exchange}
        title={exchange}
      >
        <SwapIcon size={13} />
      </button>
    </div>
  );
}

/** The frame every pane wears. Focus follows the pointer down rather than a
 *  click, so dragging a divider or selecting text in a pane also makes it the
 *  current one — the same moment a window manager would have taken focus.
 *
 * Memoized at the frame boundary because a ratio change only changes each
 * slot's rectangle. `setRatio` preserves leaf identity and `Workspace` keeps
 * the context stable, so re-rendering the pane subtree on every drag frame was
 * pure work: headers, composers and native-view measurement effects all ran
 * even though their inputs had not changed. */
const Pane = memo(function Pane({
  leaf,
  context,
  state,
  status,
  webBody,
  webNameOf,
  expanded,
  hidden,
}: {
  leaf: Leaf;
  context: PaneContext;
  state?: SessionState;
  status?: Status;
  webBody: RefObject<HTMLDivElement | null>;
  webNameOf?: (session: string) => string;
  /** This pane is the one filling the field. */
  expanded: boolean;
  /** Another pane fills the field and this slot is `visibility: hidden`. The
   *  browser's native webview needs it told separately (`hidden` in WebPane):
   *  CSS visibility does not reach it. */
  hidden: boolean;
}) {
  const current = context.split && leaf.id === context.focus;
  const session = paneSession(leaf.pane) ?? "";
  const cwd = context.sessions.find((open) => open.id === session)?.cwd ?? "";

  // Where a link in this pane's prose goes. Bound per pane because that is the
  // fact the router is missing: an `href` is relative to the conversation it
  // was written in, and "which conversation" is what a pane answers.
  const { onOpen, onOpenAside, onOpenUrl } = context;
  const follow = useCallback<Follow>(
    (href, aside) => {
      const target = linkTarget(href, cwd);
      if (target.kind === "web") return onOpenUrl(target.url);
      if (target.kind === "none") return;
      // The same viewer `show` opens into, reached the same way anything else
      // in the transcript is: one `open`, and the pane decides how to draw it.
      const value: Inspect = {
        kind: "shown",
        path: target.path,
        label: basename(target.path),
      };
      (aside ? onOpenAside : onOpen)(leaf.id, session, value);
    },
    [onOpen, onOpenAside, onOpenUrl, cwd, leaf.id, session],
  );

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
          every layer of the transcript.

          The browser publishes nothing, because it is the window's and not a
          conversation's: anything under it that asked "which session am I in"
          would be asking a question with no answer, and an empty string is a
          worse answer than none. */}
      {leaf.pane.kind === "web" ? (
        <WebPane
          bodyRef={webBody}
          onClose={() => context.onClosePane(leaf.id)}
          expanded={expanded}
          onToggleExpanded={() => context.onToggleExpanded(leaf.id)}
          hidden={hidden}
          request={context.webRequest}
          // A lookup, not an identity. The note above still holds: this pane
          // belongs to no conversation. It is handed a way to put a name to an
          // id a tab already carries, which is the same thing the finder does
          // for a row it did not open either.
          nameOf={webNameOf!}
          onHandOver={context.onHandOverTab}
        />
      ) : leaf.pane.kind === "terminal" ? (
        <TermPane
          cwd={context.terminalCwd}
          onClose={() => context.onClosePane(leaf.id)}
          expanded={expanded}
          onToggleExpanded={() => context.onToggleExpanded(leaf.id)}
          focused={leaf.id === context.focus}
        />
      ) : (
        <SessionContext.Provider value={leaf.pane.session}>
          <LinkContext.Provider value={follow}>
            {leaf.pane.kind === "session" ? (
              <SessionPane
                leaf={leaf}
                session={leaf.pane.session}
                state={state ?? BLANK}
                status={status ?? "idle"}
                context={context}
              />
            ) : (
              <InspectPane
                leaf={leaf}
                state={state ?? BLANK}
                context={context}
              />
            )}
          </LinkContext.Provider>
        </SessionContext.Provider>
      )}
    </section>
  );
});

/**
 * Give this pane the whole field, and give it back.
 *
 * Absent with one pane on screen, which is most of the time: expanding is
 * relative to the neighbours it covers, so with no neighbours the control does
 * nothing you can see and is just a button whose job you have to guess at.
 * `split` goes false while a pane is expanded — that is what expanding means —
 * so the second half of the test is what keeps the way back visible.
 */
function ExpandPane({ leaf, context }: { leaf: Leaf; context: PaneContext }) {
  const expanded = context.expanded === leaf.id;
  if (!context.split && !expanded) return null;
  return (
    <button
      className="icon-btn"
      onClick={() => context.onToggleExpanded(leaf.id)}
      aria-pressed={expanded}
      aria-label={expanded ? "Restore this pane's size" : "Expand this pane"}
      title={expanded ? "Restore this pane's size" : "Expand this pane"}
    >
      {expanded ? <CollapseIcon size={14} /> : <ExpandIcon size={14} />}
    </button>
  );
}

function SessionPane({
  leaf,
  session,
  state,
  status,
  context,
}: {
  leaf: Leaf;
  session: string;
  state: SessionState;
  status: Status;
  context: PaneContext;
}) {
  const info = context.sessions.find((open) => open.id === session);
  // A draft is built once per plan, not per render: `draftOf` mints the row ids
  // the editor keys on, so rebuilding it while someone is typing would remount
  // every field and take the caret with it.
  const draft = useMemo(
    () => state.planDraft ?? (state.plan ? draftOf(state.plan) : null),
    [state.planDraft, state.plan],
  );

  // The transcript is the expensive thing in this window and it is memoized, so
  // these two are the props that decide whether it gets to skip a render. Bound
  // once per pane rather than in the JSX, where they would be new functions
  // every time anything in the window changed.
  const { onOpen, onAskRewind } = context;
  const openHere = useCallback(
    (value: Inspect) => onOpen(leaf.id, session, value),
    [onOpen, leaf.id, session],
  );
  const askRewind = useCallback(
    (target: RewindTarget) => onAskRewind(session, target),
    [onAskRewind, session],
  );

  if (!info) return <p className="pane-empty">this conversation was closed</p>;

  return (
    <>
      <header className="pane-head">
        <StatusDot status={status} />
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
          <FileIcon size={14} />
        </button>
        <button
          className="icon-btn"
          onClick={() => context.onToggleWorkspace(leaf.id, session)}
          aria-label={`Browse ${info.name} workspace`}
          title={`Browse ${info.name} workspace`}
        >
          <FolderIcon size={14} />
        </button>
        {/* The browser and the terminals used to be two more buttons here, on
            the argument that this row was where the window's other surfaces
            were reached from and the title bar had nothing discoverable in it.
            The title bar's left now carries the rail's toggle and the finder —
            the window's surfaces, grouped — so the argument reversed and they
            went there. Neither belonged to a conversation anyway: there is one
            browser and one terminal dock per window, and a split view drew two
            buttons for each, both toggling the same single thing. */}
        <ExpandPane leaf={leaf} context={context} />
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
          rewindTargets={state.rewindTargets}
          onOpen={openHere}
          onRewind={askRewind}
          onRevealBrowserTab={context.onRevealBrowserTab}
        />

        {/* Docked, not modal: every other pane stays readable while this one
            waits. See `Approval.tsx`. */}
        {state.approval && (
          <Approval
            request={state.approval}
            plan={state.plan}
            draft={draft}
            onAnswer={(decision, comment, setMode) =>
              context.onAnswer(session, decision, comment, setMode)
            }
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
        {/* Above the composer and below the conversation, in the order the
            things there happen: what is being asked, then what is waiting to be
            said, then where the plan stands, then the field. The rewind question
            joins that stack rather than covering it — the messages it is about
            are directly above, and a modal would hide them at the moment they
            matter (AGENTS.md rule 9b). */}
        {state.rewindAsk && (
          <RewindBar
            preview={state.rewindAsk}
            busy={state.rewinding}
            onConfirm={(restoreFiles) =>
              context.onRewind(session, restoreFiles)
            }
            onCancel={() => context.onAskRewind(session, null)}
          />
        )}

        <QueueStrip
          queued={state.queued}
          onWithdraw={(index, text) =>
            context.onWithdrawQueued(session, index, text)
          }
          onSendNow={(turn) => context.onSendQueuedNow(session, turn)}
        />

        {state.running && <TurnStatus phase={state.activity} />}

        {state.plan &&
          !(state.approval && isPlanReview(state.approval.input)) && (
            <ProgressStrip
              plan={state.plan}
              expanded={state.planOpen}
              onToggle={() => context.onPlanOpen(session, !state.planOpen)}
              onOpen={() => context.onOpen(leaf.id, session, { kind: "plan" })}
            />
          )}

        <Composer
          value={state.draft}
          running={state.running}
          disabled={false}
          current={leaf.id === context.focus}
          attachments={state.attachments}
          meter={state.meter}
          planFirst={state.planFirst}
          onPlanFirst={(on) => context.onPlanFirst(session, on)}
          onChange={(value) => context.onDraft(session, value)}
          onAttach={(items) => context.onAttach(session, items)}
          onDetach={(id) => context.onDetach(session, id)}
          onSubmit={(text) => context.onSend(session, text)}
          onInterrupt={() => context.onInterrupt(session)}
        />
      </div>
    </>
  );
}

/**
 * The turn's live state belongs with the next prompt, not in the record above
 * it. The transcript is scrollable history; this stays adjacent to the composer
 * so its answer survives while someone reads earlier messages.
 */
function TurnStatus({ phase }: { phase: string }) {
  return (
    <p className="working" aria-live="polite">
      <span className="working-spinner" aria-hidden>
        ✦
      </span>
      <span className="working-phase">{statusLabel(phase)}</span>
    </p>
  );
}

function InspectPane({
  leaf,
  state,
  context,
}: {
  leaf: Leaf;
  state: SessionState;
  context: PaneContext;
}) {
  const pane = leaf.pane.kind === "inspect" ? leaf.pane : null;
  const [fileControls, setFileControls] = useState<WorkspaceFileControls | null>(null);
  const registerFileControls = useCallback((next: WorkspaceFileControls) => {
    setFileControls(next);
    return () => setFileControls((current) => (current === next ? null : current));
  }, []);
  // Same reason as the session pane: one draft per plan, not one per render.
  const draft = useMemo(
    () => state.planDraft ?? (state.plan ? draftOf(state.plan) : null),
    [state.planDraft, state.plan],
  );

  // Bound once, for the same reason the transcript's are: what is inside can be
  // a whole file run through a grammar, and it is memoized so that typing in
  // the pane beside it does not tokenise the file again.
  const held = pane?.session ?? "";
  const { onOpen, onOpenAside, onMention, onPlanDraft, onSavePlan } = context;
  const openHere = useCallback(
    (next: Inspect) => onOpen(leaf.id, held, next),
    [onOpen, leaf.id, held],
  );
  const openAside = useCallback(
    (next: Inspect) => onOpenAside(leaf.id, held, next),
    [onOpenAside, leaf.id, held],
  );
  const mention = useCallback(
    (path: string) => onMention(held, path),
    [onMention, held],
  );
  const changeDraft = useCallback(
    (next: PlanDraft) => onPlanDraft(held, next),
    [onPlanDraft, held],
  );
  const save = useCallback(() => onSavePlan(held), [onSavePlan, held]);

  if (!pane) return null;
  const { session, nav } = pane;
  const value = navValue(nav);
  const activeFileControls =
    value.kind === "workspace-file" &&
    matchesWorkspaceFileControls(fileControls, session, value.path)
      ? fileControls
      : null;
  const info = context.sessions.find((open) => open.id === session);

  useEffect(() => {
    if (!activeFileControls || leaf.id !== context.focus) return;
    const saveFile = (event: KeyboardEvent) => {
      if (
        !(event.ctrlKey || event.metaKey) ||
        event.altKey ||
        event.shiftKey ||
        event.key.toLocaleLowerCase() !== "s"
      ) {
        return;
      }
      event.preventDefault();
      if (
        activeFileControls.onSave &&
        !activeFileControls.saveDisabled &&
        !activeFileControls.saving
      ) {
        activeFileControls.onSave();
      }
    };
    window.addEventListener("keydown", saveFile);
    return () => window.removeEventListener("keydown", saveFile);
  }, [activeFileControls, context.focus, leaf.id]);

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
        <span
          className="pane-name"
          title={value.kind === "workspace-file" ? value.path : undefined}
        >
          {activeFileControls?.dirty && (
            <span
              className="workspace-file-dirty"
              role="img"
              aria-label="Unsaved changes"
              title={`Unsaved changes — ${MOD}+S saves`}
            />
          )}
          {inspectTitle(value)}
        </span>
        {activeFileControls?.onMode && activeFileControls.mode && (
          <button
            type="button"
            className={`chip is-toggle workspace-file-mode${
              activeFileControls.mode === "edit" ? " is-on" : ""
            }`}
            onClick={() =>
              activeFileControls.onMode?.(
                activeFileControls.mode === "edit" ? "preview" : "edit",
              )
            }
            disabled={activeFileControls.loading || activeFileControls.saving}
            aria-pressed={activeFileControls.mode === "edit"}
            aria-label={
              activeFileControls.mode === "edit" ? "Preview Markdown" : "Edit Markdown"
            }
            title={
              activeFileControls.mode === "edit" ? "Preview Markdown" : "Edit Markdown"
            }
          >
            <PencilIcon size={12} />
          </button>
        )}
        {activeFileControls && (
          <button
            type="button"
            className="icon-btn"
            onClick={activeFileControls.onReload}
            disabled={activeFileControls.loading || activeFileControls.saving}
            aria-label="Read this file again"
            title="Read this file again"
          >
            <RefreshIcon size={14} />
          </button>
        )}
        <ExpandPane leaf={leaf} context={context} />
        <button
          className="icon-btn"
          onClick={() => context.onClosePane(leaf.id)}
          aria-label="Close this pane"
        >
          <CloseIcon size={14} />
        </button>
      </header>

      <div
        className={`pane-body is-inspect${
          value.kind === "workspace-file" ? " is-workspace-file" : ""
        }`}
      >
        <WorkspaceFileControlsContext.Provider value={registerFileControls}>
          <InspectView
            value={value}
            blocks={state.blocks}
            files={state.files}
            cwd={info?.cwd ?? ""}
            plan={state.plan}
            planDraft={draft}
            onOpen={openHere}
            onOpenAside={openAside}
            onMention={mention}
            onPlanDraft={changeDraft}
            onSavePlan={save}
          />
        </WorkspaceFileControlsContext.Provider>
      </div>
    </>
  );
}
