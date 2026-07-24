import { useEffect, useRef, useState } from "react";
import type { Block } from "./transcript";

export function Transcript({
  blocks,
  running,
}: {
  blocks: Block[];
  running: boolean;
}) {
  const end = useRef<HTMLDivElement>(null);

  // Follow the stream, but only from the bottom: scrolling up to read
  // something must not be yanked away by the next delta.
  const [pinned, setPinned] = useState(true);
  useEffect(() => {
    if (pinned) end.current?.scrollIntoView({ block: "end" });
  }, [blocks, pinned]);

  return (
    <main
      className="transcript"
      onScroll={(event) => {
        const box = event.currentTarget;
        const distance = box.scrollHeight - box.scrollTop - box.clientHeight;
        setPinned(distance < 40);
      }}
    >
      {blocks.length === 0 && !running && (
        <p className="empty">Ask for something to get started.</p>
      )}
      {blocks.map((block, index) => (
        <BlockView key={index} block={block} />
      ))}
      {running && <div className="pulse" aria-label="running" />}
      <div ref={end} />
    </main>
  );
}

function BlockView({ block }: { block: Block }) {
  switch (block.kind) {
    case "user":
      return <div className="block user">{block.text}</div>;
    case "assistant":
      return <div className="block assistant">{block.text}</div>;
    case "thinking":
      return <div className="block thinking">{block.text}</div>;
    case "note":
      return <div className="block note">{block.text}</div>;
    case "error":
      return <div className="block error">{block.text}</div>;
    case "tool":
      return <ToolView block={block} />;
  }
}

function ToolView({ block }: { block: Extract<Block, { kind: "tool" }> }) {
  const [open, setOpen] = useState(false);
  const result = block.result;
  return (
    <div className={`block tool${result?.isError ? " failed" : ""}`}>
      <button className="tool-head" onClick={() => setOpen((was) => !was)}>
        <span className="tool-name">{block.name}</span>
        <span className="tool-summary">{block.summary}</span>
        <span className="tool-status">
          {result ? (result.isError ? "failed" : "done") : "running…"}
        </span>
      </button>
      {result && !open && result.preview && (
        <pre className="tool-preview">{result.preview}</pre>
      )}
      {result && open && <pre className="tool-preview">{result.content}</pre>}
    </div>
  );
}
