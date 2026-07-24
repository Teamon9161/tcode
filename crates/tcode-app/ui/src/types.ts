// The wire contract. These mirror `crates/tcode-app/src/bridge.rs`,
// `commands.rs`, `projects.rs` and `AgentEvent` in tcode-core; the Rust side
// pins the envelope shape with tests (`event_wire_tests`), so changing either
// without the other is a caught error rather than a silently dead UI.

export const AGENT_EVENT = "tcode://agent-event";
export const APPROVAL_REQUEST = "tcode://approval-request";
export const TURN_FINISHED = "tcode://turn-finished";

/** `AgentEvent`, adjacently tagged: unit variants carry no `data` at all. */
export type AgentEvent =
  | { type: "Started" }
  | { type: "TextDelta"; data: string }
  | { type: "ThinkingDelta"; data: string }
  | { type: "ToolInputDelta"; data: string }
  | { type: "Note"; data: string }
  | { type: "Compacted"; data: string }
  | { type: "Compacting" }
  | { type: "AwaitingUserInput" }
  | { type: "Interrupted" }
  | { type: "TurnEnd" }
  | {
      type: "ToolStart";
      data: { call_id: string; name: string; summary: string; input: unknown };
    }
  | {
      type: "ToolEnd";
      data: {
        call_id: string;
        name: string;
        preview: string;
        content: string;
        is_error: boolean;
      };
    }
  | {
      type: "Retrying";
      data: { attempt: number; max: number; error: string; delay_ms: number };
    }
  | { type: "StepLimitReached"; data: { max: number } }
  // Everything not spelled out above still arrives; the transcript ignores it
  // rather than crashing on a variant added since this file was written.
  | { type: string; data?: unknown };

export type SessionEvent = { session: string; event: AgentEvent };
export type TurnFinished = { session: string; error: string | null };

export type ApprovalRequest = {
  session: string;
  id: string;
  tool: string;
  summary: string;
  descriptor: string;
  is_edit: boolean;
  allows_project: boolean;
  input: unknown;
};

/** Anything the backend cannot parse is treated as a denial. */
export type Decision = "yes" | "yes-session" | "yes-project" | "no";

/** A conversation open in this window. */
export type SessionInfo = {
  id: string;
  cwd: string;
  name: string;
  /** Home directory, for rendering `~/…`. */
  home: string;
};

/** A folder tcode has held a conversation in. */
export type ProjectInfo = {
  path: string;
  name: string;
  session_count: number;
  last_active: number | null;
  /** False when the recorded folder is gone. Shown, not hidden. */
  exists: boolean;
};

/** A resumable conversation log inside a project. */
export type StoredSession = {
  id: string;
  preview: string;
  modified: number | null;
};

export type Launchpad = {
  projects: ProjectInfo[];
  /** The backend's clock, so relative times agree with the timestamps. */
  now: number;
  /** Home directory, for abbreviating paths to `~/…`. */
  home: string;
};

/** What a session is doing, for every status affordance in the app. */
export type Status = "idle" | "running" | "waiting" | "failed";

export const STATUS_LABEL: Record<Status, string> = {
  idle: "idle",
  running: "running",
  waiting: "needs you",
  failed: "failed",
};
