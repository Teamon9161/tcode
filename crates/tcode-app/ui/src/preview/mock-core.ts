/**
 * Stand-in for `@tauri-apps/api/core` in the design preview.
 *
 * The preview exists so the interface can be looked at — every state, in a
 * browser, without a provider or a running turn. It is aliased in only when
 * mode `preview`, so the shipped bundle never contains it.
 */
import type {
  ProjectList,
  OpenedSession,
  SessionInfo,
  StoredSession,
  StoredSessionsPage,
} from "../types";
import type { PickerState, PinChoice } from "../picker";
import { TERMINAL_OUTPUT } from "../types";
import { deliver } from "./mock-event";

const NOW = Math.floor(Date.now() / 1000);

const PROJECTS: ProjectList = {
  now: NOW,
  home: "/home/teamon",
  projects: [
    {
      path: "/home/teamon/code/rust/tcode",
      name: "tcode",
      session_count: 44,
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
    // A tail, so the rail's `Recent` band is drawn in the state it is designed
    // for rather than the comfortable one: past the cap, with the `N more` row
    // under it and the column actually scrolling. Four projects proved none of
    // that — and "does this still read at forty folders" is the question the
    // whole two-band arrangement exists to answer.
    ...[
      ["multi-asset-alloc", "/home/teamon/work/multi-asset-alloc", 9],
      ["pl_ext", "/home/teamon/code/py/pl_ext", 17],
      ["short_term_rs", "/home/teamon/code/rust/short_term_rs", 4],
      ["bond-curve", "/home/teamon/work/bond-curve", 31],
      ["tcode-voiced", "/home/teamon/code/rust/tcode/crates/tcode-voiced", 3],
      ["dotfiles", "/home/teamon/dotfiles", 1],
      ["notes", "/home/teamon/notes", 12],
      ["irs-pricing", "/home/teamon/work/irs-pricing", 8],
      ["l2-replay", "/home/teamon/work/l2-replay", 22],
      ["scratch", "/home/teamon/scratch", 5],
    ].map(([name, path, count], at) => ({
      path: path as string,
      name: name as string,
      session_count: count as number,
      last_active: NOW - 60 * 60 * 24 * (2 + at * 3),
      exists: true,
    })),
  ],
};

const HISTORY: StoredSession[] = [
  {
    id: "0193f0",
    preview: "refactor the agent loop so retries are testable",
    modified: NOW - 720,
  },
  {
    id: "0193ef",
    preview: "why does /resume drop the last tool result?",
    modified: NOW - 60 * 60 * 5,
  },
  {
    id: "0193ee",
    preview: "add a test for the ledger compact path",
    modified: NOW - 60 * 60 * 30,
  },
  ...Array.from({ length: 41 }, (_item, index): StoredSession => ({
    id: `0193${(0xed - index).toString(16).padStart(2, "0")}`,
    preview: `older session ${index + 4}: keep the history list incremental`,
    modified: NOW - 60 * 60 * (48 + index * 7),
  })),
];

const OPEN: SessionInfo[] = [];

/** Mirrors `picker::MODES`; the wording is the product's, so the preview has to
 *  show the real strings rather than placeholders. The chip shows `key` itself —
 *  there is no second, friendlier name for a mode. */
const MODES = [
  { key: "default", detail: "Rules decide; anything else asks you." },
  { key: "accept-edits", detail: "File edits go through; commands still ask." },
  {
    key: "auto",
    detail: "Runs without prompting; a safety classifier reviews the rest.",
  },
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
    {
      profile: "anthropic",
      label: "Opus 5",
      efforts: ["low", "medium", "high"],
    },
    {
      profile: "anthropic",
      label: "Sonnet 5",
      efforts: ["low", "medium", "high"],
    },
    { profile: "openai", label: "gpt-5.1-codex", efforts: ["medium", "high"] },
    { profile: "deepseek", label: "deepseek-v4-flash[1m]", efforts: [] },
  ],
  role_models: [
    {
      profile: "anthropic",
      label: "Opus 5",
      efforts: ["low", "medium", "high"],
    },
    {
      profile: "anthropic",
      label: "Sonnet 5",
      efforts: ["low", "medium", "high"],
    },
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
    {
      key: "explore",
      label: "explore",
      helper: false,
      allows_off: false,
      pin: { kind: "model", index: 3, effort: null },
    },
    {
      key: "general",
      label: "general",
      helper: false,
      allows_off: false,
      pin: { kind: "inherit" },
    },
    {
      key: "auto",
      label: "auto",
      helper: true,
      allows_off: false,
      pin: { kind: "model", index: 1, effort: "low" },
    },
    {
      key: "compact",
      label: "compact",
      helper: true,
      allows_off: false,
      pin: { kind: "inherit" },
    },
    {
      key: "suggest",
      label: "suggest",
      helper: true,
      allows_off: false,
      pin: { kind: "inherit" },
    },
    {
      key: "vision",
      label: "vision",
      helper: true,
      allows_off: false,
      pin: { kind: "inherit" },
    },
    {
      key: "fetch",
      label: "web-fetch",
      helper: true,
      allows_off: true,
      pin: { kind: "off" },
    },
  ],
  modes: MODES,
  mode: "accept-edits",
  mode_staged: false,
  // The preview's main model (Opus 5) is vision-capable, so the line-up can
  // view images and the vision row stays quiet. Flip to false to look at the
  // warning state in the panel.
  can_view_images: true,
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
  /**
   * A report, not a fragment — and it has a script, because a script is the
   * whole reason this file stopped going through the sandbox frame. If the
   * fixture were static markup it would look identical either way, and the
   * preview would go on passing after a change that put reports back behind a
   * boundary that cannot run them.
   */
  html: [
    "<!doctype html><meta charset='utf-8'>",
    "<body style='font-family:sans-serif;margin:0;padding:16px'>",
    "<h2>Carry report</h2><p id='drawn'>this paragraph is replaced by script</p>",
    "<script>document.getElementById('drawn').textContent =",
    "'drawn by the page itself, at ' + new Date().toISOString().slice(11, 19);</script>",
  ].join("\n"),
};

