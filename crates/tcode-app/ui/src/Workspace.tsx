import { useCallback, useEffect, useMemo, useRef, useState } from "react";

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
  swap,
  show,
  showBeside,
  toggleTerminal,
  webPane,
  type Tiling,
} from "./layout";
import { inTerminal, MOD } from "./keys";
import { nearestPane, type Box, type Dir4 } from "./focus";
import { fieldAspect } from "./field";
import { handOverText } from "./webHost";
import { revealBrowserTab } from "./browserReveal";
import { GlobeIcon, SidebarIcon, TerminalIcon } from "./components/Icons";
import { cwdForTerminal } from "./terminal";
import { sessionTitle, type FoundSession } from "./railData";
import { Finder } from "./Finder";
import { Panes, type PaneContext } from "./Panes";
import { SettingsPanel } from "./DisplayMenu";
import type { Display } from "./display";
import { WindowControls, WindowDragRegion } from "./components/WindowControls";
import { Rail } from "./Rail";
import { FieldEmpty } from "./FieldEmpty";
import { isTyping } from "./typing";

/**
 * The window: the rail on the left, a tiled field of panes beside it, and a
 * title bar over both. It is the only screen the app has.
 *
 * That is recent. There used to be a launchpad in front of it — a full screen
 * of open sessions and every project, which every conversation was reached
 * *through* and which the title bar kept a back arrow for. Its "Open" section
 * was the rail drawn a second time, and the two parts that were genuinely only
 * there (folders with nothing open, and each folder's earlier conversations)
 * are things the rail can hold. So they moved into it (`Rail.tsx`) and the
 * screen went, along with the back arrow and the mode it led to.
 *
 * The rail accounts for every conversation this process is holding, on screen or
 * not. It is shown by default rather than being a drawer: knowing that another
 * session needs you is the reason this app exists, and information you have to
 * open a menu to see is information you will miss. It can still be folded away
 * from the title bar, for the stretch where one conversation is the whole job —
 * the toggle lives there because the rail is the window's, not any pane's, which
 * is the same rule that keeps session actions out of the bar (AGENTS.md rule 9c).
 *
 * The app-owned caption uses the same theme token as the rest of the window.
 * The browser child webview is confined to pane bodies below this bar, so it
 * cannot intercept the window controls. This toolbar deliberately carries only
 * app-wide display controls; with the window split, no single conversation is
 * "the" one to name here.
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
  onAskRewind,
  onRewind,
  onAnswer,
  onDecidePlan,
  onPlanDraft,
  onSavePlan,
  onPlanOpen,
  onPlanFirst,
  onCloseSession,
  onOpenFolder,
  onChangeFolder,
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
  onAskRewind: PaneContext["onAskRewind"];
  onRewind: PaneContext["onRewind"];
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
  onCloseSession: (session: string) => void;
  /** `resume` replays a stored log rather than starting a fresh conversation;
   *  the rail's earlier-conversation rows are the only caller that passes it. */
  onOpenFolder: (path: string, resume?: string) => Promise<void>;
  onChangeFolder: PaneContext["onChangeFolder"];
  display: Display;
  onDisplay: (next: Display) => void;
}) {
  const narrow = useNarrow();
  const [rail, setRail] = useState(true);
  // A temporary viewing mode, not a layout operation: keeping it here preserves
  // the full tiling tree and every pane's local state for the return trip.
  const [expanded, setExpanded] = useState<string | null>(null);
  // The page the browser has been asked for, if any. Window-level like the
  // browser itself: a link is followed *into* the one browser this window has,
  // whichever conversation it was written in.
  const [webRequest, setWebRequest] = useState<{
    url: string;
    at: number;
  } | null>(null);
  const leaves = panes(tiling);
  const here = focused(tiling);
  const onScreen = useMemo(() => new Set(sessionsInView(tiling)), [tiling]);
  /** Event handlers need the newest draft without making every callback depend
   *  on the render-time selector. Rendering still uses `stateOf` directly; this
   *  ref is only read after a click, when reading current state is the point. */
  const latestStateOf = useRef(stateOf);
  latestStateOf.current = stateOf;

  useEffect(() => {
    if (expanded && !findLeaf(tiling, expanded)) setExpanded(null);
  }, [expanded, tiling]);

  // Bound once rather than written inline in the rail's props, for the reason
  // every handler here is: a fresh arrow each render is a changed prop.
  const showHere = useCallback(
    (session: string) =>
      onTiling((current) => show(current, session, fieldAspect())),
    [onTiling],
  );

  // What the finder can take you to, named by the rail's own rules so the two
  // lists cannot end up calling one conversation two different things. Built
  // here rather than inside the rail because the finder sits in the title bar,
  // which is what lets it survive the rail being folded away.
  const findable: FoundSession[] = useMemo(
    () =>
      sessions.map((session) => {
        const state = stateOf(session.id);
        return {
          kind: "session",
          session,
          title: sessionTitle(state.blocks) ?? state.activity,
          activity: state.activity,
          status: statusOf(session.id),
        };
      }),
    [sessions, stateOf, statusOf],
  );
  const home = sessions[0]?.home ?? "";

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
          (leaf) =>
            leaf.pane.kind === "inspect" && leaf.pane.session === session,
        );
        return sibling
          ? close(current, sibling.id)
          : openInspect(
              current,
              pane,
              session,
              { kind: "files" },
              fieldAspect(),
            );
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
          : openInspect(
              current,
              pane,
              session,
              { kind: "workspace-tree" },
              fieldAspect(),
            );
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
      const draft = latestStateOf.current(session).draft;
      onDraft(
        session,
        draft.trim() ? `${draft.replace(/\s+$/, "")} ${quoted} ` : `${quoted} `,
      );
    },
    [onDraft],
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
      if (isTyping(event.target)) return;
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
   *   Mod+Alt+S        exchange the two sides of this split
   *   Mod+J            show, focus, or hide the terminals
   *
   * Digits are read from `event.code`, not `event.key`: with Shift down the
   * key is "!" on a US layout and something else again elsewhere, so the digit
   * is only reliably in the physical code.
   *
   * Inside a terminal almost none of this applies, and that is the point of
   * `appOwnedInTerminal` in `keys.ts`: `Mod+W` deletes a word there, `Mod+N`
   * steps the history, and an app that takes them is an app you cannot work in.
   * What survives is the pair with no readline meaning — `Mod+J`, which is the
   * way back out, and the `Mod+Alt` moves.
   */
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (!(event.ctrlKey || event.metaKey)) return;
      const shell = inTerminal(event.target);

      // First, because it is the one binding that has to work from anywhere —
      // including from inside the terminal it hides.
      if (
        !event.altKey &&
        !event.shiftKey &&
        (event.key === "j" || event.key === "J")
      ) {
        event.preventDefault();
        onTiling(toggleTerminal);
        return;
      }

      // Everything below this line is given back to the shell, and only the
      // `Mod+Alt` pair survives — nothing in a terminal uses that modifier
      // combination, while every bare `Mod` chord below means something to
      // readline. The tab verbs are answered by the pane itself (`TermPane`),
      // which is where "which tab" is a question with an answer.
      if (shell && !event.altKey) return;

      // A new conversation in *this pane's* folder, which is the only folder the
      // keyboard can name without guessing: with the window split there are two
      // on screen, so "the current folder" is not a question the window answers
      // (AGENTS.md rule 9c) — the focused pane is what makes it answerable.
      // Same destination as the folder chip's first item, reached without the
      // menu.
      if (
        !event.altKey &&
        !event.shiftKey &&
        (event.key === "n" || event.key === "N")
      ) {
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
        for (const node of document.querySelectorAll<HTMLElement>(
          "[data-pane]",
        )) {
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

      if (event.altKey && (event.key === "s" || event.key === "S")) {
        event.preventDefault();
        onTiling((current) => {
          const divider = parentSplit(current, current.focus);
          return divider ? swap(current, divider) : current;
        });
        return;
      }

      if (
        !event.altKey &&
        !event.shiftKey &&
        (event.key === "w" || event.key === "W")
      ) {
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
    (divider: string, value: number) =>
      onTiling((current) => setRatio(current, divider, value)),
    [onTiling],
  );
  const open = useCallback(
    (
      pane: string,
      session: string,
      value: Parameters<PaneContext["onOpen"]>[2],
    ) =>
      // Measured at the click rather than closed over: these handlers are bound
      // once for the memoized panes' sake (rule 21), and the window's shape
      // changes under them.
      onTiling((current) =>
        openInspect(current, pane, session, value, fieldAspect()),
      ),
    [onTiling],
  );
  const openBeside = useCallback(
    (
      pane: string,
      session: string,
      value: Parameters<PaneContext["onOpen"]>[2],
    ) =>
      onTiling((current) =>
        openAside(current, pane, session, value, fieldAspect()),
      ),
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
  // No aspect: the terminals are a dock across the bottom whatever shape the
  // window is, which is what makes it read as an IDE's rather than as another
  // pane that landed wherever there was room (`toggleTerminal`).
  const openTerminal = useCallback(() => onTiling(toggleTerminal), [onTiling]);
  // The folder a new tab starts in. The focused conversation's folder — for the
  // reason rule 9c gives, with the window split "the current folder" is a guess
  // and the pane you are in is what makes it answerable — is remembered rather
  // than inferred once focus is in the session-less terminal.
  const lastSessionCwd = useRef<string | null>(null);
  const terminalCwd = cwdForTerminal(tiling, sessions, lastSessionCwd.current);
  const terminalSource = focused(tiling);
  if (terminalSource && paneSession(terminalSource.pane)) {
    lastSessionCwd.current = terminalCwd;
  }
  // Handing a browser tab to a conversation. Same shape as `mention` and for
  // the same reasons — it writes a draft, and it appends — but the session has
  // to be worked out rather than passed in: the browser pane belongs to the
  // window and not to any conversation, so "which one" is answered by the
  // focused pane, exactly as `terminalCwd` above answers "which folder".
  const handOverTab = useCallback(
    (tab: string) => {
      const seat = focused(tiling);
      const held = seat && paneSession(seat.pane);
      const session = sessions.find((open) => open.id === held)?.id ?? sessions[0]?.id;
      const reference = handOverText(tab);
      if (!session || !reference) return;
      const draft = latestStateOf.current(session).draft;
      onDraft(
        session,
        draft.trim() ? `${draft.replace(/\s+$/, "")} ${reference} ` : `${reference} `,
      );
    },
    [tiling, sessions, onDraft],
  );
  // Following a link, as opposed to reaching for the browser. It must not go
  // through `openWeb`: that one is a toggle, so asking it for a page while the
  // browser is already open would put the browser away instead.
  const openUrl = useCallback(
    (url: string) => {
      onTiling((current) => {
        const already = webPane(current);
        return already
          ? focusPane(current, already.id)
          : openWeb(current, fieldAspect());
      });
      // A serial rather than the address alone: clicking the same link twice is
      // two requests, and the second one has to reach a pane that has already
      // seen the first.
      setWebRequest((last) => ({ url, at: (last?.at ?? 0) + 1 }));
    },
    [onTiling],
  );
  const revealBrowser = useCallback(
    (tab: string) => {
      revealBrowserTab(tab, onTiling, () => setExpanded(null));
    },
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
  const swapSeam = useCallback(
    (seam: string) => onTiling((current) => swap(current, seam)),
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
      focus: tiling.focus,
      split,
      onFocus: focusOn,
      onClosePane: closePane,
      onRotate: turnSeam,
      onSwap: swapSeam,
      onRatio: ratio,
      onOpen: open,
      onOpenAside: openBeside,
      onMention: mention,
      onNavigate: goTo,
      onToggleFiles: toggleFiles,
      onToggleWorkspace: toggleWorkspace,
      onOpenBrowser: openBrowser,
      onHandOverTab: handOverTab,
      onRevealBrowserTab: revealBrowser,
      onToggleTerminal: openTerminal,
      terminalCwd,
      onOpenUrl: openUrl,
      webRequest,
      expanded,
      onToggleExpanded: toggleExpanded,
      onDraft,
      onAttach,
      onDetach,
      onSend,
      onInterrupt,
      onWithdrawQueued,
      onSendQueuedNow,
      onAskRewind,
      onRewind,
      onAnswer,
      onDecidePlan,
      onPlanDraft,
      onSavePlan,
      onPlanOpen,
      onPlanFirst,
      onOpenFolder,
      onChangeFolder,
    }),
    [
      sessions,
      tiling.focus,
      split,
      focusOn,
      closePane,
      turnSeam,
      swapSeam,
      ratio,
      open,
      openBeside,
      mention,
      goTo,
      toggleFiles,
      toggleWorkspace,
      openBrowser,
      handOverTab,
      revealBrowser,
      openTerminal,
      terminalCwd,
      openUrl,
      webRequest,
      expanded,
      toggleExpanded,
      onDraft,
      onAttach,
      onDetach,
      onSend,
      onInterrupt,
      onWithdrawQueued,
      onSendQueuedNow,
      onAskRewind,
      onRewind,
      onAnswer,
      onDecidePlan,
      onPlanDraft,
      onSavePlan,
      onPlanOpen,
      onPlanFirst,
      onOpenFolder,
      onChangeFolder,
    ],
  );

  // Below the threshold the tree stops being shown and only the current pane
  // is: two panes in 700px are two unreadable panes. Structural, not fluid —
  // nothing shrinks, one thing is chosen (PRODUCT.md § Design Principles).
  const shown = narrow && here ? { root: here, focus: here.id } : tiling;

  return (
    <div className={`workspace${rail ? "" : " is-folded"}`}>
      {/* No wordmark and no back arrow. The arrow went with the launchpad —
          there is nowhere behind this any more — and the wordmark went because
          it was a whole row saying the name of the application you are looking
          at, over a rail whose first row already says which folder you are in.
          What is left are the two controls that act on the rail itself, at the
          rail's own edge, and the window's controls at the other. */}
      <header className="topbar">
        {/* No "on" styling: the rail being open is already answered by the rail
            being there, and spending the brand colour on a control whose state
            is the largest object on screen is exactly what the palette rule
            exists to prevent. */}
        <button
          className="icon-btn"
          onClick={() => setRail((shown) => !shown)}
          aria-pressed={rail}
          aria-label={
            rail ? "Hide the conversation list" : "Show the conversation list"
          }
          title={
            rail ? "Hide the conversation list" : "Show the conversation list"
          }
        >
          <SidebarIcon size={15} />
        </button>
        {/* Beside the fold toggle rather than inside the rail, and that is what
            makes it work when the rail is folded: finding a conversation is the
            way back to one, so it must not be inside the thing you put away. It
            is the window's either way — a conversation in another folder is not
            a question any pane can answer — which is the same test the display
            switches pass to sit here. */}
        <Finder
          sessions={findable}
          home={home}
          onShow={showHere}
          onOpenFolder={onOpenFolder}
        />
        {/* The window's other two surfaces, beside the two above for the same
            reason: there is one browser and one terminal dock per window, so
            neither is a conversation's to offer. They were on every pane's
            header, which meant a split view drew two browser buttons that
            toggled the same browser — and put six icons in the corner of a
            window whose whole argument is that density is earned. */}
        <button
          className="icon-btn"
          onClick={openBrowser}
          aria-label="Open or close the browser"
          title="Open or close the browser"
        >
          <GlobeIcon size={15} />
        </button>
        <button
          className="icon-btn"
          onClick={openTerminal}
          aria-label="Open or hide the terminals"
          title={`Terminals (${MOD}+J)`}
        >
          <TerminalIcon size={15} />
        </button>
        {/* What the window draws, not what any conversation holds — which is why
            it passes the bar's own test (rule 9c) where a session action would
            not. With the window split, "show reasoning" cannot mean one thing in
            the left pane and another in the right. */}
        <WindowDragRegion />
        <SettingsPanel display={display} onChange={onDisplay} />
        <WindowControls />
      </header>

      {/* Unmounted rather than hidden when folded: a `display: none` rail is
          still a grid column somebody has to remember to zero. */}
      {rail && (
        <Rail
          sessions={sessions}
          onScreen={onScreen}
          stateOf={stateOf}
          statusOf={statusOf}
          onShow={showHere}
          onCloseSession={onCloseSession}
          onOpenFolder={onOpenFolder}
        />
      )}

      {/* The field, or what stands in for it. An empty tiling used to swap the
          whole window for the launchpad; now it is a state of the field, which
          is what it always was — the rail beside it is unchanged either way. */}
      {shown.root ? (
        <Panes
          tiling={shown}
          context={context}
          stateOf={stateOf}
          statusOf={statusOf}
        />
      ) : (
        <FieldEmpty />
      )}
    </div>
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
