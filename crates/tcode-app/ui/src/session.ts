import { createContext, useContext } from "react";

import type { Block } from "./blocks";
import type { TouchedFile } from "./files";
import type { Pasted } from "./paste";
import type { Plan, PlanDraft } from "./plan";
import type { ApprovalRequest } from "./types";

/**
 * Everything the UI knows about one open conversation.
 *
 * This lives outside `App.tsx` because panes read it: with the window split,
 * two conversations are on screen at once and each pane looks its own state up
 * by session id. The reducers that fill it still run for every session whether
 * or not it is visible — that is the whole point of the app.
 */
export type SessionState = {
  blocks: Block[];
  files: TouchedFile[];
  running: boolean;
  approval: ApprovalRequest | null;
  failed: boolean;
  draft: string;
  /** Images pasted into the draft, not yet sent. */
  attachments: Pasted[];
  /** One line for the launchpad card: the last thing that happened. */
  activity: string;
  /** Whether the next message asks for a plan first. A property of the message
   *  about to be sent, like an attachment, so it clears when one is sent. */
  planFirst: boolean;
  /**
   * The plan this conversation is working through, as the backend read it from
   * disk. `null` when it has none — most conversations, most of the time.
   *
   * Not derived from the event stream, unlike everything else here. A plan is an
   * externally mutable file the user may edit by hand, and its `detail` is only
   * in a tool call when the model happened to resend it; the backend reads the
   * file, so the panel shows what is true rather than what was said.
   */
  plan: Plan | null;
  /** Edits and comments in flight, shared by the review dock and the plan pane
   *  so the two can never disagree about what is being approved. */
  planDraft: PlanDraft | null;
  /** Whether the strip above the composer is showing its phase list. Collapsed
   *  by default: the phase you are on is the answer most of the time. */
  planOpen: boolean;
  /** Set when a review was approved with "execute in a new session": the handoff
   *  waits for the planning turn to *end*, because the `progress` tool still has
   *  to mark the plan active before another session may adopt it. */
  handoffPending: boolean;
};

/**
 * Which conversation the subtree being drawn belongs to.
 *
 * A context rather than a prop because the things that need it are *leaves* —
 * a `show` artifact rendered at its call site has to ask the backend for the
 * file, and the transcript between it and the pane knows nothing about
 * sessions. Threading an id through `BlockList` → group → call → view would put
 * a parameter on every one of them for the benefit of one leaf.
 *
 * `Panes.tsx` provides it: a pane is exactly the boundary where "which
 * conversation" is answered, for both halves of a split.
 */
export const SessionContext = createContext<string>("");

export function useSession(): string {
  return useContext(SessionContext);
}

export const BLANK: SessionState = {
  blocks: [],
  files: [],
  running: false,
  approval: null,
  failed: false,
  draft: "",
  attachments: [],
  activity: "not started",
  planFirst: false,
  plan: null,
  planDraft: null,
  planOpen: false,
  handoffPending: false,
};