const PDF_FIXTURE = makePdf([
  "Paper Mode MVP",
  "Select text on this page, then use Translate, Explain, or Ask.",
  "The prompt lands in the existing composer draft.",
]);

function makePdf(lines: string[]): string {
  const stream = [
    "BT",
    "/F1 18 Tf",
    "72 740 Td",
    "24 TL",
    ...lines.map((line, index) => `${index === 0 ? "" : "T* "}(${pdfString(line)}) Tj`),
    "ET",
  ].join("\n");
  const objects = [
    "1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n",
    "2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n",
    "3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>\nendobj\n",
    "4 0 obj\n<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>\nendobj\n",
    `5 0 obj\n<< /Length ${stream.length} >>\nstream\n${stream}\nendstream\nendobj\n`,
  ];
  let body = "%PDF-1.4\n";
  const offsets = [0];
  for (const object of objects) {
    offsets.push(body.length);
    body += object;
  }
  const xref = body.length;
  body += `xref\n0 ${objects.length + 1}\n0000000000 65535 f \n`;
  body += offsets.slice(1).map((offset) => `${String(offset).padStart(10, "0")} 00000 n \n`).join("");
  body += `trailer\n<< /Size ${objects.length + 1} /Root 1 0 R >>\nstartxref\n${xref}\n%%EOF\n`;
  return body;
}

function pdfString(value: string): string {
  return value.replace(/[\\()]/g, (char) => `\\${char}`);
}

/**
 * A URL a frame can really load, standing in for the loopback origin.
 *
 * A `blob:` rather than a fake `http://127.0.0.1:…`, because the preview has no
 * server behind it and a URL that 404s would demonstrate the error state of
 * every report forever. It differs from production in one way worth knowing: a
 * blob inherits *this* document's origin, so the frame is same-origin here and
 * cross-origin in the app. What the preview is for is seeing the thing rendered
 * at the right size in the right place; the boundary itself is held still by
 * `FileBody.test.tsx` and `boundary.test.ts`, which do not depend on this.
 */
