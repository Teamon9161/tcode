import type { AgentEvent } from "./types";

/**
 * The transcript model.
 *
 * Streaming deltas arrive one fragment at a time, so the reducer's job is to
 * decide when a fragment *extends* the last block and when it starts a new one.
 * Everything else about rendering is the component's.
 *
 * It stays a pure function of the event stream. That is what lets a resumed
 * session rebuild its whole view by replaying, with nothing extra persisted —
 * and it is why the tree below nests instead of carrying ids that point
 * sideways at each other.
 *
 * Two shapes earn the nesting:
 *
 *  - **A sub-agent run.** `TaskRunEvent` wraps a complete inner `AgentEvent`,
 *    so a run's contents are produced by calling this same reducer one level
 *    down. A run inside a run costs nothing extra; it is the same recursion.
 *  - **A tool batch.** `ToolBatchStart` announces calls that are about to run
 *    concurrently. Holding them as children is what lets five parallel reads
 *    collapse under one header instead of five identical ones.
 *
 * (Previously `transcript.ts`. Renamed because a case-insensitive filesystem
 * cannot tell it apart from `Transcript.tsx`, which made `tsc` resolve one
 * import to the other and fail the build on Windows.)
 */

export type ToolUiMetadata = { kind: "browser_tab"; id: string };

export type ToolResult = {
  preview: string;
  content: string;
  isError: boolean;
  uiMetadata?: ToolUiMetadata;
};

export type RunMeta = {
  kind: string;
  model: string;
  prompt: string;
  summary: string;
  /** The `tool_use` id of the call that spawned this run, which is what lets the
   *  transcript draw the call and its run as the one step they are. */
  parentCall: string;
  /** `TaskRunStatus`, snake_cased: `done` / `failed` / `cancelled` /
   *  `interrupted`. Absent while the run is still going. */
  status?: string;
  toolCalls?: number;
};

export type Block =
  /** `images` are `data:` URLs of what was pasted into the prompt — kept for
   *  display only; what the model got is the base64 in the request. */
  | { kind: "user"; text: string; entryIndex?: number; images?: string[] }
  | { kind: "assistant"; text: string }
  | { kind: "thinking"; text: string }
  | { kind: "note"; text: string }
  | { kind: "error"; text: string }
  /**
   * The boundary where a compaction replaced everything above with a summary.
   *
   * Its own kind rather than a note, because it is the one thing in a transcript
   * that is *about* the transcript: it marks where the conversation the model can
   * still see begins, and the summary under it is a document — the only surviving
   * account of everything before that line. A note renders one paragraph of body
   * text, so the summary either buried the boundary in prose nobody reads or the
   * boundary threw the summary away. Held together and drawn folded (the TUI
   * folds the same summary behind the same divider), the mark is always readable
   * and the document is one click away.
   */
  | { kind: "compact"; summary: string }
  | {
      kind: "tool";
      callId: string;
      name: string;
      summary: string;
      input: unknown;
      /** Set once the call returns; `undefined` renders as still running. */
      result?: ToolResult;
    }
  | { kind: "batch"; label: string; blocks: Block[] }
  | { kind: "run"; run: string; meta: RunMeta; blocks: Block[] };

/** Blocks that hold other blocks, so the walkers below stay honest as the
 *  union grows. */
type Parent = Extract<Block, { blocks: Block[] }>;

const isParent = (block: Block): block is Parent => "blocks" in block;

