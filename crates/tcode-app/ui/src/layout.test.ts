import { describe, expect, it } from "vitest";

import {
  EMPTY,
  close,
  closeSession,
  dirFor,
  focusPane,
  focused,
  frames,
  navigate,
  openAside,
  openInspect,
  openWeb,
  webPane,
  toggleTerminal,
  terminalPane,
  browserPane,
  panes,
  parentSplit,
  rotate,
  showBeside,
  replaceSession,
  swap,
  sessionsInView,
  setRatio,
  show,
  single,
  split,
  updatePane,
  type Layout,
  type Pane,
  type Tiling,
} from "./layout";
import { navBack, navForward, navOf, navValue, type Inspect } from "./inspect";

const talk = (session: string): Pane => ({ kind: "session", session });
const diff = (session: string, callId: string): Pane => ({
  kind: "inspect",
  session,
  nav: navOf({ kind: "diff", callId }),
});

/** Structure as a string, so assertions read as the layout they describe and
 *  never depend on generated ids. */
function shape(tiling: Tiling): string {
  const draw = (node: Layout): string =>
    node.kind === "leaf"
      ? label(node.pane)
      : `${node.dir}(${draw(node.a)}, ${draw(node.b)})`;
  return tiling.root ? draw(tiling.root) : "-";
}

function label(pane: Pane): string {
  if (pane.kind === "web") return "web";
  if (pane.kind === "terminal") return "term";
  return pane.kind === "session"
    ? pane.session
    : `${pane.session}:${navValue(pane.nav).kind}`;
}

/** The history of the one inspect pane in `tiling`, oldest first, with the
 *  cursor's entry marked. */
function history(tiling: Tiling): string[] {
  const leaf = panes(tiling).find((entry) => entry.pane.kind === "inspect");
  if (!leaf || leaf.pane.kind !== "inspect") throw new Error("no inspect pane");
  const { entries, at } = leaf.pane.nav;
  return entries.map((entry, index) =>
    index === at ? `[${entry.kind}]` : entry.kind,
  );
}

/** Every pane's share of the window's width, by label — the number the person
 *  actually sees, as opposed to a ratio that only means something relative to
 *  whatever pane happened to be split. */
function widths(tiling: Tiling): Record<string, number> {
  const out: Record<string, number> = {};
  for (const { leaf, rect } of frames(tiling).panes)
    out[label(leaf.pane)] = rect.width;
  return out;
}

/** One split's ratio, for asserting that a drag somewhere else survived. */
function ratioOf(tiling: Tiling, split: string): number {
  const walk = (node: Layout): number | null =>
    node.kind === "leaf"
      ? null
      : node.id === split
        ? node.ratio
        : (walk(node.a) ?? walk(node.b));
  return (tiling.root && walk(tiling.root)) ?? NaN;
}

function at(tiling: Tiling): string {
  const leaf = focused(tiling);
  return leaf ? label(leaf.pane) : "-";
}

/** The leaf showing `name`, by the label used in these tests. */
function id(tiling: Tiling, name: string): string {
  const leaf = panes(tiling).find((entry) => label(entry.pane) === name);
  if (!leaf) throw new Error(`no pane labelled ${name} in ${shape(tiling)}`);
  return leaf.id;
}

describe("single", () => {
  it("is one pane, focused", () => {
    const tiling = single(talk("tcode"));
    expect(shape(tiling)).toBe("tcode");
    expect(at(tiling)).toBe("tcode");
    expect(sessionsInView(tiling)).toEqual(["tcode"]);
  });
});

