import { createContext, useContext, useLayoutEffect } from "react";

import type { WorkspaceMode } from "./workspaceDrafts";

export type WorkspaceFileControls = {
  session: string;
  path: string;
  mode: WorkspaceMode | null;
  onMode: ((mode: WorkspaceMode) => void) | null;
  onReload: () => void;
  onSave: (() => void) | null;
  dirty: boolean;
  loading: boolean;
  saving: boolean;
  saveDisabled: boolean;
};

type Register = (controls: WorkspaceFileControls) => () => void;

export const WorkspaceFileControlsContext = createContext<Register | null>(null);

/** Register actions with the pane frame that owns this file. Layout effect is
 * intentional: identity changes, old cleanup and new registration all finish
 * before the header can paint stale actions. The registrar also guards cleanup
 * by object identity, so an old file cannot clear a newer registration. */
export function useWorkspaceFileControls(controls: WorkspaceFileControls): void {
  const register = useContext(WorkspaceFileControlsContext);
  useLayoutEffect(() => (register ? register(controls) : undefined), [register, controls]);
}

export function matchesWorkspaceFileControls(
  controls: WorkspaceFileControls | null,
  session: string,
  path: string,
): controls is WorkspaceFileControls {
  return controls?.session === session && controls.path === path;
}
