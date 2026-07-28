import { useLayoutEffect, useRef, useState } from "react";

import type { Block } from "./blocks";
import type { Inspect } from "./inspect";
import { rich } from "./rich";
import { readChanges } from "./diff";
import { useToolMeta, viewFor, displayToolOutput, displayToolSummary, transcriptGroupFor } from "./toolViews";
import { ChevronDown, ChevronRight } from "./components/Icons";
import { StatusDot } from "./components/Status";

/**
 * The conversation.
 *
 * Autoscroll follows the stream only while the user is already at the bottom.
 * Scrolling up to read something is a deliberate act, and yanking the view back
 * down on the next delta is the single most irritating thing a streaming
 * transcript can do.
 *
 * Blocks nest (a sub-agent run holds its own), so the renderer recurses. The
 * same list component draws a run's contents inside the panel, which is what
 * keeps a delegated turn from needing a second, subtly different transcript.
 */
export function Transcript({
  blocks,
  running,
  onOpen,
}: {
  blocks: Block[];
  running: boolean;
  onOpen: (value: Inspect) => void;
}) {
  const scroller = useRef<HTMLDivElement>(null);
  const pinned = useRef(true);

  // Layout effect, not effect: the scroll correction has to land in the same
  // frame as the new content, or a long delta paints once at the old offset.
  useLayoutEffect(() => {
    const node = scroller.current;
    if (node && pinned.current) node.scrollTop = node.scrollHeight;
  }, [blocks, running]);

  if (blocks.length === 0 && !running) {
    return (
      <div className="transcript is-empty">
        <FirstRun />
      </div>
    );
  }

  return (
    <div
      className="transcript"
      ref={scroller}
      onScroll={(event) => {
        const box = event.currentTarget;
        pinned.current = box.scrollHeight - box.scrollTop - box.clientHeight < 40;
      }}
    >
      <div className="transcript-inner">
        <BlockList blocks={blocks} onOpen={onOpen} />
        {running && <Working />}
      </div>
    </div>
  );
}

export function BlockList({
  blocks,
  onOpen,
  groupExploration = true,
}: {
  blocks: Block[];
  onOpen: (value: Inspect) => void;
  /** Batches are already a deliberate grouping boundary. */
  groupExploration?: boolean;
}) {
  const items = groupExploration ? groupTranscriptBlocks(blocks) : blocks.map((block) => ({ kind: "block" as const, block }));
  return (
    <>
      {items.map((item, index) =>
        item.kind === "exploration" ? (
          <ExplorationGroup key={index} blocks={item.blocks} onOpen={onOpen} />
        ) : item.kind === "changes" ? (
          <ChangesGroup key={index} blocks={item.blocks} onOpen={onOpen} />
        ) : item.kind === "commands" ? (
          <CommandsGroup key={index} blocks={item.blocks} onOpen={onOpen} />
        ) : (
          <BlockView key={index} block={item.block} onOpen={onOpen} />
        ),
      )}
    </>
  );
}

type ToolBlock = Extract<Block, { kind: "tool" }>;
type ExplorationBlock = ToolBlock | Extract<Block, { kind: "thinking" }>;
export type TranscriptItem =
  | { kind: "block"; block: Block }
  | { kind: "exploration"; blocks: ExplorationBlock[] }
  | { kind: "changes"; blocks: ToolBlock[] }
  | { kind: "commands"; blocks: ToolBlock[] };

/** Consecutive low-risk inspection calls are one trace step, not a stack of
 * cards. Thinking stays with the surrounding inspection so the live trace
 * keeps one coherent boundary; it has its own disclosure inside the group.
 *
 * Consecutive edits and commands receive their own boundaries, but only when
 * there are at least two: one call is already a complete, useful trace row. */
