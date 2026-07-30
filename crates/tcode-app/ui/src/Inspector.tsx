import { useMemo } from "react";

import { findRun, findToolCall, reportOf, type Block } from "./blocks";
import { Diff } from "./components/Diff";
import { Code } from "./components/Code";
import { languageOf } from "./diff";
import { useFileHistory, type Inspect } from "./inspect";
import { relativeTo, type TouchedFile } from "./files";
import { FilesView } from "./FilePanel";
import { WorkspaceFiles } from "./WorkspaceFiles";
import { WorkspaceEditor } from "./WorkspaceEditor";
import { rich } from "./rich";
import { Sandbox } from "./Sandbox";
import { ShownView } from "./Shown";
import { agentKind, BlockList } from "./Transcript";
import { displayToolOutput } from "./toolViews";
import { PlanEditor } from "./PlanEditor";
import { useSession } from "./session";
import { draftOf, type Plan, type PlanDraft } from "./plan";

/**
 * The body of an inspect pane: one `Inspect` value, drawn.
 *
 * It dispatches on `Inspect["kind"]` and holds no state of its own — the
 * navigation stack lives in the pane (`layout.ts` + `inspect.ts`). Transcript
 * views below remain pure functions of the recorded conversation; live workspace
 * values are explicit exceptions whose state belongs to their own session pane.
 *
 * Everything here normally reads from blocks rather than from disk. A review
 * surface that re-read the file would answer a different question than the one
 * being asked — what the agent did, not what happens to be there now. `shown`,
 * plans, and the session-confined workspace tree are deliberate live exceptions;
 * each says so through a separate inspect kind rather than changing Files'
 * transcript-derived semantics.
 *
 * The frame around it — header, history buttons, close — belongs to
 * `Panes.tsx`, because it is the same frame every pane wears.
 */
export function InspectView({
  value,
  blocks,
  files,
  cwd,
  plan,
  planDraft,
  onOpen,
  onOpenAside,
  onMention,
  onPlanDraft,
  onSavePlan,
}: {
  value: Inspect;
  blocks: Block[];
  files: TouchedFile[];
  cwd: string;
  plan: Plan | null;
  planDraft: PlanDraft | null;
  onOpen: (next: Inspect) => void;
  onOpenAside: (next: Inspect) => void;
  onMention: (path: string) => void;
  onPlanDraft: (draft: PlanDraft) => void;
  onSavePlan: () => void;
}) {
  const session = useSession();

  switch (value.kind) {
    case "files":
      return <FilesView files={files} cwd={cwd} onOpen={onOpen} />;
    case "workspace-tree":
      return (
        <WorkspaceFiles
          cwd={cwd}
          onOpenFile={(path) => onOpen({ kind: "workspace-file", path })}
          onOpenAside={(path) => onOpenAside({ kind: "workspace-file", path })}
          onMention={onMention}
        />
      );
    case "workspace-file":
      return <WorkspaceEditor key={`${session}:${value.path}`} path={value.path} />;
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
    // No sandbox: an image is decoded by the browser, not executed. The
    // `data:` URL is the same one the transcript thumbnail already draws, and
    // `img-src data:` is in the CSP for exactly this.
    case "image":
      return (
        <div className="inspect-image">
          <img src={value.url} alt={value.label} />
        </div>
      );
    case "shown":
      return <ShownView value={value} cwd={cwd} />;
    case "doc":
      return <div className="doc">{rich(value.text)}</div>;
    case "plan":
      // Room to work: the same editor the review dock mounts, on the same draft,
      // with the whole height of a pane instead of half of one. Editing a plan
      // outside a review is a legal thing to do at any time — the file is the
      // user's, and the model is handed their version on its next call.
      return plan ? (
        <div className="inspect-plan">
          <p className="inspect-path" title={plan.path}>
            {plan.file}
          </p>
          <PlanEditor
            plan={plan}
            draft={planDraft ?? draftOf(plan)}
            mode="edit"
            onDraft={onPlanDraft}
            onSave={onSavePlan}
          />
        </div>
      ) : (
        <p className="inspect-empty">
          this conversation has no plan — ask for one with “plan”
        </p>
      );
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

/**
 * A sub-agent's whole turn, drawn by the same list the main transcript uses —
 * which is also how its reasoning obeys the same display switch as the
 * conversation that delegated it.
 *
 * The order here is the order the questions get asked: what it was told to do,
 * what it did, what it came back with. The report is last because it is the
 * answer, and reading an answer before the work is what made this pane feel like
 * three unrelated blocks.
 */
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
  const report = useMemo(() => reportOf(blocks, run), [blocks, run]);
  if (!found) return <p className="inspect-empty">that run is no longer in this conversation</p>;

  const calls = found.meta.toolCalls;
  return (
    <div className="inspect-run">
      <dl className="inspect-facts">
        <div>
          <dt>agent</dt>
          <dd>{agentKind(found.meta.kind)}</dd>
        </div>
        <div>
          <dt>model</dt>
          <dd>{found.meta.model}</dd>
        </div>
        <div>
          <dt>status</dt>
          <dd>{found.meta.status ?? "running"}</dd>
        </div>
        {calls !== undefined && (
          <div>
            <dt>calls</dt>
            <dd>{calls}</dd>
          </div>
        )}
      </dl>

      {/* The prompt is what one agent told another, and it arrives with the
          paragraphs and lists the sender wrote. It used to go into a plain `<p>`,
          which collapses every newline: a structured brief came out as one
          run-on block whose second half read as though it belonged to a different
          message. `pre-wrap` and nothing else — it is an instruction, not a
          document, and putting it through the markdown renderer would dress up
          text nobody wrote for a reader. */}
      <section className="inspect-part">
        <h4 className="inspect-part-head">Asked to</h4>
        <p className="inspect-prompt">{found.meta.prompt}</p>
      </section>

      <section className="inspect-part">
        <h4 className="inspect-part-head">Did</h4>
        <div className="inspect-transcript">
          <BlockList blocks={found.blocks} onOpen={onOpen} />
          {found.blocks.length === 0 && <p className="inspect-empty">nothing recorded yet</p>}
        </div>
      </section>

      {/* Model-authored prose for a human, so it goes through `rich` like the
          conversation's own text (rule 10 covers it either way). */}
      {report && (
        <section className="inspect-part">
          <h4 className="inspect-part-head">Reported back</h4>
          <div className="run-report">{rich(report)}</div>
        </section>
      )}
    </div>
  );
}
