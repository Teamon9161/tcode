import { useLayoutEffect, useRef, useState } from "react";

import type { Block } from "./transcript";
import { rich } from "./rich";
import { ChevronDown, ChevronRight } from "./components/Icons";

/**
 * The conversation.
 *
 * Autoscroll follows the stream only while the user is already at the bottom.
 * Scrolling up to read something is a deliberate act, and yanking the view back
 * down on the next delta is the single most irritating thing a streaming
 * transcript can do.
 */
export function Transcript({
  blocks,
  running,
  onPickFile,
}: {
  blocks: Block[];
  running: boolean;
  onPickFile: (path: string) => void;
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
        {blocks.map((block, index) => (
          <BlockView key={index} block={block} onPickFile={onPickFile} />
        ))}
        {running && <Working />}
      </div>
    </div>
  );
}

function BlockView({
  block,
  onPickFile,
}: {
  block: Block;
  onPickFile: (path: string) => void;
}) {
  switch (block.kind) {
    case "user":
      return <div className="msg msg-user">{block.text}</div>;
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
      return <ToolCall block={block} onPickFile={onPickFile} />;
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

/** A path-shaped summary, which the file tools always produce. */
function looksLikePath(summary: string): boolean {
  return /^[~/.]/.test(summary) || /\.[A-Za-z0-9]{1,6}$/.test(summary);
}

function ToolCall({
  block,
  onPickFile,
}: {
  block: Extract<Block, { kind: "tool" }>;
  onPickFile: (path: string) => void;
}) {
  const [open, setOpen] = useState(false);
  const done = block.result !== undefined;
  const failed = block.result?.isError ?? false;
  const target = looksLikePath(block.summary) ? block.summary : null;

  return (
    <div className={`tool${failed ? " is-failed" : ""}${open ? " is-open" : ""}`}>
      <div className="tool-head">
        <button
          className="tool-expand"
          onClick={() => setOpen((was) => !was)}
          aria-expanded={open}
          disabled={!done}
          title={done ? "Show the full result" : undefined}
        >
          {open ? <ChevronDown size={13} /> : <ChevronRight size={13} />}
          <span className="tool-name">{block.name}</span>
        </button>
        {target ? (
          <button
            className="tool-target"
            onClick={() => onPickFile(target)}
            title="Show this file in the side panel"
          >
            {block.summary}
          </button>
        ) : (
          <span className="tool-summary">{block.summary}</span>
        )}
        {!done && <span className="tool-spinner" aria-label="running" />}
        {failed && <span className="tool-failed">failed</span>}
      </div>
      {open && block.result && (
        <pre className="tool-output">
          {block.result.content || block.result.preview || "(no output)"}
        </pre>
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
        Describe what you want done in this folder. The agent reads and edits
        files here, and asks before anything that changes them.
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
