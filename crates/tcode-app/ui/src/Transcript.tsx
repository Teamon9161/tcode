import { memo, useLayoutEffect, useMemo, useRef, useState } from "react";

import { runPairs, runSteps, type Block, type RunMeta } from "./blocks";
import type { Inspect } from "./inspect";
import { useDisplay } from "./display";
import { RewindContext, rewindPoints, useRewinding, type RewindTarget } from "./rewind";
import { Prose } from "./Prose";
import { readChanges } from "./diff";
import { isPlanSubmission } from "./plan";
import { MOD } from "./keys";
import {
  useToolMeta,
  useToolName,
  viewFor,
  inspectFor,
  displayToolOutput,
  displayToolSummary,
  transcriptGroupFor,
} from "./toolViews";
import { ChevronDown, ChevronRight, PanelIcon, RewindIcon } from "./components/Icons";
import { StatusDot } from "./components/Status";
import type { Status } from "./types";

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
 *
 * **Memoized, and everything above it is arranged so that it can be.** This is
 * far and away the most expensive thing the window draws — a hundred turns is
 * some tens of milliseconds of markdown, grammars and reconciliation — and
 * almost nothing that happens in this app is about it. A keystroke in the
 * composer, a divider moving, another conversation streaming: all of them used
 * to redraw every message here, because the window holds one state object and
 * this sat underneath it. The comparison is only worth anything while its props
 * hold still, which is why `App` keeps its handlers constant and `Panes` binds
 * these two per pane rather than in the JSX.
 */
export const Transcript = memo(function Transcript({
  blocks,
  running,
  rewindTargets,
  onOpen,
  onRewind,
}: {
  blocks: Block[];
  running: boolean;
  /** Where this conversation can go back to, from the backend. Empty while a
   *  turn holds the session, which is also when rewinding is refused. */
  rewindTargets?: RewindTarget[];
  onOpen: (value: Inspect) => void;
  onRewind?: (target: RewindTarget) => void;
}) {
  const scroller = useRef<HTMLDivElement>(null);
  const pinned = useRef(true);

  // Matched here rather than per row: one walk over the conversation, and the
  // map keys on the block itself so grouping and filtering below cannot move it.
  const rewinding = useMemo(
    () => ({ points: rewindPoints(blocks, rewindTargets ?? []), onRewind }),
    [blocks, rewindTargets, onRewind],
  );

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
        pinned.current = isAtBottom(event.currentTarget);
      }}
      // React can commit the next stream delta before the browser dispatches
      // the following scroll event. Mark an upward wheel motion immediately so
      // that commit cannot pull the reader back to the bottom mid-scroll.
      onWheel={(event) => {
        if (event.deltaY < 0) pinned.current = false;
      }}
    >
      <div className="transcript-inner">
        <RewindContext.Provider value={rewinding}>
          <BlockList blocks={blocks} onOpen={onOpen} />
        </RewindContext.Provider>
      </div>
    </div>
  );
});

export function isAtBottom({
  scrollHeight,
  scrollTop,
  clientHeight,
}: Pick<HTMLElement, "scrollHeight" | "scrollTop" | "clientHeight">): boolean {
  return scrollHeight - scrollTop - clientHeight < 40;
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
  const { thinking } = useDisplay();
  const pairs = useMemo(() => runPairs(blocks), [blocks]);

  // Two things leave the list before anything is grouped, so grouping stays one
  // rule about steps: reasoning nobody asked to see, and the delegating call a
  // run beside it already accounts for.
  const shown = useMemo(
    () =>
      blocks.filter(
        (block) =>
          !(block.kind === "thinking" && !thinking) &&
          !(block.kind === "tool" && pairs.superseded.has(block.callId)),
      ),
    [blocks, thinking, pairs],
  );

  const items = useMemo(
    () =>
      groupExploration
        ? groupTranscriptBlocks(shown)
        : shown.map((block, at) => ({ kind: "block" as const, block, at })),
    [shown, groupExploration],
  );

  return (
    <>
      {items.map((item) =>
        item.kind === "exploration" ? (
          <ExplorationGroup key={keyOf(item)} blocks={item.blocks} onOpen={onOpen} />
        ) : item.kind === "changes" ? (
          <ChangesGroup key={keyOf(item)} blocks={item.blocks} onOpen={onOpen} />
        ) : item.kind === "commands" ? (
          <CommandsGroup key={keyOf(item)} blocks={item.blocks} onOpen={onOpen} />
        ) : item.block.kind === "run" ? (
          <RunCall
            key={keyOf(item)}
            block={item.block}
            report={pairs.report.get(item.block.run)}
            onOpen={onOpen}
          />
        ) : (
          <BlockView key={keyOf(item)} block={item.block} onOpen={onOpen} />
        ),
      )}
    </>
  );
}