describe("split", () => {
  it("puts the new pane beside the target and focuses it", () => {
    const one = single(talk("tcode"));
    const two = split(one, id(one, "tcode"), "row", diff("tcode", "edit-1"));

    expect(shape(two)).toBe("row(tcode, tcode:diff)");
    expect(at(two)).toBe("tcode:diff");
  });

  it("nests, so a third pane lands inside the half it was asked for", () => {
    const one = single(talk("tcode"));
    const two = split(one, id(one, "tcode"), "row", diff("tcode", "edit-1"));
    const three = split(two, id(two, "tcode:diff"), "col", talk("duck_ext"));

    expect(shape(three)).toBe("row(tcode, col(tcode:diff, duck_ext))");
    expect(panes(three).map((leaf) => label(leaf.pane))).toEqual([
      "tcode",
      "tcode:diff",
      "duck_ext",
    ]);
  });

  it("starts the window when there is nothing to split", () => {
    expect(shape(split(EMPTY, "whatever", "row", talk("tcode")))).toBe("tcode");
  });

  it("does nothing for a target that is gone", () => {
    const one = single(talk("tcode"));
    expect(split(one, "pane-does-not-exist", "row", talk("duck_ext"))).toBe(
      one,
    );
  });
});

describe("close", () => {
  it("collapses the split into the sibling and hands it focus", () => {
    const one = single(talk("tcode"));
    const two = split(one, id(one, "tcode"), "row", diff("tcode", "edit-1"));
    const back = close(two, id(two, "tcode:diff"));

    expect(shape(back)).toBe("tcode");
    expect(at(back)).toBe("tcode");
  });

  it("leaves focus alone when something else closed", () => {
    const one = single(talk("tcode"));
    const two = split(one, id(one, "tcode"), "row", diff("tcode", "edit-1"));
    const three = split(two, id(two, "tcode"), "col", talk("duck_ext"));
    const focusedOnDuck = focusPane(three, id(three, "duck_ext"));

    const back = close(focusedOnDuck, id(three, "tcode:diff"));
    expect(shape(back)).toBe("col(tcode, duck_ext)");
    expect(at(back)).toBe("duck_ext");
  });

  it("empties the window when the last pane goes", () => {
    const one = single(talk("tcode"));
    expect(close(one, id(one, "tcode"))).toEqual(EMPTY);
  });

  it("removes a whole subtree when handed a split", () => {
    const one = single(talk("tcode"));
    const two = split(one, id(one, "tcode"), "row", diff("tcode", "edit-1"));
    const three = split(two, id(two, "tcode:diff"), "col", talk("duck_ext"));
    const branch = three.root?.kind === "split" ? three.root.b.id : "";

    const back = close(three, branch);
    expect(shape(back)).toBe("tcode");
    expect(at(back)).toBe("tcode");
  });

  it("does nothing for an id that is gone", () => {
    const one = single(talk("tcode"));
    expect(close(one, "pane-does-not-exist")).toBe(one);
  });
});

describe("closeSession", () => {
  it("takes every pane the conversation owns with it", () => {
    const one = single(talk("tcode"));
    const two = split(one, id(one, "tcode"), "row", diff("tcode", "edit-1"));
    const three = split(two, id(two, "tcode:diff"), "col", talk("duck_ext"));

    const left = closeSession(three, "tcode");
    expect(shape(left)).toBe("duck_ext");
    expect(at(left)).toBe("duck_ext");
    expect(sessionsInView(left)).toEqual(["duck_ext"]);
  });

  it("empties the window when it owned everything", () => {
    const one = single(talk("tcode"));
    const two = split(one, id(one, "tcode"), "row", diff("tcode", "edit-1"));
    expect(closeSession(two, "tcode")).toEqual(EMPTY);
  });
});

describe("show", () => {
  it("focuses the conversation's existing pane instead of opening a second", () => {
    const one = single(talk("tcode"));
    const two = split(one, id(one, "tcode"), "row", talk("duck_ext"));

    const back = show(two, "tcode");
    expect(shape(back)).toBe("row(tcode, duck_ext)");
    expect(at(back)).toBe("tcode");
  });

  it("takes over the focused conversation pane rather than splitting", () => {
    const one = single(talk("tcode"));
    const two = split(one, id(one, "tcode"), "row", talk("duck_ext"));
    const onTcode = focusPane(two, id(two, "tcode"));

    const back = show(onTcode, "pybond");
    expect(shape(back)).toBe("row(pybond, duck_ext)");
    expect(at(back)).toBe("pybond");
  });

  it("splits rather than evicting whatever an inspect pane was showing", () => {
    const one = single(talk("tcode"));
    const two = split(one, id(one, "tcode"), "row", diff("tcode", "edit-1"));

    // Focus is on the diff pane. Overwriting it would throw the diff away and
    // leave tcode without anywhere to look into things.
    const back = show(two, "pybond");
    expect(shape(back)).toBe("row(tcode, row(tcode:diff, pybond))");
    expect(at(back)).toBe("pybond");
  });

  it("starts the window from the launchpad", () => {
    const started = show(EMPTY, "tcode");
    expect(shape(started)).toBe("tcode");
    expect(at(started)).toBe("tcode");
  });
});