export function groupTranscriptBlocks(blocks: Block[]): TranscriptItem[] {
  const grouped: TranscriptItem[] = [];
  let exploration: ExplorationBlock[] = [];
  let changes: ToolBlock[] = [];
  let commands: ToolBlock[] = [];
  const flushExploration = () => {
    if (exploration.length > 0) grouped.push({ kind: "exploration", blocks: exploration });
    exploration = [];
  };
  const flushChanges = () => {
    if (changes.length === 1) grouped.push({ kind: "block", block: changes[0] });
    else if (changes.length > 1) grouped.push({ kind: "changes", blocks: changes });
    changes = [];
  };
  const flushCommands = () => {
    if (commands.length === 1) grouped.push({ kind: "block", block: commands[0] });
    else if (commands.length > 1) grouped.push({ kind: "commands", blocks: commands });
    commands = [];
  };

  for (const block of blocks) {
    const group = block.kind === "tool" ? transcriptGroupFor(block.name) : undefined;
    if (block.kind === "tool" && group === "exploration") {
      flushChanges();
      flushCommands();
      exploration.push(block);
    } else if (block.kind === "thinking" && exploration.length > 0) {
      exploration.push(block);
    } else if (block.kind === "tool" && group === "changes") {
      flushExploration();
      flushCommands();
      changes.push(block);
    } else if (block.kind === "tool" && group === "commands") {
      flushExploration();
      flushChanges();
      commands.push(block);
    } else {
      flushExploration();
      flushChanges();
      flushCommands();
      grouped.push({ kind: "block", block });
    }
  }
  flushExploration();
  flushChanges();
  flushCommands();
  return grouped;
}

function BlockView({ block, onOpen }: { block: Block; onOpen: (value: Inspect) => void }) {
  switch (block.kind) {
    case "user":
      return (
        <div className="msg msg-user">
          {block.images && block.images.length > 0 && (
            <div className="msg-images">
              {block.images.map((url, at) => (
                <img key={at} src={url} alt="pasted image" />
              ))}
            </div>
          )}
          {block.text}
        </div>
      );
    case "queued":
      return (
        <div className="msg msg-user is-queued">
          <span className="queued-tag">queued</span>
          {block.text}
        </div>
      );
    case "assistant":
      return <div className="msg msg-assistant">{rich(block.text)}</div>;
    case "thinking":
      return <Thinking text={block.text} />;
    case "note":
      return <p className="msg-note">{block.text}</p>;
    case "error":
      return (
        <p className="msg-error" role="alert">
          {block.text}
        </p>
      );
    case "tool":
      return <ToolCall block={block} onOpen={onOpen} />;
    case "batch":
      return <BatchCall block={block} onOpen={onOpen} />;
    case "run":
      return <RunCall block={block} onOpen={onOpen} />;
  }
}

/** Reasoning is collapsed by default: available, not in the way. */
function Thinking({ text }: { text: string }) {
  const [open, setOpen] = useState(false);
  return (
    <div className={`thinking${open ? " is-open" : ""}`}>
      <button className="thinking-head" onClick={() => setOpen((was) => !was)} aria-expanded={open}>
        {open ? <ChevronDown size={13} /> : <ChevronRight size={13} />}
        thinking
      </button>
      {open && <div className="thinking-body">{text}</div>}
    </div>
  );
}

function ExplorationGroup({
  blocks,
  onOpen,
}: {
  blocks: ExplorationBlock[];
  onOpen: (value: Inspect) => void;
}) {
  const [open, setOpen] = useState(false);
  const tools = blocks.filter((block): block is ToolBlock => block.kind === "tool");
  const done = tools.every((block) => block.result !== undefined);
  const failed = tools.some((block) => block.result?.isError);

  return (
    <section className={`exploration${open ? " is-open" : ""}${failed ? " is-failed" : ""}`}>
      <button
        className="exploration-head"
        onClick={() => setOpen((was) => !was)}
        aria-expanded={open}
      >
        {open ? <ChevronDown size={13} /> : <ChevronRight size={13} />}
        <span className="exploration-label">{explorationSummary(tools)}</span>
        {!done && <span className="tool-spinner" aria-label="running" />}
        {failed && <span className="tool-failed">failed</span>}
      </button>
      {open && (
        <div className="exploration-body">
          {blocks.map((block, index) =>
            block.kind === "thinking" ? (
              <div className="exploration-thinking" key={`thinking-${index}`}>
                <Thinking text={block.text} />
              </div>
            ) : (
              <ExplorationItem key={block.callId} block={block} onOpen={onOpen} />
            ),
          )}
        </div>
      )}
    </section>
  );
}

function ChangesGroup({
  blocks,
  onOpen,
}: {
  blocks: ToolBlock[];
  onOpen: (value: Inspect) => void;
}) {
  const [open, setOpen] = useState(true);
  const done = blocks.every((block) => block.result !== undefined);
  const failed = blocks.some((block) => block.result?.isError);

  return (
    <section className={`change-set${open ? " is-open" : ""}${failed ? " is-failed" : ""}`}>
      <button className="change-set-head" onClick={() => setOpen((was) => !was)} aria-expanded={open}>
        {open ? <ChevronDown size={13} /> : <ChevronRight size={13} />}
        <span className="change-set-label">{changeSetLabel(blocks)}</span>
        {!done && <span className="tool-spinner" aria-label="running" />}
        {failed && <span className="tool-failed">failed</span>}
      </button>
      {open && (
        <div className="change-set-body">
          {blocks.map((block) => (
            <ToolCall key={block.callId} block={block} onOpen={onOpen} />
          ))}
        </div>
      )}
    </section>
  );
}