/**
 * A key that survives the list around it changing shape.
 *
 * Keyed by position in the *list of items* — which is what an index key does —
 * a step's identity moved every time a group formed: the moment a second edit
 * arrives, one item becomes two blocks' worth of one item and everything after
 * it shifts up a slot. React then unmounts and rebuilds every step below the
 * change, which during a burst of edits is most of the conversation, repeatedly,
 * and takes every open disclosure with it.
 *
 * Keyed by where the step *starts* in the conversation, only the item that
 * genuinely changed kind gets a new key. Its neighbours keep theirs, because a
 * conversation is appended to and their starting points do not move.
 */
function keyOf(item: TranscriptItem): string {
  return `${item.kind}:${item.at}`;
}

type ToolBlock = Extract<Block, { kind: "tool" }>;
/** `at` is where this step begins in the list it was grouped from — its
 *  identity across renders, see `keyOf`. */
export type TranscriptItem =
  | { kind: "block"; block: Block; at: number }
  | { kind: "exploration"; blocks: ToolBlock[]; at: number }
  | { kind: "changes"; blocks: ToolBlock[]; at: number }
  | { kind: "commands"; blocks: ToolBlock[]; at: number };

/** Consecutive low-risk inspection calls are one trace step, not a stack of
 * cards.
 *
 * Consecutive edits and commands receive their own boundaries, but only when
 * there are at least two: one call is already a complete, useful trace row.
 *
 * Reasoning used to be swept into the surrounding exploration group, from when
 * it was a folded row that looked like a step. It is prose now — shown as prose
 * or not at all — so it is a boundary between steps like any other prose, and a
 * group must not be able to swallow it: folded shut, "show me the reasoning"
 * would have answered by hiding it. */
export function groupTranscriptBlocks(blocks: Block[]): TranscriptItem[] {
  const grouped: TranscriptItem[] = [];
  // Where each run of like steps began, which is the resulting item's identity
  // (`keyOf`). Held alongside the blocks rather than derived afterwards: once a
  // group is closed there is nothing left that says where it started.
  let exploration: ToolBlock[] = [];
  let changes: ToolBlock[] = [];
  let commands: ToolBlock[] = [];
  let explorationAt = 0;
  let changesAt = 0;
  let commandsAt = 0;
  const flushExploration = () => {
    if (exploration.length > 0) {
      grouped.push({ kind: "exploration", blocks: exploration, at: explorationAt });
    }
    exploration = [];
  };
  const flushChanges = () => {
    if (changes.length === 1) grouped.push({ kind: "block", block: changes[0], at: changesAt });
    else if (changes.length > 1) grouped.push({ kind: "changes", blocks: changes, at: changesAt });
    changes = [];
  };
  const flushCommands = () => {
    if (commands.length === 1) grouped.push({ kind: "block", block: commands[0], at: commandsAt });
    else if (commands.length > 1) {
      grouped.push({ kind: "commands", blocks: commands, at: commandsAt });
    }
    commands = [];
  };

  blocks.forEach((block, at) => {
    const group = block.kind === "tool" ? transcriptGroupFor(block.name) : undefined;
    if (block.kind === "tool" && group === "exploration") {
      flushChanges();
      flushCommands();
      if (exploration.length === 0) explorationAt = at;
      exploration.push(block);
    } else if (block.kind === "tool" && group === "changes") {
      flushExploration();
      flushCommands();
      if (changes.length === 0) changesAt = at;
      changes.push(block);
    } else if (block.kind === "tool" && group === "commands") {
      flushExploration();
      flushChanges();
      if (commands.length === 0) commandsAt = at;
      commands.push(block);
    } else {
      flushExploration();
      flushChanges();
      flushCommands();
      grouped.push({ kind: "block", block, at });
    }
  });
  flushExploration();
  flushChanges();
  flushCommands();
  return grouped;
}

