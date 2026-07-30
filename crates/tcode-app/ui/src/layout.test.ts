import { describe, expect, it } from "vitest";

import {
  EMPTY,
  close,
  closeSession,
  focusPane,
  focused,
  frames,
  navigate,
  openAside,
  openInspect,
  browserPane,
  panes,
  parentSplit,
  rotate,
  showBeside,
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
    node.kind === "leaf" ? label(node.pane) : `${node.dir}(${draw(node.a)}, ${draw(node.b)})`;
  return tiling.root ? draw(tiling.root) : "-";
}

function label(pane: Pane): string {
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
  return entries.map((entry, index) => (index === at ? `[${entry.kind}]` : entry.kind));
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
    expect(split(one, "pane-does-not-exist", "row", talk("duck_ext"))).toBe(one);
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
    expect(shape(rotate(three, inner!))).toBe("row(tcode, row(duck_ext, pybond))");
  });

  it("has none for the root", () => {
    const one = single(talk("tcode"));
    expect(parentSplit(one, id(one, "tcode"))).toBeNull();
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
    const two = openInspect(one, id(one, "tcode"), "tcode", { kind: "diff", callId: "e1" });

    expect(shape(two)).toBe("row(tcode, tcode:diff)");
    expect(at(two)).toBe("tcode:diff");
  });

  it("reuses the conversation's pane after that, stacking history", () => {
    const one = single(talk("tcode"));
    const two = openInspect(one, id(one, "tcode"), "tcode", { kind: "diff", callId: "e1" });
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
    const three = openInspect(two, id(two, "tcode"), "tcode", { kind: "diff", callId: "e1" });
    const four = openInspect(three, id(three, "duck_ext"), "duck_ext", { kind: "files" });

    expect(shape(four)).toBe("row(row(tcode, tcode:diff), row(duck_ext, duck_ext:files))");
  });
});

describe("navigate", () => {
  it("walks one pane's history without touching the layout", () => {
    const one = single(talk("tcode"));
    const two = openInspect(one, id(one, "tcode"), "tcode", { kind: "diff", callId: "e1" });
    const three = openInspect(two, id(two, "tcode"), "tcode", { kind: "output", callId: "e1" });
    const pane = id(three, "tcode:output");

    const back = navigate(three, pane, navBack);
    expect(shape(back)).toBe("row(tcode, tcode:diff)");
    expect(history(back)).toEqual(["[diff]", "output"]);

    const forward = navigate(back, pane, navForward);
    expect(history(forward)).toEqual(["diff", "[output]"]);
  });

  it("clamps at both ends", () => {
    const one = single(talk("tcode"));
    const two = openInspect(one, id(one, "tcode"), "tcode", { kind: "diff", callId: "e1" });
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
    expect(setRatio(two, divider, 9).root).toMatchObject({ kind: "split", ratio: 0.9 });
    expect(setRatio(two, divider, 0.35).root).toMatchObject({ kind: "split", ratio: 0.35 });
  });

  it("rotates between side by side and stacked", () => {
    const one = single(talk("tcode"));
    const two = split(one, id(one, "tcode"), "row", talk("duck_ext"));
    const divider = two.root?.id ?? "";

    expect(shape(rotate(two, divider))).toBe("col(tcode, duck_ext)");
    expect(shape(rotate(rotate(two, divider), divider))).toBe("row(tcode, duck_ext)");
  });

  it("ignores ids that are not dividers", () => {
    const one = single(talk("tcode"));
    expect(setRatio(one, id(one, "tcode"), 0.3)).toBe(one);
    expect(rotate(one, id(one, "tcode"))).toBe(one);
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

    expect(laid.panes.map(({ leaf, rect }) => [label(leaf.pane), rect])).toEqual([
      ["tcode", { left: 0, top: 0, width: 0.5, height: 1 }],
      ["duck_ext", { left: 0.5, top: 0, width: 0.5, height: 1 }],
    ]);
    expect(laid.dividers).toHaveLength(1);
    expect(laid.dividers[0]).toMatchObject({ dir: "row", ratio: 0.5, within: { left: 0, width: 1 } });
  });

  it("nests, so an inner split only divides its own half", () => {
    const one = single(talk("tcode"));
    const two = split(one, id(one, "tcode"), "row", talk("duck_ext"));
    const three = split(two, id(two, "duck_ext"), "col", talk("pybond"));
    const laid = frames(three);

    expect(laid.panes.map(({ leaf, rect }) => [label(leaf.pane), rect])).toEqual([
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
    const three = split(two, id(two, "tcode"), "col", diff("duck_ext", "edit-1"));

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
    const second = openInspect(first, browserPane(first, "tcode")!.id, "tcode", file("b.rs"));

    // Still two panes, and the file pane is the one that changed.
    expect(shape(second)).toBe(
      "row(tcode, row(tcode:workspace-tree, tcode:workspace-file))",
    );
    expect(second.focus).toBe(first.focus);
  });

  it("keeps a diff from the transcript out of the tree as well", () => {
    const open = browsing();
    const seat = panes(open).find((leaf) => leaf.pane.kind === "session")!;
    const looked = openInspect(open, seat.id, "tcode", { kind: "diff", callId: "edit-1" });

    // Beside the pane it was opened from, which is the transcript — and the
    // tree is still there, which is the whole point.
    expect(shape(looked)).toBe("row(row(tcode, tcode:diff), tcode:workspace-tree)");
  });

  it("gives openAside a pane even when a file pane already exists", () => {
    const open = browsing();
    const first = openInspect(open, open.focus, "tcode", file("a.rs"));
    const beside = openAside(first, first.focus, "tcode", file("b.rs"));

    expect(panes(beside).filter((leaf) => leaf.pane.kind === "inspect")).toHaveLength(3);
    expect(browserPane(beside, "tcode")).not.toBeNull();
  });

  it("finds the browsing pane and only that one", () => {
    const open = browsing();
    const picked = openInspect(open, open.focus, "tcode", file("a.rs"));
    const found = browserPane(picked, "tcode")!;

    expect(found.pane.kind === "inspect" && navValue(found.pane.nav).kind).toBe("workspace-tree");
    expect(browserPane(picked, "duck_ext")).toBeNull();
  });
});
