import { useCallback, useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { invoke } from "@tauri-apps/api/core";
import { open as openDialog } from "@tauri-apps/plugin-dialog";

import type { Launchpad, ProjectInfo } from "./types";
import { useSeat } from "./seat";
import { Path } from "./components/Path";
import { ChevronDown, FolderIcon } from "./components/Icons";

/**
 * Which folder this conversation is in, and where to start the next one.
 *
 * It lives in the pane's own header, at the top, because that is where "which
 * folder" was already being answered — the header has shown the path since the
 * first version, it was just not clickable. Making the answer the control is
 * what lets the rail stop carrying an "Open folder" button: a session rail is a
 * list of conversations, and a button that starts a different kind of thing at
 * the bottom of it was the one row in that list that was not a conversation.
 *
 * A pane rather than the title bar, and that is load-bearing rather than
 * convenient: with the window split there are two conversations on screen in two
 * folders, so "the current folder" is not a question the window can answer
 * (AGENTS.md rule 9c). Each pane can, and does.
 *
 * Picking never moves this conversation. A session's folder is fixed at the
 * moment it opens — the agent works inside it and nowhere else — so every item
 * here *starts* one, which is what the menu says in the only place it could be
 * misread.
 */
export function FolderMenu({
  name,
  cwd,
  home,
  onOpenFolder,
}: {
  /** The folder's own name, which is also the conversation's. It is carried
   *  here rather than beside the chip because it was the same word twice: a
   *  session is named after its folder, so a header reading `tcode  ~/…/tcode`
   *  spent two elements on one fact. */
  name: string;
  cwd: string;
  home: string;
  onOpenFolder: (path: string) => Promise<void>;
}) {
  const [open, setOpen] = useState(false);
  const [data, setData] = useState<Launchpad | null>(null);
  const [failure, setFailure] = useState<string | null>(null);
  const trigger = useRef<HTMLButtonElement>(null);
  const box = useRef<HTMLDivElement>(null);

  const close = useCallback(() => {
    setOpen(false);
    trigger.current?.focus();
  }, []);
  useSeat({ open, trigger, box, onEscape: close, onOutside: () => setOpen(false) });

  // Read when the menu is opened, not when the pane is drawn: this is a list of
  // every folder tcode has ever worked in, and most panes are never asked for it.
  // Re-read on each open, because another pane may have added one since.
  useEffect(() => {
    if (!open) return;
    invoke<Launchpad>("launchpad")
      .then((value) => {
        setData(value);
        setFailure(null);
      })
      .catch((error) => setFailure(String(error)));
  }, [open]);

  const start = (path: string) => {
    setOpen(false);
    onOpenFolder(path).catch((error) => {
      setFailure(String(error));
      setOpen(true);
    });
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

  // Folders that are gone are left out here, unlike on the launchpad: that
  // screen is an account of everywhere tcode has been and says so, while this
  // menu has exactly one job and an item that cannot be opened does not do it.
  const projects = (data?.projects ?? []).filter((project) => project.exists);

  return (
    <div className="folder-box">
      <button
        ref={trigger}
        type="button"
        className="folder-chip"
        aria-expanded={open}
        onClick={() => setOpen((was) => !was)}
        title="Start a conversation in another folder"
      >
        <span className="folder-chip-name">{name}</span>
        <Path className="folder-chip-path" path={cwd} home={home} keep={3} />
        <ChevronDown size={12} />
      </button>

      {open &&
        createPortal(
          <div className="seated fmenu" ref={box} role="menu" aria-label="Folders">
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
                  This is the only folder tcode has worked in so far.
                </p>
              )}
              {projects.map((project) => (
                <FolderItem
                  key={project.path}
                  project={project}
                  home={data?.home ?? home}
                  current={project.path === cwd}
                  onPick={() => start(project.path)}
                />
              ))}
            </div>

            <button type="button" className="fmenu-pick" role="menuitem" onClick={pick}>
              <FolderIcon size={14} />
              Choose a folder…
            </button>
          </div>,
          document.body,
        )}
    </div>
  );
}

/** One folder. The current one is marked rather than removed: seeing where you
 *  are is half of what the list is for, and a second conversation in the same
 *  folder is an ordinary thing to want. */
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
      <Path className="fmenu-item-path" path={project.path} home={home} keep={3} />
    </button>
  );
}
