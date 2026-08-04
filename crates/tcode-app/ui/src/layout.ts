import { navOf, navOpen, navValue, type Inspect, type Nav } from "./inspect";

/**
 * The window's pane tree.
 *
 * This file is pure data and pure functions — no React, no DOM, no knowledge of
 * what a pane looks like. That separation is the point: every layout question
 * ("what happens to focus when this closes?", "does splitting twice nest the
 * right way?") becomes a unit test instead of something only reproducible by
 * clicking.
 *
 * ## Why a binary tree
 *
 * A list of columns cannot express "the conversation on the left, and on the
 * right a diff above a chart"; a free-floating grid can, but then every pane
 * needs coordinates and the user needs to place them. A binary space partition
 * — the tiling model hyprland and every window manager like it uses — gets
 * arbitrary nesting from one rule: a leaf splits into two, forever. The whole
 * structure is `{dir, ratio, a, b}`, which serializes to JSON without help and
 * maps one-to-one onto nested split containers when it is time to render.
 *
 * ## Why `Pane` unions session and inspect
 *
 * A conversation and a diff are not different species of thing to this file;
 * they are both "something occupying a rectangle". Making them one union is
 * what lets a diff be opened beside its conversation, or two conversations sit
 * side by side, without either case being special. `inspect.ts` already
 * established that everything lookable-into is one value dispatched on `kind`;
 * this lifts that value one level up, so the panel is no longer a fixed column
 * bolted to the right edge but a pane like any other.
 *
 * Both variants carry `session`, and that is load-bearing rather than
 * incidental: closing a conversation has to take its diffs and charts with it,
 * and `closeSession` can only be this short because every pane knows whose it
 * is.
 */

/** What occupies one rectangle. */
export type Pane =
  /** A conversation: transcript, approval dock, composer. */
  | { kind: "session"; session: string }
  /** Something belonging to a conversation, looked into. The pane carries its
   *  own back/forward history rather than a bare value — see `inspect.ts`. */
  | { kind: "inspect"; session: string; nav: Nav }
  /**
   * The window's browser.
   *
   * The one pane with **no `session`**, and that absence is the whole design
   * rather than an omission. You open it to read a doc or watch a dev server;
   * which conversation happened to be focused at the time is not a fact about
   * it. Three things follow from carrying no session, and they are the reason
   * it is shaped this way:
   *
   *  - `closeSession` filters on `pane.session`, so closing a conversation
   *    cannot take the page you are reading with it.
   *  - Its button belongs on the topbar, which rule 9c reserves for things that
   *    are the window's rather than one conversation's.
   *  - There is one of it, so when tabs arrive there is exactly one tab strip
   *    and no question of which browser a tab belongs to.
   *
   * It holds no URL either: the page lives in a native child webview that the
   * backend owns (`src/browser.rs`), which keeps its own history. Putting a URL
   * here would make this tree the second, always slightly stale, account of
   * where the browser is.
   */
  | { kind: "web" };

/** Whether a pane belongs to a conversation. The browser is the one that does
 *  not, so this is the guard everywhere a pane's session is read. */
export function paneSession(pane: Pane): string | null {
  return pane.kind === "web" ? null : pane.session;
}

/** Which way a split lays its two children out, named after the flex axis:
 *  `row` puts them side by side, `col` stacks them. */
export type Dir = "row" | "col";

export type Leaf = { kind: "leaf"; id: string; pane: Pane };

export type Split = {
  kind: "split";
  id: string;
  dir: Dir;
  /** Share of the space given to `a`, 0–1. `b` gets the rest. */
  ratio: number;
  a: Layout;
  b: Layout;
};

export type Layout = Leaf | Split;

/**
 * The tree and the cursor into it, as one value.
 *
 * Same reasoning as `Nav` in `inspect.ts`: a focus id that can be updated
 * without the tree is a focus id that will eventually point at a pane that was
 * closed three renders ago. Every function here returns both or neither, so
 * that state is unrepresentable.
 *
 * `root: null` means no panes at all — the launchpad, not an empty window.
 */
export type Tiling = { root: Layout | null; focus: string };

export const EMPTY: Tiling = { root: null, focus: "" };

// Ids identify tree nodes for React keys and for every operation below. They
// are opaque and never meaningful across a reload, so a process-wide counter is
// enough; nothing may parse or order them.
let seq = 0;
const nextId = () => `pane-${(seq += 1)}`;

