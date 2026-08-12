import { useCallback, useRef, useState } from "react";
import { createPortal } from "react-dom";

import { useSeat } from "./seat";
import { SwitchFolderPicker } from "./FolderPicker";
import { ChevronDown } from "./components/Icons";

/**
 * The directory for this conversation, with an in-place switcher.
 *
 * It lives in the pane's own header because split panes can show conversations
 * from different folders. The visible label stays to the directory name; the
 * complete path remains available through the control's hover title.
 *
 * A pane rather than the title bar, and that is load-bearing rather than
 * convenient: with the window split there are two conversations on screen in two
 * folders, so "the current folder" is not a question the window can answer
 * (AGENTS.md rule 9c). Each pane can, and does.
 *
 * Picking applies the same `/cd` semantics to this conversation. Its session
 * identity and transcript stay in place; only its workspace root changes.
 */
export function FolderMenu({
  name,
  cwd,
  home,
  onChangeFolder,
}: {
  /** The folder's own name, which is also the conversation's. It is carried
   *  here rather than beside the chip because it was the same word twice: a
   *  session is named after its folder, so a header reading `tcode  ~/…/tcode`
   *  spent two elements on one fact. */
  name: string;
  cwd: string;
  home: string;
  onChangeFolder: (path: string) => Promise<void>;
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
        aria-label={`Switch directory for ${name}: ${cwd}`}
        onClick={() => setOpen((was) => !was)}
        title={cwd}
      >
        <span className="folder-chip-name">{name}</span>
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
            <SwitchFolderPicker
              current={cwd}
              home={home}
              onChangeFolder={onChangeFolder}
              onDone={() => setOpen(false)}
            />
          </div>,
          document.body,
        )}
    </div>
  );
}
