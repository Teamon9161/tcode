/**
 * Stand-in for `@tauri-apps/api/core` in the design preview.
 *
 * The preview exists so the interface can be looked at — every state, in a
 * browser, without a provider or a running turn. It is aliased in only when
 * `PREVIEW=1`, so the shipped bundle never contains it.
 */
import type { Launchpad, OpenedSession, SessionInfo, StoredSession } from "../types";
import type { PickerState, PinChoice } from "../picker";

const NOW = Math.floor(Date.now() / 1000);

const PROJECTS: Launchpad = {
  now: NOW,
  home: "/home/teamon",
  projects: [
    {
      path: "/home/teamon/code/rust/tcode",
      name: "tcode",
      session_count: 14,
      last_active: NOW - 60 * 12,
      exists: true,
    },
    {
      path: "/home/teamon/code/py/duck_ext",
      name: "duck_ext",
      session_count: 6,
      last_active: NOW - 60 * 60 * 3,
      exists: true,
    },
    {
      path: "/home/teamon/code/rust/pybond",
      name: "pybond",
      session_count: 21,
      last_active: NOW - 60 * 60 * 26,
      exists: true,
    },
    {
      path: "/home/teamon/scratch/old-experiment",
      name: "old-experiment",
      session_count: 2,
      last_active: NOW - 60 * 60 * 24 * 94,
      exists: false,
    },
  ],
};

const HISTORY: StoredSession[] = [
  { id: "0193f0", preview: "refactor the agent loop so retries are testable", modified: NOW - 720 },
  { id: "0193ef", preview: "why does /resume drop the last tool result?", modified: NOW - 60 * 60 * 5 },
  { id: "0193ee", preview: "add a test for the ledger compact path", modified: NOW - 60 * 60 * 30 },
];

const OPEN: SessionInfo[] = [];

/** Mirrors `picker::MODES`; the wording is the product's, so the preview has to
 *  show the real strings rather than placeholders. The chip shows `key` itself —
 *  there is no second, friendlier name for a mode. */
const MODES = [
  { key: "default", detail: "Rules decide; anything else asks you." },
  { key: "accept-edits", detail: "File edits go through; commands still ask." },
  { key: "auto", detail: "Runs without prompting; a safety classifier reviews the rest." },
  {
    key: "unsafe",
    detail: "Nothing asks. Deny rules still apply. For isolated environments.",
  },
];

/**
 * The model panel's world: two profiles, one preset in force, and roles in every
 * state the panel can draw — inheriting, pinned with an effort, pinned without
 * one, and the one role that is allowed to be off.
 *
 * Mutable, unlike every other fixture here: see `picker_state` below.
 */
const PICKER: PickerState = {
  models: [
    { profile: "anthropic", label: "Opus 5", efforts: ["low", "medium", "high"] },
    { profile: "anthropic", label: "Sonnet 5", efforts: ["low", "medium", "high"] },
    { profile: "openai", label: "gpt-5.1-codex", efforts: ["medium", "high"] },
    { profile: "deepseek", label: "deepseek-v4-flash[1m]", efforts: [] },
  ],
  model: 0,
  effort: "high",
  context_window: 200_000,
  presets: [
    { key: "quant", label: "quant" },
    { key: "cheap", label: "cheap" },
  ],
  preset: 0,
  roles: [
    { key: "explore", label: "explore", helper: false, allows_off: false,
      pin: { kind: "model", index: 3, effort: null } },
    { key: "general", label: "general", helper: false, allows_off: false,
      pin: { kind: "inherit" } },
    { key: "auto", label: "auto", helper: true, allows_off: false,
      pin: { kind: "model", index: 1, effort: "low" } },
    { key: "compact", label: "compact", helper: true, allows_off: false,
      pin: { kind: "inherit" } },
    { key: "suggest", label: "suggest", helper: true, allows_off: false,
      pin: { kind: "inherit" } },
    { key: "vision", label: "vision", helper: true, allows_off: false,
      pin: { kind: "inherit" } },
    { key: "fetch", label: "web-fetch", helper: true, allows_off: true,
      pin: { kind: "off" } },
  ],
  modes: MODES,
  mode: "accept-edits",
  mode_staged: false,
};

/** What `show` panes read. Keyed by extension so one fixture covers the whole
 *  registry in `show.ts` — the preview's job is to make every rendered form
 *  visible without a script having to produce one first. */