const SERVED = new Map<string, string>();

function servedUrl(path: string): string {
  const existing = SERVED.get(path);
  if (existing) return existing;
  const extension = path.slice(path.lastIndexOf(".") + 1);
  const body = extension === "pdf"
    ? PDF_FIXTURE
    : SHOWN[extension] ?? "<p>no fixture for this file</p>";
  const type = extension === "pdf" ? "application/pdf" : "text/html";
  const url = URL.createObjectURL(new Blob([body], { type }));
  SERVED.set(path, url);
  return url;
}

type WorkspaceKind = "file" | "directory" | "link";
/** `binary` is the `data:` URL the real `workspace_read_binary` would build. A
 *  node has one or the other, exactly as the two commands do. */
type WorkspaceNode = {
  kind: WorkspaceKind;
  text?: string;
  binary?: string;
  revision: number;
  truncated?: boolean;
  totalBytes?: number;
};
type WorkspaceFixture = {
  nodes: Record<string, WorkspaceNode>;
  changedAfterFirstSave: boolean;
};
type WorkspaceEntry = { name: string; path: string; kind: WorkspaceKind };
type WorkspaceText = {
  path: string;
  text: string;
  revision: string;
  fingerprint: string;
  bytes: number;
  truncated: boolean;
};
type WorkspaceStat = {
  path: string;
  fingerprint: string;
  bytes: number;
};
type WorkspaceBinary = { path: string; url: string; bytes: number };

/** A 64px mark, small enough to sit in the source: what an image in the tree
 *  looks like once the pane can open one at all. */