/** How small a split's `a` side may be dragged. The rest of the floor is the
 *  panes' own min sizes, which are a rendering concern, not this file's. */
const MIN_RATIO = 0.1;

/**
 * How much of the field a pane needs to be worth opening, along the axis it is
 * being squeezed on.
 *
 * A conversation and a file want most of a column; a list of file names wants
 * enough for a name and not a pixel more. These are fractions of the whole
 * window rather than of the pane something opened out of, and that difference
 * is the entire point of `makeRoom` — see it for why.
 *
 * They are fractions rather than pixels because nothing else in this file knows
 * what a pixel is. The cost is that a very small window scales the floors down
 * with it; the alternative is threading a size through every layout operation
 * to serve a case the tiling is already too small for.
 */
function roomFor(pane: Pane): number {
  if (pane.kind === "session") return 0.28;
  // A column of names, not a column of content: the file tree and the changed
  // files index are read by their left edge.
  if (pane.kind === "inspect") {
    const kind = navValue(pane.nav).kind;
    if (kind === "workspace-tree" || kind === "files") return 0.14;
  }
  return 0.3;
}

/** The least a subtree can live in, along `axis`. Children of a split that cuts
 *  the other way share their parent's whole extent here, so they take the
 *  larger of the two rather than the sum. */
function needs(node: Layout, axis: Dir): number {
  if (node.kind === "leaf") return roomFor(node.pane);
  const a = needs(node.a, axis);
  const b = needs(node.b, axis);
  return node.dir === axis ? a + b : Math.max(a, b);
}

/**
 * Gives a pane that just opened the room it needs, out of whichever neighbour
 * can spare it.
 *
 * `split` only ever subdivides its target's own rectangle, so the space for a
 * new pane comes from the pane it opened out of and from nothing else. That is
 * right for two peers and wrong for everything nested: opening the file tree
 * beside a conversation, then a file out of the tree, left the file with a
 * third of a third — the two panes on the right crammed into 34% of the window
 * while the conversation, which nobody asked to keep, held the other 66%.
 *
 * So the shares in this file are shares of the *window*, not of the pane that
 * happened to be split. This walks the splits between the root and the new leaf
 * and, at each one, moves the shortfall across from the side that has slack —
 * which is how the deficit reaches a neighbour several levels away. Only that
 * path is touched: a subtree off to the side keeps its internal ratios exactly
 * as they were dragged, and is protected from being squeezed below the sum of
 * what it holds by `needs`.
 *
 * Only the axis of the new split is fixed up. A stacked split changes nobody's
 * width, so there is nothing to settle there.
 */
function makeRoom(root: Layout, axis: Dir, opened: string): Layout {
  const fit = (node: Layout, size: number): Layout => {
    if (node.kind === "leaf") return node;
    const inA = !!findNode(node.a, opened);
    if (!inA && !findNode(node.b, opened)) return node;
    if (node.dir !== axis) {
      // Both children span the parent's whole extent along this axis, so there
      // is nothing to divide here — only the branch holding the new pane is
      // followed down.
      const a = inA ? fit(node.a, size) : node.a;
      const b = inA ? node.b : fit(node.b, size);
      return a === node.a && b === node.b ? node : { ...node, a, b };
    }

    let sa = size * node.ratio;
    let sb = size - sa;
    const wantA = needs(node.a, axis);
    const wantB = needs(node.b, axis);
    // At most one side is short — the other is the one that just gave up space
    // to it, or there is no room here and the shortfall passes further up.
    const move = Math.min(Math.max(wantA - sa, 0), Math.max(sb - wantB, 0));
    const back = Math.min(Math.max(wantB - sb, 0), Math.max(sa - wantA, 0));
    sa += move - back;
    sb -= move - back;

    const ratio = size > 0 ? Math.min(Math.max(sa / size, MIN_RATIO), 1 - MIN_RATIO) : node.ratio;
    const a = inA ? fit(node.a, sa) : node.a;
    const b = inA ? node.b : fit(node.b, sb);
    return a === node.a && b === node.b && ratio === node.ratio ? node : { ...node, ratio, a, b };
  };
  return fit(root, 1);
}

