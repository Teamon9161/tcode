import { useCallback, useRef, useState } from "react";
import { createPortal } from "react-dom";

import { useSeat } from "./seat";
import { FolderPicker } from "./FolderPicker";
import { Path } from "./components/Path";
import { ChevronDown } from "./components/Icons";

/**
 * Which folder this conversation is in, and where to start the next one.
 *
 * It lives in the pane's own header, at the top, because that is where "which
 * folder" was already being answered — the header has shown the path since the
 * first version, it was just not clickable. Making the answer the control is
 * what lets the rail carry one `New conversation` button instead of a second
 * list of folders.
 *
 * A pane rather than the title bar, and that is load-bearing rather than
 * convenient: with the window split there are two conversations on screen in two
 * folders, so "the current folder" is not a question the window can answer
 * (AGENTS.md rule 9c). Each pane can, and does.
 *
 * Picking never moves this conversation. A session's folder is fixed at the
 * moment it opens — the agent works inside it and nowhere else — so every item
 * here *starts* one, which is what `FolderPicker`'s heading says.
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
          <div
            className="seated fmenu"
            ref={box}
            role="menu"
            aria-label="Folders"
          >
            <FolderPicker
              current={cwd}
              home={home}
              onOpenFolder={onOpenFolder}
              onDone={() => setOpen(false)}
            />
          </div>,
          document.body,
        )}
    </div>
  );
}
