import { describe, expect, it } from "vitest";

import { byProfile, effortSlots, pinLabel, type ModelChoice } from "./picker";

const models: ModelChoice[] = [
  { profile: "anthropic", label: "Opus 5", efforts: ["low", "high"] },
  { profile: "anthropic", label: "Sonnet 5", efforts: [] },
  { profile: "openai", label: "gpt-5.1-codex", efforts: ["medium", "high"] },
];

describe("byProfile", () => {
  it("groups adjacent runs and keeps each model's index in the flat menu", () => {
    const groups = byProfile(models);

    expect(groups.map((group) => group.profile)).toEqual(["anthropic", "openai"]);
    expect(groups[0].items.map((item) => item.at)).toEqual([0, 1]);
    // The index, not the position in the group: it is what `choose_model` takes.
    expect(groups[1].items[0].at).toBe(2);
  });

  it("does not merge the same profile across a gap", () => {
    // The backend emits one profile at a time, so this shape means the config
    // really does list them apart. Sorting it would show an order `/model` never
    // shows.
    const groups = byProfile([models[0], models[2], models[1]]);

    expect(groups.map((group) => group.profile)).toEqual([
      "anthropic",
      "openai",
      "anthropic",
    ]);
  });

  it("has no groups when nothing is configured", () => {
    expect(byProfile([])).toEqual([]);
  });
});

describe("pinLabel", () => {
  it("words each kind of pin the way the config does", () => {
    expect(pinLabel({ kind: "inherit" }, models)).toBe("inherit");
    expect(pinLabel({ kind: "off" }, models)).toBe("off");
    expect(pinLabel({ kind: "model", index: 0, effort: null }, models)).toBe("Opus 5");
    expect(pinLabel({ kind: "model", index: 0, effort: "high" }, models)).toBe(
      "Opus 5 · high",
    );
  });

  it("says a vanished model is unavailable rather than naming another one", () => {
    expect(pinLabel({ kind: "model", index: 9, effort: null }, models)).toBe(
      "unavailable",
    );
  });
});

describe("effortSlots", () => {
  it("offers auto ahead of the model's own levels", () => {
    expect(effortSlots(models[0])).toEqual(["auto", "low", "high"]);
  });

  it("offers nothing at all for a model with no dial", () => {
    // Absent, not a lone "auto" segment: a control with one option is furniture.
    expect(effortSlots(models[1])).toEqual([]);
    expect(effortSlots(undefined)).toEqual([]);
  });
});
