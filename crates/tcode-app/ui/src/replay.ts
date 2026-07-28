import { applyEvent, type Block } from "./blocks";
import { applyFileEvent, type TouchedFile } from "./files";
import type { AgentEvent, LedgerEntry } from "./types";

/** The persisted ledger is authoritative after a restart; live events no longer
 * exist. Rebuild the same display model that the event stream normally feeds. */
export function replayLedger(history: LedgerEntry[]): { blocks: Block[]; files: TouchedFile[] } {
  let blocks: Block[] = [];
  let files: TouchedFile[] = [];
  const feed = (event: AgentEvent) => {
    blocks = applyEvent(blocks, event);
    files = applyFileEvent(files, event);
  };

  for (const entry of history) {
    switch (entry.kind) {
      case "user": {
        const content = blocksOf(entry.data);
        const text = content
          .filter(isText)
          .map((block) => block.text)
          .filter((text) => !text.startsWith("<tcode-status>"))
          .join("\n");
        const images = content.filter(isImage).map((block) => `data:${block.media_type};base64,${block.data}`);
        if (text || images.length > 0) {
          blocks = [...blocks, { kind: "user", text, images: images.length > 0 ? images : undefined }];
        }
        break;
      }
      case "assistant":
        for (const block of blocksOf(entry.data)) {
          if (isText(block)) feed({ type: "TextDelta", data: block.text });
          if (isThinking(block)) feed({ type: "ThinkingDelta", data: block.thinking });
          if (isToolUse(block)) {
            feed({
              type: "ToolStart",
              data: { call_id: block.id, name: block.name, summary: block.name, input: block.input },
            });
          }
        }
        break;
      case "tool_results":
        for (const block of blocksOf(entry.data)) {
          if (isToolResult(block)) {
            feed({
              type: "ToolEnd",
              data: {
                call_id: block.tool_use_id,
                name: "tool",
                preview: preview(block.content),
                content: block.content,
                is_error: block.is_error,
              },
            });
          }
        }
        break;
      case "note":
        feed({ type: "Note", data: stringOf(entry.data) });
        break;
      case "user_note": {
        const note = recordOf(entry.data);
        feed({
          type: "UserNote",
          data: { text: typeof note.text === "string" ? note.text : "", answer: note.answer === true },
        });
        break;
      }
      case "summary":
        feed({ type: "Compacted", data: stringOf(entry.data) });
        break;
      case "incomplete_assistant": {
        const incomplete = recordOf(entry.data);
        if (typeof incomplete.text === "string") feed({ type: "TextDelta", data: incomplete.text });
        if (typeof incomplete.error === "string") {
          blocks = [...blocks, { kind: "error", text: `stream failed: ${incomplete.error}` }];
        }
        break;
      }
      case "imported_tool": {
        const imported = recordOf(entry.data);
        const name = typeof imported.name === "string" ? imported.name : "imported tool";
        const callId = `imported-${blocks.length}`;
        feed({ type: "ToolStart", data: { call_id: callId, name, summary: name, input: imported.input } });
        feed({
          type: "ToolEnd",
          data: {
            call_id: callId,
            name,
            preview: preview(typeof imported.content === "string" ? imported.content : ""),
            content: typeof imported.content === "string" ? imported.content : "",
            is_error: false,
          },
        });
        break;
      }
      // Instructions are filtered by the backend. Unknown future entries stay
      // invisible rather than turning persisted data into a broken transcript.
      default:
        break;
    }
  }

  return { blocks, files };
}

type PersistedBlock = Record<string, unknown> & { type?: string };

function blocksOf(value: unknown): PersistedBlock[] {
  return Array.isArray(value) ? value.filter((block): block is PersistedBlock => typeof block === "object" && block !== null) : [];
}

function recordOf(value: unknown): Record<string, unknown> {
  return typeof value === "object" && value !== null ? value as Record<string, unknown> : {};
}

function stringOf(value: unknown): string {
  return typeof value === "string" ? value : "";
}

function isText(block: PersistedBlock): block is PersistedBlock & { text: string } {
  return block.type === "text" && typeof block.text === "string";
}

function isThinking(block: PersistedBlock): block is PersistedBlock & { thinking: string } {
  return block.type === "thinking" && typeof block.thinking === "string";
}

function isImage(block: PersistedBlock): block is PersistedBlock & { media_type: string; data: string } {
  return block.type === "image" && typeof block.media_type === "string" && typeof block.data === "string";
}

function isToolUse(block: PersistedBlock): block is PersistedBlock & { id: string; name: string; input: unknown } {
  return block.type === "tool_use" && typeof block.id === "string" && typeof block.name === "string";
}

function isToolResult(block: PersistedBlock): block is PersistedBlock & { tool_use_id: string; content: string; is_error: boolean } {
  return block.type === "tool_result" && typeof block.tool_use_id === "string" && typeof block.content === "string" && typeof block.is_error === "boolean";
}

function preview(content: string): string {
  return content.split("\n", 1)[0] || "done";
}