/**
 * One thing that happened, drawn.
 *
 * Memoized because a conversation is appended to: a streaming delta replaces
 * the block it is extending and leaves every earlier one exactly as it was, so
 * the identity check here is the difference between redrawing the last message
 * and redrawing all of them, several times a second, for the length of a turn.
 * `blocks.ts` is what makes it true — its reducers rebuild the list and reuse
 * the blocks — so a change there that starts copying blocks on the way past
 * silently costs this.
 */
const BlockView = memo(function BlockView({
  block,
  onOpen,
}: {
  block: Block;
  onOpen: (value: Inspect) => void;
}) {
  switch (block.kind) {
    case "user":
      return <UserMessage block={block} onOpen={onOpen} />;
    case "assistant":
      return <Prose className="msg msg-assistant" text={block.text} />;
    case "thinking":
      return <Thinking text={block.text} />;
    case "note":
      return <HarnessNote text={block.text} />;
    case "compact":
      return <CompactMark summary={block.summary} />;
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
    // Reached only for a run whose parent call is not in the same list — the
    // paired case is drawn by `BlockList`, which is where the pairing is known.
    case "run":
      return <RunCall block={block} onOpen={onOpen} />;
  }
});

/**
 * Images that rode with a prompt, at a size that says which one it was.
 *
 * The full size is a pane, not a lightbox and not this element grown: a
 * thumbnail answers "which image was that" and nothing else, and enlarging
 * things is what panes are for in this app — an image beside the conversation
 * that mentions it beats one covering it. The control is `PopOut` rather than
 * the thumbnail itself, because "somewhere to go" has exactly one control in
 * this transcript (AGENTS.md rule 14) and an image is a poor place to open a
 * second: a picture that is silently also a button teaches nothing about the
 * rows above it that work the same way.
 */
function Images({ urls, onOpen }: { urls: string[]; onOpen: (value: Inspect) => void }) {
  if (urls.length === 0) return null;
  const name = (at: number) => (urls.length > 1 ? `image ${at + 1}` : "image");
  return (
    <div className="msg-images">
      {urls.map((url, at) => (
        <figure key={at} className="msg-image">
          <img src={url} alt={name(at)} />
          <PopOut onOpen={() => onOpen({ kind: "image", url, label: name(at) })} />
        </figure>
      ))}
    </div>
  );
}

/**
 * Something you said, and the way back to it.
 *
 * The control is on the message because the message *is* the checkpoint: going
 * back means "start again from here", and here is a thing already on screen with
 * its own text on it. A picker listing prompts by their first line would be a
 * second copy of the conversation to read.
 *
 * It appears on hover and on keyboard focus, like every other row-level control
 * in this transcript. It does not act on the click: rewinding deletes
 * conversation and can roll files back, so what this opens is a question
 * (`RewindBar`), not the operation.
 *
 * Absent while a turn runs, and that is the backend's answer rather than a
 * guess: `rewind_targets` is empty when a turn holds the session, because
 * truncating a ledger something is still appending to is not a thing to offer
 * and then refuse.
 */
