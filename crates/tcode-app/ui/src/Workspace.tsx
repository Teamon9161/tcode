import { useCallback, useEffect, useMemo, useState } from "react";

import type { ApprovalMode, Decision, SessionInfo, Status } from "./types";
import type { SessionState } from "./session";
import type { PlanDraft } from "./plan";
import type { PlanDecision } from "./PlanEditor";
import {
  browserPane,
  close,
  findLeaf,
  focusPane,
  focused,
  navigate,
  openAside,
  openInspect,
  openWeb,
  paneSession,
  panes,
  parentSplit,
  rotate,
  sessionsInView,
  setRatio,
  show,
  showBeside,
  type Tiling,
} from "./layout";
import { nearestPane, type Box, type Dir4 } from "./focus";
import { fieldAspect } from "./field";
import { statusLabel } from "./activity";
import { StatusDot } from "./components/Status";
import { BackIcon, ChevronDown, ChevronRight, CloseIcon, PlusIcon, SidebarIcon } from "./components/Icons";
import { Panes, type PaneContext } from "./Panes";
import { DisplayMenu } from "./DisplayMenu";
import type { Display } from "./display";
import { Wordmark } from "./components/Mark";
import { WindowControls, WindowDragRegion } from "./components/WindowControls";
import {
  loadOrder,
  moveProject,
  railGroups,
  saveOrder,
  sessionTitle,
  type RailGroup,
} from "./rail";

/**
 * The window: a rail of conversations on the left, a tiled field of panes
 * beside it, and a title bar over both.
 *
 * The rail lists every conversation the process is holding, on screen or not —
 * the panes show a few, the rail accounts for all of them. It is shown by
 * default rather than being a drawer: knowing that another session needs you is
 * the reason this app exists, and information you have to open a menu to see is
 * information you will miss. It can still be folded away from the title bar,
 * for the stretch where one conversation is the whole job — the toggle lives
 * there because the rail is the window's, not any pane's, which is the same rule
 * that keeps session actions out of the bar (AGENTS.md rule 9c).
 *
 * It groups by folder (`rail.ts`), and that is not a nicety: a conversation is
 * named after its folder, so two in one folder were two identical rows and the
 * list could account for both without saying which was which. The folder is a
 * heading and each conversation is named by what it was asked to do — the two
 * facts a reader needs are different facts, so they are different elements.
 *
 * Each folder heading also starts a new conversation in that folder. The
 * folder chip in each pane remains the path for choosing a different folder,
 * while the rail keeps the same-folder action next to the group it names.
 *
 * The app-owned caption uses the same theme token as the rest of the window.
 * The browser child webview is confined to pane bodies below this bar, so it
 * cannot intercept the window controls. This toolbar deliberately carries only
 * app-wide navigation and display controls; with the window split, no single
 * conversation is "the" one to name here.
 */