/**
 * Which way to cut a pane in two: across its longer side, so panes tend toward
 * squares instead of slivers.
 *
 * `aspect` is the field's width ÷ height in pixels — the one fact about the
 * screen this file cannot derive from its own tree, and the reason the answer
 * is a function here rather than a constant `"row"` at four call sites. A
 * caller that has no field to measure (a test, a fixture) leaves it out and
 * gets side by side, which is what this window did before any of this.
 *
 * The judgement is only about shape, so callers that know the *meaning* of the
 * two panes overrule it: a list of file names belongs beside what it opens, at
 * whatever aspect, never stacked above it.
 */
export function dirFor(tiling: Tiling, target: string, aspect = Infinity): Dir {
  if (!Number.isFinite(aspect) || aspect <= 0) return "row";
  const placed = frames(tiling).panes.find(({ leaf }) => leaf.id === target);
  if (!placed) return "row";
  return placed.rect.width * aspect >= placed.rect.height ? "row" : "col";
}

export function leafOf(pane: Pane): Leaf {
  return { kind: "leaf", id: nextId(), pane };
}

/** A window showing exactly one thing. */
export function single(pane: Pane): Tiling {
  const only = leafOf(pane);
  return { root: only, focus: only.id };
}

/** Every leaf, in the order they read on screen (left/top first). */
export function panes(tiling: Tiling): Leaf[] {
  const out: Leaf[] = [];
  const walk = (node: Layout) => {
    if (node.kind === "leaf") out.push(node);
    else {
      walk(node.a);
      walk(node.b);
    }
  };
  if (tiling.root) walk(tiling.root);
  return out;
}

/** The distinct sessions with at least one pane on screen, in reading order. */
export function sessionsInView(tiling: Tiling): string[] {
  const seen = new Set<string>();
  for (const leaf of panes(tiling)) {
    const session = paneSession(leaf.pane);
    if (session) seen.add(session);
  }
  return [...seen];
}

export function findLeaf(tiling: Tiling, id: string): Leaf | null {
  const node = tiling.root && findNode(tiling.root, id);
  return node && node.kind === "leaf" ? node : null;
}

export function focused(tiling: Tiling): Leaf | null {
  return findLeaf(tiling, tiling.focus);
}

/** Moves the cursor. Unknown ids and splits are ignored rather than clearing
 *  focus: a stray click must never leave the window with no current pane. */
export function focusPane(tiling: Tiling, id: string): Tiling {
  return findLeaf(tiling, id) ? { ...tiling, focus: id } : tiling;
}

/**
 * Splits `target` in two and puts `pane` in the new half, which takes focus —
 * you asked for it, so you are looking at it.
 *
 * `ratio` divides the target's own rectangle; `makeRoom` then settles the
 * result against the window, so a pane opened deep in the tree is not left with
 * a sliver of a sliver.
 *
 * Splitting an unknown target is a no-op rather than an append: the caller
 * named a pane that is gone, and guessing where they meant instead is worse
 * than doing nothing.
 */
export function split(
  tiling: Tiling,
  target: string,
  dir: Dir,
  pane: Pane,
  ratio = 0.5,
): Tiling {
  if (!tiling.root) return single(pane);
  const added = leafOf(pane);
  const grown = mapNode(tiling.root, target, (node) => ({
    kind: "split",
    id: nextId(),
    dir,
    ratio,
    a: node,
    b: added,
  }));
  if (grown === tiling.root) return tiling;
  return { root: makeRoom(grown, dir, added.id), focus: added.id };
}

/**
 * The width a navigation list receives when it opens beside another pane.
 *
 * Halves are right when two panes hold comparable things. A file tree or the
 * conversation's changed-files index only needs enough width for a name; the
 * transcript or file it navigates needs all the rest. This is only the initial
 * split — the divider remains available for a deliberate adjustment.
 */
const SIDEBAR_SHARE = 0.34;
const MAIN_SHARE = 0.66;

/**
 * Removes a node — a leaf, or a whole subtree — and collapses the split that
 * held it into its sibling, which is what keeps the tree from filling up with
 * one-child splits.
 *
 * Focus lands on the sibling's first leaf, the way closing a window in a tiling
 * WM hands focus to whatever grew to fill the space.
 */
export function close(tiling: Tiling, id: string): Tiling {
  if (!tiling.root) return tiling;
  const heir = siblingHeir(tiling.root, id);
  const root = without(tiling.root, id);
  if (root === tiling.root) return tiling;
  if (!root) return EMPTY;
  const focus = findNode(root, tiling.focus) ? tiling.focus : (heir ?? firstLeaf(root).id);
  return { root, focus };
}

/** Closes every pane belonging to one conversation — its transcript and each
 *  diff, run and artifact opened out of it. */
