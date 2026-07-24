import type { AgentEvent } from "./types";

/**
 * The transcript model.
 *
 * Streaming deltas arrive one fragment at a time, so the reducer's job is to
 * decide when a fragment *extends* the last block and when it starts a new
 * one. Everything else about rendering is the component's.
 */
export type Block =
  | { kind: "user"; text: string }
  | { kind: "assistant"; text: string }
  | { kind: "thinking"; text: string }
  | { kind: "note"; text: string }
  | { kind: "error"; text: string }
  | {
      kind: "tool";
      callId: string;
      name: string;
      summary: string;
      /** Set once the call returns; `undefined` renders as still running. */
      result?: { preview: string; content: string; isError: boolean };
    };

/** Append `text` to the last block if it is already of `kind`, else open one. */
function extend(blocks: Block[], kind: "assistant" | "thinking", text: string): Block[] {
  const last = blocks[blocks.length - 1];
  if (last && last.kind === kind) {
    return [...blocks.slice(0, -1), { ...last, text: last.text + text }];
  }
  return [...blocks, { kind, text }];
}

export function applyEvent(blocks: Block[], event: AgentEvent): Block[] {
  switch (event.type) {
    case "TextDelta":
      return extend(blocks, "assistant", event.data as string);
    case "ThinkingDelta":
      return extend(blocks, "thinking", event.data as string);
    case "Note":
      return [...blocks, { kind: "note", text: event.data as string }];
    case "Compacted":
      return [...blocks, { kind: "note", text: "history compacted" }];
    case "ToolStart": {
      const data = event.data as {
        call_id: string;
        name: string;
        summary: string;
      };
      return [
        ...blocks,
        {
          kind: "tool",
          callId: data.call_id,
          name: data.name,
          summary: data.summary,
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
      return blocks.map((block) =>
        block.kind === "tool" && block.callId === data.call_id
          ? {
              ...block,
              result: {
                preview: data.preview,
                content: data.content,
                isError: data.is_error,
              },
            }
          : block,
      );
    }
    case "Retrying": {
      const data = event.data as { attempt: number; max: number; error: string };
      return [
        ...blocks,
        {
          kind: "note",
          text: `retrying (${data.attempt}/${data.max}): ${data.error}`,
        },
      ];
    }
    case "StepLimitReached":
      return [...blocks, { kind: "note", text: "step limit reached — ask to continue" }];
    case "Interrupted":
      return [...blocks, { kind: "note", text: "interrupted" }];
    case "AwaitingUserInput":
      return [...blocks, { kind: "note", text: "waiting for your direction" }];
    default:
      // Usage, rate limits, sub-agent traces, and anything added later. They
      // are real events, just not ones this minimal transcript draws.
      return blocks;
  }
}

export function userBlock(text: string): Block {
  return { kind: "user", text };
}

export function errorBlock(text: string): Block {
  return { kind: "error", text };
}