export function Workspace({
  tiling,
  sessions,
  stateOf,
  statusOf,
  onTiling,
  onDraft,
  onAttach,
  onDetach,
  onSend,
  onInterrupt,
  onWithdrawQueued,
  onSendQueuedNow,
  onResume,
  onDismissResume,
  onAskRewind,
  onRewind,
  onAnswer,
  onDecidePlan,
  onPlanDraft,
  onSavePlan,
  onPlanOpen,
  onPlanFirst,
  onCloseSession,
  onHome,
  onOpenFolder,
  display,
  onDisplay,
}: {
  tiling: Tiling;
  sessions: SessionInfo[];
  stateOf: (session: string) => SessionState;
  statusOf: (session: string) => Status;
  onTiling: (step: (current: Tiling) => Tiling) => void;
  onDraft: (session: string, value: string) => void;
  onAttach: (session: string, items: SessionState["attachments"]) => void;
  onDetach: (session: string, id: string) => void;
  onSend: PaneContext["onSend"];
  onInterrupt: (session: string) => void;
  onWithdrawQueued: PaneContext["onWithdrawQueued"];
  onSendQueuedNow: PaneContext["onSendQueuedNow"];
  onResume: PaneContext["onResume"];
  onDismissResume: PaneContext["onDismissResume"];
  onAskRewind: PaneContext["onAskRewind"];
  onRewind: PaneContext["onRewind"];
  onAnswer: (session: string, decision: Decision, comment: string, setMode?: ApprovalMode) => void;
  onDecidePlan: (session: string, choice: PlanDecision) => void;
  onPlanDraft: (session: string, draft: PlanDraft) => void;
  onSavePlan: (session: string) => void;
  onPlanOpen: (session: string, open: boolean) => void;
  onPlanFirst: (session: string, on: boolean) => void;
  onCloseSession: (session: string) => void;
  onHome: () => void;
  onOpenFolder: (path: string) => Promise<void>;
  display: Display;
  onDisplay: (next: Display) => void;
}) {
  const narrow = useNarrow();
  const [rail, setRail] = useState(true);
  // The arrangement of folders persists; which ones are folded does not. An
  // order is something you set once and then rely on, while a fold is a thing
  // you do to get a long list out of the way for the next few minutes — coming
  // back to a rail with half of it collapsed by a decision you made last week is
  // the opposite of accounting for every conversation.
  const [order, setOrder] = useState<string[]>(loadOrder);
  const [folded, setFolded] = useState<Set<string>>(() => new Set());
  // A temporary viewing mode, not a layout operation: keeping it here preserves
  // the full tiling tree and every pane's local state for the return trip.
  const [expanded, setExpanded] = useState<string | null>(null);
  const leaves = panes(tiling);
  const here = focused(tiling);
  const onScreen = useMemo(() => new Set(sessionsInView(tiling)), [tiling]);
  const groups = useMemo(() => railGroups(sessions, order), [sessions, order]);

  useEffect(() => {
    if (expanded && !findLeaf(tiling, expanded)) setExpanded(null);
  }, [expanded, tiling]);

  const move = useCallback(
    (path: string, to: number) => {
      const next = moveProject(groups, path, to);
      setOrder(next);
      saveOrder(next);
    },
    [groups],
  );

  // The files index is the one inspect view not reached by pointing at
  // something in the transcript, so it keeps a button — on the conversation's
  // own header, because that is whose files it shows. A single window-level
  // toggle had to guess which conversation was meant, and with two on screen it
  // would have guessed wrong half the time.
  //
  // It toggles: a second press closes the pane it opened rather than stacking
  // another history entry nobody asked for.
  const toggleFiles = useCallback(
    (pane: string, session: string) => {
      onTiling((current) => {
        const sibling = panes(current).find(
          (leaf) => leaf.pane.kind === "inspect" && leaf.pane.session === session,
        );
        return sibling
          ? close(current, sibling.id)
          : openInspect(current, pane, session, { kind: "files" }, fieldAspect());
      });
    },
    [onTiling],
  );

  // Workspace browsing is an inspect value too, so turning it on follows normal
  // inspect history. Turning it off closes the pane that is browsing — a tree is
  // not something you navigate away from, it is the place you navigate *from*
  // (`browsing` in `layout.ts`).
  const toggleWorkspace = useCallback(
    (pane: string, session: string) => {
      onTiling((current) => {
        // Specifically the pane already browsing, not simply this session's
        // first inspect pane: once a file is open beside the tree there are two
        // of them, and closing the wrong one would shut the file while leaving
        // the tree the button was meant to hide.
        const browser = browserPane(current, session);
        return browser
          ? close(current, browser.id)
          : openInspect(current, pane, session, { kind: "workspace-tree" }, fieldAspect());
      });
    },
    [onTiling],
  );

  // Naming a file to the agent, from the list of files. It writes into the
  // composer instead of sending, because `@thing` is the start of a sentence
  // somebody is still writing — and it appends rather than replaces, so
  // mentioning three files in one message is three clicks.
  const mention = useCallback(
    (session: string, path: string) => {
      const quoted = /\s/.test(path) ? `@"${path}"` : `@${path}`;
      const draft = stateOf(session).draft;
      onDraft(session, draft.trim() ? `${draft.replace(/\s+$/, "")} ${quoted} ` : `${quoted} `);
    },
    [stateOf, onDraft],
  );

  // Esc closes the pane you are looking into, before it reaches the composer's
  // interrupt. A conversation pane ignores it: Esc must never be the key that
  // removes the thing you are typing into.
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      // Escape belongs to the innermost thing that can take it, and this — the
      // pane — is the outermost. Both of the things ahead of it listen on
      // `window` in the capture phase too, where `stopPropagation` cannot
      // separate them: the handler that runs first is whichever mounted first,
      // which is this one. So the precedence is stated here rather than left as
      // an accident of mounting order.
      //
      // A popover goes first (`seat.ts` closes it on this key). Then anything
      // being typed into: a rename in the file tree, an unsaved edit in the
      // editor. Escape must never be the key that throws away what you were
      // writing — that is the same rule the composer's own Escape follows.
      if (document.querySelector(".seated")) return;
      if (
        event.target instanceof HTMLInputElement ||
        event.target instanceof HTMLTextAreaElement
      ) {
        return;
      }
      if (expanded) {
        event.stopPropagation();
        setExpanded(null);
        return;
      }
      const seat = focused(tiling);
      if (!seat || seat.pane.kind !== "inspect") return;
      event.stopPropagation();
      onTiling((current) => close(current, seat.id));
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [tiling, onTiling, expanded]);

  /**
   * Driving the layout from the keyboard.
   *
   * Every binding carries a modifier, and that is not a style choice: the hand
   * that works this app is in a composer nearly all the time, so a bare key is
   * text. `Mod` is Ctrl, or Cmd on a Mac.
   *
   *   Mod+N            start a fresh conversation in this pane's folder
   *   Mod+1…9          show that conversation here
   *   Mod+Shift+1…9    open it beside this one
   *   Mod+Alt+←↑↓→     move focus to the pane that way
   *   Mod+W            close this pane (the conversation keeps running)
   *   Mod+Alt+R        turn this split from side-by-side to stacked
   *
   * Digits are read from `event.code`, not `event.key`: with Shift down the
   * key is "!" on a US layout and something else again elsewhere, so the digit
   * is only reliably in the physical code.
   */
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (!(event.ctrlKey || event.metaKey)) return;

      // A new conversation in *this pane's* folder, which is the only folder the
      // keyboard can name without guessing: with the window split there are two
      // on screen, so "the current folder" is not a question the window answers
      // (AGENTS.md rule 9c) — the focused pane is what makes it answerable.
      // Same destination as the folder chip's first item, reached without the
      // menu.
      if (!event.altKey && !event.shiftKey && (event.key === "n" || event.key === "N")) {
        const seat = focused(tiling);
        // The browser has no folder, so with focus there this key does nothing
        // rather than guessing at one of the folders on screen.
        const held = seat && paneSession(seat.pane);
        const cwd = sessions.find((open) => open.id === held)?.cwd;
        if (!cwd) return;
        event.preventDefault();
        onOpenFolder(cwd).catch(() => {});
        return;
      }

      const digit = /^Digit([1-9])$/.exec(event.code);
      if (digit && !event.altKey) {
        const pick = sessions[Number(digit[1]) - 1];
        if (!pick) return;
        event.preventDefault();
        onTiling((current) =>
          event.shiftKey
            ? showBeside(current, pick.id, fieldAspect())
            : show(current, pick.id, fieldAspect()),
        );
        return;
      }

      if (event.altKey && ARROWS[event.key]) {
        event.preventDefault();
        const boxes = new Map<string, Box>();
        for (const node of document.querySelectorAll<HTMLElement>("[data-pane]")) {
          const id = node.dataset.pane;
          if (id) boxes.set(id, node.getBoundingClientRect());
        }
        const next = nearestPane(boxes, tiling.focus, ARROWS[event.key]);
        if (next) onTiling((current) => focusPane(current, next));
        return;
      }

      if (event.altKey && (event.key === "r" || event.key === "R")) {
        event.preventDefault();
        onTiling((current) => {
          const divider = parentSplit(current, current.focus);
          return divider ? rotate(current, divider) : current;
        });
        return;
      }

      if (!event.altKey && !event.shiftKey && (event.key === "w" || event.key === "W")) {
        event.preventDefault();
        onTiling((current) => close(current, current.focus));
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [sessions, tiling, onTiling, onOpenFolder]);

  // Each of these is written once rather than rebuilt in the object literal
  // below, and that is not tidiness: a pane compares the handlers it was given
  // to decide whether it has to redraw, and a fresh arrow every render answers
  // "yes" every time — which is how a keystroke in one conversation came to
  // re-render the transcript of another.
  const focusOn = useCallback(
    (pane: string) => onTiling((current) => focusPane(current, pane)),
    [onTiling],
  );
  const closePane = useCallback(
    (pane: string) => onTiling((current) => close(current, pane)),
    [onTiling],
  );
  const ratio = useCallback(
    (divider: string, value: number) => onTiling((current) => setRatio(current, divider, value)),
    [onTiling],
  );
  const open = useCallback(
    (pane: string, session: string, value: Parameters<PaneContext["onOpen"]>[2]) =>
      // Measured at the click rather than closed over: these handlers are bound
      // once for the memoized panes' sake (rule 21), and the window's shape
      // changes under them.
      onTiling((current) => openInspect(current, pane, session, value, fieldAspect())),
    [onTiling],
  );
  const openBeside = useCallback(
    (pane: string, session: string, value: Parameters<PaneContext["onOpen"]>[2]) =>
      onTiling((current) => openAside(current, pane, session, value, fieldAspect())),
    [onTiling],
  );
  const goTo = useCallback(
    (pane: string, step: Parameters<PaneContext["onNavigate"]>[1]) =>
      onTiling((current) => navigate(current, pane, step)),
    [onTiling],
  );
  const openBrowser = useCallback(
    () => onTiling((current) => openWeb(current, fieldAspect())),
    [onTiling],
  );
  // The one control that *asks* for a direction, as opposed to `dirFor`
  // guessing one when a pane opens. It acts on the seam rather than on either
  // pane, which is why the handle rides on the divider and this takes a divider
  // id — `Mod+Alt+R` is the same operation reached from whichever pane has
  // focus, and it is the one that has to look the seam up.
  const turnSeam = useCallback(
    (seam: string) => onTiling((current) => rotate(current, seam)),
    [onTiling],
  );
  const toggleExpanded = useCallback(
    (pane: string) => {
      onTiling((current) => focusPane(current, pane));
      setExpanded((current) => (current === pane ? null : pane));
    },
    [onTiling],
  );

  const split = leaves.length > 1 && !narrow && !expanded;
  const context: PaneContext = useMemo(
    () => ({
      sessions,
      stateOf,
      statusOf,
      focus: tiling.focus,
      split,
      onFocus: focusOn,
      onClosePane: closePane,
      onRotate: turnSeam,
      onRatio: ratio,
      onOpen: open,
      onOpenAside: openBeside,
      onMention: mention,
      onNavigate: goTo,
      onToggleFiles: toggleFiles,
      onToggleWorkspace: toggleWorkspace,
      onOpenBrowser: openBrowser,
      expanded,
      onToggleExpanded: toggleExpanded,
      onDraft,
      onAttach,
      onDetach,
      onSend,
      onInterrupt,
      onWithdrawQueued,
      onSendQueuedNow,
      onResume,
      onDismissResume,
      onAskRewind,
      onRewind,
      onAnswer,
      onDecidePlan,
      onPlanDraft,
      onSavePlan,
      onPlanOpen,
      onPlanFirst,
      onOpenFolder,
    }),
    [
      sessions,
      stateOf,
      statusOf,
      tiling.focus,
      split,
      focusOn,
      closePane,
      turnSeam,
      ratio,
      open,
      openBeside,
      mention,
      goTo,
      toggleFiles,
      toggleWorkspace,
      openBrowser,
      expanded,
      toggleExpanded,
      onDraft,
      onAttach,
      onDetach,
      onSend,
      onInterrupt,
      onWithdrawQueued,
      onSendQueuedNow,
      onResume,
      onDismissResume,
      onAskRewind,
      onRewind,
      onAnswer,
      onDecidePlan,
      onPlanDraft,
      onSavePlan,
      onPlanOpen,
      onPlanFirst,
      onOpenFolder,
    ],
  );

  // Below the threshold the tree stops being shown and only the current pane
  // is: two panes in 700px are two unreadable panes. Structural, not fluid —
  // nothing shrinks, one thing is chosen (PRODUCT.md § Design Principles).
  const shown = narrow && here ? { root: here, focus: here.id } : tiling;

  return (
    <div className={`workspace${rail ? "" : " is-folded"}`}>
      <header className="topbar">
        <Wordmark />
        <button className="icon-btn" onClick={onHome} aria-label="Back to all projects">
          <BackIcon size={15} />
        </button>
        {/* No "on" styling: the rail being open is already answered by the rail
            being there, and spending the brand colour on a control whose state
            is the largest object on screen is exactly what the palette rule
            exists to prevent. */}
        <button
          className="icon-btn"
          onClick={() => setRail((shown) => !shown)}
          aria-pressed={rail}
          aria-label={rail ? "Hide the conversation list" : "Show the conversation list"}
          title={rail ? "Hide the conversation list" : "Show the conversation list"}
        >
          <SidebarIcon size={15} />
        </button>
        {/* What the window draws, not what any conversation holds — which is why
            it passes the bar's own test (rule 9c) where a session action would
            not. With the window split, "show reasoning" cannot mean one thing in
            the left pane and another in the right. */}
        <WindowDragRegion />
        <DisplayMenu display={display} onChange={onDisplay} />
        <WindowControls />
      </header>

      {/* Unmounted rather than hidden when folded: it holds no state of its own,
          and a `display: none` rail is still a grid column somebody has to
          remember to zero. */}
      {rail && (
        <nav className="rail">
          {/* No home button here: the title bar's back arrow already goes there,
              and the mark that used to sit in this corner was a second status
              light for the session the rail is already showing. */}
          <ul className="rail-list">
            {groups.map((group, at) => (
              <RailProject
                key={group.path}
                group={group}
                at={at}
                total={groups.length}
                folded={folded.has(group.path)}
                onScreen={onScreen}
                stateOf={stateOf}
                statusOf={statusOf}
                onFold={() =>
                  setFolded((was) => {
                    const next = new Set(was);
                    if (!next.delete(group.path)) next.add(group.path);
                    return next;
                  })
                }
                onMove={(to) => move(group.path, to)}
                onShow={(id) => onTiling((current) => show(current, id, fieldAspect()))}
                onCloseSession={onCloseSession}
                onOpenFolder={onOpenFolder}
              />
            ))}
          </ul>
        </nav>
      )}

      <Panes tiling={shown} context={context} />
    </div>
  );
}