export function closeSession(tiling: Tiling, session: string): Tiling {
  let out = tiling;
  for (const leaf of panes(tiling)) {
    // The browser answers `null` here and is therefore never swept up: it is
    // the window's, not this conversation's.
    if (paneSession(leaf.pane) === session) out = close(out, leaf.id);
  }
  return out;
}

/** Puts something else in a pane, keeping its place and its id. */
export function updatePane(tiling: Tiling, id: string, pane: Pane): Tiling {
  if (!tiling.root) return tiling;
  const root = mapNode(tiling.root, id, (node) =>
    node.kind === "leaf" ? { ...node, pane } : node,
  );
  return root === tiling.root ? tiling : { ...tiling, root };
}

/**
 * Brings a conversation on screen.
 *
 * If it already has a pane, that pane is simply focused — opening a second
 * transcript of the same conversation beside the first is never what was meant.
 * Otherwise it takes over the focused pane. Splitting instead would tile the
 * window down to slivers over a working day; a split is something the user asks
 * for explicitly.
 *
 * The exceptions are landing on an inspect pane or on the browser: taking
 * either over would throw away what it was showing, and in the inspect case
 * would leave its conversation without a place to look into things. Both split
 * rather than overwrite. The browser is the sharper of the two — it is the
 * window's only one, so overwriting it would mean a conversation arriving on
 * screen could silently close the page you were reading.
 */
export function show(tiling: Tiling, session: string, aspect?: number): Tiling {
  const already = panes(tiling).find(
    (leaf) => leaf.pane.kind === "session" && leaf.pane.session === session,
  );
  if (already) return { ...tiling, focus: already.id };

  const pane: Pane = { kind: "session", session };
  if (!tiling.root) return single(pane);
  const seat = focused(tiling) ?? firstLeaf(tiling.root);
  if (seat.pane.kind !== "session")
    return split(tiling, seat.id, dirFor(tiling, seat.id, aspect), pane);
  return { root: mapNode(tiling.root, seat.id, () => ({ ...seat, pane })), focus: seat.id };
}

/**
 * Brings the browser on screen, or hides it again if it is already there.
 *
 * A toggle, because the button that brings the window-level browser in is the
 * one natural gesture for taking it away — a second click on "Open the browser"
 * doing nothing visible is how this looked broken when it only focused. Hiding
 * is not closing: the native webview stays alive underneath, page and profile
 * intact, so a re-open shows the page as it was. The pane's own close menu
 * sends the page back to `about:blank` ("Exit browser") or just hides it
 * ("Hide for now"); the webview itself is only torn down by the app's exit.
 *
 * It splits off the focused pane at an even share: a page and a conversation
 * are the same order of thing, unlike a column of file names (`SIDEBAR_SHARE`).
 * Being the same order of thing is also why it takes the automatic direction —
 * beside a full-height pane, under a wide one.
 */
export function openWeb(tiling: Tiling, aspect?: number): Tiling {
  const already = panes(tiling).find((leaf) => leaf.pane.kind === "web");
  if (already) return close(tiling, already.id);
  if (!tiling.root) return single({ kind: "web" });
  const seat = focused(tiling) ?? firstLeaf(tiling.root);
  return split(tiling, seat.id, dirFor(tiling, seat.id, aspect), { kind: "web" });
}

/** The browser's pane, if the window has one open. */
export function webPane(tiling: Tiling): Leaf | null {
  return panes(tiling).find((leaf) => leaf.pane.kind === "web") ?? null;
}

/**
 * True while a pane is browsing the workspace.
 *
 * The distinction it draws is between a pane you *look at* and a pane you *look
 * things up in*. Every other inspect value is the second half of an act that
 * started elsewhere — a diff you clicked, a run you opened — and replacing one
 * with the next is exactly right, because you are done with the last one. The
 * file tree is the opposite: it is how you get to the next thing, so a tree that
 * turns into the file you picked is a tree you have to walk back to after every
 * single look.
 */
export function browsing(pane: Pane): boolean {
  return pane.kind === "inspect" && navValue(pane.nav).kind === "workspace-tree";
}

/** The pane browsing `session`'s workspace, if one is open.
 *
 *  "Browsing" here is the *file tree*, and predates the web browser by a long
 *  way — see `webPane` for that one. The names sit close together; the values
 *  do not. */
