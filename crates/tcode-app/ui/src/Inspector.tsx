import { useMemo } from "react";

import { findRun, findToolCall, type Block } from "./blocks";
import { Diff } from "./components/Diff";
import { Code } from "./components/Code";
import { languageOf } from "./diff";
import { useFileHistory, type Inspect } from "./inspect";
import { relativeTo, type TouchedFile } from "./files";
import { FilesView } from "./FilePanel";
import { rich } from "./rich";
import { Sandbox } from "./Sandbox";
import { ShownView } from "./Shown";
import { BlockList } from "./Transcript";
import { displayToolOutput } from "./toolViews";

/**
 * The body of an inspect pane: one `Inspect` value, drawn.
 *
 * It dispatches on `Inspect["kind"]` and holds no state of its own — the
 * navigation stack lives in the pane (`layout.ts` + `inspect.ts`), and every
 * view below is a pure function of the transcript. That is what lets "open the
 * file this sub-agent edited" and "open the file the transcript mentioned" be
 * the same code path.
 *
 * Everything here reads from blocks rather than from disk. A review surface
 * that re-read the file would answer a different question than the one being
 * asked — what the agent did, not what happens to be there now. The single
 * exception is `shown`, and it is one for the opposite reason: that file was
 * written by a script so it would *not* have to pass through the conversation,
 * so the transcript has nothing to draw (see `Shown.tsx`).
 *
 * The frame around it — header, history buttons, close — belongs to
 * `Panes.tsx`, because it is the same frame every pane wears.
 */
export function InspectView({
  value,
  blocks,
  files,
  cwd,
  onOpen,
}: {
  value: Inspect;
  blocks: Block[];
  files: TouchedFile[];
  cwd: string;
  onOpen: (next: Inspect) => void;
}) {
  switch (value.kind) {
    case "files":
      return <FilesView files={files} cwd={cwd} onOpen={onOpen} />;
    case "file":
      return <FileView value={value} blocks={blocks} cwd={cwd} onOpen={onOpen} />;
    case "diff":
      return <DiffView callId={value.callId} blocks={blocks} cwd={cwd} />;
    case "output":
      return <OutputView callId={value.callId} blocks={blocks} />;
    case "run":
      return <RunView run={value.run} blocks={blocks} onOpen={onOpen} />;
    case "artifact":
      return <Sandbox kind={value.sandbox} source={value.source} label={value.label} />;
    case "shown":
      return <ShownView value={value} cwd={cwd} />;
    case "doc":
      return <div className="doc">{rich(value.text)}</div>;
  }
}

/**
 * A file as one call saw it.
 *
 * The snapshot is deliberate. `ToolEnd` carries the call's complete output, so
 * this shows what the model actually read — and says plainly when something has
 * changed the file since, which is the fact a fresh read would erase.
 */
function FileView({
  value,
  blocks,
  cwd,
  onOpen,
}: {
  value: Extract<Inspect, { kind: "file" }>;
  blocks: Block[];
  cwd: string;
  onOpen: (next: Inspect) => void;
}) {
  const touches = useFileHistory(blocks, value.path);

  // Without an explicit call, the newest touch that actually returned the file
  // wins — not simply the newest touch. An edit names the file but returns a
  // result about the change, so taking the last touch would show an empty panel
  // for exactly the files most worth looking at.
  const pinned = useMemo(() => {
    if (value.at) return value.at;
    for (let at = touches.length - 1; at >= 0; at -= 1) {
      const call = findToolCall(blocks, touches[at].callId);
      if (call?.result?.content) return touches[at].callId;
    }
    return touches[touches.length - 1]?.callId;
  }, [value.at, touches, blocks]);

  const call = pinned ? findToolCall(blocks, pinned) : null;
  const since = useMemo(() => {
    const at = touches.findIndex((touch) => touch.callId === pinned);
    return at === -1 ? [] : touches.slice(at + 1).filter((touch) => touch.changed);
  }, [touches, pinned]);

  const content = call?.result?.content ?? call?.result?.preview ?? null;

  return (
    <div className="inspect-file">
      <p className="inspect-path" title={value.path}>
        {relativeTo(cwd, value.path)}
      </p>

      {since.length > 0 && (
        <p className="inspect-stale">
          {since.length} later {since.length === 1 ? "change" : "changes"} to this file —{" "}
          <button className="link-btn" onClick={() => onOpen({ kind: "diff", callId: since[0].callId })}>
            see the next one
          </button>
        </p>
      )}

      {content ? (
        <Code source={content} language={languageOf(value.path)} />
      ) : (
        <p className="inspect-empty">
          {call ? "this call is still running" : "no output was recorded for this file"}
        </p>
      )}
    </div>
  );
}

function DiffView({ callId, blocks, cwd }: { callId: string; blocks: Block[]; cwd: string }) {
  const call = findToolCall(blocks, callId);
  if (!call) return <p className="inspect-empty">that call is no longer in this conversation</p>;

  return (
    <div className="inspect-diff">
      <p className="inspect-path">
        {call.name} · {relativeTo(cwd, call.summary)}
      </p>
      <Diff input={call.input} />
    </div>
  );
}

function OutputView({ callId, blocks }: { callId: string; blocks: Block[] }) {
  const call = findToolCall(blocks, callId);
  if (!call?.result) return <p className="inspect-empty">no output was recorded</p>;

  return (
    <div className="inspect-output">
      <p className="inspect-path">
        {call.name} · {call.summary}
      </p>
      <Code source={displayToolOutput(call.name, call.result.content || call.result.preview)} language="" />
    </div>
  );
}

/** A sub-agent's whole turn, drawn by the same list the main transcript uses. */
function RunView({
  run,
  blocks,
  onOpen,
}: {
  run: string;
  blocks: Block[];
  onOpen: (next: Inspect) => void;
}) {
  const found = findRun(blocks, run);
  if (!found) return <p className="inspect-empty">that run is no longer in this conversation</p>;

  return (
    <div className="inspect-run">
      <dl className="inspect-facts">
        <div>
          <dt>kind</dt>
          <dd>{found.meta.kind}</dd>
        </div>
        <div>
          <dt>model</dt>
          <dd>{found.meta.model}</dd>
        </div>
        <div>
          <dt>status</dt>
          <dd>{found.meta.status ?? "running"}</dd>
        </div>
      </dl>
      <p className="inspect-prompt">{found.meta.prompt}</p>
      <div className="inspect-transcript">
        <BlockList blocks={found.blocks} onOpen={onOpen} />
      </div>
    </div>
  );
}