describe("showBeside", () => {
  it("splits rather than taking the pane over", () => {
    const one = single(talk("tcode"));
    const two = showBeside(one, "duck_ext");

    expect(shape(two)).toBe("row(tcode, duck_ext)");
    expect(at(two)).toBe("duck_ext");
  });

  it("focuses a conversation that is already up instead of opening it twice", () => {
    const one = single(talk("tcode"));
    const two = showBeside(one, "duck_ext");
    const back = showBeside(two, "tcode");

    expect(shape(back)).toBe("row(tcode, duck_ext)");
    expect(at(back)).toBe("tcode");
  });

  it("starts the window from the launchpad", () => {
    expect(shape(showBeside(EMPTY, "tcode"))).toBe("tcode");
  });
});

describe("parentSplit", () => {
  it("finds the divider a pane hangs from", () => {
    const one = single(talk("tcode"));
    const two = split(one, id(one, "tcode"), "row", talk("duck_ext"));
    const three = split(two, id(two, "duck_ext"), "col", talk("pybond"));

    const inner = parentSplit(three, id(three, "pybond"));
    expect(inner).not.toBeNull();
    expect(shape(rotate(three, inner!))).toBe(
      "row(tcode, row(duck_ext, pybond))",
    );
  });

  it("has none for the root", () => {
    const one = single(talk("tcode"));
    expect(parentSplit(one, id(one, "tcode"))).toBeNull();
  });
});

describe("replaceSession", () => {
  it("renames session panes and their inspect panes without moving them", () => {
    const one = single(talk("pending"));
    const two = split(one, id(one, "pending"), "row", diff("pending", "edit-1"));
    const sessionPane = id(two, "pending");
    const inspectPane = id(two, "pending:diff");

    const opened = replaceSession(two, "pending", "real");

    expect(shape(opened)).toBe("row(real, real:diff)");
    expect(panes(opened).map((leaf) => leaf.id)).toEqual([sessionPane, inspectPane]);
  });

  it("does nothing when the placeholder is gone", () => {
    const one = single(talk("tcode"));
    expect(replaceSession(one, "pending", "real")).toBe(one);
  });
});

describe("updatePane", () => {
  it("swaps the contents and keeps the place", () => {
    const one = single(talk("tcode"));
    const two = split(one, id(one, "tcode"), "row", talk("duck_ext"));
    const seat = id(two, "tcode");

    const back = updatePane(two, seat, diff("tcode", "edit-1"));
    expect(shape(back)).toBe("row(tcode:diff, duck_ext)");
    expect(id(back, "tcode:diff")).toBe(seat);
  });

  it("does nothing for a pane that is gone", () => {
    const one = single(talk("tcode"));
    expect(updatePane(one, "pane-does-not-exist", talk("duck_ext"))).toBe(one);
  });
});

