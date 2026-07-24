import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open as openDialog } from "@tauri-apps/plugin-dialog";

import type { Launchpad as LaunchpadData, ProjectInfo, SessionInfo, Status, StoredSession } from "./types";
import { ago } from "./time";
import { Wordmark } from "./components/Mark";
import { Path } from "./components/Path";
import { StatusPill } from "./components/Status";
import { ChevronDown, ChevronRight, FolderIcon, PlusIcon } from "./components/Icons";

/**
 * The first screen: what is open, and every folder tcode has worked in.
 *
 * Two affordances on purpose (DESIGN.md § Component vocabulary). Open sessions
 * are cards — each one is a discrete thing you resume — and projects are rows,
 * because a scannable list of folders is a list, not a grid of boxes.
 */
export function Launchpad({
  open,
  statusOf,
  activityOf,
  onEnter,
  onOpenFolder,
}: {
  open: SessionInfo[];
  statusOf: (id: string) => Status;
  activityOf: (id: string) => string;
  onEnter: (id: string) => void;
  onOpenFolder: (path: string, resume?: string) => Promise<void>;
}) {
  const [data, setData] = useState<LaunchpadData | null>(null);
  const [failure, setFailure] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);

  useEffect(() => {
    invoke<LaunchpadData>("launchpad")
      .then(setData)
      .catch((error) => setFailure(String(error)));
  }, []);

  const pick = useCallback(async () => {
    const chosen = await openDialog({ directory: true, multiple: false }).catch(
      (error) => {
        setFailure(`the folder picker could not open: ${String(error)}`);
        return null;
      },
    );
    if (typeof chosen !== "string") return;
    setBusy(chosen);
    await onOpenFolder(chosen).catch((error) => setFailure(String(error)));
    setBusy(null);
  }, [onOpenFolder]);

  const enter = useCallback(
    async (path: string, resume?: string) => {
      setBusy(path);
      await onOpenFolder(path, resume).catch((error) => setFailure(String(error)));
      setBusy(null);
    },
    [onOpenFolder],
  );

  return (
    <div className="launchpad">
      <header className="topbar">
        <Wordmark />
        <button className="btn btn-secondary" onClick={pick}>
          <FolderIcon />
          Open folder
        </button>
      </header>

      <div className="launchpad-scroll">
        <div className="launchpad-inner">
          {failure && (
            <p className="inline-error" role="alert">
              {failure}
            </p>
          )}

          {open.length > 0 && (
            <section className="section">
              <h2 className="section-title">Open</h2>
              <div className="cards">
                {open.map((session) => (
                  <button
                    key={session.id}
                    className="card"
                    onClick={() => onEnter(session.id)}
                  >
                    <span className="card-head">
                      <span className="card-title">{session.name}</span>
                      <StatusPill status={statusOf(session.id)} />
                    </span>
                    <Path
                      className="card-path"
                      path={session.cwd}
                      home={data?.home ?? null}
                      keep={3}
                    />
                    <span className="card-line">{activityOf(session.id)}</span>
                  </button>
                ))}
              </div>
            </section>
          )}

          <section className="section">
            <h2 className="section-title">Projects</h2>
            {!data && !failure && <ProjectSkeleton />}
            {data && data.projects.length === 0 && <NoProjects onPick={pick} />}
            {data && data.projects.length > 0 && (
              <ul className="rows">
                {data.projects.map((project) => (
                  <ProjectRow
                    key={project.path}
                    project={project}
                    home={data.home}
                    now={data.now}
                    busy={busy === project.path}
                    onEnter={enter}
                  />
                ))}
              </ul>
            )}
          </section>
        </div>
      </div>
    </div>
  );
}

function ProjectRow({
  project,
  home,
  now,
  busy,
  onEnter,
}: {
  project: ProjectInfo;
  home: string;
  now: number;
  busy: boolean;
  onEnter: (path: string, resume?: string) => void;
}) {
  const [expanded, setExpanded] = useState(false);
  const [history, setHistory] = useState<StoredSession[] | null>(null);

  // Loaded only when the row is opened: building previews replays every log in
  // the project, which is affordable once and not for every folder on launch.
  useEffect(() => {
    if (!expanded || history) return;
    invoke<StoredSession[]>("project_sessions", { path: project.path })
      .then(setHistory)
      .catch(() => setHistory([]));
  }, [expanded, history, project.path]);

  return (
    <li className={`row-group${expanded ? " is-expanded" : ""}`}>
      {/* The whole row is one disclosure. An earlier version opened a new
          session on the row body and expanded on the chevron; two actions in
          one row means the chevron reads as "go" and the row reads as "maybe
          go", and neither is right. Opening is now an item inside. */}
      <button
        className="row"
        onClick={() => setExpanded((current) => !current)}
        aria-expanded={expanded}
      >
        <span className="row-chevron">
          {expanded ? <ChevronDown /> : <ChevronRight />}
        </span>
        <span className="row-name">{project.name}</span>
        <Path className="row-path" path={project.path} home={home} keep={4} />
        <span className="row-meta">
          {project.exists ? (
            <>
              {project.session_count}
              {project.session_count === 1 ? " session" : " sessions"}
              <span className="row-dot">·</span>
              {ago(project.last_active, now)}
            </>
          ) : (
            <span className="row-gone">folder missing</span>
          )}
        </span>
      </button>

      {expanded && (
        <div className="row-history">
          <button
            className="history-item history-new"
            onClick={() => onEnter(project.path)}
            disabled={!project.exists || busy}
          >
            <PlusIcon size={14} />
            New conversation
          </button>
          {history === null && <p className="history-note">reading history…</p>}
          {history?.length === 0 && (
            <p className="history-note">
              No resumable conversation in this folder yet.
            </p>
          )}
          {history?.map((entry) => (
            <button
              key={entry.id}
              className="history-item"
              onClick={() => onEnter(project.path, entry.id)}
              disabled={!project.exists}
            >
              <span className="history-preview">{entry.preview || "(no prompt yet)"}</span>
              <span className="history-time">{ago(entry.modified, now)}</span>
            </button>
          ))}
        </div>
      )}
    </li>
  );
}

/** Teaches what the list will hold and how to put something in it. */
function NoProjects({ onPick }: { onPick: () => void }) {
  return (
    <div className="empty">
      <h3>No folders yet</h3>
      <p>
        A folder appears here once tcode has held a conversation in it. Open one
        to start — the agent works inside that folder and nowhere else.
      </p>
      <button className="btn btn-primary" onClick={onPick}>
        <FolderIcon />
        Open a folder
      </button>
    </div>
  );
}

/** A shape for the list, not a spinner over the middle of the page. */
function ProjectSkeleton() {
  return (
    <ul className="rows" aria-hidden="true">
      {[0, 1, 2].map((index) => (
        <li key={index} className="row-group">
          <div className="row row-skeleton">
            <span className="skeleton skeleton-name" />
            <span className="skeleton skeleton-path" />
            <span className="skeleton skeleton-meta" />
          </div>
        </li>
      ))}
    </ul>
  );
}