export function browserPane(tiling: Tiling, session: string): Leaf | null {
  return (
    panes(tiling).find(
      (leaf) => paneSession(leaf.pane) === session && browsing(leaf.pane),
    ) ?? null
  );
}

/**
 * Shows something belonging to `session`, beside the pane it was opened from.
 *
 * One inspect pane per conversation, reused. Opening a diff, then a file, then
 * a sub-agent's run must not tile the window into four slivers — they are the
 * same act of looking, so they share a pane and stack into its history. Only
 * the first one splits.
 *
 * The workspace browser is the one pane never reused this way (`browsing`), so
 * picking a file out of the tree lands beside the tree rather than on top of it,
 * and the second pick reuses the pane the first one made. That is the whole of
 * "click swaps the file, the tree stays" — no tab strip, no preview slot, just
 * one pane that is a list and one pane that is a file.
 */
export function openInspect(
  tiling: Tiling,
  from: string,
  session: string,
  value: Inspect,
  aspect?: number,
): Tiling {
  const existing = panes(tiling).find(
    (leaf) =>
      leaf.pane.kind === "inspect" && leaf.pane.session === session && !browsing(leaf.pane),
  );
  if (existing && existing.pane.kind === "inspect") {
    const grown = updatePane(tiling, existing.id, {
      ...existing.pane,
      nav: navOpen(existing.pane.nav, value),
    });
    return { ...grown, focus: existing.id };
  }
  return openAside(tiling, from, session, value, aspect);
}

/**
 * The same, but always in a pane of its own.
 *
 * This is how a second file joins the first rather than replacing it. It is a
 * separate function instead of a flag because it is a separate decision the
 * person made — `openInspect` is "show me this", and this is "show me this *as
 * well*", which is the same distinction `show` and `showBeside` already draw for
 * conversations.
 */
export function openAside(
  tiling: Tiling,
  from: string,
  session: string,
  value: Inspect,
  aspect?: number,
): Tiling {
  const leaf = findLeaf(tiling, from);
  const list = value.kind === "files" || value.kind === "workspace-tree";
  const share = leaf && browsing(leaf.pane) ? SIDEBAR_SHARE : list ? MAIN_SHARE : undefined;
  // A navigation list and what it navigates are a row whatever the shape of the
  // pane: a column of names stacked above a file is a column of names with its
  // width wasted and its length cut off. Everything else is two comparable
  // things, so it takes the automatic direction.
  const dir = list || (leaf && browsing(leaf.pane)) ? "row" : dirFor(tiling, from, aspect);
  return split(tiling, from, dir, { kind: "inspect", session, nav: navOf(value) }, share);
}

/** Steps one inspect pane's history — back, forward, or on to something new. */
export function navigate(tiling: Tiling, id: string, step: (nav: Nav) => Nav): Tiling {
  const leaf = findLeaf(tiling, id);
  if (!leaf || leaf.pane.kind !== "inspect") return tiling;
  return updatePane(tiling, id, { ...leaf.pane, nav: step(leaf.pane.nav) });
}

/**
 * Brings a conversation on screen *beside* the current pane instead of in place
 * of it — the deliberate "put these two side by side" act, which is why it has
 * a keystroke and `show` has the rail.
 *
 * A conversation that already has a pane is focused rather than opened twice.
 */
export function showBeside(tiling: Tiling, session: string, aspect?: number): Tiling {
  const already = panes(tiling).find(
    (leaf) => leaf.pane.kind === "session" && leaf.pane.session === session,
  );
  if (already) return { ...tiling, focus: already.id };

  const seat = focused(tiling);
  if (!tiling.root || !seat) return single({ kind: "session", session });
  return split(tiling, seat.id, dirFor(tiling, seat.id, aspect), { kind: "session", session });
}

/** The split holding a node, for rotating it. The root has none. */
export function parentSplit(tiling: Tiling, id: string): string | null {
  const walk = (node: Layout): string | null => {
    if (node.kind === "leaf") return null;
    if (node.a.id === id || node.b.id === id) return node.id;
    return walk(node.a) ?? walk(node.b);
  };
  return tiling.root ? walk(tiling.root) : null;
}

/** Drag of a divider. Clamped here rather than at the handle so a restored
 *  layout cannot come back with a pane pinched to nothing. */
export function setRatio(tiling: Tiling, id: string, ratio: number): Tiling {
  if (!tiling.root) return tiling;
  const clamped = Math.min(Math.max(ratio, MIN_RATIO), 1 - MIN_RATIO);
  const root = mapNode(tiling.root, id, (node) =>
    node.kind === "split" ? { ...node, ratio: clamped } : node,
  );
  return root === tiling.root ? tiling : { ...tiling, root };
}