function UserMessage({
  block,
  onOpen,
}: {
  block: Extract<Block, { kind: "user" }>;
  onOpen: (value: Inspect) => void;
}) {
  const { points, onRewind } = useRewinding();
  const target = points.get(block);

  return (
    <div className="msg-turn">
      <div className="msg msg-user">
        <Images urls={block.images ?? []} onOpen={onOpen} />
        {block.text}
      </div>
      {target && onRewind && (
        <button
          type="button"
          className="msg-rewind"
          onClick={() => onRewind(target)}
          title="Go back to here — everything after this is dropped"
          aria-label="Go back to this message"
        >
          <RewindIcon size={12} />
          back to here
        </button>
      )}
    </div>
  );
}

/**
 * Reasoning, when the reader has asked for it (`display.ts`).
 *
 * Prose in the column, not a row in the trace. Folded behind a chevron it was
 * the worst available shape: identical to a step the agent took, so the eye had
 * to check every row in the column to find out which ones were things that
 * happened — and what it hid was the one kind of content nobody needs a
 * disclosure for, because either you are reading the reasoning or you have it
 * switched off.
 *
 * Deliberately not through `rich`. Reasoning is a draft — half-finished
 * sentences, stray backticks, sometimes a fenced block that never closes — and
 * rendering it as a document promises an editorial pass that is not there.
 * `pre-wrap` keeps the line structure the model actually produced, which is the
 * whole reason this is legible at all.
 */
function Thinking({ text }: { text: string }) {
  return (
    <div className="thinking">
      <span className="thinking-tag">thinking</span>
      <div className="thinking-body">{text}</div>
    </div>
  );
}

/**
 * Where a compaction cut the conversation, and the summary it left behind.
 *
 * Deliberately *not* a `TraceGroup`. That component is the vocabulary for one
 * step the agent took (AGENTS.md rule 9e), and this is not a step — it is a
 * boundary across the whole conversation, marking where what the model can still
 * see begins. Drawn as one: a rule spanning the column with the label sitting in
 * it, which is the same shape the TUI draws and the same shape any reader already
 * knows means "time passed here".
 *
 * Folded by default because of what the two halves are worth. The mark answers
 * the question people actually have — *why does the model not remember that* —
 * and answers it at a glance. The summary is a document, and the longest one in
 * the transcript: unfolded by default it would push the conversation off screen
 * at the exact moment the context was being reclaimed. It goes through `rich`
 * like any other model-authored prose (rule 10); it is a summary of a
 * conversation that contained file contents and fetched pages, so it is data.
 */
/**
 * Something the harness is telling both of you: a monitor fired, a background
 * command exited, a dispatched run came back.
 *
 * Folded to its first line, because the note is written to the model and the
 * rest of it is addressed to the model — the command it re-runs, the log path it
 * reads with an offset, the hint about redirection. Printing all of that in the
 * conversation put a wall of `cd … && rm -f …` where a sentence belonged, and
 * the one fact a person wanted from it ("the watch fired") was the first six
 * words. Core keeps its side of this: every harness note's first line is a
 * standalone headline (`background.rs`), so this is a fold, not a parse.
 *
 * The detail is verbatim in a `<pre>` rather than prose — it is machine text,
 * and a path that got re-wrapped as a paragraph is a path nobody can use.
 */
function HarnessNote({ text }: { text: string }) {
  const [open, setOpen] = useState(false);
  const split = text.indexOf("\n");
  // The trailing colon introduces the lines that follow, and folded there is
  // nothing for it to introduce — a sentence left mid-promise.
  const headline = (split === -1 ? text : text.slice(0, split)).replace(/:\s*$/, "");
  const detail = split === -1 ? "" : text.slice(split + 1).trim();
  return (
    <section className={`note${open ? " is-open" : ""}`}>
      <button
        type="button"
        className="note-line"
        onClick={() => detail && setOpen((was) => !was)}
        aria-expanded={detail ? open : undefined}
        disabled={!detail}
      >
        <span className="note-mark" aria-hidden="true" />
        <span className="note-headline">{headline}</span>
        {detail && (open ? <ChevronDown size={12} /> : <ChevronRight size={12} />)}
      </button>
      {open && detail && <pre className="note-detail">{detail}</pre>}
    </section>
  );
}

