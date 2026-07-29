import { createContext, useContext } from "react";

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
};
