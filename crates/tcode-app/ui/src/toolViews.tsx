import { createContext, useContext, type ReactNode } from "react";

import type { ToolResult } from "./blocks";
import { Diff } from "./components/Diff";
import { isEditShape } from "./diff";
import type { Inspect } from "./inspect";

/**
 * How each tool call draws, and what clicking it opens.
 *
 * This is the front-end half of the TUI's `ToolRenderer`. The split between the
 * two halves is deliberate and load-bearing:
 *
 *  - **Routing comes from the backend**, as data from `tool_views()`. Whether a
 *    call belongs in the transcript, feeds the progress display, or renders
 *    nothing at all never appears in this file. Note what that does and does
 *    not buy: `quiet_output` really is derived from the live tool set, while
 *    `route` is still a name list on the Rust side because `CallRoute` lives in
 *    `tcode-tui` and this app cannot depend on it. `src/commands.rs` documents
 *    that gap and the trait method that would close it.
 *  - **Presentation lives here.** Only the webview knows what a diff or a phase
 *    list should look like.
 *
 * A tool with no entry gets the default treatment, which is why the map stays
 * short: the registry exists so `Transcript.tsx` never grows a chain of
 * `if (name === …)`, not so every tool gets a bespoke card.
 */

/** Mirrors `ToolViewMeta` in `src/commands.rs`. */
export type ToolMeta = {
  name: string;
  route: "transcript" | "progress" | "silent";
  quiet_output: boolean;
  hide_success_result: boolean;
};

const DEFAULT_META: Omit<ToolMeta, "name"> = {
  route: "transcript",
  quiet_output: false,
  hide_success_result: false,
};

const MetaContext = createContext<Map<string, ToolMeta>>(new Map());

export function ToolMetaProvider({
  meta,
  children,
}: {
  meta: Map<string, ToolMeta>;
  children: ReactNode;
}) {
  return <MetaContext.Provider value={meta}>{children}</MetaContext.Provider>;
}

export function useToolMeta(name: string): ToolMeta {
  const table = useContext(MetaContext);
  return table.get(name) ?? { name, ...DEFAULT_META };
}

export type ToolView = {
  /** Change preview under the header, shown at the call site. */
  body?(input: unknown): ReactNode;
  /** What opening this call shows in the inspector. */
  inspect?(input: unknown, callId: string, result?: ToolResult): Inspect | null;
  /** What this call is *about*, when the event stream did not say.
   *  See `describe` below for why that happens. */
  summary?(input: unknown): string | null;
};

const targetPath = (input: unknown): string | null => {
  if (typeof input !== "object" || input === null) return null;
  const record = input as Record<string, unknown>;
  for (const key of ["file_path", "path", "notebook_path", "filePath"]) {
    const value = record[key];
    if (typeof value === "string" && value.length > 0) return value;
  }
  return null;
};

/** An edit's own diff is its best summary, at the call site and in the panel. */
const editing: ToolView = {
  body: (input) => (isEditShape(input) ? <Diff input={input} dense /> : null),
  inspect: (input, callId) => (isEditShape(input) ? { kind: "diff", callId } : null),
};

/** A read opens the snapshot it returned — what the model saw, not what is on
 *  disk now. The difference is the entire point when something edited the file
 *  afterwards. */
const reading: ToolView = {
  inspect: (input, callId) => {
    const path = targetPath(input);
    return path ? { kind: "file", path, at: callId } : null;
  },
};

const VIEWS: Record<string, ToolView> = {
  edit: editing,
  write: editing,
  multi_edit: editing,
  notebook_edit: editing,
  read: reading,
  update_progress: { body: (input) => <Phases input={input} /> },
};

/** The fallback opens the call's complete output, which is the only thing every
 *  tool has. */
const FALLBACK: ToolView = {
  inspect: (_input, callId, result) => (result?.content ? { kind: "output", callId } : null),
  summary: describe,
};

/**
 * A one-line "what this call is about", derived from its input.
 *
 * Needed because `ToolBatchStart` carries `(call_id, name, input)` and nothing
 * else: core emits no per-call `ToolStart` for the concurrent paths (parallel
 * reads, file-mutation lanes), so batched calls arrive with no summary at all.
 * Without this, expanding a batch of five reads shows five rows saying `read`
 * and nothing about *what* was read — the batch header collapses them, and then
 * opening it tells you nothing.
 *
 * This is presentation, not routing: it names the argument the tool's own
 * schema calls the target. The durable fix is core carrying the same summary in
 * `ToolBatchStart` that it already computes for `ToolStart` (`summarize_call`),
 * which would also give the TUI one definition instead of two.
 */
function describe(input: unknown): string | null {
  if (typeof input !== "object" || input === null) return null;
  const record = input as Record<string, unknown>;
  for (const key of ["file_path", "path", "notebook_path", "filePath", "command", "pattern", "url", "query"]) {
    const value = record[key];
    if (typeof value === "string" && value.length > 0) return value;
  }
  return null;
}

export function viewFor(name: string): ToolView {
  const view = VIEWS[name];
  if (!view) return FALLBACK;
  return { ...FALLBACK, ...view };
}

/**
 * `update_progress` at the call site.
 *
 * Core routes this to a progress display rather than the transcript, and the
 * persistent panel that will hold it is not built yet. Until it is, the phases
 * render here: dropping the call because its final home does not exist would
 * lose the one thing it carries.
 */
function Phases({ input }: { input: unknown }) {
  const phases = readPhases(input);
  if (!phases) return null;
  if (phases.length === 0) return <p className="phases-cleared">progress cleared</p>;

  return (
    <ol className="phases">
      {phases.map((phase, index) => (
        <li key={index} className={`phase is-${phase.status}`}>
          <span className="phase-mark" aria-hidden />
          <span className="phase-text">{phase.phase}</span>
        </li>
      ))}
    </ol>
  );
}

type Phase = { phase: string; status: string };

/** `plan` / `step` keep sessions recorded before the rename readable; live
 *  calls use `phases` / `phase` exclusively. */
function readPhases(input: unknown): Phase[] | null {
  if (typeof input !== "object" || input === null) return null;
  const record = input as Record<string, unknown>;
  const list = record.phases ?? record.plan;
  if (!Array.isArray(list)) return null;

  return list.map((entry) => {
    const item = (entry ?? {}) as Record<string, unknown>;
    return {
      phase: String(item.phase ?? item.step ?? ""),
      status: String(item.status ?? "pending"),
    };
  });
}
