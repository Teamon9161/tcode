// The wire contract. These mirror `crates/tcode-app/src/bridge.rs`,
// `commands.rs`, `projects.rs` and `AgentEvent` in tcode-core; the Rust side
// pins the envelope shape with tests (`event_wire_tests`), so changing either
// without the other is a caught error rather than a silently dead UI.

export const AGENT_EVENT = "tcode://agent-event";
export const APPROVAL_REQUEST = "tcode://approval-request";
export const TURN_FINISHED = "tcode://turn-finished";
/** Mirrors `browser::BROWSER_NAVIGATED`. Where the browser is, reported by the
 *  webview that owns it — the address bar reads this and never sets it. */
export const BROWSER_NAVIGATED = "tcode://browser-navigated";

/** Mirrors `terminal::TERMINAL_OUTPUT`. Chunks of one terminal's output,
 *  coalesced on the backend — see `terminal.rs` for why a flood must not be one
 *  event per read. */
export const TERMINAL_OUTPUT = "tcode://terminal-output";
/** Mirrors `terminal::TERMINAL_EXIT`. The program ended; the tab does not. */
export const TERMINAL_EXIT = "tcode://terminal-exit";

/** Mirrors `browser::Navigated`. `title` is empty on the navigation itself and
 *  arrives filled in when the document sets one, which is a second event for
 *  the same page rather than a correction. */
export type Navigated = { url: string; title: string };

/**
 * A chunk of a terminal's output.
 *
 * `data` is base64, and that is not a wrapper anybody chose for convenience: a
 * PTY read lands wherever the kernel put it, routinely inside a UTF-8 sequence
 * or an escape sequence, so the bytes have to cross intact and be reassembled
 * by the emulator. Decoding to a string on either side destroys the split
 * character rather than delaying it.
 */
export type TerminalOutput = { id: string; data: string };
export type TerminalExit = { id: string; code: number };

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
  /** A concurrently-dispatched group: `[call_id, name, input]` in model order. */
  | {
      type: "ToolBatchStart";
      data: { label: string; calls: [string, string, unknown][] };
    }
  /** Typed while the turn ran, delivered into the ledger at a safe boundary. */
  | {
      type: "QueuedInput";
      data: { text: string; attachments: string[]; entry_index: number };
    }
  | { type: "UserNote"; data: { text: string; answer: boolean } }
  /** A `task` sub-agent run. Its whole event stream arrives nested inside
   *  `TaskRunEvent`, which is why the transcript reducer can recurse. */
  | {
      type: "TaskRunStarted";
      data: {
        run: string;
        parent_call: string;
        kind: string;
        model: string;
        prompt: string;
        summary: string;
      };
    }
  | { type: "TaskRunEvent"; data: { run: string; event: AgentEvent } }
  | {
      type: "TaskRunFinished";
      data: { run: string; status: string; tool_calls: number };
    }
  /** A staged permission mode took effect at a Core permission boundary. */
  | { type: "ModeChanged"; data: string }
  | { type: "AutoModePaused"; data: string }
  /** One model request's normalized token counts. `input_tokens` is the
   *  NON-cached input only — see `usage.ts` for why the two figures it feeds
   *  must not be mixed. */
  | {
      type: "Usage";
      data: {
        input_tokens: number;
        output_tokens: number;
        cache_read_tokens: number;
        cache_write_tokens: number;
      };
    }
  /** Spend inside a delegated sub-agent: it costs money, but occupies its own
   *  window rather than this conversation's. */
  | { type: "DelegatedUsage"; data: unknown }
  /** What the subscription's budget windows have left, off the response
   *  headers. Absent entirely for providers that report none. */
  | {
      type: "RateLimits";
      data: {
        primary: { used_percent: number; window_minutes: number; resets_at: number };
        secondary: {
          used_percent: number;
          window_minutes: number;
          resets_at: number;
        } | null;
      };
    }
  /** `@path` context expanded into the prompt before a message is appended. The
   *  transcript keeps the short marker; the meter counts the real snapshot. */
  | { type: "ReferencesExpanded"; data: { labels: string[]; added_tokens: number } }
  // Everything not spelled out above still arrives; the transcript ignores it
  // rather than crashing on a variant added since this file was written.
  | { type: string; data?: unknown };

export type SessionEvent = { session: string; event: AgentEvent };
/** Mirrors `bridge.rs::TurnFinished`. The context pair is the backend's
 *  authoritative reading of what the conversation now occupies — see
 *  `usage.ts::adoptContext` for why a turn boundary is where it is needed. */
export type TurnFinished = {
  session: string;
  error: string | null;
  context_tokens: number;
  context_estimated: boolean;
};

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

/** The only permission transition an ordinary approval may carry. */
export type ApprovalMode = "accept-edits";

/** A conversation open in this window. */
export type SessionInfo = {
  id: string;
  cwd: string;
  name: string;
  /** Home directory, for rendering `~/…`. */
  home: string;
};

/** Mirrors `commands.rs::WorkspaceTextView`, including the complete-file revision. */
export type WorkspaceTextView = {
  path: string;
  text: string;
  revision: string;
  bytes: number;
  truncated: boolean;
};

/** Mirrors `commands.rs::WorkspaceBinaryView`: a file the viewer draws rather
 *  than reads. Which files those are is `show.ts`'s `isBinary`, on both sides. */
export type WorkspaceBinaryView = {
  path: string;
  /** `data:<media type>;base64,…` — drawable with no asset protocol. */
  url: string;
  bytes: number;
};

/** A durable ledger entry serialized by `tcode_core::Entry`. */
export type LedgerEntry = { kind: string; data: unknown };

/** One prompt typed while a turn was running, still owed to the model.
 *  Mirrors `commands.rs::QueuedView`. */
export type Queued = {
  text: string;
  attachments: string[];
  /** Turn that owned the queue snapshot; used to reject stale stop actions. */
  turn: number | null;
};

/** What rewinding to a point would cost, asked before anything is done.
 *  Mirrors `commands.rs::RewindPreview`. */
export type RewindPreview = {
  /** The prompt, to hand back for editing. */
  text: string;
  /** Whether that era changed any files, so "roll them back too" is worth
   *  offering at all. */
  dirty: boolean;
  /** How many prompts stop existing — the part no click undoes. */
  dropped: number;
};

/** Mirrors `commands.rs::RestoredFile`. Three outcomes, not a boolean. */
export type RestoredFile = { path: string; outcome: string };

/** Mirrors `commands.rs::Rewound`: the conversation as replay will rebuild it,
 *  plus what the rewind did. */
export type Rewound = {
  session: OpenedSession;
  text: string;
  restored: RestoredFile[];
};

/**
 * The session identity plus its persisted display history, if any.
 *
 * `history` is the *human's* view and keeps everything a compaction moved out of
 * the model's window, so it must not be measured to size the context meter —
 * `context_tokens` is the backend's reading of the model-visible prompt
 * (`SessionHandle::context`), system prompt and tool schemas included.
 */
export type OpenedSession = {
  session: SessionInfo;
  history: LedgerEntry[];
  context_tokens: number;
  context_estimated: boolean;
};

/** Mirrors `commands.rs::SlashResult`. */
export type SlashResult =
  | { kind: "conversation"; opened: OpenedSession; notice: string | null }
  | { kind: "compact_started" }
  | { kind: "resume_picker"; sessions: StoredSession[] }
  | { kind: "notice"; text: string; error: boolean };

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