describe("openInspect", () => {
  it("splits the conversation the first time something is looked into", () => {
    const one = single(talk("tcode"));
    const two = openInspect(one, id(one, "tcode"), "tcode", {
      kind: "diff",
      callId: "e1",
    });

    expect(shape(two)).toBe("row(tcode, tcode:diff)");
    expect(at(two)).toBe("tcode:diff");
  });

  it("opens navigation lists as narrow right sidebars", () => {
    const sidebars: Inspect[] = [{ kind: "files" }, { kind: "workspace-tree" }];

    for (const value of sidebars) {
      const one = single(talk("tcode"));
      const two = openInspect(one, id(one, "tcode"), "tcode", value);

      expect(two.root).toMatchObject({ kind: "split", ratio: 0.66 });
    }
  });

  it("reuses the conversation's pane after that, stacking history", () => {
    const one = single(talk("tcode"));
    const two = openInspect(one, id(one, "tcode"), "tcode", {
      kind: "diff",
      callId: "e1",
    });
    const three = openInspect(two, id(two, "tcode"), "tcode", {
      kind: "file",
      path: "src/main.rs",
    });

    expect(shape(three)).toBe("row(tcode, tcode:file)");
    expect(history(three)).toEqual(["diff", "[file]"]);
  });

  it("gives a second conversation its own pane", () => {
    const one = single(talk("tcode"));
    const two = split(one, id(one, "tcode"), "row", talk("duck_ext"));
    const three = openInspect(two, id(two, "tcode"), "tcode", {
      kind: "diff",
      callId: "e1",
    });
    const four = openInspect(three, id(three, "duck_ext"), "duck_ext", {
      kind: "files",
    });

    expect(shape(four)).toBe(
      "row(row(tcode, tcode:diff), row(duck_ext, duck_ext:files))",
    );
  });
});

describe("navigate", () => {
  it("walks one pane's history without touching the layout", () => {
    const one = single(talk("tcode"));
    const two = openInspect(one, id(one, "tcode"), "tcode", {
      kind: "diff",
      callId: "e1",
    });
    const three = openInspect(two, id(two, "tcode"), "tcode", {
      kind: "output",
      callId: "e1",
    });
    const pane = id(three, "tcode:output");

    const back = navigate(three, pane, navBack);
    expect(shape(back)).toBe("row(tcode, tcode:diff)");
    expect(history(back)).toEqual(["[diff]", "output"]);

    const forward = navigate(back, pane, navForward);
    expect(history(forward)).toEqual(["diff", "[output]"]);
  });

  it("clamps at both ends", () => {
    const one = single(talk("tcode"));
    const two = openInspect(one, id(one, "tcode"), "tcode", {
      kind: "diff",
      callId: "e1",
    });
    const pane = id(two, "tcode:diff");

    expect(history(navigate(two, pane, navBack))).toEqual(["[diff]"]);
    expect(history(navigate(two, pane, navForward))).toEqual(["[diff]"]);
  });

  it("does nothing to a conversation pane", () => {
    const one = single(talk("tcode"));
    expect(navigate(one, id(one, "tcode"), navBack)).toBe(one);
  });
});

describe("dividers", () => {
  it("clamps a drag so neither side can be pinched away", () => {
    const one = single(talk("tcode"));
    const two = split(one, id(one, "tcode"), "row", talk("duck_ext"));
    const divider = two.root?.id ?? "";

    const squeezed = setRatio(two, divider, 0.001);
    expect(squeezed.root).toMatchObject({ kind: "split", ratio: 0.1 });
    expect(setRatio(two, divider, 9).root).toMatchObject({
      kind: "split",
      ratio: 0.9,
    });
    expect(setRatio(two, divider, 0.35).root).toMatchObject({
      kind: "split",
      ratio: 0.35,
    });
  });

  it("rotates between side by side and stacked", () => {
    const one = single(talk("tcode"));
    const two = split(one, id(one, "tcode"), "row", talk("duck_ext"));
    const divider = two.root?.id ?? "";

    expect(shape(rotate(two, divider))).toBe("col(tcode, duck_ext)");
    expect(shape(rotate(rotate(two, divider), divider))).toBe(
      "row(tcode, duck_ext)",
    );
  });

  it("exchanges the two sides while preserving their leaf ids and ratio", () => {
    const one = single(talk("tcode"));
    const two = split(one, id(one, "tcode"), "row", talk("duck_ext"), 0.3);
    const divider = two.root?.id ?? "";
    const tcode = id(two, "tcode");
    const duck = id(two, "duck_ext");

    const exchanged = swap(two, divider);
    expect(shape(exchanged)).toBe("row(duck_ext, tcode)");
    expect(id(exchanged, "tcode")).toBe(tcode);
    expect(id(exchanged, "duck_ext")).toBe(duck);
    expect(exchanged.root).toMatchObject({ kind: "split", ratio: 0.3 });
  });

  it("ignores ids that are not dividers", () => {
    const one = single(talk("tcode"));
    expect(setRatio(one, id(one, "tcode"), 0.3)).toBe(one);
    expect(rotate(one, id(one, "tcode"))).toBe(one);
    expect(swap(one, id(one, "tcode"))).toBe(one);
  });
});