export function changeSetLabel(blocks: ToolBlock[]): string {
  const paths = new Set<string>();
  for (const block of blocks) {
    for (const change of readChanges(block.input)) {
      if (change.path) paths.add(change.path);
    }
  }
  const files = paths.size || blocks.length;
  return `edit ${files} ${files === 1 ? "file" : "files"}`;
}

function CommandsGroup({
  blocks,
  onOpen,
}: {
  blocks: ToolBlock[];
  onOpen: (value: Inspect) => void;
}) {
  const [open, setOpen] = useState(false);
  const done = blocks.every((block) => block.result !== undefined);
  const failed = blocks.some((block) => block.result?.isError);
  const commandCount = blocks.length;

  return (
    <section className={`command-set${open ? " is-open" : ""}${failed ? " is-failed" : ""}`}>
      <button className="command-set-head" onClick={() => setOpen((was) => !was)} aria-expanded={open}>
        {open ? <ChevronDown size={13} /> : <ChevronRight size={13} />}
        <span className="command-set-label">
          run {commandCount} {commandCount === 1 ? "command" : "commands"}
        </span>
        {!done && <span className="tool-spinner" aria-label="running" />}
        {failed && <span className="tool-failed">failed</span>}
      </button>
      {open && (
        <div className="command-set-body">
          {blocks.map((block) => (
            <ToolCall key={block.callId} block={block} onOpen={onOpen} />
          ))}
        </div>
      )}
    </section>
  );
}

function ExplorationItem({ block, onOpen }: { block: ToolBlock; onOpen: (value: Inspect) => void }) {
  const view = viewFor(block.name);
  const target = view.inspect?.(block.input, block.callId, block.result) ?? null;
  const summary = displayToolSummary(block.name, block.summary, block.input);
  const failed = block.result?.isError ?? false;

  return (
    <div className={`exploration-item${failed ? " is-failed" : ""}`}>
      <span className="exploration-tool">{block.name}</span>
      {target ? (
        <button className="exploration-target" onClick={() => onOpen(target)} title="Open in the panel">
          {summary || "details"}
        </button>
      ) : (
        <span className="exploration-summary">{summary || "details"}</span>
      )}
      {failed && <span className="tool-failed">failed</span>}
    </div>
  );
}

function explorationSummary(blocks: ToolBlock[]): string {
  const reads = blocks.filter((block) => block.name === "read").length;
  const searches = blocks.length - reads;
  const labels: string[] = [];
  if (reads > 0) labels.push(`read ${reads} ${reads === 1 ? "file" : "files"}`);
  if (searches > 0) labels.push(`search ${searches} ${searches === 1 ? "pattern" : "patterns"}`);
  return labels.join(" · ");
}

function ToolCall({
  block,
  onOpen,
}: {
  block: Extract<Block, { kind: "tool" }>;
  onOpen: (value: Inspect) => void;
}) {
  const meta = useToolMeta(block.name);
  const view = viewFor(block.name);
  const [open, setOpen] = useState(false);

  // Core decides where a call belongs; `silent` means another surface already
  // told its story (an `ask_user` question is baked by its approval).
  if (meta.route === "silent") return null;

  const done = block.result !== undefined;
  const failed = block.result?.isError ?? false;
  const body = view.body?.(block.input) ?? null;
  const target = view.inspect?.(block.input, block.callId, block.result) ?? null;
  const summary = displayToolSummary(block.name, block.summary, block.input);
  const detail = view.detail?.(block.input) ?? null;
  const preview = block.result ? displayToolOutput(block.name, block.result.preview) : "";
  const output = block.result ? displayToolOutput(block.name, block.result.content) : "";
  const canExpand = Boolean(detail || output);

  // The transcript is an execution trace, not an output log. A successful call
  // already has a stable destination in the inspector; only failures need a
  // diagnostic in the main reading flow.
  const showResult = done && failed;

  return (
    <div className={`tool${failed ? " is-failed" : ""}${open ? " is-open" : ""}`}>
      <div className="tool-head">
        <span className="tool-name">{block.name}</span>
        <span className="tool-separator" aria-hidden>
          ·
        </span>

        {target ? (
          <button className="tool-target" onClick={() => onOpen(target)} title="Open in the panel">
            {summary || "details"}
          </button>
        ) : (
          <span className="tool-summary">{summary || "details"}</span>
        )}

        {!done && <span className="tool-spinner" aria-label="running" />}
        {failed && <span className="tool-failed">failed</span>}
        {canExpand && (
          <button
            className="tool-expand"
            onClick={() => setOpen((was) => !was)}
            aria-expanded={open}
            title={open ? "Hide details" : "Show full details"}
          >
            {open ? <ChevronDown size={13} /> : <ChevronRight size={13} />}
            <span className="tool-expand-label">details</span>
          </button>
        )}
      </div>

      {body && <div className="tool-body">{body}</div>}

      {showResult && preview && <p className="tool-preview">{preview}</p>}

      {open && (detail || output) && (
        <div className="tool-details">
          {detail && <pre className="tool-command">{detail}</pre>}
          {output && <pre className="tool-output">{output}</pre>}
        </div>
      )}
    </div>
  );
}