export function applyEvent(blocks: Block[], event: AgentEvent): Block[] {
  switch (event.type) {
    case "TextDelta":
      return extend(blocks, "assistant", event.data as string);
    case "ThinkingDelta":
      return extend(blocks, "thinking", event.data as string);

    case "Note":
      return [...blocks, { kind: "note", text: event.data as string }];
    case "UserNote": {
      const data = event.data as { text: string; answer: boolean };
      return [...blocks, { kind: "note", text: `${data.answer ? "answer" : "note"}: ${data.text}` }];
    }
    case "Compacting":
      return [...blocks, { kind: "note", text: "compacting history…" }];
    // The event's payload *is* the summary — the whole document, not a headline.
    case "Compacted":
      return [...blocks, { kind: "compact", summary: event.data as string }];

    case "QueuedInput": {
      const data = event.data as {
        text: string;
        attachments: string[];
        entry_index: number;
      };
      return [
        ...blocks,
        {
          kind: "user",
          text: data.text,
          images: data.attachments?.length ? data.attachments : undefined,
          entryIndex: data.entry_index,
        },
      ];
    }

    case "ToolBatchStart": {
      const data = event.data as { label: string; calls: [string, string, unknown][] };
      return [
        ...blocks,
        {
          kind: "batch",
          label: data.label,
          blocks: data.calls.map(([callId, name, input]) => ({
            kind: "tool" as const,
            callId,
            name,
            summary: "",
            input,
          })),
        },
      ];
    }

    case "ToolStart": {
      const data = event.data as {
        call_id: string;
        name: string;
        summary: string;
        input: unknown;
      };
      // A batched call was already announced; this only fills in its summary.
      if (findCall(blocks, data.call_id)) {
        return updateCall(blocks, data.call_id, (call) => ({
          ...call,
          summary: data.summary,
          input: data.input,
        }));
      }
      return [
        ...blocks,
        {
          kind: "tool",
          callId: data.call_id,
          name: data.name,
          summary: data.summary,
          input: data.input,
        },
      ];
    }

    case "ToolEnd": {
      const data = event.data as {
        call_id: string;
        preview: string;
        content: string;
        is_error: boolean;
        ui_metadata?: ToolUiMetadata;
      };
      return updateCall(blocks, data.call_id, (call) => ({
        ...call,
        result: {
          preview: data.preview,
          content: data.content,
          isError: data.is_error,
          uiMetadata: data.ui_metadata,
        },
      }));
    }

    case "TaskRunStarted": {
      const data = event.data as {
        run: string;
        parent_call: string;
        kind: string;
        model: string;
        prompt: string;
        summary: string;
      };
      return [
        ...blocks,
        {
          kind: "run",
          run: data.run,
          meta: {
            kind: data.kind,
            model: data.model,
            prompt: data.prompt,
            summary: data.summary,
            parentCall: data.parent_call ?? "",
          },
          blocks: [],
        },
      ];
    }

    // The inner event is a complete `AgentEvent`, so a run's contents are this
    // same function one level down. Nested runs need no extra handling.
    case "TaskRunEvent": {
      const data = event.data as { run: string; event: AgentEvent };
      return updateRun(blocks, data.run, (run) => ({
        ...run,
        blocks: applyEvent(run.blocks, data.event),
      }));
    }

    case "TaskRunFinished": {
      const data = event.data as { run: string; status: string; tool_calls: number };
      return updateRun(blocks, data.run, (run) => ({
        ...run,
        meta: { ...run.meta, status: data.status, toolCalls: data.tool_calls },
      }));
    }

    case "Retrying": {
      const data = event.data as { attempt: number; max: number; error: string };
      return [
        ...blocks,
        { kind: "note", text: `retrying (${data.attempt}/${data.max}): ${data.error}` },
      ];
    }
    case "StepLimitReached":
      return [...blocks, { kind: "note", text: "step limit reached — ask to continue" }];
    case "Interrupted":
      return [...blocks, { kind: "note", text: "interrupted" }];
    case "AwaitingUserInput":
      return [...blocks, { kind: "note", text: "waiting for your direction" }];
    case "ModeChanged":
      return [...blocks, { kind: "note", text: `permission mode → ${event.data}` }];
    case "AutoModePaused":
      return [
        ...blocks,
        { kind: "note", text: `auto mode paused: ${String(event.data ?? "")}` },
      ];

    default:
      // Usage, rate limits, references, and anything added since. Real events,
      // just not ones the conversation itself draws.
      return blocks;
  }
}

/** Append `text` to the last block if it is already of `kind`, else open one. */
function extend(blocks: Block[], kind: "assistant" | "thinking", text: string): Block[] {
  const last = blocks[blocks.length - 1];
  if (last && last.kind === kind) {
    return [...blocks.slice(0, -1), { ...last, text: last.text + text }];
  }
  return [...blocks, { kind, text }];
}

/** Call ids are provider-issued and unique, so the search can descend through
 *  batches and runs alike without needing to know which holds what. */
function findCall(blocks: Block[], callId: string): boolean {
  return blocks.some((block) => {
    if (block.kind === "tool") return block.callId === callId;
    return isParent(block) && findCall(block.blocks, callId);
  });
}

function updateCall(
  blocks: Block[],
  callId: string,
  change: (call: Extract<Block, { kind: "tool" }>) => Block,
): Block[] {
  return blocks.map((block) => {
    if (block.kind === "tool") return block.callId === callId ? change(block) : block;
    if (isParent(block)) return { ...block, blocks: updateCall(block.blocks, callId, change) };
    return block;
  });
}

function updateRun(
  blocks: Block[],
  run: string,
  change: (block: Extract<Block, { kind: "run" }>) => Block,
): Block[] {
  return blocks.map((block) => {
    if (block.kind === "run" && block.run === run) return change(block);
    if (isParent(block)) return { ...block, blocks: updateRun(block.blocks, run, change) };
    return block;
  });
}