/**
 * Room, which is the difference between a share of the window and a share of
 * whatever pane was split.
 */
describe("making room", () => {
  const file = (path: string): Inspect => ({ kind: "workspace-file", path });

  it("takes a nested pane's width off the conversation, not off its neighbour", () => {
    const one = single(talk("tcode"));
    const tree = openInspect(one, id(one, "tcode"), "tcode", {
      kind: "workspace-tree",
    });
    const picked = openInspect(
      tree,
      id(tree, "tcode:workspace-tree"),
      "tcode",
      file("a.rs"),
    );

    // Without this the file landed in a third of the tree's third — 22% of the
    // window — while the conversation kept two thirds of it untouched.
    const wide = widths(picked);
    expect(wide["tcode:workspace-file"]).toBeCloseTo(0.3);
    expect(wide["tcode:workspace-tree"]).toBeCloseTo(0.14);
    expect(wide["tcode"]).toBeCloseTo(0.56);
  });

  it("leaves a layout that already fits exactly as it was", () => {
    const one = single(talk("tcode"));
    const tree = openInspect(one, id(one, "tcode"), "tcode", {
      kind: "workspace-tree",
    });

    expect(tree.root).toMatchObject({ kind: "split", ratio: 0.66 });
  });

  it("does not undo a drag in a subtree it did not open into", () => {
    const two = showBeside(single(talk("tcode")), "duck_ext");
    const three = openInspect(two, id(two, "duck_ext"), "duck_ext", {
      kind: "diff",
      callId: "e1",
    });
    const seam = parentSplit(three, id(three, "duck_ext:diff"))!;
    const dragged = setRatio(three, seam, 0.8);

    const elsewhere = openInspect(dragged, id(dragged, "tcode"), "tcode", {
      kind: "diff",
      callId: "e2",
    });
    expect(ratioOf(elsewhere, seam)).toBe(0.8);
  });

  it("gives what it can when the window is out of room, rather than refusing", () => {
    let tiling = single(talk("a"));
    for (const name of ["b", "c", "d", "e"]) tiling = showBeside(tiling, name);

    const wide = widths(tiling);
    expect(Object.keys(wide)).toHaveLength(5);
    expect(
      Object.values(wide).reduce((sum, part) => sum + part, 0),
    ).toBeCloseTo(1);
    for (const part of Object.values(wide)) expect(part).toBeGreaterThan(0);
  });
});

/**
 * Which way a split cuts. The tree has always been able to stack — `Dir` has
 * had `col` since the beginning and `rotate` flips one — but every call site
 * asked for `row`, so nothing ever opened that way on its own.
 */
describe("split direction", () => {
  const wide = 16 / 9;

  it("stacks rather than shaving another column off a half-width pane", () => {
    const one = single(talk("tcode"));
    const two = openInspect(
      one,
      id(one, "tcode"),
      "tcode",
      { kind: "diff", callId: "e1" },
      wide,
    );
    expect(shape(two)).toBe("row(tcode, tcode:diff)");

    const three = openAside(
      two,
      id(two, "tcode:diff"),
      "tcode",
      { kind: "output", callId: "e1" },
      wide,
    );
    expect(shape(three)).toBe("row(tcode, col(tcode:diff, tcode:output))");
  });

  it("keeps a list beside what it opens however narrow the pane", () => {
    const two = showBeside(single(talk("tcode")), "duck_ext", wide);
    const tree = openInspect(
      two,
      id(two, "duck_ext"),
      "duck_ext",
      { kind: "workspace-tree" },
      wide,
    );
    expect(shape(tree)).toBe(
      "row(tcode, row(duck_ext, duck_ext:workspace-tree))",
    );

    const picked = openInspect(
      tree,
      id(tree, "duck_ext:workspace-tree"),
      "duck_ext",
      { kind: "workspace-file", path: "a.rs" },
      wide,
    );
    expect(shape(picked)).toBe(
      "row(tcode, row(duck_ext, row(duck_ext:workspace-tree, duck_ext:workspace-file)))",
    );
  });

  it("answers row for a caller with no field to measure", () => {
    const one = single(talk("tcode"));
    expect(dirFor(one, id(one, "tcode"))).toBe("row");
    expect(dirFor(one, "pane-does-not-exist", wide)).toBe("row");
  });

  it("reads the field's shape, not the tree's", () => {
    const one = single(talk("tcode"));
    // The same single pane: wide window beside, tall window under.
    expect(dirFor(one, id(one, "tcode"), 16 / 9)).toBe("row");
    expect(dirFor(one, id(one, "tcode"), 9 / 16)).toBe("col");
  });
});

