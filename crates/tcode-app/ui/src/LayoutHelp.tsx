import { useCallback, useRef, useState } from "react";
import { createPortal } from "react-dom";

import { ColumnsIcon } from "./components/Icons";
import { MOD } from "./keys";
import { useSeat } from "./seat";

/** The focused-pane vocabulary, available while panes are actually on screen. */
export function LayoutHelp() {
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
    <>
      <button
        ref={trigger}
        type="button"
        className="icon-btn"
        aria-expanded={open}
        aria-label="Pane layout shortcuts"
        title="Pane layout shortcuts"
        onClick={() => setOpen((current) => !current)}
      >
        <ColumnsIcon size={15} />
      </button>

      {open &&
        createPortal(
          <div
            ref={box}
            className="seated layout-help"
            role="dialog"
            aria-label="Pane layout shortcuts"
          >
            <header className="layout-help-head">
              <h2>Pane layout</h2>
              <p>Actions use the focused pane unless they say split.</p>
            </header>
            <dl className="layout-help-keys">
              {SHORTCUTS.map(([keys, action]) => (
                <div key={action}>
                  <dt>{keys}</dt>
                  <dd>{action}</dd>
                </div>
              ))}
            </dl>
          </div>,
          document.body,
        )}
    </>
  );
}

const SHORTCUTS: [string, string][] = [
  [`Drag a pane header`, "Center exchanges; edges place beside"],
  [`${MOD} + Alt + ← ↑ ↓ →`, "Focus a pane"],
  [`${MOD} + Alt + Shift + ← ↑ ↓ →`, "Exchange with a neighbor"],
  [`${MOD} + Alt + Enter`, "Make this pane a main half"],
  [`${MOD} + Alt + F`, "Expand or restore this pane"],
  [`${MOD} + Alt + Space`, "Resize mode; arrows grow, Esc finishes"],
  [`${MOD} + Alt + R`, "Rotate the current split"],
  [`${MOD} + Alt + S`, "Swap both sides of the current split"],
];
