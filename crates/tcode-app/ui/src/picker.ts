/**
 * The wire shape of `src/picker.rs` and the pure reads over it.
 *
 * Kept out of the component for the usual reason in this codebase: grouping a
 * flat model list under its profiles and wording a role's pin are decisions with
 * right and wrong answers, and answers are testable. `ModelPanel.tsx` draws what
 * these return.
 */

export type ModelChoice = {
  profile: string;
  label: string;
  /** Reasoning efforts this model accepts, lowest first. Empty = no dial. */
  efforts: string[];
};

export type PresetChoice = { key: string; label: string };

/** What one role runs on. Mirrors `picker::PinChoice`. */
export type PinChoice =
  | { kind: "off" }
  | { kind: "inherit" }
  | { kind: "model"; index: number; effort: string | null };

export type RoleChoice = {
  key: string;
  label: string;
  allows_off: boolean;
  /** true = machinery around a turn; false = a sub-agent you hand work to. */
  helper: boolean;
  pin: PinChoice;
};

export type PickerState = {
  models: ModelChoice[];
  model: number;
  effort: string | null;
  /** The running model's window, in tokens. What the context ring divides by. */
  context_window: number;
  presets: PresetChoice[];
  preset: number | null;
  roles: RoleChoice[];
  /** The mode's own name (`default`, `unsafe`, …) — the same word `/mode` and
   *  the config file use. There is deliberately no prettier second label. */
  modes: { key: string; detail: string }[];
  mode: string;
  mode_staged: boolean;
};

/** One profile's models, carrying each one's index in the flat menu. */
export type ProfileGroup = {
  profile: string;
  items: { at: number; model: ModelChoice }[];
};

/**
 * Models grouped under the profile that offers them.
 *
 * The profile is a property of a *set* of models, so it belongs to a heading
 * rather than to a second line under every row — six rows each repeating
 * "anthropic" is six chances to read the same word and none to see the shape of
 * the list. Menu order is preserved, and so is the index each model has in the
 * flat list, because that index is what the switch command takes.
 *
 * Adjacent runs, not a sort: the backend already emits one profile at a time,
 * and re-sorting here would put the list in a different order than `/model`
 * shows in the terminal for no reason a reader could name.
 */
export function byProfile(models: ModelChoice[]): ProfileGroup[] {
  const groups: ProfileGroup[] = [];
  models.forEach((model, at) => {
    const last = groups[groups.length - 1];
    if (last && last.profile === model.profile) last.items.push({ at, model });
    else groups.push({ profile: model.profile, items: [{ at, model }] });
  });
  return groups;
}

/**
 * What a role's row says it runs on, in the vocabulary of the config it writes:
 * `inherit`, `off`, or the model, with its effort when one is pinned.
 *
 * A pin whose model is gone says so rather than falling back to a plausible
 * name: it means the config changed under a running app, and quietly showing the
 * wrong model is worse than showing that something is wrong.
 */
export function pinLabel(pin: PinChoice, models: ModelChoice[]): string {
  if (pin.kind === "off") return "off";
  if (pin.kind === "inherit") return "inherit";
  const model = models[pin.index];
  if (!model) return "unavailable";
  return pin.effort ? `${model.label} · ${pin.effort}` : model.label;
}

/** The effort slots a model offers, `auto` first. `auto` is the absence of an
 *  explicit effort, which is a choice and so has to be selectable. */
export function effortSlots(model: ModelChoice | undefined): string[] {
  return model && model.efforts.length > 0 ? ["auto", ...model.efforts] : [];
}