/** Finds one run anywhere in the tree, for the inspector's run view. */
export function findRun(
  blocks: Block[],
  run: string,
): Extract<Block, { kind: "run" }> | null {
  for (const block of blocks) {
    if (block.kind === "run" && block.run === run) return block;
    if (isParent(block)) {
      const found = findRun(block.blocks, run);
      if (found) return found;
    }
  }
  return null;
}

/** Finds one call anywhere in the tree, for the inspector's diff and output
 *  views. */
export function findToolCall(
  blocks: Block[],
  callId: string,
): Extract<Block, { kind: "tool" }> | null {
  for (const block of blocks) {
    if (block.kind === "tool" && block.callId === callId) return block;
    if (isParent(block)) {
      const found = findToolCall(block.blocks, callId);
      if (found) return found;
    }
  }
  return null;
}

/**
 * Which calls in this list are a run standing beside them.
 *
 * A delegating call and the run it started are two records of one step. The run
 * carries the kind, the model, the call count, the status and its own whole
 * transcript; the call carries the report that came back. Drawn as two rows the
 * step took two lines, the first of them the redundant `agent · agent(explore)`
 * — the tool's name twice and nothing the run did not already say.
 *
 * Paired by `parent_call`, which is a fact on the wire, so nothing in the
 * transcript has to know what the delegating tool is called. A run whose parent
 * is not in this list (an older log recorded before `parent_call` existed) pairs
 * with nothing and both rows draw, which is the honest degradation: two rows
 * beats a row that vanished.
 */
export function runPairs(blocks: Block[]): {
  /** run id → the call that started it, for the report it returned. */
  report: Map<string, Extract<Block, { kind: "tool" }>>;
  /** call ids whose row the run beside them replaces. */
  superseded: Set<string>;
} {
  const calls = new Map<string, Extract<Block, { kind: "tool" }>>();
  for (const block of blocks) {
    if (block.kind === "tool") calls.set(block.callId, block);
  }
  const report = new Map<string, Extract<Block, { kind: "tool" }>>();
  const superseded = new Set<string>();
  for (const block of blocks) {
    if (block.kind !== "run") continue;
    const call = calls.get(block.meta.parentCall);
    if (!call) continue;
    report.set(block.run, call);
    superseded.add(call.callId);
  }
  return { report, superseded };
}

/**
 * A run's own steps, without the message that *is* its report.
 *
 * A sub-agent's report is not a separate artefact it composes at the end: core
 * takes the text of the final assistant entry in the run's ledger and returns
 * that as the tool result (`tcode-tools/src/agent/mod.rs`, "the report = text
 * of the final assistant entry"). So the same paragraphs arrived twice — as the
 * last thing the run said, and again under "Reported back" — and a reader had
 * to compare them word by word to discover they were one thing.
 *
 * The report is the copy that is kept, because it is the one the parent
 * conversation actually received. Matching is by text, not by position: the
 * result carries a header line for a resumable run, and a run that ended some
 * other way (cancelled, failed, an older log with no `parent_call`) has no
 * report at all, in which case every step stays exactly where it is.
 */
export function runSteps(blocks: Block[], report: string | undefined): Block[] {
  const wanted = report?.trim();
  if (!wanted) return blocks;
  const last = blocks[blocks.length - 1];
  if (!last || last.kind !== "assistant") return blocks;
  const said = last.text.trim();
  if (!said || !wanted.endsWith(said)) return blocks;
  return blocks.slice(0, -1);
}

/** What a run came back with: the result of the call that started it, found
 *  anywhere in the tree. Null for a log recorded before runs carried their
 *  parent call, and while the run is still going. */
export function reportOf(blocks: Block[], run: string): string | null {
  const found = findRun(blocks, run);
  if (!found?.meta.parentCall) return null;
  return findToolCall(blocks, found.meta.parentCall)?.result?.content || null;
}

/** Runs that have started and not yet reported a status. */
export function runningRuns(blocks: Block[]): Extract<Block, { kind: "run" }>[] {
  const out: Extract<Block, { kind: "run" }>[] = [];
  const walk = (list: Block[]) => {
    for (const block of list) {
      if (block.kind === "run") {
        if (!block.meta.status) out.push(block);
        walk(block.blocks);
      } else if (isParent(block)) {
        walk(block.blocks);
      }
    }
  };
  walk(blocks);
  return out;
}

export function userBlock(text: string, images: string[] = []): Block {
  return { kind: "user", text, images: images.length > 0 ? images : undefined };
}

export function errorBlock(text: string): Block {
  return { kind: "error", text };
}

/** Something the harness did that the conversation should record — a rewind
 *  putting files back, say. Not an error, and not the model speaking. */
export function noteBlock(text: string): Block {
  return { kind: "note", text };
}