/** Concurrent calls under one header, which is the whole reason core announces
 *  them as a group instead of letting five identical rows stack up. */
function BatchCall({
  block,
  onOpen,
}: {
  block: Extract<Block, { kind: "batch" }>;
  onOpen: (value: Inspect) => void;
}) {
  const [open, setOpen] = useState(false);
  const done = block.blocks.every((child) => child.kind !== "tool" || child.result !== undefined);

  return (
    <div className={`batch${open ? " is-open" : ""}`}>
      <button className="batch-head" onClick={() => setOpen((was) => !was)} aria-expanded={open}>
        {open ? <ChevronDown size={13} /> : <ChevronRight size={13} />}
        <span className="batch-label">{block.label}</span>
        <span className="batch-count">{block.blocks.length}</span>
        {!done && <span className="tool-spinner" aria-label="running" />}
      </button>
      {open && (
        <div className="batch-body">
          <BlockList blocks={block.blocks} onOpen={onOpen} groupExploration={false} />
        </div>
      )}
    </div>
  );
}

/**
 * A delegated run.
 *
 * Heavier than a tool card because it is a whole conversation, and headed by
 * the objective a human wrote rather than by the tool's name — when several are
 * in flight, "what was this one for" is the only question worth answering at a
 * glance.
 */
function RunCall({
  block,
  onOpen,
}: {
  block: Extract<Block, { kind: "run" }>;
  onOpen: (value: Inspect) => void;
}) {
  const [open, setOpen] = useState(false);
  const status = block.meta.status;
  const state = !status ? "running" : status === "ok" ? "idle" : "failed";

  return (
    <div className={`run is-${state}${open ? " is-open" : ""}`}>
      <div className="run-head">
        <button className="tool-expand" onClick={() => setOpen((was) => !was)} aria-expanded={open}>
          {open ? <ChevronDown size={13} /> : <ChevronRight size={13} />}
        </button>
        <StatusDot status={state} />
        <button
          className="run-title"
          onClick={() => onOpen({ kind: "run", run: block.run })}
          title="Open this run in the panel"
        >
          {block.meta.summary || block.meta.kind}
        </button>
        <span className="run-meta">
          {block.meta.kind}
          {block.meta.model && ` · ${block.meta.model}`}
          {block.meta.toolCalls !== undefined &&
            ` · ${block.meta.toolCalls} call${block.meta.toolCalls === 1 ? "" : "s"}`}
        </span>
      </div>
      {open && (
        <div className="run-body">
          <BlockList blocks={block.blocks} onOpen={onOpen} />
          {block.blocks.length === 0 && <p className="run-waiting">starting…</p>}
        </div>
      )}
    </div>
  );
}

/** Shown while a turn is in flight. The one continuous animation in the app. */
function Working() {
  return (
    <p className="working" aria-live="polite">
      <span className="working-dot" />
      working
    </p>
  );
}

/** The empty transcript teaches the surface rather than announcing emptiness. */
function FirstRun() {
  return (
    <div className="first-run">
      <h3>Ready</h3>
      <p>
        Describe what you want done in this folder. The agent reads and edits files here, and asks
        before anything that changes them.
      </p>
      <dl className="shortcuts">
        <div>
          <dt>Enter</dt>
          <dd>send</dd>
        </div>
        <div>
          <dt>Shift + Enter</dt>
          <dd>new line</dd>
        </div>
        <div>
          <dt>Esc</dt>
          <dd>stop the turn</dd>
        </div>
      </dl>
    </div>
  );
}