describe("focusPane", () => {
  it("never leaves the window without a current pane", () => {
    const one = single(talk("tcode"));
    expect(focusPane(one, "pane-does-not-exist")).toBe(one);
    expect(at(focusPane(one, "pane-does-not-exist"))).toBe("tcode");
  });
});

describe("frames", () => {
  it("splits the field in two along the ratio", () => {
    const one = single(talk("tcode"));
    const two = setRatio(
      split(one, id(one, "tcode"), "row", talk("duck_ext")),
      panes(split(one, id(one, "tcode"), "row", talk("duck_ext")))[0].id,
      0.5,
    );
    const laid = frames(two);

    expect(
      laid.panes.map(({ leaf, rect }) => [label(leaf.pane), rect]),
    ).toEqual([
      ["tcode", { left: 0, top: 0, width: 0.5, height: 1 }],
      ["duck_ext", { left: 0.5, top: 0, width: 0.5, height: 1 }],
    ]);
    expect(laid.dividers).toHaveLength(1);
    expect(laid.dividers[0]).toMatchObject({
      dir: "row",
      ratio: 0.5,
      within: { left: 0, width: 1 },
    });
  });

  it("nests, so an inner split only divides its own half", () => {
    const one = single(talk("tcode"));
    const two = split(one, id(one, "tcode"), "row", talk("duck_ext"));
    const three = split(two, id(two, "duck_ext"), "col", talk("pybond"));
    const laid = frames(three);

    expect(
      laid.panes.map(({ leaf, rect }) => [label(leaf.pane), rect]),
    ).toEqual([
      ["tcode", { left: 0, top: 0, width: 0.5, height: 1 }],
      ["duck_ext", { left: 0.5, top: 0, width: 0.5, height: 0.5 }],
      ["pybond", { left: 0.5, top: 0.5, width: 0.5, height: 0.5 }],
    ]);
    // The inner divider spans only the right half, which is what lets a drag on
    // it be read back as that split's ratio rather than the whole field's.
    expect(laid.dividers[1]).toMatchObject({
      dir: "col",
      within: { left: 0.5, top: 0, width: 0.5, height: 1 },
    });
  });

  it("respects an uneven ratio", () => {
    const one = single(talk("tcode"));
    const two = split(one, id(one, "tcode"), "row", talk("duck_ext"));
    const laid = frames(setRatio(two, two.root!.id, 0.25));

    expect(laid.panes[0].rect.width).toBeCloseTo(0.25);
    expect(laid.panes[1].rect).toMatchObject({ left: 0.25, width: 0.75 });
  });

  it("is empty for an empty window", () => {
    expect(frames(EMPTY)).toEqual({ panes: [], dividers: [] });
  });
});

describe("sessionsInView", () => {
  it("lists each conversation once, in reading order", () => {
    const one = single(talk("duck_ext"));
    const two = split(one, id(one, "duck_ext"), "row", talk("tcode"));
    const three = split(
      two,
      id(two, "tcode"),
      "col",
      diff("duck_ext", "edit-1"),
    );

    expect(sessionsInView(three)).toEqual(["duck_ext", "tcode"]);
  });
});

