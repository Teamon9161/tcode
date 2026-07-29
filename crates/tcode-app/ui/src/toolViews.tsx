import { createContext, useContext, type ReactNode } from "react";

import type { ToolResult } from "./blocks";
import { Diff } from "./components/Diff";
import { isEditShape } from "./diff";
import type { Inspect } from "./inspect";
import { basename } from "./show";
import { ShownView } from "./Shown";

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

export type TranscriptGroup = "exploration" | "changes" | "commands";

export type ToolView = {
  /** Change preview under the header, shown at the call site. */
  body?(input: unknown): ReactNode;
  /** What opening this call shows in the inspector — where the call's
   *  pop-out button leads. */
  inspect?(input: unknown, callId: string, result?: ToolResult): Inspect | null;
  /** What this call is *about*, derived from its input. */
  summary?(input: unknown): string | null;
  /** Prefer the input-derived summary over core's generic `tool(argument)` label. */
  preferInputSummary?: boolean;
  /** Detail that is useful before a result exists, such as a multi-line command. */
  detail?(input: unknown): string | null;
  /** Normalize output for this tool before any transcript or inspector renders it. */
  output?(content: string): string;
  /** Adjacent calls in the same group become one collapsed transcript step. */
  transcriptGroup?: TranscriptGroup;
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
  transcriptGroup: "changes",
};

/** A read opens the snapshot it returned — what the model saw, not what is on
 *  disk now. The difference is the entire point when something edited the file
 *  afterwards. */
const reading: ToolView = {
  inspect: (input, callId) => {
    const path = targetPath(input);
    return path ? { kind: "file", path, at: callId } : null;
  },
  summary: readTarget,
  preferInputSummary: true,
  transcriptGroup: "exploration",
};

const shell: ToolView = {
  summary: commandPreview,
  preferInputSummary: true,
  detail: commandOf,
  output: stripTerminalEscapes,
  transcriptGroup: "commands",
};

const pattern: ToolView = {
  summary: patternTarget,
  preferInputSummary: true,
  transcriptGroup: "exploration",
};

/**
 * `show` puts a file on screen, at the call site.
 *
 * The artifact *is* the result, so it draws in the transcript where the
 * conversation is, exactly like an edit draws its diff. `inspect` then makes the
 * pop-out button lead to the same thing with room to breathe. It is deliberately
 * not grouped with exploration: a chart is not a step on the way to an answer.
 */
const showing: ToolView = {
  body: (input) => {
    const value = shownValue(input);
    return value ? <ShownView value={value} inline /> : null;
  },
  inspect: (input) => shownValue(input),
  summary: (input) => targetPath(input),
  preferInputSummary: true,
};

function shownValue(input: unknown): Extract<Inspect, { kind: "shown" }> | null {
  const path = targetPath(input);
  if (!path) return null;
  const label =
    typeof input === "object" && input !== null
      ? (input as Record<string, unknown>).label
      : null;
  return {
    kind: "shown",
    path,
    label: typeof label === "string" && label.trim() ? label.trim() : basename(path),
  };
}

const VIEWS: Record<string, ToolView> = {
  show: showing,
  edit: editing,
  write: editing,
  multi_edit: editing,
  notebook_edit: editing,
  read: reading,
  shell,
  bash: shell,
  grep: pattern,
  glob: pattern,
  update_progress: { body: (input) => <Phases input={input} /> },
};

/** The fallback opens the call's complete output, which is the only thing every
 *  tool has. */
const FALLBACK: ToolView = {
  inspect: (_input, callId, result) => (result?.content ? { kind: "output", callId } : null),
  summary: describe,
};

function readTarget(input: unknown): string | null {
  const path = targetPath(input);
  if (!path || typeof input !== "object" || input === null) return path;
  const record = input as Record<string, unknown>;
  const offset = typeof record.offset === "number" && record.offset > 1 ? record.offset : 1;
  const limit = typeof record.limit === "number" && record.limit > 0 ? record.limit : null;
  if (limit) return `${path}:${offset}-${offset + limit - 1}`;
  return offset > 1 ? `${path}:${offset}-` : path;
}

function patternTarget(input: unknown): string | null {
  if (typeof input !== "object" || input === null) return null;
  const record = input as Record<string, unknown>;
  const pattern = typeof record.pattern === "string" ? record.pattern.trim() : "";
  if (!pattern) return null;
  const path = typeof record.path === "string" ? record.path.trim() : "";
  return path && path !== "." ? `${pattern} in ${path}` : pattern;
}

function commandOf(input: unknown): string | null {
  if (typeof input !== "object" || input === null) return null;
  const command = (input as Record<string, unknown>).command;
  return typeof command === "string" && command.trim() ? command : null;
}

/** Matches the TUI's capped first-line command label; the complete command
 * remains available from the disclosure beside it. */
function commandPreview(input: unknown): string | null {
  const command = commandOf(input);
  if (!command) return null;
  const firstLine = command.trim().split("\n", 1)[0];
  const limit = 56;
  return firstLine.length > limit ? `${firstLine.slice(0, limit)}…` : firstLine;
}

/** Colored process output is terminal presentation data, not transcript text.
 * This mirrors the shell filter's CSI/OSC handling so live output and replay
 * stay readable even when no project filter is configured. */
export function stripTerminalEscapes(output: string): string {
  let clean = "";
  for (let index = 0; index < output.length; index += 1) {
    if (output[index] !== "\x1b") {
      clean += output[index];
      continue;
    }

    const kind = output[++index];
    if (kind === "[") {
      while (++index < output.length) {
        const code = output.charCodeAt(index);
        if (code >= 0x40 && code <= 0x7e) break;
      }
    } else if (kind === "]") {
      while (++index < output.length) {
        if (output[index] === "\x07") break;
        if (output[index] === "\x1b" && output[index + 1] === "\\") {
          index += 1;
          break;
        }
      }
    }
  }
  return clean;
}

/**
 * The live stream already carries core's concise call summary. Ledger replay
 * cannot: it has the tool name and input but no ephemeral `ToolStart` event,
 * so it supplies the bare name. The header always owns the verb, therefore a
 * bare or generic `tool(argument)` summary is replaced with the registry's
 * input-derived target instead of becoming `read read` on resume.
 */
export function displayToolSummary(name: string, supplied: string, input: unknown): string {
  const view = viewFor(name);
  const fromInput = view.summary?.(input) ?? "";
  const normalized = supplied.trim();
  const generic = normalized === name || normalized === `${name}()` || normalized.startsWith(`${name}(`);
  if ((view.preferInputSummary || generic) && fromInput) return fromInput;
  return normalized || fromInput;
}

/** Applies a tool's presentation-only output normalization everywhere it is
 * shown, without altering the recorded result or model-visible ledger. */
export function displayToolOutput(name: string, content: string): string {
  return viewFor(name).output?.(content) ?? content;
}

/**
 * A one-line "what this call is about", derived from its input.
 *
 * Batch calls arrive without a `ToolStart` summary, so this preserves their
 * target rather than leaving a list of indistinguishable tool names.
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

export function transcriptGroupFor(name: string): ToolView["transcriptGroup"] {
  return viewFor(name).transcriptGroup;
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