/** Flips a split between side-by-side and stacked, in place. */
export function rotate(tiling: Tiling, id: string): Tiling {
  if (!tiling.root) return tiling;
  const root = mapNode(tiling.root, id, (node) =>
    node.kind === "split" ? { ...node, dir: node.dir === "row" ? "col" : "row" } : node,
  );
  return root === tiling.root ? tiling : { ...tiling, root };
}

/** A box as fractions of the field, 0–1. */
export type Rect = { left: number; top: number; width: number; height: number };

export type Placed = { leaf: Leaf; rect: Rect };
/** A divider, plus the box of the split it belongs to — which is what turns a
 *  pointer position anywhere on the field back into that split's own ratio. */
export type PlacedDivider = { id: string; dir: Dir; ratio: number; within: Rect };

const WHOLE: Rect = { left: 0, top: 0, width: 1, height: 1 };

/**
 * The tree flattened into boxes: every pane and every divider, positioned.
 *
 * This exists so the renderer can draw one flat list instead of nesting
 * containers, and that is a correctness requirement rather than a preference.
 * Nesting means a pane's depth in the DOM changes whenever the tree around it
 * does — splitting once wraps the existing leaf in a new node — and React
 * unmounts and rebuilds a subtree that moves. A rebuilt pane loses its scroll
 * position (so opening a panel yanked the conversation back to the bottom), its
 * expanded tool output, and any artifact iframe it was running.
 *
 * Flat and keyed by leaf id, a pane survives every split, close, resize and
 * rotate around it untouched.
 */
export function frames(tiling: Tiling): { panes: Placed[]; dividers: PlacedDivider[] } {
  const panes: Placed[] = [];
  const dividers: PlacedDivider[] = [];

  const walk = (node: Layout, rect: Rect) => {
    if (node.kind === "leaf") {
      panes.push({ leaf: node, rect });
      return;
    }
    dividers.push({ id: node.id, dir: node.dir, ratio: node.ratio, within: rect });
    if (node.dir === "row") {
      const width = rect.width * node.ratio;
      walk(node.a, { ...rect, width });
      walk(node.b, { ...rect, left: rect.left + width, width: rect.width - width });
    } else {
      const height = rect.height * node.ratio;
      walk(node.a, { ...rect, height });
      walk(node.b, { ...rect, top: rect.top + height, height: rect.height - height });
    }
  };

  if (tiling.root) walk(tiling.root, WHOLE);
  return { panes, dividers };
}

// ---------------------------------------------------------------- internals

/**
 * Rebuilds the tree with one node replaced.
 *
 * Returns the *same reference* when the id is not present, and that is relied
 * on: every operation above uses `root === tiling.root` to tell "nothing
 * matched" from "matched and produced an identical-looking tree". Untouched
 * subtrees keep their identity too, so React reconciliation skips them.
 */
function mapNode(node: Layout, id: string, fn: (node: Layout) => Layout): Layout {
  if (node.id === id) return fn(node);
  if (node.kind === "leaf") return node;
  const a = mapNode(node.a, id, fn);
  const b = mapNode(node.b, id, fn);
  return a === node.a && b === node.b ? node : { ...node, a, b };
}

function without(node: Layout, id: string): Layout | null {
  if (node.id === id) return null;
  if (node.kind === "leaf") return node;
  const a = without(node.a, id);
  const b = without(node.b, id);
  if (a === node.a && b === node.b) return node;
  if (!a) return b;
  if (!b) return a;
  return { ...node, a, b };
}

function findNode(node: Layout, id: string): Layout | null {
  if (node.id === id) return node;
  if (node.kind === "leaf") return null;
  return findNode(node.a, id) ?? findNode(node.b, id);
}

function firstLeaf(node: Layout): Leaf {
  return node.kind === "leaf" ? node : firstLeaf(node.a);
}

/** The leaf that should inherit focus when `id` is removed: the first one on
 *  the other side of the split holding it. */
function siblingHeir(node: Layout, id: string): string | null {
  if (node.kind === "leaf") return null;
  if (node.a.id === id) return firstLeaf(node.b).id;
  if (node.b.id === id) return firstLeaf(node.a).id;
  return siblingHeir(node.a, id) ?? siblingHeir(node.b, id);
}
