import { useCallback, useLayoutEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { invoke } from "@ipc";

import { useSeat } from "./seat";

import {
  byProfile,
  effortSlots,
  pinLabel,
  type PickerState,
  type PinChoice,
} from "./picker";
import { BackIcon, ChevronRight, PlusIcon, ReturnIcon } from "./components/Icons";

/**
 * Everything about what this window thinks with, behind one chip: the model, its
 * effort, the saved presets, and what every sub-agent runs on.
 *
 * It is one control because they are one decision. The strip used to carry four
 * chips side by side — preset, model, effort — which asked the reader to already
 * know that picking a preset moves the chip next to it, and left the two rarest
 * and most consequential settings (what `explore` runs on, how to keep a line-up)
 * reachable only from the terminal.
 *
 * The shape is a panel, not a menu, and the difference is load-bearing:
 *
 * - **The list scrolls; the dials do not.** Effort and preset sit in a footer
 *   pinned to the bottom edge — which, because the panel opens upward, is the
 *   part nearest the chip you just clicked. Effort was previously the last
 *   section of a scrolling list, i.e. furthest from the cursor and often out of
 *   view, for something changed as often as the model itself.
 * - **The profile is a heading, not a second line per row.** Six rows each
 *   repeating "anthropic" is six chances to read the same word and none to see
 *   the shape of the list.
 * - **Nothing here closes the panel.** One rule with no exceptions to remember:
 *   picks apply instantly and are read back on the spot (the dot moves, the
 *   segment fills), and the panel is dismissed by Escape, by clicking away, or by
 *   the chip again. A surface holding four kinds of setting cannot close after
 *   one of them and stay predictable.
 * - **Roles are a second view, not a section.** Ten more rows under the models
 *   would put the common case behind a scroll to serve the rare one.
 *
 * It renders through a portal: a fixed popover inside the composer's `<form>`
 * would be clipped by the pane, and a text field inside that form submits the
 * message on Enter.
 */

type View = { at: "model" } | { at: "agents" } | { at: "role"; key: string };

export function ModelPanel({
  state,
  refresh,
}: {
  state: PickerState;
  /** Re-read the picker after a change: every command here can move the rest. */
  refresh: () => void;
}) {
  const [open, setOpen] = useState(false);
  const [view, setView] = useState<View>({ at: "model" });
  const [failure, setFailure] = useState<string | null>(null);
  const [naming, setNaming] = useState<string | null>(null);
  const trigger = useRef<HTMLButtonElement>(null);
  const box = useRef<HTMLDivElement>(null);

  const model = state.models[state.model];
  const preset = state.preset !== null ? state.presets[state.preset] : null;

  const close = useCallback(() => {
    setOpen(false);
    setView({ at: "model" });
    setNaming(null);
    setFailure(null);
    // Back to the chip, so Escape leaves the hand where it started rather than
    // stranding focus on a panel that no longer exists.
    trigger.current?.focus();
  }, []);

  // Anchoring, dismissal and the resize follow are `seat.ts`'s — the same three
  // chores every portalled popover in this window has. Escape is the one thing
  // this panel does differently, and it is the reason the hook takes it apart
  // from the outside click: from a role, Escape goes back one level rather than
  // closing the whole surface.
  useSeat({
    open,
    trigger,
    box,
    onEscape: () => (view.at === "role" ? setView({ at: "agents" }) : close()),
    onOutside: close,
  });

  useLayoutEffect(() => {
    if (!open) return;
    // Focus lands on the row already in force, which is both where the eye is
    // and where the arrow keys should start. Without it the panel opens with
    // focus still on the chip outside it, and the keyboard cannot reach the list
    // at all — the pointer would be the only way in, in an app that is otherwise
    // driven from the keys.
    const rows = box.current?.querySelectorAll<HTMLElement>("[data-row]");
    const here = box.current?.querySelector<HTMLElement>('[data-row][aria-current="true"]');
    (here ?? rows?.[0])?.focus();
  }, [open]);

  /** Apply, then re-read. Resolves to whether it worked, because the preset
   *  field must keep what was typed when the name was refused. */
  const act = (run: Promise<unknown>): Promise<boolean> =>
    run
      .then(() => {
        setFailure(null);
        refresh();
        return true;
      })
      .catch((error) => {
        setFailure(String(error));
        return false;
      });

  /** Name the live line-up. The field stays put when the name is refused —
   *  the rules for a preset name live in the config writer, and its complaint is
   *  worth more next to what was typed than replacing it. */
  const save = (name: string) => {
    if (name.trim().length === 0) return;
    act(invoke("save_preset", { name })).then((ok) => ok && setNaming(null));
  };

  /** Arrow keys walk the rows, as they do in any list you can point at. */
  const onKeyDown = (event: React.KeyboardEvent) => {
    const step = event.key === "ArrowDown" ? 1 : event.key === "ArrowUp" ? -1 : 0;
    if (step === 0 && event.key !== "Home" && event.key !== "End") return;
    const rows = Array.from(box.current?.querySelectorAll<HTMLElement>("[data-row]") ?? []);
    if (rows.length === 0) return;
    event.preventDefault();
    const at = rows.indexOf(document.activeElement as HTMLElement);
    const next =
      event.key === "Home"
        ? 0
        : event.key === "End"
          ? rows.length - 1
          : at < 0
            ? step > 0
              ? 0
              : rows.length - 1
            : (at + step + rows.length) % rows.length;
    rows[next]?.focus();
  };

  // A role that has vanished from the config under an open panel falls back to
  // the main view rather than drawing a header for nothing.
  const role =
    (view.at === "role" ? state.roles.find((one) => one.key === view.key) : null) ?? null;

  return (
    <div className="chip-box">
      <button
        ref={trigger}
        type="button"
        className="chip is-model"
        aria-expanded={open}
        aria-haspopup="dialog"
        title={
          preset
            ? `Running the '${preset.label}' preset — model, effort and every role`
            : "Model, effort, presets and sub-agent models"
        }
        onClick={() => (open ? close() : setOpen(true))}
      >
        <span className="chip-model">{model?.label ?? "no model"}</span>
        {state.effort && <span className="chip-effort">{state.effort}</span>}
      </button>

      {open &&
        createPortal(
          <div
            className="seated mpanel"
            ref={box}
            role="dialog"
            aria-label="Model"
            onKeyDown={onKeyDown}
          >
            {role ? (
              <div className="mpanel-head">
                <button
                  type="button"
                  className="icon-btn"
                  onClick={() => setView({ at: "agents" })}
                  aria-label="Back to agents"
                >
                  <BackIcon size={14} />
                </button>
                <span className="mpanel-title">{role.label}</span>
                <span className="mpanel-head-note">
                  {role.helper ? "runs around your turn" : "runs your delegated work"}
                </span>
              </div>
            ) : (
              <div className="mpanel-tabs" role="group" aria-label="What to set">
                {(["model", "agents"] as const).map((at) => (
                  <button
                    key={at}
                    type="button"
                    aria-pressed={view.at === at}
                    className={`mpanel-tab${view.at === at ? " is-on" : ""}`}
                    onClick={() => setView({ at })}
                  >
                    {at}
                  </button>
                ))}
              </div>
            )}

            <div className="mpanel-body">
              {view.at === "agents" ? (
                <Roles state={state} onOpen={(key) => setView({ at: "role", key })} />
              ) : (
                <Models
                  state={state}
                  role={role}
                  onPick={(index, effort) =>
                    act(
                      role
                        ? invoke("pin_role", {
                            kind: role.key,
                            pin: { kind: "model", index, effort },
                          })
                        : invoke("choose_model", { index, effort }),
                    )
                  }
                  onPin={(pin) => role && act(invoke("pin_role", { kind: role.key, pin }))}
                />
              )}
            </div>

            {failure && (
              <p className="mpanel-note" role="alert">
                {failure}
              </p>
            )}

            {view.at !== "agents" && (
              <div className="mpanel-foot">
                <Effort state={state} role={role} onRun={act} />

                {/* Presets belong to the main view: one names the whole line-up,
                    including every role, so it is not a property of one of them. */}
                {view.at === "model" &&
                  (naming === null ? (
                    <div className="mfield">
                      <span className="mfield-label">preset</span>
                      <div className="mfield-body">
                        {state.presets.map((one, at) => (
                          <button
                            key={one.key}
                            type="button"
                            data-row
                            className={`mseg-item${at === state.preset ? " is-on" : ""}`}
                            onClick={() => act(invoke("choose_preset", { key: one.key }))}
                          >
                            {one.label}
                          </button>
                        ))}
                        <button
                          type="button"
                          data-row
                          className="mfield-add"
                          onClick={() => setNaming("")}
                          title="Save the model, effort and every role as a named preset"
                        >
                          <PlusIcon size={12} />
                          save
                        </button>
                      </div>
                    </div>
                  ) : (
                    <div className="mfield">
                      <span className="mfield-label">name</span>
                      <div className="mfield-body">
                        <input
                          className="mfield-input"
                          value={naming}
                          autoFocus
                          spellCheck={false}
                          placeholder="letters, digits, - and _"
                          onChange={(event) => setNaming(event.target.value)}
                          onKeyDown={(event) => {
                            // This field is portalled out of the composer's form,
                            // but Enter still has to be caught here: it is the
                            // one that saves.
                            if (event.key === "Enter") {
                              event.preventDefault();
                              save(naming);
                            }
                            if (event.key === "Escape") {
                              // The panel's own Escape would leave the field
                              // open behind a closed panel.
                              event.stopPropagation();
                              setNaming(null);
                              setFailure(null);
                            }
                          }}
                        />
                        <button
                          type="button"
                          className="mfield-go"
                          aria-label="Save preset"
                          disabled={naming.trim().length === 0}
                          onClick={() => save(naming)}
                        >
                          <ReturnIcon size={14} />
                        </button>
                      </div>
                    </div>
                  ))}
              </div>
            )}
          </div>,
          document.body,
        )}
    </div>
  );
}

/**
 * The models, under the profile that offers them.
 *
 * The same list serves the main model and a role's pin — a pin is a model
 * choice, and offering it in a different shape would make the reader learn the
 * list twice. What differs is the two rows on top, which only a role has.
 */
function Models({
  state,
  role,
  onPick,
  onPin,
}: {
  state: PickerState;
  role: PickerState["roles"][number] | null;
  onPick: (index: number, effort: string | null) => void;
  onPin: (pin: PinChoice) => void;
}) {
  const current = role
    ? role.pin.kind === "model"
      ? role.pin.index
      : -1
    : state.model;
  const effort = role
    ? role.pin.kind === "model"
      ? role.pin.effort
      : null
    : state.effort;

  if (state.models.length === 0) {
    return (
      <p className="mpanel-empty">
        No provider is configured. Add one to <code>~/.tcode/config.toml</code>, or run{" "}
        <code>tcode</code> in a terminal for the setup wizard.
      </p>
    );
  }

  return (
    <>
      {role && (
        <>
          <p className="mpanel-group">follow</p>
          <Row
            current={role.pin.kind === "inherit"}
            label="inherit"
            detail="Whatever the main model is, live — it moves with the chip."
            onPick={() => onPin({ kind: "inherit" })}
          />
          {role.allows_off && (
            <Row
              current={role.pin.kind === "off"}
              label="off"
              detail="Do not run this at all."
              onPick={() => onPin({ kind: "off" })}
            />
          )}
        </>
      )}

      {byProfile(state.models).map((group) => (
        <div key={group.profile} className="mpanel-section">
          <p className="mpanel-group is-id">{group.profile}</p>
          {group.items.map(({ at, model }) => (
            <Row
              key={`${group.profile}/${model.label}`}
              current={at === current}
              label={model.label}
              mono
              onPick={() =>
                // Carry the effort across when the target model has the same
                // dial: switching model is not a request to forget you asked for
                // `high`. Dropped when it has no such setting, since there is
                // nothing there to carry it to.
                onPick(at, effort && model.efforts.includes(effort) ? effort : null)
              }
            />
          ))}
        </div>
      ))}
    </>
  );
}

/** The roles: what you delegate to, then the machinery around a turn. */
function Roles({
  state,
  onOpen,
}: {
  state: PickerState;
  onOpen: (key: string) => void;
}) {
  const sections = [
    { label: "sub-agents", roles: state.roles.filter((one) => !one.helper) },
    { label: "helpers", roles: state.roles.filter((one) => one.helper) },
  ].filter((section) => section.roles.length > 0);

  if (sections.length === 0) {
    return <p className="mpanel-empty">No roles are available in this configuration.</p>;
  }

  return (
    <>
      {sections.map((section) => (
        <div key={section.label} className="mpanel-section">
          <p className="mpanel-group">{section.label}</p>
          {section.roles.map((role) => (
            <button
              key={role.key}
              type="button"
              data-row
              className="mrow is-nav"
              onClick={() => onOpen(role.key)}
            >
              <span className="mrow-mark" aria-hidden="true" />
              <span className="mrow-name">{role.label}</span>
              <span className={`mrow-meta${role.pin.kind === "model" ? " is-id" : ""}`}>
                {pinLabel(role.pin, state.models)}
              </span>
              <ChevronRight size={13} className="mrow-more" />
            </button>
          ))}
        </div>
      ))}
    </>
  );
}

/**
 * The effort dial, pinned to the panel's bottom edge and always about whatever
 * the current view is choosing: the main model, or the model a role is pinned to.
 * One control in one place, rather than a second list somewhere else for the
 * same question.
 *
 * Absent, not disabled, when the subject has no dial — a control that cannot do
 * anything is worse than no control.
 */
function Effort({
  state,
  role,
  onRun,
}: {
  state: PickerState;
  role: PickerState["roles"][number] | null;
  onRun: (run: Promise<unknown>) => void;
}) {
  const at = role ? (role.pin.kind === "model" ? role.pin.index : -1) : state.model;
  const model = state.models[at];
  const slots = effortSlots(model);
  if (slots.length === 0) return null;

  const now = (role && role.pin.kind === "model" ? role.pin.effort : state.effort) ?? "auto";

  return (
    <div className="mfield">
      <span className="mfield-label">effort</span>
      <div className="mfield-body is-seg">
        {slots.map((slot) => (
          <button
            key={slot}
            type="button"
            data-row
            aria-pressed={slot === now}
            className={`mseg-item${slot === now ? " is-on" : ""}`}
            onClick={() => {
              const effort = slot === "auto" ? null : slot;
              onRun(
                role
                  ? invoke("pin_role", {
                      kind: role.key,
                      pin: { kind: "model", index: at, effort },
                    })
                  : invoke("choose_model", { index: at, effort }),
              );
            }}
          >
            {slot}
          </button>
        ))}
      </div>
    </div>
  );
}

/** One choosable row: a dot in the gutter for the current one, a name, and an
 *  optional line of what it means. */
function Row({
  current,
  label,
  detail,
  mono,
  onPick,
}: {
  current: boolean;
  label: string;
  detail?: string;
  mono?: boolean;
  onPick: () => void;
}) {
  return (
    <button
      type="button"
      data-row
      className={`mrow${current ? " is-current" : ""}`}
      aria-current={current}
      onClick={onPick}
    >
      <span className="mrow-mark" aria-hidden="true" />
      <span className={`mrow-name${mono ? " is-id" : ""}`}>{label}</span>
      {detail && <span className="mrow-detail">{detail}</span>}
    </button>
  );
}
