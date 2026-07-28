import { useLayoutEffect, useRef, useState } from "react";

import type { Block } from "./blocks";
import type { Inspect } from "./inspect";
import { rich } from "./rich";
import { useToolMeta, viewFor, displayToolSummary } from "./toolViews";
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
}: {
  blocks: Block[];
  onOpen: (value: Inspect) => void;
}) {
  return (
    <>
      {blocks.map((block, index) => (
        <BlockView key={index} block={block} onOpen={onOpen} />
      ))}
    </>
  );
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
  const canExpand = Boolean(detail || block.result?.content);

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

      {showResult && block.result?.preview && <p className="tool-preview">{block.result.preview}</p>}

      {open && (detail || block.result?.content) && (
        <div className="tool-details">
          {detail && <pre className="tool-command">{detail}</pre>}
          {block.result?.content && <pre className="tool-output">{block.result.content}</pre>}
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
          <BlockList blocks={block.blocks} onOpen={onOpen} />
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