const SHOWN: Record<string, string> = {
  csv: [
    "date,bond,ytm,net_basis",
    "2026-07-20,240215.IB,1.6725,0.0412",
    "2026-07-21,240215.IB,1.6690,0.0388",
    '2026-07-22,"CDB, 10Y",1.7415,0.0501',
    "2026-07-23,240215.IB,1.6712,0.0367",
  ].join("\n"),
  json: JSON.stringify({
    xAxis: { type: "category", data: ["Mar", "Apr", "May", "Jun", "Jul"] },
    yAxis: { type: "value" },
    series: [{ type: "line", data: [12, 8, 19, 15, 24], smooth: true }],
  }),
  md: "# Carry report\n\nThe 10Y **outperformed** the futures leg by 4bp.\n",
  html: "<h2 style='font-family:sans-serif'>rendered artifact</h2>",
};

type WorkspaceKind = "file" | "directory" | "link";
type WorkspaceNode = { kind: WorkspaceKind; text?: string; revision: number };
type WorkspaceFixture = { nodes: Record<string, WorkspaceNode>; changedAfterFirstSave: boolean };
type WorkspaceEntry = { name: string; path: string; kind: WorkspaceKind };
type WorkspaceText = { path: string; text: string; revision: string; bytes: number; truncated: boolean };

/**
 * The workspace fixtures use the same relative paths as the workspace wire
 * contract. Keeping them per session matters: two sessions can have a
 * README.md without a preview edit in one leaking into the other.
 */
function workspace(nodes: Record<string, WorkspaceNode>): WorkspaceFixture {
  return { nodes, changedAfterFirstSave: false };
}

function fixtureWorkspaces(): Record<string, WorkspaceFixture> {
  return {
    a: workspace({
      "README.md": {
        kind: "file",
        revision: 1,
        text: [
          "# tcode workspace preview",
          "",
          "This is a **real Markdown editor preview** backed by the workspace fixture.",
          "",
          "- expand `crates` to inspect nested source",
          "- `empty-fixture` intentionally has no children",
          "- links remain visible but cannot be opened",
        ].join("\n"),
      },
      crates: { kind: "directory", revision: 1 },
      "crates/tcode-app": { kind: "directory", revision: 1 },
      "crates/tcode-app/src": { kind: "directory", revision: 1 },
      "crates/tcode-app/src/Workspace.tsx": {
        kind: "file",
        revision: 1,
        text: [
          "export function workspaceTitle(session: string): string {",
          "  return `workspace for ${session}`;",
          "}",
          "",
          "const focused = new Set([\"workspace-tree\", \"workspace-file\"]);",
          "export const hasWorkspaceView = (view: string) => focused.has(view);",
        ].join("\n"),
      },
      "crates/tcode-app/src/theme": { kind: "directory", revision: 1 },
      "crates/tcode-app/src/theme/preview.css": {
        kind: "file",
        revision: 1,
        text: ".workspace-preview { display: grid; gap: var(--s-3); }\n",
      },
      docs: { kind: "directory", revision: 1 },
      "docs/fixture-notes.md": {
        kind: "file",
        revision: 1,
        text: "# Fixture notes\n\nWorkspace paths are relative to the session root.\n",
      },
      "empty-fixture": { kind: "directory", revision: 1 },
      "outside-workspace": { kind: "link", revision: 1 },
    }),
    b: workspace({
      "README.md": { kind: "file", revision: 1, text: "# duck_ext\n\nA separate session fixture.\n" },
      src: { kind: "directory", revision: 1 },
      "src/curve.py": { kind: "file", revision: 1, text: "def load_curve(date):\n    return date\n" },
      cache: { kind: "directory", revision: 1 },
    }),
    c: workspace({
      "README.md": { kind: "file", revision: 1, text: "# pybond\n\nIndependent workspace fixture.\n" },
      pybond: { kind: "directory", revision: 1 },
      "pybond/pricing.rs": { kind: "file", revision: 1, text: "pub fn price() -> f64 { 100.0 }\n" },
    }),
  };
}

let WORKSPACES = fixtureWorkspaces();

/** Reset only the mutable workspace state so fixture tests remain deterministic. */
export function resetPreviewFixtures(): void {
  WORKSPACES = fixtureWorkspaces();
}

function workspaceFor(args: Record<string, unknown> | undefined): [string, WorkspaceFixture] {
  const session = typeof args?.session === "string" ? args.session : "";
  const fixture = WORKSPACES[session];
  if (!fixture) throw new Error(`session '${session}' is not open`);
  return [session, fixture];
}

function relativePath(value: unknown, label: string): string {
  if (typeof value !== "string" || value.length === 0 || value.startsWith("/") || value.includes("\\")) {
    throw new Error(`${label} must be a non-empty workspace-relative path`);
  }
  if (value.split("/").some((part) => part === "" || part === "." || part === "..")) {
    throw new Error(`${label} must be a normalized workspace-relative path`);
  }
  return value;
}

