import { useCallback, useEffect, useRef, useState, type ReactNode } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

import { AGENT_EVENT, type SessionEvent } from "./types";
import { useSession } from "./session";
import { ModelPanel } from "./ModelPanel";
import { UsagePanel } from "./UsagePanel";
import type { PickerState } from "./picker";
import type { Meter } from "./usage";

/**
 * The strip under the composer: what this conversation is allowed to do, and
 * what it is thinking with.
 *
 * These two facts belong at the input because they are properties of the
 * message you are about to send, not of the app. Putting them in a settings
 * screen means the answer to "what will this run as" lives one navigation away
 * from the moment the question is asked — which is how a bypass-permissions
 * session gets left on by accident.
 *
 * Three controls, and they are deliberately different shapes. Permission mode is
 * a short list of named answers, so it is a menu. Everything about the model —
 * which one, its effort, the saved presets, what each sub-agent runs on — is one
 * panel (`ModelPanel.tsx`), because those are one decision with four dials and
 * not four decisions. What the conversation is spending is neither: it is a
 * reading, so it is a ring you glance at and a panel you open (`UsagePanel.tsx`).
 *
 * The order is the sentence the strip reads as, left to right: what this message
 * is, then what it costs, then what it runs on. The two readings sit together on
 * the right because "how full is the window" and "which model's window" are one
 * question asked twice.
 *
 * The mode chip is the one that carries colour, and only when it is not the
 * careful default: chroma is reserved for "this is not what you'd assume".
 */

export function Chips({ meter }: { meter: Meter }) {
  const session = useSession();
  const [state, setState] = useState<PickerState | null>(null);
  const [failure, setFailure] = useState<string | null>(null);

  const refresh = useCallback(
    () =>
      invoke<PickerState>("picker_state", { session })
        .then(setState)
        .catch((error) => setFailure(String(error))),
    [session],
  );

  useEffect(() => {
    refresh();
  }, [refresh]);

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    listen<SessionEvent>(AGENT_EVENT, ({ payload }) => {
      if (payload.session === session && payload.event.type === "ModeChanged") refresh();
    })
      .then((stop) => {
        unlisten = stop;
      })
      .catch((error) => setFailure(`cannot watch permission mode: ${String(error)}`));
    return () => unlisten?.();
  }, [refresh, session]);

  if (failure) return <p className="chips-note">{failure}</p>;
  if (!state) return <div className="chips" />;

  const act = (run: Promise<unknown>) =>
    run.then(refresh).catch((error) => setFailure(String(error)));

  return (
    <div className="chips">
      <Menu
        className={`chip is-mode is-${state.mode}`}
        label={state.mode_staged ? `→ ${state.mode}` : state.mode}
        title={
          state.mode_staged
            ? `Switches to ${state.mode} at the next permission boundary`
            : "What this conversation may do without asking"
        }
      >
        {(close) =>
          state.modes.map((one) => (
            <MenuItem
              key={one.key}
              current={one.key === state.mode}
              detail={one.detail}
              onPick={() => {
                close();
                act(invoke("choose_mode", { session, mode: one.key }));
              }}
            >
              {one.key}
            </MenuItem>
          ))
        }
      </Menu>

      <span className="chips-gap" />

      <UsagePanel meter={meter} window={state.context_window} />
      <ModelPanel state={state} refresh={refresh} />
    </div>
  );
}

/**
 * A chip that opens a list.
 *
 * Not a `<select>`: the options carry a second line of explanation, and the
 * native control cannot. Not a modal either — this is a small choice made in
 * passing, and anything that dims the conversation to ask it has overstated the
 * question. It closes on Escape, on picking, and on a click anywhere else,
 * which are the three ways anyone tries.
 */
function Menu({
  className,
  label,
  title,
  children,
}: {
  className: string;
  label: string;
  title: string;
  children: (close: () => void) => ReactNode;
}) {
  const [open, setOpen] = useState(false);
  const box = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    const onDown = (event: MouseEvent) => {
      if (!box.current?.contains(event.target as Node)) setOpen(false);
    };
    const onKey = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      // Before the pane's own Escape handler, which would close the pane.
      event.stopPropagation();
      setOpen(false);
    };
    window.addEventListener("mousedown", onDown);
    window.addEventListener("keydown", onKey, true);
    return () => {
      window.removeEventListener("mousedown", onDown);
      window.removeEventListener("keydown", onKey, true);
    };
  }, [open]);

  return (
    <div className="chip-box" ref={box}>
      <button
        type="button"
        className={className}
        title={title}
        aria-expanded={open}
        onClick={() => setOpen((was) => !was)}
      >
        {label}
      </button>
      {open && (
        <div className="chip-menu" role="menu">
          {children(() => setOpen(false))}
        </div>
      )}
    </div>
  );
}

function MenuItem({
  current,
  detail,
  onPick,
  children,
}: {
  current: boolean;
  detail?: string;
  onPick: () => void;
  children: ReactNode;
}) {
  return (
    <button
      type="button"
      className={`chip-item${current ? " is-current" : ""}`}
      role="menuitem"
      onClick={onPick}
    >
      <span className="chip-item-label">{children}</span>
      {detail && <span className="chip-item-detail">{detail}</span>}
    </button>
  );
}
