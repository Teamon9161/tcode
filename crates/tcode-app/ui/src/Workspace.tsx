import { useCallback, useEffect, useMemo, useState } from "react";

import type { Decision, SessionInfo, Status } from "./types";
import type { SessionState } from "./session";
import type { PlanDraft } from "./plan";
import type { PlanDecision } from "./PlanEditor";
import {
  close,
  focusPane,
  focused,
  navigate,
  openInspect,
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
import { StatusDot } from "./components/Status";
import { BackIcon, CloseIcon, SidebarIcon } from "./components/Icons";
import { navValue } from "./inspect";
import { WindowControls } from "./components/WindowControls";
import { DRAG } from "./components/drag";
import { Panes, type PaneContext } from "./Panes";

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
 * Nothing in the rail starts a *new* conversation any more. That button used to
 * sit at the bottom of the list and was the one row in it that was not a
 * conversation; the folder chip in each pane's header owns it now, where the
 * folder was already being named.
 *
 * The title bar spans the whole window rather than sitting inside the field,
 * because it is also the window's own bar (`decorations: false`, see
 * `components/WindowControls.tsx`): window buttons belong at the window's
 * corner. It deliberately carries no title. With the window split, no single
 * conversation is "the" one, and a bar naming one of them would be a second,
 * sometimes-wrong answer to a question each pane's own header already answers.
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
  onAnswer,
  onDecidePlan,
  onPlanDraft,
  onSavePlan,
  onPlanOpen,
  onPlanFirst,
  onCloseSession,
  onHome,
  onOpenFolder,
}: {
  tiling: Tiling;
  sessions: SessionInfo[];
  stateOf: (session: string) => SessionState;
  statusOf: (session: string) => Status;
  onTiling: (step: (current: Tiling) => Tiling) => void;
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
  onCloseSession: (session: string) => void;
  onHome: () => void;
  onOpenFolder: (path: string) => Promise<void>;
}) {
  const narrow = useNarrow();
  const [rail, setRail] = useState(true);
  const leaves = panes(tiling);
  const here = focused(tiling);
  const onScreen = useMemo(() => new Set(sessionsInView(tiling)), [tiling]);

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
          : openInspect(current, pane, session, { kind: "files" });
      });
    },
    [onTiling],
  );

  // Workspace browsing is an inspect value too: when another view is current,
  // follow normal inspect history to the tree; when the tree is current, close
  // the pane. This keeps a session's one inspect pane and its navigation intact.
  const toggleWorkspace = useCallback(
    (pane: string, session: string) => {
      onTiling((current) => {
        const sibling = panes(current).find(
          (leaf) => leaf.pane.kind === "inspect" && leaf.pane.session === session,
        );
        if (sibling?.pane.kind === "inspect" && navValue(sibling.pane.nav).kind === "workspace-tree") {
          return close(current, sibling.id);
        }
        return openInspect(current, pane, session, { kind: "workspace-tree" });
      });
    },
    [onTiling],
  );

  // Esc closes the pane you are looking into, before it reaches the composer's
  // interrupt. A conversation pane ignores it: Esc must never be the key that
  // removes the thing you are typing into.
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      const seat = focused(tiling);
      if (!seat || seat.pane.kind !== "inspect") return;
      event.stopPropagation();
      onTiling((current) => close(current, seat.id));
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [tiling, onTiling]);

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
        const cwd = sessions.find((open) => open.id === seat?.pane.session)?.cwd;
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
          event.shiftKey ? showBeside(current, pick.id) : show(current, pick.id),
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

  const context: PaneContext = {
    sessions,
    stateOf,
    statusOf,
    focus: tiling.focus,
    split: leaves.length > 1 && !narrow,
    onFocus: (pane) => onTiling((current) => focusPane(current, pane)),
    onClosePane: (pane) => onTiling((current) => close(current, pane)),
    onRatio: (divider, ratio) => onTiling((current) => setRatio(current, divider, ratio)),
    onOpen: (pane, session, value) =>
      onTiling((current) => openInspect(current, pane, session, value)),
    onNavigate: (pane, step) => onTiling((current) => navigate(current, pane, step)),
    onToggleFiles: toggleFiles,
    onToggleWorkspace: toggleWorkspace,
    onDraft,
    onAttach,
    onDetach,
    onSend,
    onInterrupt,
    onAnswer,
    onDecidePlan,
    onPlanDraft,
    onSavePlan,
    onPlanOpen,
    onPlanFirst,
    onOpenFolder,
  };

  // Below the threshold the tree stops being shown and only the current pane
  // is: two panes in 700px are two unreadable panes. Structural, not fluid —
  // nothing shrinks, one thing is chosen (PRODUCT.md § Design Principles).
  const shown = narrow && here ? { root: here, focus: here.id } : tiling;

  return (
    <div className={`workspace${rail ? "" : " is-folded"}`}>
      <header className="topbar" {...DRAG}>
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
        <span className="topbar-gap" {...DRAG} />
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
            {/* Two lines, and the second is the one that makes the list usable:
                a conversation is named after its folder, so several open in one
                folder were several identical rows and the rail could account for
                them without telling you which was which. The activity line is
                what each one is doing or last did — the same line the launchpad
                card carries, which is where it was already the answer. */}
            {sessions.map((entry) => (
              <li key={entry.id}>
                <button
                  className={`rail-item${onScreen.has(entry.id) ? " is-onscreen" : ""}`}
                  onClick={() => onTiling((current) => show(current, entry.id))}
                  title={entry.cwd}
                >
                  <StatusDot status={statusOf(entry.id)} />
                  <span className="rail-lines">
                    <span className="rail-name">{entry.name}</span>
                    <span className="rail-activity">{stateOf(entry.id).activity}</span>
                  </span>
                </button>
                <button
                  className="rail-close"
                  onClick={() => onCloseSession(entry.id)}
                  aria-label={`Close ${entry.name}`}
                >
                  <CloseIcon size={13} />
                </button>
              </li>
            ))}
          </ul>
        </nav>
      )}

      <Panes tiling={shown} context={context} />
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