const PNG_FIXTURE =
  "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAEAAAABACAIAAAAlC+aJAAABQUlEQVR42uWaOw7CMBBE51w0nISz0yMOAS0SJLazM5snpLS25yXx/nW9XTae5+N+hmdDobYBzsCwLW8foJdhV9sQQBfDiLBRgDzDoKoJgCTDuKQ5gAzDlJ5pADfDrJgVAB/DgpJFAAfDmgwtr6xlWNagg+t71b/XqmSXLvUfAC0Mx09U+Y5J9V8AYgxVp8i6u1v9TwArQ+3Oip1k2lPJt+V4I4p9cdM/qcyd890oBeyG1aYpYLmtFlkZ7+PziYr5f5NHVzKGccRUCkeR5RGh8pFwbUyullykMKNQVz5VldOpMaMtyUjVnpX3A7C/APsOsK0Q2w+wPTE7FmJHo+x8gJ2RsXNidlWCXRdiV+bYtVF2dZrdH2B3aNg9MnaXkt0nZnfq2bMS+GmVf5wXYk9ssWfm2FOL7LlR9uQue3b65NPrL1rOMHuXA4KKAAAAAElFTkSuQmCC";

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
          'const focused = new Set(["workspace-tree", "workspace-file"]);',
          "export const hasWorkspaceView = (view: string) => focused.has(view);",
          "",
          ...Array.from({ length: 48 }, (_, index) => [
            `export function fixtureLine${index + 1}(value: number): number {`,
            `  return value + ${index + 1};`,
            "}",
            "",
          ]).flat(),
        ].join("\n"),
      },
      "crates/tcode-app/src/theme": { kind: "directory", revision: 1 },
      "crates/tcode-app/src/theme/preview.css": {
        kind: "file",
        revision: 1,
        text: ".workspace-preview { display: grid; gap: var(--s-3); }\n",
      },
      // The three files the pane used to get wrong, and the whole reason this
      // scene exists: a picture (which could not be opened at all), a diagram
      // that has to render behind the sandbox boundary, and a source file whose
      // resting state is highlighted rather than a textarea.
      icons: { kind: "directory", revision: 1 },
      "icons/mark.png": { kind: "file", revision: 1, binary: PNG_FIXTURE },
      "icons/mark.svg": {
        kind: "file",
        revision: 1,
        text: [
          '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64" width="64" height="64">',
          '  <rect width="64" height="64" fill="#eceadf"/>',
          '  <path d="M12 52 L32 12 L52 52 Z" fill="none" stroke="#3f5d3a" stroke-width="4"/>',
          "</svg>",
        ].join("\n"),
      },
      docs: { kind: "directory", revision: 1 },
      "docs/fixture-notes.md": {
        kind: "file",
        revision: 1,
        text: [
          "# Fixture notes",
          "",
          "Workspace paths are relative to the session root.",
          "",
          "## Editing contract",
          "",
          "Markdown opens as this restricted document and can switch to source without losing either scroll position.",
          "",
          "## Enough prose to scroll",
          "",
          ...Array.from(
            { length: 18 },
            (_, index) =>
              `Paragraph ${index + 1}. The preview keeps a reading position independently from the editor selection and undo history.`,
          ),
        ].join("\n\n"),
      },
      "docs/report.html": {
        kind: "file",
        revision: 1,
        text: [
          "<!doctype html>",
          '<html lang="en">',
          "  <head>",
          '    <meta charset="utf-8">',
          "    <title>Workspace HTML stays source</title>",
          "  </head>",
          "  <body>",
          "    <h1>This must not execute in a workspace pane</h1>",
          "    <script>document.body.dataset.executed = 'never';</script>",
          "  </body>",
          "</html>",
        ].join("\n"),
      },
      "docs/fixture-paper.pdf": { kind: "file", revision: 1 },
      fixtures: { kind: "directory", revision: 1 },
      "fixtures/truncated.log": {
        kind: "file",
        revision: 1,
        text: Array.from(
          { length: 36 },
          (_, index) => `${String(index + 1).padStart(4, "0")} preview log prefix`,
        ).join("\n"),
        truncated: true,
        totalBytes: 4_800_000,
      },
      "fixtures/conflict.ts": {
        kind: "file",
        revision: 1,
        text: "export const revision = 'disk';\n",
      },
      "empty-fixture": { kind: "directory", revision: 1 },
      "outside-workspace": { kind: "link", revision: 1 },
    }),
    b: workspace({
      "README.md": {
        kind: "file",
        revision: 1,
        text: "# duck_ext\n\nA separate session fixture.\n",
      },
      src: { kind: "directory", revision: 1 },
      "src/curve.py": {
        kind: "file",
        revision: 1,
        text: "def load_curve(date):\n    return date\n",
      },
      cache: { kind: "directory", revision: 1 },
    }),
    c: workspace({
      "README.md": {
        kind: "file",
        revision: 1,
        text: "# pybond\n\nIndependent workspace fixture.\n",
      },
      pybond: { kind: "directory", revision: 1 },
      "pybond/pricing.rs": {
        kind: "file",
        revision: 1,
        text: "pub fn price() -> f64 { 100.0 }\n",
      },
    }),
  };
}

let WORKSPACES = fixtureWorkspaces();

/** Reset only the mutable workspace state so fixture tests remain deterministic. */
export function resetPreviewFixtures(): void {
  WORKSPACES = fixtureWorkspaces();
}

function workspaceFor(
  args: Record<string, unknown> | undefined,
): [string, WorkspaceFixture] {
  const session = typeof args?.session === "string" ? args.session : "";
  const fixture = WORKSPACES[session];
  if (!fixture) throw new Error(`session '${session}' is not open`);
  return [session, fixture];
}

