import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open as openDialog } from "@tauri-apps/plugin-dialog";

import type { ProjectInfo, ProjectList } from "./types";
import { Path } from "./components/Path";
import { FolderIcon } from "./components/Icons";

/**
 * Where the next conversation starts.
 *
 * The body of a popover rather than a popover itself, because two controls open
 * exactly this list and they are not the same shape: the pane header's folder
 * chip (`FolderMenu.tsx`), and the rail's `New conversation` button. Drawn twice
 * they would be two menus that answer one question, and the second copy is
 * always the one that drifts.
 *
 * **This is also the answer to "where is the button that adds a project".**
 * There isn't one, and there cannot be: a folder enters the project list by
 * having a conversation in it — `projects.rs` reconstructs the list by reading
 * the opening record of every session log, and there is no registry to write to.
 * So adding a folder is not a second action that overlaps with this one, it is
 * the last row of this one: the list holds the folders tcode already knows, and
 * `Choose a folder…` is how one that is not in it gets there. Both rows do the
 * same thing on the way out — open a conversation — which is why the heading can
 * say so once, at the top, for every item under it.
 */
export function FolderPicker({
  /** Marked rather than removed, where there is one: seeing where you are is
   *  half of what the list is for, and a second conversation in the same folder
   *  is an ordinary thing to want. */
  current,
  home,
  onOpenFolder,
  onDone,
}: {
  current?: string;
  home: string;
  onOpenFolder: (path: string) => Promise<void>;
  /** The picker chose something and the popover around it should close. Not
   *  called when the attempt failed — the failure is shown in place, and a
   *  menu that vanishes with the reason on it has told nobody anything. */
  onDone: () => void;
}) {
  const [data, setData] = useState<ProjectList | null>(null);
  const [failure, setFailure] = useState<string | null>(null);

  // Read when the menu is opened, not when its trigger is drawn: this is a list
  // of every folder tcode has ever worked in, and most triggers are never
  // asked. Mounting *is* opening, since the popover only renders while open.
  useEffect(() => {
    invoke<ProjectList>("project_list")
      .then((value) => {
        setData(value);
        setFailure(null);
      })
      .catch((error) => setFailure(String(error)));
  }, []);

  const start = (path: string) => {
    onOpenFolder(path).then(onDone, (error) => setFailure(String(error)));
  };

  const pick = async () => {
    const chosen = await openDialog({ directory: true, multiple: false }).catch(
      (error) => {
        setFailure(`the folder picker could not open: ${String(error)}`);
        return null;
      },
    );
    if (typeof chosen === "string") start(chosen);
  };

  // Folders that are gone are left out. The rail lists them — an account of
  // everywhere tcode has been is worth having, and silently dropping one looks
  // identical to a bug — but this menu has exactly one job, and an item that
  // cannot be opened does not do it.
  const projects = (data?.projects ?? []).filter((project) => project.exists);

  return (
    <>
      <p className="fmenu-head">Start a conversation in</p>

      {failure && (
        <p className="fmenu-note" role="alert">
          {failure}
        </p>
      )}

      <div className="fmenu-list">
        {!data && !failure && <p className="fmenu-note">reading folders…</p>}
        {data && projects.length === 0 && (
          <p className="fmenu-note">
            No folder has a conversation in it yet. Choose one below to start.
          </p>
        )}
        {projects.map((project) => (
          <FolderItem
            key={project.path}
            project={project}
            home={data?.home ?? home}
            current={project.path === current}
            onPick={() => start(project.path)}
          />
        ))}
      </div>

      <button
        type="button"
        className="fmenu-pick"
        role="menuitem"
        onClick={pick}
      >
        <FolderIcon size={14} />
        Choose a folder…
      </button>
    </>
  );
}

function FolderItem({
  project,
  home,
  current,
  onPick,
}: {
  project: ProjectInfo;
  home: string;
  current: boolean;
  onPick: () => void;
}) {
  return (
    <button
      type="button"
      className={`fmenu-item${current ? " is-current" : ""}`}
      role="menuitem"
      onClick={onPick}
    >
      <span className="fmenu-item-name">{project.name}</span>
      <Path
        className="fmenu-item-path"
        path={project.path}
        home={home}
        keep={3}
      />
    </button>
  );
}