describe("the workspace browser's pane", () => {
  const tree = (session: string): Pane => ({
    kind: "inspect",
    session,
    nav: navOf({ kind: "workspace-tree" }),
  });
  const file = (path: string): Inspect => ({ kind: "workspace-file", path });

  /** A conversation with the file tree open beside it. */
  function browsing(): Tiling {
    const one = single(talk("tcode"));
    return split(one, id(one, "tcode"), "row", tree("tcode"));
  }

  it("is never taken over by what is opened out of it", () => {
    const open = browsing();
    const picked = openInspect(open, open.focus, "tcode", file("src/main.rs"));

    expect(shape(picked)).toBe(
      "row(tcode, row(tcode:workspace-tree, tcode:workspace-file))",
    );
  });

  it("hands the second file to the pane the first one made", () => {
    const open = browsing();
    const first = openInspect(open, open.focus, "tcode", file("a.rs"));
    const second = openInspect(
      first,
      browserPane(first, "tcode")!.id,
      "tcode",
      file("b.rs"),
    );

    // Still two panes, and the file pane is the one that changed.
    expect(shape(second)).toBe(
      "row(tcode, row(tcode:workspace-tree, tcode:workspace-file))",
    );
    expect(second.focus).toBe(first.focus);
  });

  it("keeps a diff from the transcript out of the tree as well", () => {
    const open = browsing();
    const seat = panes(open).find((leaf) => leaf.pane.kind === "session")!;
    const looked = openInspect(open, seat.id, "tcode", {
      kind: "diff",
      callId: "edit-1",
    });

    // Beside the pane it was opened from, which is the transcript — and the
    // tree is still there, which is the whole point.
    expect(shape(looked)).toBe(
      "row(row(tcode, tcode:diff), tcode:workspace-tree)",
    );
  });

  it("gives openAside a pane even when a file pane already exists", () => {
    const open = browsing();
    const first = openInspect(open, open.focus, "tcode", file("a.rs"));
    const beside = openAside(first, first.focus, "tcode", file("b.rs"));

    expect(
      panes(beside).filter((leaf) => leaf.pane.kind === "inspect"),
    ).toHaveLength(3);
    expect(browserPane(beside, "tcode")).not.toBeNull();
  });

  it("finds the browsing pane and only that one", () => {
    const open = browsing();
    const picked = openInspect(open, open.focus, "tcode", file("a.rs"));
    const found = browserPane(picked, "tcode")!;

    expect(found.pane.kind === "inspect" && navValue(found.pane.nav).kind).toBe(
      "workspace-tree",
    );
    expect(browserPane(picked, "duck_ext")).toBeNull();
  });
});

/**
 * The browser, which is the window's rather than any conversation's.
 *
 * Everything here follows from it carrying no `session`, so these are the tests
 * that would catch someone "tidying" that up by giving it one.
 */
describe("the browser pane", () => {
  /** The same button both brings the browser in and takes it away. */
  it("opens the browser, and the same button closes it again", () => {
    const first = openWeb(single(talk("tcode")));
    expect(shape(first)).toBe("row(tcode, web)");

    const again = openWeb(first);
    expect(shape(again)).toBe("tcode");
    expect(webPane(again)).toBeNull();
  });

  /** The point of the whole shape: you are reading a doc, you close the
   *  conversation you happened to open it from, and the doc stays. */
  it("survives closing every conversation", () => {
    const open = openWeb(showBeside(single(talk("tcode")), "duck_ext"));
    const gone = closeSession(closeSession(open, "tcode"), "duck_ext");

    expect(shape(gone)).toBe("web");
    expect(webPane(gone)).not.toBeNull();
  });

  it("is not counted as a conversation on screen", () => {
    const open = openWeb(single(talk("tcode")));
    expect(sessionsInView(open)).toEqual(["tcode"]);
  });

  /** `show` overwrites the focused pane rather than splitting forever. With
   *  focus on the browser that would close the page being read, which is the
   *  one pane in the window with nowhere to be reopened from. */
  it("is never overwritten by a conversation arriving", () => {
    const open = openWeb(single(talk("tcode")));
    const arrived = show({ ...open, focus: webPane(open)!.id }, "duck_ext");

    expect(shape(arrived)).toBe("row(tcode, row(web, duck_ext))");
    expect(webPane(arrived)).not.toBeNull();
  });

  it("closes from its own header like any other pane", () => {
    const open = openWeb(single(talk("tcode")));
    const shut = close(open, webPane(open)!.id);

    expect(shape(shut)).toBe("tcode");
    expect(webPane(shut)).toBeNull();
  });
});