function CompactMark({ summary }: { summary: string }) {
  const [open, setOpen] = useState(false);
  const has = summary.trim().length > 0;
  return (
    <section className={`compact${open ? " is-open" : ""}`}>
      <button
        type="button"
        className="compact-mark"
        onClick={() => has && setOpen((was) => !was)}
        aria-expanded={has ? open : undefined}
        disabled={!has}
        title={has ? "The summary that replaced the earlier conversation" : undefined}
      >
        <span className="compact-rule" aria-hidden="true" />
        <span className="compact-label">
          earlier conversation compacted
          {has && (open ? <ChevronDown size={12} /> : <ChevronRight size={12} />)}
        </span>
        <span className="compact-rule" aria-hidden="true" />
      </button>
      {open && has && <Prose className="compact-body" text={summary} />}
    </section>
  );
}

/**
 * One step in the trace, collapsible.
 *
 * Every grouping in the transcript is this component: consecutive reads,
 * consecutive edits, consecutive commands, a concurrent batch. They used to be
 * four near-identical components with four class namespaces, which is why the
 * same tool could look like a bordered card or a bare line depending only on
 * whether it happened to have a neighbour of its own kind.
 *
 * A row, not a card. The transcript is a list of steps, and DESIGN.md already
 * says lists are rows — cards do not nest, and this one has to: a group holds
 * calls, a call holds output, a delegated run holds a whole transcript. Rows
 * nest by indentation, for as deep as it goes. There is no rule between them
 * either; the rhythm of the column does that work.
 */
function TraceGroup({
  label,
  defaultOpen = false,
  running = false,
  failed = false,
  count,
  children,
}: {
  label: string;
  defaultOpen?: boolean;
  running?: boolean;
  failed?: boolean;
  count?: number;
  children: React.ReactNode;
}) {
  const [open, setOpen] = useState(defaultOpen);
  return (
    <section
      className={`trace${open ? " is-open" : ""}${failed ? " is-failed" : ""}${
        running ? " is-running" : ""
      }`}
    >
      <button className="trace-head" onClick={() => setOpen((was) => !was)} aria-expanded={open}>
        <span className="trace-label">{label}</span>
        {count !== undefined && <span className="trace-count">{count}</span>}
        {running && <span className="tool-spinner" aria-label="running" />}
        {failed && <span className="tool-failed">failed</span>}
        {/* At the end and dim until pointed at, like every other row's
            disclosure. Leading, it pushed this row's label one glyph right of
            every single call's name, so the column of steps had two left edges
            depending on whether a step happened to have neighbours of its own
            kind — the exact raggedness one row shape was meant to remove. The
            whole row is still the target; this is the hint, not the handle. */}
        <span className="trace-chevron" aria-hidden="true">
          {open ? <ChevronDown size={13} /> : <ChevronRight size={13} />}
        </span>
      </button>
      {open && <div className="trace-body">{children}</div>}
    </section>
  );
}

function ExplorationGroup({
  blocks,
  onOpen,
}: {
  blocks: ToolBlock[];
  onOpen: (value: Inspect) => void;
}) {
  return (
    <TraceGroup
      label={explorationSummary(blocks)}
      running={!blocks.every((block) => block.result !== undefined)}
      failed={blocks.some((block) => block.result?.isError)}
    >
      {blocks.map((block) => (
        <ExplorationItem key={block.callId} block={block} onOpen={onOpen} />
      ))}
    </TraceGroup>
  );
}

