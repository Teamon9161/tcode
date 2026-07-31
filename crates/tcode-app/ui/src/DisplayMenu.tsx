import { useCallback, useRef, useState } from "react";
import { createPortal } from "react-dom";

import type { Display } from "./display";
import { useSeat } from "./seat";
import { SettingsIcon } from "./components/Icons";

/**
 * What the window shows — the one settings surface in the app.
 *
 * It sits in the title bar, and that passes rule 9c's test rather than bending
 * it: these are the *window's* preferences about how it draws, not any
 * conversation's. With the window split, "show reasoning" cannot belong to one
 * pane and mean something in the other — a display switch per pane would be four
 * answers to a question with one.
 *
 * A dropdown of switches, not a dialog. Every item is one line stating what it
 * shows, and picking never closes the panel: the point of a switch panel is that
 * you see the effect and can change your mind on the spot, which a menu that
 * dismisses itself takes away (the same rule the model panel keeps).
 */
export function DisplayMenu({
  display,
  onChange,
}: {
  display: Display;
  onChange: (next: Display) => void;
}) {
  const [open, setOpen] = useState(false);
  const trigger = useRef<HTMLButtonElement>(null);
  const box = useRef<HTMLDivElement>(null);

  const close = useCallback(() => {
    setOpen(false);
    trigger.current?.focus();
  }, []);
  useSeat({ open, trigger, box, onEscape: close, onOutside: () => setOpen(false) });

  return (
    <>
      <button
        ref={trigger}
        type="button"
        className="icon-btn"
        aria-expanded={open}
        aria-label="What this window shows"
        title="What this window shows"
        onClick={() => setOpen((was) => !was)}
      >
        <SettingsIcon size={15} />
      </button>

      {open &&
        createPortal(
          <div className="seated dmenu" ref={box} aria-label="Display">
            <p className="dmenu-head">Show in the conversation</p>
            <Switch
              label="Reasoning"
              hint="The model's thinking, as prose between the steps."
              on={display.thinking}
              onToggle={() => onChange({ ...display, thinking: !display.thinking })}
            />
            <Switch
              label="Edit details"
              hint="Show file changes in the conversation by default."
              on={display.editDetails}
              onToggle={() => onChange({ ...display, editDetails: !display.editDetails })}
            />
          </div>,
          document.body,
        )}
    </>
  );
}

/** The composer strip's switch, at list width: the box carries the state, the
 *  row carries the name and what turning it on costs you. */
function Switch({
  label,
  hint,
  on,
  onToggle,
}: {
  label: string;
  hint: string;
  on: boolean;
  onToggle: () => void;
}) {
  return (
    <button
      type="button"
      className={`dmenu-switch${on ? " is-on" : ""}`}
      role="switch"
      aria-checked={on}
      onClick={onToggle}
    >
      <span className="chip-tick" aria-hidden="true" />
      <span className="dmenu-lines">
        <span className="dmenu-label">{label}</span>
        <span className="dmenu-hint">{hint}</span>
      </span>
    </button>
  );
}
