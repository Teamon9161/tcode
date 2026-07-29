import { useEffect, useRef, useState, type ReactNode } from "react";
import { invoke } from "@tauri-apps/api/core";

import { useSession } from "./session";

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
 * Everything here is a chip: a label you can read without clicking, that opens
 * a list when you do. No icons standing in for words, no toggle whose state you
 * have to remember the convention for. The permission chip is the one that
 * carries colour, and only when it is not the careful default — chroma is
 * reserved for "this is not what you'd assume".
 */

/** Mirrors `PickerState` in `src/picker.rs`. */
type PickerState = {
  models: { profile: string; label: string; efforts: string[] }[];
  model: number;
  effort: string | null;
  presets: { key: string; label: string }[];
  preset: number | null;
  modes: { key: string; label: string; detail: string }[];
  mode: string;
  mode_staged: boolean;
};

export function Chips() {
  const session = useSession();
  const [state, setState] = useState<PickerState | null>(null);
  const [failure, setFailure] = useState<string | null>(null);

  const refresh = () =>
    invoke<PickerState>("picker_state", { session })
      .then(setState)
      .catch((error) => setFailure(String(error)));

  useEffect(() => {
    refresh();
    // Re-read on session change only. Nothing else moves these without going
    // through the commands below, each of which refreshes on the way back.
  }, [session]);

  if (failure) return <p className="chips-note">{failure}</p>;
  if (!state) return <div className="chips" />;

  const model = state.models[state.model];
  const mode = state.modes.find((one) => one.key === state.mode);
  const efforts = model?.efforts ?? [];

  const act = (run: Promise<unknown>) =>
    run.then(refresh).catch((error) => setFailure(String(error)));

  return (
    <div className="chips">
      <Menu
        className={`chip is-mode is-${state.mode}`}
        label={`${state.mode_staged ? "→ " : ""}${mode?.label ?? state.mode}`}
        title="What this conversation may do without asking"
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
              {one.label}
            </MenuItem>
          ))
        }
      </Menu>

      {state.presets.length > 0 && (
        <Menu
          className="chip"
          label={
            state.preset !== null ? state.presets[state.preset].label : "no preset"
          }
          title="A saved line-up: main model plus every role"
        >
          {(close) =>
            state.presets.map((one, at) => (
              <MenuItem
                key={one.key}
                current={at === state.preset}
                onPick={() => {
                  close();
                  act(invoke("choose_preset", { key: one.key }));
                }}
              >
                {one.label}
              </MenuItem>
            ))
          }
        </Menu>
      )}

      <span className="chips-gap" />

      <Menu
        className="chip is-quiet"
        label={model?.label ?? "no model"}
        title="The model this conversation runs on"
        align="right"
      >
        {(close) =>
          state.models.length === 0 ? (
            <MenuItem current={false} onPick={close}>
              no usable provider is configured
            </MenuItem>
          ) : (
            state.models.map((one, at) => (
              <MenuItem
                key={`${one.profile}/${one.label}`}
                current={at === state.model}
                detail={one.profile}
                onPick={() => {
                  close();
                  act(invoke("choose_model", { index: at, effort: null }));
                }}
              >
                {one.label}
              </MenuItem>
            ))
          )
        }
      </Menu>

      {/* Absent, not disabled, when the model has no effort dial: a control
          that cannot do anything is worse than no control. */}
      {efforts.length > 0 && (
        <Menu
          className="chip is-quiet"
          label={state.effort ?? "auto"}
          title="Reasoning effort"
          align="right"
        >
          {(close) =>
            ["auto", ...efforts].map((one) => (
              <MenuItem
                key={one}
                current={one === (state.effort ?? "auto")}
                onPick={() => {
                  close();
                  act(
                    invoke("choose_model", {
                      index: state.model,
                      effort: one === "auto" ? null : one,
                    }),
                  );
                }}
              >
                {one}
              </MenuItem>
            ))
          }
        </Menu>
      )}
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
  align = "left",
  children,
}: {
  className: string;
  label: string;
  title: string;
  align?: "left" | "right";
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
        <div className={`chip-menu is-${align}`} role="menu">
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