describe("the terminals", () => {
  /** The IDE shape: a dock across the *window*, not a box under whichever pane
   *  happened to be focused when the key was pressed. */
  it("opens across the bottom of the window, whatever is focused", () => {
    const one = single(talk("tcode"));
    const two = split(one, id(one, "tcode"), "row", talk("duck_ext"));
    const open = toggleTerminal(two);

    expect(shape(open)).toBe("col(row(tcode, duck_ext), term)");
    expect(at(open)).toBe("term");
  });

  it("is the whole window when there was nothing else", () => {
    expect(shape(toggleTerminal(EMPTY))).toBe("term");
  });

  /** One key, three jobs. The middle one is the common case and the one a
   *  plain toggle gets wrong: visible, focus in a composer, you want the shell. */
  it("focuses before it hides", () => {
    const open = toggleTerminal(single(talk("tcode")));
    const away = { ...open, focus: id(open, "tcode") };

    const back = toggleTerminal(away);
    expect(shape(back)).toBe("col(tcode, term)");
    expect(at(back)).toBe("term");

    const hidden = toggleTerminal(back);
    expect(shape(hidden)).toBe("tcode");
    expect(terminalPane(hidden)).toBeNull();
  });

  it("takes most of the window for the conversation above it", () => {
    const open = toggleTerminal(single(talk("tcode")));
    const heights: Record<string, number> = {};
    for (const { leaf, rect } of frames(open).panes)
      heights[label(leaf.pane)] = rect.height;

    expect(heights.tcode).toBeGreaterThan(heights.term);
  });

  /** The reason it carries no session. Something is *running* in there. */
  it("survives closing every conversation, dev server and all", () => {
    const one = single(talk("tcode"));
    const two = split(one, id(one, "tcode"), "row", talk("duck_ext"));
    const open = toggleTerminal(two);
    const gone = closeSession(closeSession(open, "tcode"), "duck_ext");

    expect(shape(gone)).toBe("term");
    expect(terminalPane(gone)).not.toBeNull();
  });

  it("is not counted as a conversation on screen", () => {
    expect(sessionsInView(toggleTerminal(single(talk("tcode"))))).toEqual([
      "tcode",
    ]);
  });

  it("is never overwritten by a conversation arriving", () => {
    const open = toggleTerminal(single(talk("tcode")));
    const arrived = show(
      { ...open, focus: terminalPane(open)!.id },
      "duck_ext",
    );

    expect(shape(arrived)).toBe("col(row(tcode, duck_ext), term)");
    expect(terminalPane(arrived)).not.toBeNull();
  });

  /**
   * The dock is a band across the bottom of the window, not a pane you put
   * things beside. `toggleTerminal` splits the root to lay it there and leaves
   * focus in it on purpose, so every opener that seats itself on the focused
   * pane used to split the dock instead.
   *
   * The browser is where that showed: it is a native child webview positioned
   * from its pane's rectangle, so landing at half the width of the bottom 30%
   * composited the page into a sliver and the whole bottom band read as blank.
   * It was reported as a white screen, which is why these assert the *shape*
   * rather than merely that the pane exists — "the browser opened" was already
   * true while the bug was there.
   */
  it("never seats a new pane inside the dock", () => {
    const open = toggleTerminal(single(talk("tcode")));

    expect(shape(openWeb(open))).toBe("col(row(tcode, web), term)");
    expect(shape(showBeside(open, "duck_ext"))).toBe(
      "col(row(tcode, duck_ext), term)",
    );
  });

  it("stays at the bottom when it is all the window has", () => {
    const alone = toggleTerminal(EMPTY);
    expect(shape(alone)).toBe("term");
    // Above the dock, not below it: the terminals are this window's floor, and
    // a dock that ends up on top of the thing it serves is the same mistake as
    // opening inside it.
    expect(shape(openWeb(alone))).toBe("col(web, term)");
  });

  it("closes from its own header like any other pane", () => {
    const open = toggleTerminal(single(talk("tcode")));
    expect(shape(close(open, terminalPane(open)!.id))).toBe("tcode");
  });
});