function parentOf(path: string): string {
  const at = path.lastIndexOf("/");
  return at === -1 ? "" : path.slice(0, at);
}

function entryFor(path: string, node: WorkspaceNode): WorkspaceEntry {
  return { name: path.slice(path.lastIndexOf("/") + 1), path, kind: node.kind };
}

function revision(session: string, path: string, number: number): string {
  return `fixture:${session}:${path}:${number}`;
}

function textView(session: string, path: string, node: WorkspaceNode): WorkspaceText {
  const text = node.text ?? "";
  return {
    path,
    text,
    revision: revision(session, path, node.revision),
    bytes: new TextEncoder().encode(text).length,
    truncated: false,
  };
}

function listWorkspace(args: Record<string, unknown> | undefined): WorkspaceEntry[] {
  const [, fixture] = workspaceFor(args);
  const path = args?.path === null || args?.path === undefined ? "" : relativePath(args.path, "path");
  if (path && fixture.nodes[path]?.kind !== "directory") throw new Error(`workspace path '${path}' is not a directory`);
  if (path && !fixture.nodes[path]) throw new Error(`workspace path '${path}' does not exist`);
  return Object.entries(fixture.nodes)
    .filter(([entryPath]) => parentOf(entryPath) === path)
    .map(([entryPath, node]) => entryFor(entryPath, node));
}

function readWorkspace(args: Record<string, unknown> | undefined): WorkspaceText {
  const [session, fixture] = workspaceFor(args);
  const path = relativePath(args?.path, "path");
  const node = fixture.nodes[path];
  if (!node) throw new Error(`workspace path '${path}' does not exist`);
  if (node.kind !== "file") throw new Error(`workspace path '${path}' is not a regular file`);
  return textView(session, path, node);
}

function writeWorkspace(args: Record<string, unknown> | undefined): WorkspaceText {
  const [session, fixture] = workspaceFor(args);
  const path = relativePath(args?.path, "path");
  const text = typeof args?.text === "string" ? args.text : null;
  const expected = typeof args?.revision === "string" ? args.revision : "";
  const node = fixture.nodes[path];
  if (!node || node.kind !== "file") throw new Error(`workspace path '${path}' is not a regular file`);
  if (text === null) throw new Error("text must be a string");
  if (expected !== revision(session, path, node.revision)) {
    throw new Error(`revision conflict: the file changed; re-read it before writing (current revision ${revision(session, path, node.revision)})`);
  }

  node.text = text;
  node.revision += 1;
  const saved = textView(session, path, node);

  // One successful preview save represents a collaborator's subsequent change.
  // The editor retains `saved.revision`, so the next edit visibly exercises its
  // normal conflict branch instead of relying on a fabricated error button.
  if (!fixture.changedAfterFirstSave) {
    fixture.changedAfterFirstSave = true;
    node.text = `${text}\n\n<!-- preview fixture: remote change after save -->`;
    node.revision += 1;
  }
  return saved;
}

function createWorkspace(args: Record<string, unknown> | undefined): WorkspaceEntry {
  const [, fixture] = workspaceFor(args);
  const parent = args?.parent === null || args?.parent === undefined ? "" : relativePath(args.parent, "parent");
  const name = relativePath(args?.name, "name");
  const kind = args?.kind;
  if (name.includes("/")) throw new Error("name must be one workspace path component");
  if (kind !== "file" && kind !== "directory") throw new Error(`'${String(kind)}' is not a workspace entry kind`);
  if (parent && fixture.nodes[parent]?.kind !== "directory") throw new Error(`workspace path '${parent}' is not a directory`);
  if (parent && !fixture.nodes[parent]) throw new Error(`workspace path '${parent}' does not exist`);
  const path = parent ? `${parent}/${name}` : name;
  if (fixture.nodes[path]) throw new Error(`workspace path '${path}' already exists`);
  const node: WorkspaceNode = { kind, revision: 1, ...(kind === "file" ? { text: "" } : {}) };
  fixture.nodes[path] = node;
  return entryFor(path, node);
}

