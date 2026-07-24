/**
 * Stand-in for `@tauri-apps/api/core` in the design preview.
 *
 * The preview exists so the interface can be looked at — every state, in a
 * browser, without a provider or a running turn. It is aliased in only when
 * `PREVIEW=1`, so the shipped bundle never contains it.
 */
import type { Launchpad, SessionInfo, StoredSession } from "../types";

const NOW = Math.floor(Date.now() / 1000);

const PROJECTS: Launchpad = {
  now: NOW,
  home: "/home/teamon",
  projects: [
    {
      path: "/home/teamon/code/rust/tcode",
      name: "tcode",
      session_count: 14,
      last_active: NOW - 60 * 12,
      exists: true,
    },
    {
      path: "/home/teamon/code/py/duck_ext",
      name: "duck_ext",
      session_count: 6,
      last_active: NOW - 60 * 60 * 3,
      exists: true,
    },
    {
      path: "/home/teamon/code/rust/pybond",
      name: "pybond",
      session_count: 21,
      last_active: NOW - 60 * 60 * 26,
      exists: true,
    },
    {
      path: "/home/teamon/scratch/old-experiment",
      name: "old-experiment",
      session_count: 2,
      last_active: NOW - 60 * 60 * 24 * 94,
      exists: false,
    },
  ],
};

const HISTORY: StoredSession[] = [
  { id: "0193f0", preview: "refactor the agent loop so retries are testable", modified: NOW - 720 },
  { id: "0193ef", preview: "why does /resume drop the last tool result?", modified: NOW - 60 * 60 * 5 },
  { id: "0193ee", preview: "add a test for the ledger compact path", modified: NOW - 60 * 60 * 30 },
];

const OPEN: SessionInfo[] = [];

export async function invoke<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  switch (command) {
    case "launchpad":
      return PROJECTS as T;
    case "project_sessions":
      return HISTORY as T;
    case "sessions":
      return OPEN as T;
    case "open_folder":
      return {
        id: "preview",
        cwd: String(args?.path ?? "/home/teamon/code/rust/tcode"),
        name: "tcode",
        home: "/home/teamon",
      } as T;
    default:
      return undefined as T;
  }
}
