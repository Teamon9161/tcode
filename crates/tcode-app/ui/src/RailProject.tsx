import { useCallback, useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { invoke } from "@ipc";

import type { Status, StoredSession } from "./types";
import type { SessionState } from "./session";
import type { RailGroup } from "./railData";
import { sessionTitle } from "./railData";
import { statusLabel } from "./activity";
import { ago } from "./time";
import { useSeat } from "./seat";
import { StatusDot } from "./components/Status";
import {
  ChevronDown,
  ChevronRight,
  CloseIcon,
  MoreIcon,
  PlusIcon,
} from "./components/Icons";

/**
 * One project in the rail: the folder, its open conversations, and — once
 * asked — the ones it can go back to.
 *
 * This is the component the launchpad's project row and the rail's session
 * group collapsed into. They were the same thing drawn twice with different
 * halves missing: the launchpad's row had the history and no notion of what was
 * running, the rail's group had the running conversations and no way to reach
 * an old one. A folder is one thing, so it is one row.
 *
 * The disclosure means the same thing in both bands — *show me more of this
 * project* — and the two defaults fall out of what a project has rather than
 * from a rule about which band it is in: a project with conversations open
 * starts open, because those are what the rail exists to publish; a project with
 * none starts closed, because a folder you have not touched today is a place,
 * not an event.
 *
 * **Stored conversations are loaded on demand, and that is a cost decision, not
 * a fold.** Building the previews replays every log in the project, so a rail
 * that fetched them for each group as it drew would spend the first second after
 * launch replaying history nobody asked to see. A group with live conversations
 * therefore shows those immediately and puts its history behind one more row; a
 * group with none loads on expand, since history is the only thing it has and
 * making you click twice for it would be a fold pretending to be thrift.
 */
export function RailProject({
  group,
  band,
  at,
  total,
  folded,
  onScreen,
  stateOf,
  statusOf,
  now,
  onFold,
  onMove,
  onHide,
  onShow,
  onCloseSession,
  onOpenFolder,
}: {
  group: RailGroup;
  band: "live" | "recent";
  at: number;
  total: number;
  folded: boolean;
  onScreen: Set<string>;
  stateOf: (session: string) => SessionState;
  statusOf: (session: string) => Status;
  /** The backend's clock, so "2 hours ago" agrees with the timestamps it sent. */
  now: number;
  onFold: () => void;
  onMove: (to: number) => void;
  onHide: () => void;
  onShow: (session: string) => void;
  onCloseSession: (session: string) => void;
  onOpenFolder: (path: string, resume?: string) => Promise<void>;
}) {
  const [failure, setFailure] = useState<string | null>(null);
  const [history, setHistory] = useState<StoredSession[] | null>(null);
  const [unreadable, setUnreadable] = useState<string | null>(null);
  const [asked, setAsked] = useState(false);
  const [busy, setBusy] = useState(false);

  const live = group.sessions;
  const open = !folded;
  const wantsHistory = open && (live.length === 0 || asked);
  const missing = group.info?.exists === false;

  useEffect(() => {
    if (!wantsHistory || history || unreadable) return;
    invoke<StoredSession[]>("project_sessions", { path: group.path })
      .then(setHistory)
      .catch((error) => setUnreadable(String(error)));
  }, [wantsHistory, history, unreadable, group.path]);

  const enter = useCallback(
    (resume?: string) => {
      setBusy(true);
      onOpenFolder(group.path, resume)
        .catch((error) => setFailure(String(error)))
        .finally(() => setBusy(false));
    },
    [onOpenFolder, group.path],
  );

  const needs = live.filter((entry) => statusOf(entry.id) === "waiting").length;
  const running = live.some((entry) => statusOf(entry.id) === "running");
  // A log with a conversation already on it is not a resume target: opening it
  // again would put a second ledger on one file. It is shown as what it is —
  // the conversation upstairs — rather than hidden, because a history that
  // silently omits today's work reads as history that was lost.
  const openLogs = new Set(
    live.map((entry) => entry.log_id).filter(Boolean) as string[],
  );

  return (
    <li
      className={`rail-group${open ? "" : " is-folded"}`}
      onKeyDown={(event) => {
        if (band !== "live") return;
        if (
          !event.altKey ||
          (event.key !== "ArrowUp" && event.key !== "ArrowDown")
        )
          return;
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
          aria-expanded={open}
          title={group.path}
        >
          {open ? <ChevronDown size={12} /> : <ChevronRight size={12} />}
          <span className={`rail-project-name${missing ? " is-gone" : ""}`}>
            {group.name}
          </span>
          {/* Folded, the group has to keep answering the rail's whole question.
              A conversation waiting on a human is said in words rather than a
              count, because it is the one state worth interrupting for. A
              project with nothing open says when you were last in it — the only
              fact it has, and the one the band is sorted by. */}
          {needs > 0 ? (
            <span className="rail-needs">
              {needs === 1 ? "needs you" : `${needs} need you`}
            </span>
          ) : live.length > 0 ? (
            !open && (
              <span className="rail-count">
                {running && <span className="rail-live" aria-label="running" />}
                {live.length}
              </span>
            )
          ) : (
            <span className={`rail-when${missing ? " is-gone" : ""}`}>
              {missing
                ? "folder missing"
                : ago(group.info?.last_active ?? null, now)}
            </span>
          )}
        </button>
        {/* On the heading rather than as a row in the body, and that is the
            second time this control has moved. As a row it appeared once per
            group — three folders open meant three identical `New conversation`
            rows standing among four actual conversations, which is furniture in
            the one column that cannot afford any. Here it costs no permanent
            width (it is hidden until the heading is pointed at) and keeps the
            one-click path to the act the rail is most often used for. */}
        <button
          type="button"
          className="icon-btn rail-add"
          title={`New conversation in ${group.name}`}
          aria-label={`New conversation in ${group.name}`}
          disabled={missing || busy}
          onClick={() => enter()}
        >
          <PlusIcon size={14} />
        </button>
        <ProjectMenu
          group={group}
          band={band}
          at={at}
          total={total}
          onMove={onMove}
          onHide={onHide}
          onNew={() => enter()}
        />
      </div>

      {failure && (
        <p className="rail-new-failure" role="alert">
          {failure}
        </p>
      )}

      {open && (
        <ul className="rail-sessions">
          {live.map((entry) => (
            <LiveRow
              key={entry.id}
              state={stateOf(entry.id)}
              status={statusOf(entry.id)}
              onScreen={onScreen.has(entry.id)}
              folder={group.name}
              onShow={() => onShow(entry.id)}
              onClose={() => onCloseSession(entry.id)}
            />
          ))}

          {/* The only row in the body that is not a conversation, and only where
              there is already something to show. Its count is the project's own,
              so the cost of the click — a replay of every log in the folder — is
              stated before it is paid. */}
          {live.length > 0 &&
            !asked &&
            (group.info?.session_count ?? 0) > 0 && (
              <li>
                <button
                  type="button"
                  className="rail-earlier"
                  onClick={() => setAsked(true)}
                >
                  Earlier
                  <span className="rail-earlier-count">
                    {group.info?.session_count}
                  </span>
                </button>
              </li>
            )}

          {wantsHistory && (
            <StoredList
              history={history}
              unreadable={unreadable}
              openLogs={openLogs}
              now={now}
              missing={missing}
              onResume={(id) => enter(id)}
            />
          )}
        </ul>
      )}
    </li>
  );
}

/** A conversation this process is holding. Two lines: what it was asked to do,
 *  and where that has got to. */
function LiveRow({
  state,
  status,
  onScreen,
  folder,
  onShow,
  onClose,
}: {
  state: SessionState;
  status: Status;
  onScreen: boolean;
  folder: string;
  onShow: () => void;
  onClose: () => void;
}) {
  // The first prompt, and the activity line only until there is one: "not
  // started" is the whole truth about a conversation nobody has typed into yet,
  // and a stale first line beats no line at all.
  const title = sessionTitle(state.blocks) ?? state.activity;
  return (
    <li>
      <button
        className={`rail-item${onScreen ? " is-onscreen" : ""}`}
        onClick={onShow}
        title={title}
      >
        <StatusDot status={status} />
        <span className="rail-lines">
          <span className="rail-name">{title}</span>
          <span className="rail-activity">{statusLabel(state.activity)}</span>
        </span>
      </button>
      <button
        className="rail-close"
        onClick={onClose}
        aria-label={`Close this conversation in ${folder}`}
      >
        <CloseIcon size={13} />
      </button>
    </li>
  );
}

/** The logs this project can be resumed from. */
function StoredList({
  history,
  unreadable,
  openLogs,
  now,
  missing,
  onResume,
}: {
  history: StoredSession[] | null;
  unreadable: string | null;
  openLogs: Set<string>;
  now: number;
  missing: boolean;
  onResume: (id: string) => void;
}) {
  // An unreadable folder says so. Falling back to the empty-state sentence
  // would tell someone their conversations are gone.
  if (unreadable) {
    return (
      <li className="rail-note" role="alert">
        could not read this folder: {unreadable}
      </li>
    );
  }
  if (history === null) return <li className="rail-note">reading history…</li>;
  if (history.length === 0) {
    return (
      <li className="rail-note">No conversation to go back to here yet.</li>
    );
  }
  return (
    <>
      {history.map((entry) => {
        const held = openLogs.has(entry.id);
        return (
          <li key={entry.id}>
            <button
              className="rail-stored"
              onClick={() => onResume(entry.id)}
              disabled={held || missing}
              title={held ? "This conversation is open above." : entry.preview}
            >
              <span className="rail-stored-preview">
                {entry.preview || "(no prompt yet)"}
              </span>
              <span className="rail-stored-time">
                {held ? "open" : ago(entry.modified, now)}
              </span>
            </button>
          </li>
        );
      })}
    </>
  );
}

/**
 * The project's own actions, behind one control.
 *
 * Reordering used to be two arrow buttons that appeared on hover beside every
 * heading. They were right about being hidden and wrong about being two: moving
 * a folder is something you do once and then rely on for weeks, and it was
 * taking permanent width in the one list that has none to spare. One `⋯` holds
 * them, and the keyboard path they had (Alt+arrow) is untouched — a menu is
 * where a rare action is *found*, not the only way to do it.
 *
 * The items differ by band because the acts do. Arranging is only meaningful
 * where the order is yours; `Recent` is sorted by when you were last there, and
 * a "move up" that the next visit undoes is a control that lies.
 */
function ProjectMenu({
  group,
  band,
  at,
  total,
  onMove,
  onHide,
  onNew,
}: {
  group: RailGroup;
  band: "live" | "recent";
  at: number;
  total: number;
  onMove: (to: number) => void;
  onHide: () => void;
  onNew: () => void;
}) {
  const [open, setOpen] = useState(false);
  const trigger = useRef<HTMLButtonElement>(null);
  const box = useRef<HTMLDivElement>(null);

  const close = useCallback(() => {
    setOpen(false);
    trigger.current?.focus();
  }, []);
  useSeat({
    open,
    trigger,
    box,
    onEscape: close,
    onOutside: () => setOpen(false),
  });

  const run = (act: () => void) => () => {
    setOpen(false);
    act();
  };

  return (
    <>
      <button
        ref={trigger}
        type="button"
        className="icon-btn rail-more"
        aria-expanded={open}
        aria-label={`More for ${group.name}`}
        title={`More for ${group.name}`}
        onClick={() => setOpen((was) => !was)}
      >
        <MoreIcon size={14} />
      </button>

      {open &&
        createPortal(
          <div
            className="seated pmenu"
            ref={box}
            role="menu"
            aria-label={group.name}
          >
            <button type="button" role="menuitem" onClick={run(onNew)}>
              New conversation
            </button>
            {band === "live" ? (
              <>
                <button
                  type="button"
                  role="menuitem"
                  disabled={at === 0}
                  onClick={run(() => onMove(at - 1))}
                >
                  Move up
                  <kbd>Alt ↑</kbd>
                </button>
                <button
                  type="button"
                  role="menuitem"
                  disabled={at === total - 1}
                  onClick={run(() => onMove(at + 1))}
                >
                  Move down
                  <kbd>Alt ↓</kbd>
                </button>
              </>
            ) : (
              /* Hides the row; the folder and its logs are untouched. Worded as
                 what it does to this list, because "remove project" in an app
                 that works inside folders reads as something far worse. */
              <button type="button" role="menuitem" onClick={run(onHide)}>
                Hide from recent
              </button>
            )}
          </div>,
          document.body,
        )}
    </>
  );
}
