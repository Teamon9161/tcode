import { describe, expect, it } from "vitest";

import {
  addPhase,
  currentPhase,
  draftOf,
  editAt,
  fromDraft,
  isEdited,
  isPlanReview,
  isPlanSubmission,
  movePhase,
  nextStatus,
  phaseRows,
  planChanges,
  removeAt,
  toDraft,
  type Plan,
  type PlanPhase,
} from "./plan";

const phase = (
  name: string,
  status: PlanPhase["status"] = "pending",
  detail = "",
  phases: PlanPhase[] = [],
): PlanPhase => ({ phase: name, status, detail, phases });

const plan = (phases: PlanPhase[]): Plan => ({
  path: "/tmp/p.md",
  file: "p.md",
  title: "Rewrite the resume path",
  state: "active",
  done: phases.filter((entry) => entry.status === "completed").length,
  total: phases.length,
  phases,
});

describe("the phase list a glance gets", () => {
  it("expands sub-phases only under the phase that is running", () => {
    const rows = phaseRows([
      phase("now", "in_progress", "", [phase("write the test", "completed"), phase("fix it")]),
      phase("later", "pending", "", [phase("hidden")]),
    ]);
    expect(rows.map((row) => row.phase)).toEqual(["now", "write the test", "fix it", "later"]);
    expect(rows.map((row) => row.depth)).toEqual([0, 1, 1, 0]);
  });

  it("names the deepest running phase as the current one", () => {
    const running = currentPhase([
      phase("done", "completed"),
      phase("now", "in_progress", "", [phase("part one", "completed"), phase("part two", "in_progress")]),
    ]);
    // The parent is already named by the plan's title; the sub-phase is the
    // specific answer to "where is this".
    expect(running?.phase).toBe("part two");
  });

  it("falls back to the next pending phase, and to nothing when finished", () => {
    expect(currentPhase([phase("done", "completed"), phase("next")])?.phase).toBe("next");
    expect(currentPhase([phase("done", "completed")])).toBeNull();
  });
});

describe("editing", () => {
  it("changes one phase and leaves every other row identical", () => {
    const draft = toDraft([phase("one", "pending", "why one"), phase("two")]);
    const next = editAt(draft, [0], (was) => ({ ...was, phase: "one, renamed" }));
    expect(next[0].phase).toBe("one, renamed");
    expect(next[0].detail).toBe("why one");
    // Identity, not just equality: an untouched row must not re-render or lose
    // the caret in a field the user is typing in.
    expect(next[1]).toBe(draft[1]);
  });

  it("reorders among siblings and refuses positions outside the list", () => {
    const draft = toDraft([phase("a"), phase("b"), phase("c")]);
    expect(movePhase(draft, [2], 0).map((entry) => entry.phase)).toEqual(["c", "a", "b"]);
    expect(movePhase(draft, [0], -1)).toBe(draft);
    expect(movePhase(draft, [0], 3)).toBe(draft);
  });

  it("reorders sub-phases without touching their parent's position", () => {
    const draft = toDraft([phase("top", "pending", "", [phase("x"), phase("y")]), phase("other")]);
    const moved = movePhase(draft, [0, 1], 0);
    expect(moved[0].phases.map((entry) => entry.phase)).toEqual(["y", "x"]);
    expect(moved[1].phase).toBe("other");
  });

  it("adds phases at the end, and sub-phases under their parent", () => {
    const draft = toDraft([phase("one")]);
    expect(addPhase(draft, null)).toHaveLength(2);
    expect(addPhase(draft, [0])[0].phases).toHaveLength(1);
  });

  it("removes a phase, and a sub-phase, by path", () => {
    const draft = toDraft([phase("one", "pending", "", [phase("x"), phase("y")]), phase("two")]);
    expect(removeAt(draft, [1]).map((entry) => entry.phase)).toEqual(["one"]);
    expect(removeAt(draft, [0, 0])[0].phases.map((entry) => entry.phase)).toEqual(["y"]);
  });

  it("cycles a status the way clicking the box does", () => {
    expect(nextStatus("pending")).toBe("in_progress");
    expect(nextStatus("in_progress")).toBe("completed");
    expect(nextStatus("completed")).toBe("pending");
  });
});

describe("what changed", () => {
  it("reports a rename as a rename rather than as a delete and an add", () => {
    const draft = draftOf(plan([phase("survey the call sites", "pending", "read only")]));
    const renamed = { ...draft, phases: editAt(draft.phases, [0], (was) => ({ ...was, phase: "survey" })) };
    expect(planChanges(renamed.base, renamed.phases)).toEqual([
      { kind: "renamed", title: "survey", from: "survey the call sites" },
    ]);
  });

  it("carries both texts for a detail edit, so the panel can diff that one field", () => {
    const draft = draftOf(plan([phase("one", "pending", "before")]));
    const edited = editAt(draft.phases, [0], (was) => ({ ...was, detail: "after" }));
    expect(planChanges(draft.base, edited)).toEqual([
      { kind: "detail", title: "one", from: "before", to: "after" },
    ]);
  });

  it("does not report the survivors of a deletion as moved", () => {
    const draft = draftOf(plan([phase("a"), phase("b"), phase("c")]));
    const changes = planChanges(draft.base, removeAt(draft.phases, [0]));
    expect(changes).toEqual([{ kind: "removed", title: "a" }]);
  });

  it("reports a move on its own", () => {
    const draft = draftOf(plan([phase("a"), phase("b")]));
    const changes = planChanges(draft.base, movePhase(draft.phases, [1], 0));
    expect(changes.map((change) => change.kind)).toEqual(["moved", "moved"]);
  });

  it("ignores a blank phase nobody typed into", () => {
    const draft = draftOf(plan([phase("a")]));
    expect(planChanges(draft.base, addPhase(draft.phases, null))).toEqual([]);
    expect(isEdited({ ...draft, phases: addPhase(draft.phases, null) })).toBe(false);
  });

  it("drops the ids on the way back out, and trims what the user typed", () => {
    const draft = toDraft([phase("  one  ", "in_progress", "  why  ")]);
    expect(fromDraft(draft)).toEqual([
      { phase: "one", status: "in_progress", detail: "why", phases: [] },
    ]);
  });
});

describe("recognizing the two shapes of a progress call", () => {
  it("treats a submission as a document and a phase flip as not one", () => {
    expect(isPlanSubmission({ state: "active", phases: [] })).toBe(true);
    expect(isPlanSubmission({ phases: [{ phase: "one", status: "in_progress" }] })).toBe(false);
    expect(isPlanSubmission({ state: "draft" })).toBe(false);
    expect(isPlanSubmission(null)).toBe(false);
  });

  it("recognizes a review by the plan body core attaches, not by the tool name", () => {
    expect(isPlanReview({ plan: "\n## [ ] 1. one\n" })).toBe(true);
    expect(isPlanReview({ plan: "   " })).toBe(false);
    expect(isPlanReview({ state: "active" })).toBe(false);
  });
});