function relativePath(value: unknown, label: string): string {
  if (
    typeof value !== "string" ||
    value.length === 0 ||
    value.startsWith("/") ||
    value.includes("\\")
  ) {
    throw new Error(`${label} must be a non-empty workspace-relative path`);
  }
  if (
    value
      .split("/")
      .some((part) => part === "" || part === "." || part === "..")
  ) {
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

function textView(
  session: string,
  path: string,
  node: WorkspaceNode,
): WorkspaceText {
  const text = node.text ?? "";
  return {
    path,
    text,
    revision: revision(session, path, node.revision),
    // Metadata identity in the real wire; the fixture ties it to the same
    // revision number so a simulated collaborator write moves both.
    fingerprint: `fixture:fp:${session}:${path}:${node.revision}`,
    bytes: node.totalBytes ?? new TextEncoder().encode(text).length,
    truncated: node.truncated ?? false,
  };
}

/** The metadata answer the editor polls; revision-derived, like `textView`. */
function statWorkspace(args: Record<string, unknown> | undefined): WorkspaceStat {
  const [session, fixture] = workspaceFor(args);
  const path = relativePath(args?.path, "path");
  const node = fixture.nodes[path];
  if (!node) throw new Error(`workspace path '${path}' does not exist`);
  if (node.kind !== "file")
    throw new Error(`workspace path '${path}' is not a regular file`);
  return {
    path,
    fingerprint: `fixture:fp:${session}:${path}:${node.revision}`,
    bytes: node.totalBytes ?? new TextEncoder().encode(node.text ?? "").length,
  };
}

function listWorkspace(
  args: Record<string, unknown> | undefined,
): WorkspaceEntry[] {
  const [, fixture] = workspaceFor(args);
  const path =
    args?.path === null || args?.path === undefined
      ? ""
      : relativePath(args.path, "path");
  if (path && fixture.nodes[path]?.kind !== "directory")
    throw new Error(`workspace path '${path}' is not a directory`);
  if (path && !fixture.nodes[path])
    throw new Error(`workspace path '${path}' does not exist`);
  return Object.entries(fixture.nodes)
    .filter(([entryPath]) => parentOf(entryPath) === path)
    .map(([entryPath, node]) => entryFor(entryPath, node));
}

/** The completion menu's source, filtered exactly as `Workspace::complete`
 *  filters it — a fixture that matched more loosely would make the menu look
 *  better here than it is. */
function completeWorkspace(
  args: Record<string, unknown> | undefined,
): WorkspaceEntry[] {
  const prefix = typeof args?.prefix === "string" ? args.prefix : "";
  const cut = prefix.lastIndexOf("/");
  const directory = cut === -1 ? "" : prefix.slice(0, cut);
  const fragment = (cut === -1 ? prefix : prefix.slice(cut + 1)).toLowerCase();
  const [, fixture] = workspaceFor(args);
  if (directory && fixture.nodes[directory]?.kind !== "directory") return [];
  return Object.entries(fixture.nodes)
    .filter(([entryPath]) => parentOf(entryPath) === directory)
    .map(([entryPath, node]) => entryFor(entryPath, node))
    .filter((entry) => entry.name.toLowerCase().startsWith(fragment))
    .slice(0, 12);
}

/** Which mentions resolve. The backend answers with the subset that exists, so
 *  a path nobody has is simply absent — the tint arrives, or it does not. */
function presentWorkspace(args: Record<string, unknown> | undefined): string[] {
  const paths = Array.isArray(args?.paths) ? (args.paths as string[]) : [];
  const [, fixture] = workspaceFor(args);
  return paths.filter((path) => Boolean(fixture.nodes[path]));
}

function readWorkspace(
  args: Record<string, unknown> | undefined,
): WorkspaceText {
  const [session, fixture] = workspaceFor(args);
  const path = relativePath(args?.path, "path");
  const node = fixture.nodes[path];
  if (!node) throw new Error(`workspace path '${path}' does not exist`);
  if (node.kind !== "file")
    throw new Error(`workspace path '${path}' is not a regular file`);
  return textView(session, path, node);
}

function readWorkspaceBinary(
  args: Record<string, unknown> | undefined,
): WorkspaceBinary {
  const [, fixture] = workspaceFor(args);
  const path = relativePath(args?.path, "path");
  const node = fixture.nodes[path];
  if (!node) throw new Error(`workspace path '${path}' does not exist`);
  if (node.kind !== "file" || !node.binary) {
    throw new Error(`workspace path '${path}' is not a regular file`);
  }
  return { path, url: node.binary, bytes: node.binary.length };
}

function writeWorkspace(
  args: Record<string, unknown> | undefined,
): WorkspaceText {
  const [session, fixture] = workspaceFor(args);
  const path = relativePath(args?.path, "path");
  const text = typeof args?.text === "string" ? args.text : null;
  const expected = typeof args?.revision === "string" ? args.revision : "";
  const force = args?.force === true;
  const node = fixture.nodes[path];
  if (!node || node.kind !== "file")
    throw new Error(`workspace path '${path}' is not a regular file`);
  if (text === null) throw new Error("text must be a string");
  if (!force && expected !== revision(session, path, node.revision)) {
    throw new Error(
      `revision conflict: the file changed; re-read it before writing (current revision ${revision(session, path, node.revision)})`,
    );
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

function createWorkspace(
  args: Record<string, unknown> | undefined,
): WorkspaceEntry {
  const [, fixture] = workspaceFor(args);
  const parent =
    args?.parent === null || args?.parent === undefined
      ? ""
      : relativePath(args.parent, "parent");
  const name = relativePath(args?.name, "name");
  const kind = args?.kind;
  if (name.includes("/"))
    throw new Error("name must be one workspace path component");
  if (kind !== "file" && kind !== "directory")
    throw new Error(`'${String(kind)}' is not a workspace entry kind`);
  if (parent && fixture.nodes[parent]?.kind !== "directory")
    throw new Error(`workspace path '${parent}' is not a directory`);
  if (parent && !fixture.nodes[parent])
    throw new Error(`workspace path '${parent}' does not exist`);
  const path = parent ? `${parent}/${name}` : name;
  if (fixture.nodes[path])
    throw new Error(`workspace path '${path}' already exists`);
  const node: WorkspaceNode = {
    kind,
    revision: 1,
    ...(kind === "file" ? { text: "" } : {}),
  };
  fixture.nodes[path] = node;
  return entryFor(path, node);
}

function renameWorkspace(
  args: Record<string, unknown> | undefined,
): WorkspaceEntry {
  const [, fixture] = workspaceFor(args);
  const path = relativePath(args?.path, "path");
  const name = relativePath(args?.name, "name");
  if (name.includes("/"))
    throw new Error("name must be one workspace path component");
  const node = fixture.nodes[path];
  if (!node) throw new Error(`workspace path '${path}' does not exist`);
  const renamed = parentOf(path) ? `${parentOf(path)}/${name}` : name;
  if (fixture.nodes[renamed])
    throw new Error(`workspace path '${renamed}' already exists`);

  const moved = Object.entries(fixture.nodes).filter(
    ([candidate]) => candidate === path || candidate.startsWith(`${path}/`),
  );
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
  if (
    node.kind === "directory" &&
    Object.keys(fixture.nodes).some((candidate) =>
      candidate.startsWith(`${path}/`),
    )
  ) {
    throw new Error(`workspace directory '${path}' is not empty`);
  }
  delete fixture.nodes[path];
}

// ------------------------------------------------------------- the terminal

/** SGR by name, so the fixture below reads as output rather than as escapes. */
const SGR = {
  reset: "\u001b[0m",
  bold: "\u001b[1m",
  dim: "\u001b[2m",
  red: "\u001b[31m",
  green: "\u001b[32m",
  yellow: "\u001b[33m",
  blue: "\u001b[34m",
  magenta: "\u001b[35m",
  cyan: "\u001b[36m",
  brightBlack: "\u001b[90m",
};

/**
 * A session with something in it.
 *
 * Chosen to touch the slots that are actually hard on a light background —
 * green and red side by side, a dim grey that still has to be readable, a
 * bright colour used as emphasis rather than as "lighter". A prompt and a
 * blinking cursor would look fine and prove none of that.
 */
function mockSession(cwd: string): string {
  const name = cwd.slice(cwd.lastIndexOf("/") + 1);
  const prompt = `${SGR.green}${SGR.bold}~/code/${name}${SGR.reset} ${SGR.brightBlack}❯${SGR.reset} `;
  return [
    `${prompt}cargo test -p tcode-core\r\n`,
    `${SGR.brightBlack}   Compiling${SGR.reset} tcode-core v0.2.3\r\n`,
    `${SGR.green}${SGR.bold}    Finished${SGR.reset} test profile in 4.21s\r\n`,
    `\r\n`,
    `running 148 tests\r\n`,
    `${SGR.dim}test ledger::tests::compact_keeps_the_prefix ... ${SGR.reset}${SGR.green}ok${SGR.reset}\r\n`,
    `${SGR.dim}test memory::tests::discovers_nested_agents_md ... ${SGR.reset}${SGR.green}ok${SGR.reset}\r\n`,
    `${SGR.dim}test watchdog::tests::two_waiters_one_clock ... ${SGR.reset}${SGR.red}FAILED${SGR.reset}\r\n`,
    `\r\n`,
    `${SGR.red}${SGR.bold}failures:${SGR.reset}\r\n`,
    `    ${SGR.yellow}watchdog::tests::two_waiters_one_clock${SGR.reset}\r\n`,
    `\r\n`,
    `test result: ${SGR.red}FAILED${SGR.reset}. 147 passed; ${SGR.red}1 failed${SGR.reset}\r\n`,
    `\r\n`,
    `${prompt}git status ${SGR.brightBlack}--short${SGR.reset}\r\n`,
    ` ${SGR.red}M${SGR.reset} crates/tcode-core/src/watchdog.rs\r\n`,
    ` ${SGR.green}A${SGR.reset} ${SGR.cyan}crates/tcode-app/src/terminal.rs${SGR.reset}\r\n`,
    ` ${SGR.magenta}??${SGR.reset} ${SGR.blue}scratch/notes.md${SGR.reset}\r\n`,
    `\r\n`,
    prompt,
  ].join("");
}

/** How many browser tabs the fixture believes are open, which is all it takes
 *  to answer `browser_close` the way the real backend does — the last webview
 *  is blanked rather than destroyed. */
let browserTabs = 0;

/** The terminal preference keeps state in the preview just as it does through
 * `[tcode_state]` in the real selected user config. */
let terminalShell = "";

let mockTerminals = 0;

function openMockTerminal(cwd: string): string {
  const id = `preview-term-${(mockTerminals += 1)}`;
  // A tick later, the way a real shell answers after the PTY exists — the
  // frontend holds early chunks for exactly this reason, and a fixture that
  // answered synchronously would never exercise it.
  setTimeout(() => {
    deliver(TERMINAL_OUTPUT, { id, data: base64(mockSession(cwd)) });
  }, 0);
  return id;
}

function echoMockTerminal(id: string, data: string) {
  // Carriage return alone leaves the cursor on the same line in a terminal, so
  // Enter has to be answered with both halves — the same thing every shell does.
  const typed = atob(data).replace(/\r/g, "\r\n");
  deliver(TERMINAL_OUTPUT, { id, data: btoa(typed) });
}

function base64(text: string): string {
  const bytes = new TextEncoder().encode(text);
  let binary = "";
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary);
}

export async function invoke<T>(
  command: string,
  args?: Record<string, unknown>,
): Promise<T> {
  switch (command) {
    // The shell's own verbs. The title bar is part of the layout, so the
    // preview has to draw it; there is no window behind it in a browser tab,
    // so acting is a no-op and the state is the one a window starts in.
    case "window_is_maximized":
      return false as T;
    case "window_minimize":
    case "window_toggle_maximize":
    case "window_close":
      return undefined as T;
    case "dialog_open_folder":
      return "/home/teamon/code/rust/tcode" as T;
    case "project_list":
      return PROJECTS as T;
    case "project_sessions": {
      const before = typeof args?.before === "string" ? args.before : null;
      const start = before
        ? Math.max(0, HISTORY.findIndex((session) => session.id === before) + 1)
        : 0;
      const sessions = HISTORY.slice(start, start + 20);
      const page: StoredSessionsPage = {
        sessions,
        next:
          start + sessions.length < HISTORY.length
            ? (sessions.at(-1)?.id ?? null)
            : null,
      };
      return page as T;
    }
    case "sessions":
      return OPEN as T;
    case "change_folder":
    case "open_folder":
      return {
        session: {
          id: "preview",
          cwd: String(args?.path ?? "/home/teamon/code/rust/tcode"),
          name: "tcode",
          home: "/home/teamon",
          log_id: "0193f1",
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
        throw new Error(
          `'${name}' is not a usable preset name — letters, digits, '-' and '_' only`,
        );
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
      const body =
        SHOWN[path.slice(path.lastIndexOf(".") + 1)] ??
        "no fixture for this file";
      return { body, bytes: body.length, truncated: false } as T;
    }
    case "serve_url":
      return servedUrl(String(args?.path ?? "")) as T;
    // The browser's verbs all succeed and do nothing, which is the honest
    // fixture: the page is a native webview the OS composites over the pane,
    // and a design preview running in an ordinary browser tab has nothing to
    // composite with. So the scene shows the chrome — which is the part that
    // was designed here — over the empty rectangle the webview would occupy.
    // Faking a page inside it with an iframe would be showing something the
    // app never draws, and would quietly answer the one question this pane
    // raises (does the native layer land where the DOM says) with a yes.
    //
    // The two that answer something have to answer honestly, because the strip
    // is drawn from those answers: `browser_open` hands back the tab's id, and
    // `browser_close` says whether the webview is gone — `false` for the last
    // one, which the real backend blanks and keeps (`browser.rs`). A fixture
    // that returned `undefined` here would show a tab strip that never gains a
    // tab, which is the one part of this pane the preview exists to show.
    case "browser_open":
      return `preview-tab-${(browserTabs += 1)}` as T;
    case "browser_close":
      return ((browserTabs -= 1) > 0) as T;
    case "browser_show":
    case "browser_select":
    case "browser_bounds":
    case "browser_visible":
    case "browser_navigate":
    case "browser_step":
    case "browser_reload":
      return undefined as T;
    case "desktop_settings":
      return { terminal_shell: terminalShell } as T;
    case "set_terminal_shell":
      terminalShell = String(args?.shell ?? "").trim();
      return { terminal_shell: terminalShell } as T;
    // The terminal is the opposite case to the browser above: it *is* drawn on
    // this side, by an emulator painting the app's own palette, so a fixture
    // can show the real thing. What it plays back is a session with colour in
    // it — that is the part being designed, and an empty prompt would prove
    // nothing about sixteen ANSI slots on a paper background.
    case "terminal_open":
      return openMockTerminal(String(args?.cwd ?? "")) as T;
    case "terminal_write":
      // Echo, so the preview can be typed into. A real PTY echoes because the
      // shell does; here it is the shortest way to make the cursor real.
      echoMockTerminal(String(args?.id ?? ""), String(args?.data ?? ""));
      return undefined as T;
    case "terminal_resize":
    case "terminal_close":
      return undefined as T;
    case "workspace_list":
      return listWorkspace(args) as T;
    case "workspace_complete":
      return completeWorkspace(args) as T;
    case "workspace_present":
      return presentWorkspace(args) as T;
    // The desktop surface implements exactly these three, and core's own help
    // is the text beside them (`commands.rs::slash_commands`).
    case "slash_commands":
      return [
        { name: "/compact", help: "summarize history · /compact <focus>" },
        { name: "/clear", help: "start a fresh conversation" },
      ] as T;
    case "workspace_read_text":
      return readWorkspace(args) as T;
    case "workspace_stat":
      return statWorkspace(args) as T;
    case "workspace_read_binary":
      return readWorkspaceBinary(args) as T;
    case "workspace_write_text":
      return writeWorkspace(args) as T;
    case "workspace_create":
      return createWorkspace(args) as T;
    case "workspace_rename":
      return renameWorkspace(args) as T;
    case "workspace_delete":
      deleteWorkspace(args);
      return undefined as T;
    case "workspace_trash":
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
