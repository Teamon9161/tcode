import { createPortal } from "react-dom";
import { useEffect, useRef, type RefObject } from "react";

import { useSeat } from "./seat";
import { FileIcon, FolderIcon } from "./components/Icons";

/**
 * The menu that finishes a `/command` or an `@path`.
 *
 * Portalled and `position: fixed` like every popover in this window, and for a
 * sharper reason than most: it belongs to the composer, whose `<form>` clips at
 * the pane's edge and turns a stray Enter into a sent message. It opens
 * *upward* out of the field, because the field is at the bottom of the pane and
 * a list below it would be off the screen.
 *
 * Focus never comes here. The caret stays in the field — you are still typing —
 * so the list is a `listbox` the field owns through `aria-activedescendant`,
 * and the keys that drive it are handled where the hand is. Clicking a row is
 * the same act by another route, which is why the rows are buttons that put
 * focus straight back.
 */
export type Suggestion = {
  /** What replaces the token, opening character and all (`/compact `, `@src/`). */
  insert: string;
  /** The thing itself: a command name, a file name. */
  label: string;
  /** What it is, in the app's own words. Never repeats the label. */
  hint?: string;
  /** A directory: accepting it continues the path rather than finishing it, so
   *  the menu stays open and lists what is inside. */
  directory?: boolean;
};

export function Completions({
  anchor,
  items,
  active,
  listId,
  onChoose,
  onClose,
}: {
  /** The field this came out of; the list is placed on its top-left corner. */
  anchor: RefObject<HTMLElement | null>;
  items: Suggestion[];
  /** Index of the row Enter would take. */
  active: number;
  /** Ties the field's `aria-activedescendant` to the row ids below. */
  listId: string;
  onChoose: (item: Suggestion) => void;
  onClose: () => void;
}) {
  const box = useRef<HTMLDivElement>(null);
  useSeat({ open: items.length > 0, trigger: anchor, box, onEscape: onClose, onOutside: onClose });

  // Keyboard movement happens in the field, so the list has to follow it here.
  // Without this the selection walks off the bottom of a scrolled list and the
  // next Enter takes something nobody can see.
  useEffect(() => {
    // By position rather than by id selector: `useId` mints identifiers with
    // characters a CSS selector has to escape, and the row's index is what the
    // keys move through anyway.
    const row = box.current?.querySelectorAll(".completion")[active];
    row?.scrollIntoView?.({ block: "nearest" });
  }, [active, items]);

  if (items.length === 0) return null;

  return createPortal(
    <div className="seated completions" ref={box}>
      <ul className="completion-list" role="listbox" id={listId} aria-label="Completions">
        {items.map((item, at) => (
          <li key={item.insert} role="presentation">
            <button
              type="button"
              id={`${listId}-${at}`}
              role="option"
              aria-selected={at === active}
              className={`completion${at === active ? " is-active" : ""}`}
              // The caret must not leave the field on the way to a click, or
              // the composer publishes its draft and the token being completed
              // is gone by the time this fires.
              onMouseDown={(event) => event.preventDefault()}
              onClick={() => onChoose(item)}
            >
              {item.directory !== undefined &&
                (item.directory ? <FolderIcon size={12} /> : <FileIcon size={12} />)}
              <span className="completion-label">{item.label}</span>
              {item.hint && <span className="completion-hint">{item.hint}</span>}
            </button>
          </li>
        ))}
      </ul>
    </div>,
    document.body,
  );
}
