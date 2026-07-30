import { describe, expect, it } from "vitest";

import {
  applyUsage,
  cacheShare,
  contextLevel,
  limitLevel,
  NO_METER,
  percent,
  resetIn,
  tokens,
  windowLabel,
  type Meter,
} from "./usage";

const usage = (
  input: number,
  output: number,
  cacheRead = 0,
  cacheWrite = 0,
) => ({
  input_tokens: input,
  output_tokens: output,
  cache_read_tokens: cacheRead,
  cache_write_tokens: cacheWrite,
});

describe("applyUsage", () => {
  it("replaces the context figure with the provider's tally instead of adding to it", () => {
    // The whole prompt for one request is `total_input`; accumulating it across
    // the steps of a turn would count the cached prefix once per request and
    // report a window several times fuller than it is.
    let meter: Meter = NO_METER;
    meter = applyUsage(meter, { type: "Usage", data: usage(500, 200, 9_000) });
    expect(meter.context).toBe(9_700);

    meter = applyUsage(meter, { type: "Usage", data: usage(300, 150, 10_000) });
    expect(meter.context).toBe(10_450);
  });

  it("adds every step to the turn receipt while the context figure stays a snapshot", () => {
    let meter: Meter = NO_METER;
    meter = applyUsage(meter, { type: "Usage", data: usage(500, 200, 9_000) });
    meter = applyUsage(meter, { type: "Usage", data: usage(300, 150, 10_000) });

    expect(meter.turn.input_tokens).toBe(800);
    expect(meter.turn.output_tokens).toBe(350);
    expect(meter.turn.cache_read_tokens).toBe(19_000);
  });

  it("counts a sub-agent's spend against the bill but not against the window", () => {
    let meter: Meter = NO_METER;
    meter = applyUsage(meter, { type: "Usage", data: usage(500, 200) });
    meter = applyUsage(meter, { type: "DelegatedUsage", data: usage(40_000, 900) });

    expect(meter.turn.input_tokens).toBe(40_500);
    expect(meter.context).toBe(700);
  });

  it("marks the figure estimated when something entered the window unmeasured", () => {
    const expanded = applyUsage(
      { ...NO_METER, context: 5_000 },
      { type: "ReferencesExpanded", data: { labels: ["plan.md"], added_tokens: 1_200 } },
    );
    expect(expanded.context).toBe(6_200);
    expect(expanded.estimated).toBe(true);

    // A compaction rewrote the history; nothing on this side knows how large the
    // summary is until the next response says so.
    const compacted = applyUsage({ ...NO_METER, context: 180_000 }, {
      type: "Compacted",
      data: "…",
    });
    expect(compacted.estimated).toBe(true);

    // And the next authoritative tally takes the mark back off.
    expect(applyUsage(compacted, { type: "Usage", data: usage(9_000, 100) }).estimated).toBe(
      false,
    );
  });

  it("does not zero the receipt on Started, which core sends per request", () => {
    // A six-step turn emits six of these. Resetting here would report only the
    // last step's cost; the receipt is zeroed where the turn is submitted.
    const before = { ...NO_METER, context: 12_000, turn: usage(900, 300) };
    expect(applyUsage(before, { type: "Started" })).toBe(before);
  });

  it("ignores the events it is not about", () => {
    const meter = { ...NO_METER, context: 4_000 };
    expect(applyUsage(meter, { type: "TextDelta", data: "hello" })).toBe(meter);
  });
});

describe("percent", () => {
  it("clamps and never divides by an undeclared window", () => {
    expect(percent(50, 200)).toBe(25);
    expect(percent(400, 200)).toBe(100);
    expect(percent(5, 0)).toBe(0);
  });
});

describe("levels", () => {
  it("stays achromatic until the number is worth a colour", () => {
    expect(contextLevel(60)).toBe("calm");
    expect(contextLevel(85)).toBe("high");
    expect(contextLevel(95)).toBe("full");
    // Subscription windows warn earlier: running out of them costs hours, not a
    // compaction.
    expect(limitLevel(60)).toBe("calm");
    expect(limitLevel(75)).toBe("high");
    expect(limitLevel(90)).toBe("full");
  });
});

describe("tokens", () => {
  it("shortens the way the terminal's meter does", () => {
    expect(tokens(980)).toBe("980");
    expect(tokens(1_178)).toBe("1.2k");
    expect(tokens(68_400)).toBe("69k");
  });
});

describe("windowLabel", () => {
  it("names a window from the minutes the provider reported", () => {
    expect(windowLabel(300)).toBe("5h");
    expect(windowLabel(10_080)).toBe("weekly");
    expect(windowLabel(1_440)).toBe("daily");
    expect(windowLabel(45)).toBe("45m");
  });

  it("says only what it knows when the provider sent no window", () => {
    expect(windowLabel(0)).toBe("window");
  });
});

describe("resetIn", () => {
  it("is precise enough to decide whether to wait", () => {
    expect(resetIn(10_000 + 4_800, 10_000)).toBe("1h 20m");
    expect(resetIn(10_000 + 259_200, 10_000)).toBe("3d");
    expect(resetIn(10_000 + 90, 10_000)).toBe("2m");
    expect(resetIn(10_000 + 30, 10_000)).toBe("under a minute");
  });

  it("draws nothing for a window that has already reset or never reported one", () => {
    expect(resetIn(0, 10_000)).toBeNull();
    expect(resetIn(9_000, 10_000)).toBeNull();
  });
});

describe("cacheShare", () => {
  it("tells no input apart from nothing cached", () => {
    expect(cacheShare(usage(100, 10, 900))).toBeCloseTo(0.9);
    expect(cacheShare(usage(100, 10))).toBe(0);
    expect(cacheShare(usage(0, 0))).toBeNull();
  });
});
