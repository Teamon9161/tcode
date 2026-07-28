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

export type ToolResult = { preview: string; content: string; isError: boolean };

export type RunMeta = {
  kind: string;
  model: string;
  prompt: string;
  summary: string;
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
  /** Typed while the turn ran, delivered at a safe boundary. */
  | { kind: "queued"; text: string; attachments: string[]; entryIndex: number }
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
    case "Compacted":
      return [...blocks, { kind: "note", text: event.data as string }];

    case "QueuedInput": {
      const data = event.data as {
        text: string;
        attachments: string[];
        entry_index: number;
      };
      return [
        ...blocks,
        {
          kind: "queued",
          text: data.text,
          attachments: data.attachments ?? [],
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
      };
      return updateCall(blocks, data.call_id, (call) => ({
        ...call,
        result: { preview: data.preview, content: data.content, isError: data.is_error },
      }));
    }

    case "TaskRunStarted": {
      const data = event.data as {
        run: string;
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