/**
 * One folder and the conversations in it.
 *
 * The heading is the folder; the rows are conversations, each named by the first
 * thing it was asked to do. The count stays visible while the group is folded,
 * which is what keeps folding from hiding the fact the rail exists to publish —
 * and a folded group whose conversation needs an answer still shows it, because
 * that is the one thing that must never be foldable away.
 *
 * Reordering is Alt+arrow and two buttons that appear on hover, which is the
 * vocabulary the plan editor already uses for the same act. Drag was the obvious
 * alternative and buys nothing here: five rows, and a keyboard path for free.
 */
function RailProject({
  group,
  at,
  total,
  folded,
  onScreen,
  stateOf,
  statusOf,
  onFold,
  onMove,
  onShow,
  onCloseSession,
  onOpenFolder,
}: {
  group: RailGroup;
  at: number;
  total: number;
  folded: boolean;
  onScreen: Set<string>;
  stateOf: (session: string) => SessionState;
  statusOf: (session: string) => Status;
  onFold: () => void;
  onMove: (to: number) => void;
  onShow: (session: string) => void;
  onCloseSession: (session: string) => void;
  onOpenFolder: (path: string) => Promise<void>;
}) {
  const [failure, setFailure] = useState<string | null>(null);
  const needs = group.sessions.filter((entry) => statusOf(entry.id) === "waiting").length;
  const running = group.sessions.some((entry) => statusOf(entry.id) === "running");

  return (
    <li
      className={`rail-group${folded ? " is-folded" : ""}`}
      onKeyDown={(event) => {
        if (!event.altKey || (event.key !== "ArrowUp" && event.key !== "ArrowDown")) return;
        event.preventDefault();
        event.stopPropagation();
        onMove(event.key === "ArrowUp" ? at - 1 : at + 1);
      }}
    >
      <div className="rail-project">
        <button
          type="button"
          className="rail-project-head"
          onClick={onFold}
          aria-expanded={!folded}
          title={group.path}
        >
          {folded ? <ChevronRight size={12} /> : <ChevronDown size={12} />}
          <span className="rail-project-name">{group.name}</span>
          {/* Folded, the group has to keep answering the rail's whole question.
              A conversation waiting on a human is said in words rather than a
              count, because it is the one state worth interrupting for. */}
          {needs > 0 ? (
            <span className="rail-needs">{needs === 1 ? "needs you" : `${needs} need you`}</span>
          ) : (
            folded && (
              <span className="rail-count">
                {running && <span className="rail-live" aria-label="running" />}
                {group.sessions.length}
              </span>
            )
          )}
        </button>
        <span className="rail-project-tools">
          <button
            type="button"
            className="icon-btn"
            title={`New conversation in ${group.name}`}
            aria-label={`New conversation in ${group.name}`}
            onClick={() =>
              onOpenFolder(group.path).catch((error) => setFailure(String(error)))
            }
          >
            <PlusIcon size={14} />
          </button>
          <button
            type="button"
            className="icon-btn"
            title="Move up (Alt+↑)"
            aria-label={`Move ${group.name} up`}
            disabled={at === 0}
            onClick={() => onMove(at - 1)}
          >
            ↑
          </button>
          <button
            type="button"
            className="icon-btn"
            title="Move down (Alt+↓)"
            aria-label={`Move ${group.name} down`}
            disabled={at === total - 1}
            onClick={() => onMove(at + 1)}
          >
            ↓
          </button>
        </span>
      </div>

      {failure && (
        <p className="rail-new-failure" role="alert">
          {failure}
        </p>
      )}

      {!folded && (
        <ul className="rail-sessions">
          {group.sessions.map((entry) => {
            const state = stateOf(entry.id);
            // The first prompt, and the activity line only until there is one:
            // "not started" is the whole truth about a conversation nobody has
            // typed into yet, and a stale first line beats no line at all.
            const title = sessionTitle(state.blocks) ?? state.activity;
            return (
              <li key={entry.id}>
                <button
                  className={`rail-item${onScreen.has(entry.id) ? " is-onscreen" : ""}`}
                  onClick={() => onShow(entry.id)}
                  title={title}
                >
                  <StatusDot status={statusOf(entry.id)} />
                  <span className="rail-lines">
                    <span className="rail-name">{title}</span>
                    <span className="rail-activity">{statusLabel(state.activity)}</span>
                  </span>
                </button>
                <button
                  className="rail-close"
                  onClick={() => onCloseSession(entry.id)}
                  aria-label={`Close this conversation in ${group.name}`}
                >
                  <CloseIcon size={13} />
                </button>
              </li>
            );
          })}
        </ul>
      )}
    </li>
  );
}

const ARROWS: Record<string, Dir4> = {
  ArrowLeft: "left",
  ArrowRight: "right",
  ArrowUp: "up",
  ArrowDown: "down",
};

/** True while the window is too narrow to tile. `matchMedia` rather than a
 *  resize listener: the browser only tells us when the answer changes. */
function useNarrow(): boolean {
  const query = "(max-width: 900px)";
  const [narrow, setNarrow] = useState(() => window.matchMedia(query).matches);
  useEffect(() => {
    const media = window.matchMedia(query);
    const update = () => setNarrow(media.matches);
    media.addEventListener("change", update);
    update();
    return () => media.removeEventListener("change", update);
  }, []);
  return narrow;
}