function renameWorkspace(args: Record<string, unknown> | undefined): WorkspaceEntry {
  const [, fixture] = workspaceFor(args);
  const path = relativePath(args?.path, "path");
  const name = relativePath(args?.name, "name");
  if (name.includes("/")) throw new Error("name must be one workspace path component");
  const node = fixture.nodes[path];
  if (!node) throw new Error(`workspace path '${path}' does not exist`);
  const renamed = parentOf(path) ? `${parentOf(path)}/${name}` : name;
  if (fixture.nodes[renamed]) throw new Error(`workspace path '${renamed}' already exists`);

  const moved = Object.entries(fixture.nodes).filter(([candidate]) => candidate === path || candidate.startsWith(`${path}/`));
  for (const [candidate] of moved) delete fixture.nodes[candidate];
  for (const [candidate, value] of moved) {
    fixture.nodes[`${renamed}${candidate.slice(path.length)}`] = value;
  }
  return entryFor(renamed, node);
}

function deleteWorkspace(args: Record<string, unknown> | undefined): void {
  const [, fixture] = workspaceFor(args);
  const path = relativePath(args?.path, "path");
  const node = fixture.nodes[path];
  if (!node) throw new Error(`workspace path '${path}' does not exist`);
  if (node.kind === "directory" && Object.keys(fixture.nodes).some((candidate) => candidate.startsWith(`${path}/`))) {
    throw new Error(`workspace directory '${path}' is not empty`);
  }
  delete fixture.nodes[path];
}

export async function invoke<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  switch (command) {
    case "launchpad":
      return PROJECTS as T;
    case "project_sessions":
      return HISTORY as T;
    case "sessions":
      return OPEN as T;
    case "open_folder":
      return {
        session: {
          id: "preview",
          cwd: String(args?.path ?? "/home/teamon/code/rust/tcode"),
          name: "tcode",
          home: "/home/teamon",
        },
        history: [],
        // A fresh folder, so the prompt is the system prompt and the tool
        // schemas and nothing else — measured, not guessed, which is why it is
        // not zero.
        context_tokens: 14_200,
        context_estimated: false,
      } satisfies OpenedSession as T;
    // The composer strip. Deliberately not the careful default: the amber mode
    // chip and a model with an effort dial are the states worth looking at.
    case "picker_state":
      return { ...PICKER } as T;
    // The picker commands really do move the fixture. Everything else here is a
    // static answer, but this one panel is judged by whether a pick reads back —
    // that is the whole complaint it was rebuilt to fix, and a fixture that
    // always answers "Opus 5 · high" cannot show it.
    case "choose_model":
      PICKER.model = Number(args?.index ?? PICKER.model);
      PICKER.effort = (args?.effort as string | null) ?? null;
      return undefined as T;
    case "choose_preset": {
      const at = PICKER.presets.findIndex((one) => one.key === args?.key);
      if (at >= 0) PICKER.preset = at;
      return undefined as T;
    }
    case "pin_role": {
      const role = PICKER.roles.find((one) => one.key === args?.kind);
      if (role) role.pin = args?.pin as PinChoice;
      return undefined as T;
    }
    case "save_preset": {
      const name = String(args?.name ?? "").trim();
      if (!/^[A-Za-z0-9_-]+$/.test(name)) {
        // The real one refuses in `Config::upsert_preset`, and the panel is
        // supposed to keep what was typed and show why.
        throw new Error(`'${name}' is not a usable preset name — letters, digits, '-' and '_' only`);
      }
      if (!PICKER.presets.some((one) => one.key === name)) {
        PICKER.presets.push({ key: name, label: name });
      }
      PICKER.preset = PICKER.presets.findIndex((one) => one.key === name);
      return undefined as T;
    }
    case "choose_mode":
      PICKER.mode = String(args?.mode ?? PICKER.mode);
      return undefined as T;
    case "shown_file": {
      const path = String(args?.path ?? "");
      const body = SHOWN[path.slice(path.lastIndexOf(".") + 1)] ?? "no fixture for this file";
      return { body, bytes: body.length, truncated: false } as T;
    }
    case "workspace_list":
      return listWorkspace(args) as T;
    case "workspace_read_text":
      return readWorkspace(args) as T;
    case "workspace_write_text":
      return writeWorkspace(args) as T;
    case "workspace_create":
      return createWorkspace(args) as T;
    case "workspace_rename":
      return renameWorkspace(args) as T;
    case "workspace_delete":
      deleteWorkspace(args);
      return undefined as T;
    // Two openers rather than all three: the backend only ever reports what is
    // installed, and a fixture where everything is present would never show the
    // shape a real machine has (rule: a fixture writes what the wire really
    // carries, or it stops being an acceptance surface).
    case "workspace_openers":
      return [
        { id: "reveal", name: "Explorer" },
        { id: "vscode", name: "VS Code" },
      ] as T;
    case "workspace_open_external":
      return undefined as T;
    default:
      return undefined as T;
  }
}
