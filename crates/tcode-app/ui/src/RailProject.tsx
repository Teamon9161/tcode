import { useCallback, useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { invoke } from "@ipc";

import type {
  ProjectInfo,
  StoredSession,
  StoredSessionsPage,
} from "./types";
import { ago } from "./time";
import { useSeat } from "./seat";
import {
  ChevronDown,
  ChevronRight,
  MoreIcon,
  PlusIcon,
} from "./components/Icons";

/** One recent project. Expanding it reads only the newest history page; every
 * later page is an explicit click so scrolling never starts disk work. */
export function RailProject({
  project,
  now,
  openLogs,
  onHide,
  onOpenFolder,
}: {
  project: ProjectInfo;
  now: number;
  openLogs: Set<string>;
  onHide: () => void;
  onOpenFolder: (path: string, resume?: string) => Promise<void>;
}) {
  const [expanded, setExpanded] = useState(false);
  const [history, setHistory] = useState<StoredSession[]>([]);
  // undefined means no page has been requested; null means the last page.
  const [next, setNext] = useState<string | null | undefined>(undefined);
  const [loading, setLoading] = useState(false);
  const [unreadable, setUnreadable] = useState<string | null>(null);
  const [failure, setFailure] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const missing = !project.exists;

  const load = useCallback(
    (before: string | null) => {
      setLoading(true);
      setUnreadable(null);
      invoke<StoredSessionsPage>("project_sessions", {
        path: project.path,
        before,
      })
        .then((page) => {
          setHistory((was) => {
            if (!before) return page.sessions;
            const held = new Set(was.map((session) => session.id));
            return [...was, ...page.sessions.filter((session) => !held.has(session.id))];
          });
          setNext(page.next);
        })
        .catch((error) => setUnreadable(String(error)))
        .finally(() => setLoading(false));
    },
    [project.path],
  );

  useEffect(() => {
    if (expanded && next === undefined && !loading && !unreadable) load(null);
  }, [expanded, next, loading, unreadable, load]);

  const enter = useCallback(
    (resume?: string) => {
      setBusy(true);
      onOpenFolder(project.path, resume)
        .catch((error) => setFailure(String(error)))
        .finally(() => setBusy(false));
    },
    [onOpenFolder, project.path],
  );

  return (
    <li className={`rail-group${expanded ? "" : " is-folded"}`}>
      <div className="rail-project">
        <button
          type="button"
          className="rail-project-head"
          onClick={() => setExpanded((open) => !open)}
          aria-expanded={expanded}
          title={project.path}
        >
          {expanded ? <ChevronDown size={12} /> : <ChevronRight size={12} />}
          <span className={`rail-project-name${missing ? " is-gone" : ""}`}>
            {project.name}
          </span>
          <span className={`rail-when${missing ? " is-gone" : ""}`}>
            {missing
              ? "folder missing"
              : `${project.session_count} · ${ago(project.last_active, now)}`}
          </span>
        </button>
        <button
          type="button"
          className="icon-btn rail-add"
          title={`New session in ${project.name}`}
          aria-label={`New session in ${project.name}`}
          disabled={missing || busy}
          onClick={() => enter()}
        >
          <PlusIcon size={14} />
        </button>
        <ProjectMenu
          name={project.name}
          onNew={() => enter()}
          onHide={onHide}
        />
      </div>

      {failure && (
        <p className="rail-new-failure" role="alert">
          {failure}
        </p>
      )}

      {expanded && (
        <ul className="rail-sessions rail-history">
          {history.map((session) => {
            const held = openLogs.has(session.id);
            return (
              <li key={session.id}>
                <button
                  className="rail-stored"
                  onClick={() => enter(session.id)}
                  disabled={held || missing || busy}
                  title={held ? "This conversation is already open." : session.preview}
                >
                  <span className="rail-stored-preview">
                    {session.preview || "(no prompt yet)"}
                  </span>
                  <span className="rail-stored-time">
                    {held ? "open" : ago(session.modified, now)}
                  </span>
                </button>
              </li>
            );
          })}
          {next === undefined && loading && (
            <li className="rail-note">reading history…</li>
          )}
          {history.length === 0 && next === null && !unreadable && (
            <li className="rail-note">No conversation to go back to yet.</li>
          )}
          {unreadable && (
            <li className="rail-note rail-history-error" role="alert">
              <span>could not read this project: {unreadable}</span>
              <button type="button" disabled={loading} onClick={() => load(next ?? null)}>
                Retry
              </button>
            </li>
          )}
          {next && !unreadable && (
            <li>
              <button
                type="button"
                className="rail-earlier"
                disabled={loading}
                onClick={() => load(next)}
              >
                <span>{loading ? "Reading…" : "Load older"}</span>
                <span className="rail-earlier-count">
                  {history.length} loaded
                </span>
              </button>
            </li>
          )}
        </ul>
      )}
    </li>
  );
}

function ProjectMenu({
  name,
  onNew,
  onHide,
}: {
  name: string;
  onNew: () => void;
  onHide: () => void;
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
        aria-label={`More for ${name}`}
        title={`More for ${name}`}
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
            aria-label={name}
          >
            <button type="button" role="menuitem" onClick={run(onNew)}>
              New session
            </button>
            <button type="button" role="menuitem" onClick={run(onHide)}>
              Hide from recent
            </button>
          </div>,
          document.body,
        )}
    </>
  );
}
