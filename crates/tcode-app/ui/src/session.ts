import type { Block } from "./blocks";
import type { TouchedFile } from "./files";
import type { Pasted } from "./paste";
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
};

export const BLANK: SessionState = {
  blocks: [],
  files: [],
  running: false,
  approval: null,
  failed: false,
  draft: "",
  attachments: [],
  activity: "not started",
};
