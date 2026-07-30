import { createContext, useContext } from "react";

import type { Block } from "./blocks";

/**
 * Going back to an earlier prompt.
 *
 * Rewinding is the one sanctioned way to shorten an append-only history, so the
 * target is always a real ledger index the backend produced — never a position
 * this side counted for itself. The transcript cannot supply one: it is replayed
 * from the whole *display* history, which includes the compacted-away era that
 * holds no valid truncation index at all (core says so at `Ledger::archived`).
 *
 * So the backend hands over its list of points and this matches them onto the
 * prompts on screen, in order, by text. The asymmetry is the safety property: a
 * prompt that fails to match gets no button, and the backend re-checks the index
 * against the same list before touching anything. The worst outcome is a missing
 * control; there is no path to truncating somewhere nobody pointed at.
 */
export type RewindTarget = { index: number; text: string; dirty: boolean };

/**
 * Which prompts can be gone back to, keyed by the block itself.
 *
 * Both lists are in conversation order, so one forward walk pairs them: each
 * target claims the next user block with the same text. A block the targets do
 * not mention — an image-only prompt, anything from before a compaction — is
 * stepped over and stays unmarked.
 *
 * Keyed by the block rather than by its position because the transcript filters
 * and groups its blocks on the way to the screen (reasoning it was asked to
 * hide, a delegating call a run stands in for), so a position here is not a
 * position there. The reducer's block objects are stable, which makes identity
 * the one key that survives every one of those passes.
 */
export function rewindPoints(blocks: Block[], targets: RewindTarget[]): Map<Block, RewindTarget> {
  const found = new Map<Block, RewindTarget>();
  let at = 0;
  for (const block of blocks) {
    if (at >= targets.length) break;
    if (block.kind !== "user") continue;
    if (block.text.trim() !== targets[at].text.trim()) continue;
    found.set(block, targets[at]);
    at += 1;
  }
  return found;
}

/**
 * The rewind affordance, published to the transcript's leaves.
 *
 * A context because the thing that needs it is a user message nested inside
 * grouping and recursion, and because a sub-agent's own transcript must *not*
 * see it: nested `BlockList`s draw blocks that are not in this map, so they get
 * no control without anyone having to remember to withhold it.
 */
export type Rewinding = {
  points: Map<Block, RewindTarget>;
  onRewind?: (target: RewindTarget) => void;
};

const NONE: Rewinding = { points: new Map() };

export const RewindContext = createContext<Rewinding>(NONE);

export function useRewinding(): Rewinding {
  return useContext(RewindContext);
}