function ChangesGroup({
  blocks,
  onOpen,
}: {
  blocks: ToolBlock[];
  onOpen: (value: Inspect) => void;
}) {
  const { editDetails } = useDisplay();
  return (
    <TraceGroup
      label={changeSetLabel(blocks)}
      defaultOpen={editDetails}
      running={!blocks.every((block) => block.result !== undefined)}
      failed={blocks.some((block) => block.result?.isError)}
    >
      {blocks.map((block) => (
        <ToolCall key={block.callId} block={block} onOpen={onOpen} />
      ))}
    </TraceGroup>
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
  return `Edit ${files} ${files === 1 ? "file" : "files"}`;
}

function CommandsGroup({
  blocks,
  onOpen,
}: {
  blocks: ToolBlock[];
  onOpen: (value: Inspect) => void;
}) {
  return (
    <TraceGroup
      label={`Run ${blocks.length} ${blocks.length === 1 ? "command" : "commands"}`}
      running={!blocks.every((block) => block.result !== undefined)}
      failed={blocks.some((block) => block.result?.isError)}
    >
      {blocks.map((block) => (
        <ToolCall key={block.callId} block={block} onOpen={onOpen} />
      ))}
    </TraceGroup>
  );
}

function ExplorationItem({ block, onOpen }: { block: ToolBlock; onOpen: (value: Inspect) => void }) {
  const toolName = useToolName();
  const failed = block.result?.isError ?? false;
  const target = inspectFor(block.name, failed)?.(block.input, block.callId, block.result) ?? null;
  const summary = displayToolSummary(block.name, block.summary, block.input);

  return (
    <div className={`exploration-item${failed ? " is-failed" : ""}`}>
      <span className="exploration-tool">{toolName(block.name)}</span>
      <span className="exploration-summary">{summary || "details"}</span>
      {failed && <span className="tool-failed">failed</span>}
      {target && <PopOut onOpen={() => onOpen(target)} />}
    </div>
  );
}

/**
 * "Give this its own pane."
 *
 * One affordance, in one place, on every row that has somewhere to go. What it
 * replaces was the summary itself: the path was a button that underlined on
 * hover and moved the thing you were reading into a pane at the side. Two
 * problems with that, and the second is the real one — a link is a promise of
 * navigation, but the target here is *the same content, bigger*, and a control
 * that only appears when the pointer is already on it is a control nobody finds
 * on purpose. A magnifier at the end of the row says what it does and stays put.
 */
function PopOut({ onOpen }: { onOpen: () => void }) {
  return (
    <button className="pop-out" onClick={onOpen} title="Open in its own pane" aria-label="Open in its own pane">
      <PanelIcon size={12} />
    </button>
  );
}

/** Our words, and capitalized like core's own batch labels ("Read 15 files").
 *  Two casings for the same phrase used to sit in one column, because one string
 *  came from `batch_label` and the other from here. */
function explorationSummary(blocks: ToolBlock[]): string {
  const reads = blocks.filter((block) => block.name === "read").length;
  const searches = blocks.length - reads;
  const labels: string[] = [];
  if (reads > 0) labels.push(`Read ${reads} ${reads === 1 ? "file" : "files"}`);
  if (searches > 0) labels.push(`Search ${searches} ${searches === 1 ? "pattern" : "patterns"}`);
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
  const toolName = useToolName();
  // Nothing here opens itself. The `editDetails` switch opens the *group*, so
  // the diffs are on screen; it never meant "and unfold the text under each one
  // as well", which is what it had come to do.
  const [open, setOpen] = useState(false);

  // Core decides where a call belongs; `silent` means another surface already
  // told its story (an `ask_user` question is baked by its approval).
  if (meta.route === "silent") return null;
  // `progress` is the one tool that answers this per call (`Tool::route`): a
  // phase flip belongs to the strip above the composer, while a plan submitted
  // for approval is a document this conversation has to keep. The backend sends
  // the tool's default route, so the exception is recognized from the call.
  if (meta.route === "progress" && !isPlanSubmission(block.input)) return null;

  const done = block.result !== undefined;
  const failed = block.result?.isError ?? false;
  // A call that failed changed nothing, so it draws no change. The diff a
  // rejected edit renders is the most convincing lie in the transcript: red and
  // green lines are how this app says "this is what happened to the file", and
  // here nothing happened to the file at all. The row says `failed` and the
  // error says why; the intended change is still one click away in its own pane.
  const body = failed ? null : (view.body?.(block.input) ?? null);
  const target = inspectFor(block.name, failed)?.(block.input, block.callId, block.result) ?? null;
  const summary = displayToolSummary(block.name, block.summary, block.input);
  const detail = view.detail?.(block.input) ?? null;
  const preview = block.result ? displayToolOutput(block.name, block.result.preview) : "";
  const output = block.result ? displayToolOutput(block.name, block.result.content) : "";
  // The transcript is an execution trace, not an output log. A successful call
  // already has a stable destination in the inspector; only failures need a
  // diagnostic in the main reading flow.
  //
  // And a failure needs exactly one. `preview` is the result's first line and an
  // error is usually one line, so a failed call printed its message twice —
  // once in the flow and again, identically, inside the disclosure below it.
  // Output belongs to a call that produced some; a failed one produced a reason,
  // which is already on screen.
  const showResult = done && failed;
  // And a tool whose body already drew the change says nothing more when it
  // works. `edit` returns "edited <path> (1 replacement). Result:" followed by
  // numbered lines — written so the *model* need not re-read the file — and
  // under a diff that is the same change again in a worse notation, for a
  // reader who has already seen it in red and green. The backend has published
  // this judgement as `hide_success_result` since the meta existed and the TUI
  // has honoured it (`view.rs::result_render`); this side simply never read it.
  const shownOutput = failed || meta.hide_success_result ? "" : output;
  const canExpand = Boolean(detail || shownOutput);

  const label = toolName(block.name);

  return (
    <div
      className={`tool${failed ? " is-failed" : ""}${open ? " is-open" : ""}${
        done ? "" : " is-running"
      }`}
    >
      <div className="tool-head">
        <span className="tool-name">{label}</span>

        {/* A tool whose summary is its own name has nothing to say after the
            separator, and `update_progress · update_progress` is not a caption.
            Drop both rather than printing the word twice. */}
        {summary && summary !== block.name && summary !== label && (
          <>
            <span className="tool-separator" aria-hidden>
              ·
            </span>
            <span className="tool-summary">{summary}</span>
          </>
        )}

        {!done && <span className="tool-spinner" aria-label="running" />}
        {failed && <span className="tool-failed">failed</span>}
        {target && <PopOut onOpen={() => onOpen(target)} />}
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

      {showResult && preview && <p className="tool-error">{preview}</p>}

      {open && (detail || shownOutput) && (
        <div className="tool-details">
          {detail && <pre className="tool-command">{detail}</pre>}
          {shownOutput && <pre className="tool-output">{shownOutput}</pre>}
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
  const { editDetails } = useDisplay();
  const done = block.blocks.every((child) => child.kind !== "tool" || child.result !== undefined);
  const hasChanges = block.blocks.some(
    (child) => child.kind === "tool" && transcriptGroupFor(child.name) === "changes",
  );

  return (
    <TraceGroup
      label={block.label}
      count={block.blocks.length}
      defaultOpen={editDetails && hasChanges}
      running={!done}
    >
      <BlockList blocks={block.blocks} onOpen={onOpen} groupExploration={false} />
    </TraceGroup>
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
const RunCall = memo(function RunCall({
  block,
  report,
  onOpen,
}: {
  block: Extract<Block, { kind: "run" }>;
  /** The delegating call, whose result is the report this run came back with.
   *  Absent for a log recorded before runs carried their parent call. */
  report?: ToolBlock;
  onOpen: (value: Inspect) => void;
}) {
  const [open, setOpen] = useState(false);
  const status = block.meta.status;
  const state = runState(status);
  const kind = agentKind(block.meta.kind);
  const steps = useMemo(
    () => runSteps(block.blocks, report?.result?.content),
    [block.blocks, report],
  );

  return (
    <div className={`run is-${state}${open ? " is-open" : ""}`}>
      <div className="run-head">
        <StatusDot status={state} />
        <span className="run-kind">{kind}</span>
        <span className="run-title">{block.meta.summary || block.meta.prompt.split("\n", 1)[0]}</span>
        <span className="run-meta">
          {block.meta.model}
          {block.meta.toolCalls !== undefined &&
            ` · ${block.meta.toolCalls} call${block.meta.toolCalls === 1 ? "" : "s"}`}
        </span>
        {/* Named only when it is not the ordinary ending. The dot carries
            running / done / failed; `cancelled` and `interrupted` are neither a
            success nor an error and have no glyph of their own, so they say so
            in words rather than borrowing one. */}
        {status && status !== "done" && status !== "failed" && (
          <span className="run-status">{status}</span>
        )}
        <PopOut onOpen={() => onOpen(runInspect(block.run, block.meta, kind))} />
        <button
          className="tool-expand"
          onClick={() => setOpen((was) => !was)}
          aria-expanded={open}
          title={open ? "Hide this run" : "Show what this run did"}
        >
          {open ? <ChevronDown size={13} /> : <ChevronRight size={13} />}
          <span className="tool-expand-label">this run</span>
        </button>
      </div>
      {open && (
        <div className="run-body">
          {/* The run's last message and its report are the same text (see
              `runSteps`), so the steps stop short of it and the report below
              is the single copy. */}
          <BlockList blocks={steps} onOpen={onOpen} />
          {block.blocks.length === 0 && <p className="run-waiting">starting…</p>}
          {report?.result?.content && (
            <Prose className="run-report" text={report.result.content} />
          )}
        </div>
      )}
    </div>
  );
});

/**
 * `TaskRunStatus` (`tcode_core::task_trace`), snake_cased on the wire.
 *
 * It was compared against `"ok"`, which no status has ever been, so every
 * sub-agent that finished perfectly wore the failure cross — and the preview
 * fixture said `"ok"` too, which is why looking at it never showed the bug.
 * `cancelled` and `interrupted` are not failures either: the work stopped, it did
 * not go wrong, and the word beside the dot is what distinguishes them.
 */
export function runState(status: string | undefined): Status {
  if (!status || status === "running") return "running";
  return status === "failed" ? "failed" : "idle";
}

/** A sub-agent's kind is a config key (`explore`, `general`), not a tool name,
 *  so it gets its own capitalization rather than the tool table's — the same
 *  thing the TUI's `title_case_tool_name` does for the same field. */
export function agentKind(kind: string): string {
  return kind ? kind.charAt(0).toUpperCase() + kind.slice(1) : "Agent";
}

/** Where this run's pop-out leads, carrying its own name: an inspect pane's
 *  header is one line of text, and "Sub-agent" told the reader nothing that
 *  distinguished it from the other four they had opened. */
export function runInspect(
  run: string,
  meta: RunMeta,
  kind: string,
): Extract<Inspect, { kind: "run" }> {
  const summary = meta.summary.trim() || meta.prompt.split("\n", 1)[0].trim();
  return { kind: "run", run, label: summary ? `${kind} · ${summary}` : kind };
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

      {/* The layout keys live here because this is the one screen with room for
          them and the one moment nobody is mid-task. A shortcut nothing ever
          mentions is a shortcut nobody uses. */}
      <dl className="shortcuts is-layout">
        <div>
          <dt>{MOD} + 1…9</dt>
          <dd>show that conversation here</dd>
        </div>
        <div>
          <dt>{MOD} + Shift + 1…9</dt>
          <dd>open it beside this one</dd>
        </div>
        <div>
          <dt>{MOD} + Alt + ← ↑ ↓ →</dt>
          <dd>move between panes</dd>
        </div>
        <div>
          <dt>{MOD} + W</dt>
          <dd>close this pane</dd>
        </div>
        <div>
          <dt>{MOD} + J</dt>
          <dd>terminals</dd>
        </div>
      </dl>
    </div>
  );
}
