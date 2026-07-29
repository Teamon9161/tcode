/**
 * Stand-in for `@tauri-apps/api/core` in the design preview.
 *
 * The preview exists so the interface can be looked at — every state, in a
 * browser, without a provider or a running turn. It is aliased in only when
 * `PREVIEW=1`, so the shipped bundle never contains it.
 */
import type { Launchpad, OpenedSession, SessionInfo, StoredSession } from "../types";

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
 *  show the real strings rather than placeholders. */
const MODES = [
  { key: "default", label: "Ask first", detail: "Rules decide; anything else asks you." },
  {
    key: "accept-edits",
    label: "Accept edits",
    detail: "File edits go through; commands still ask.",
  },
  {
    key: "auto",
    label: "Auto",
    detail: "Runs without prompting; a safety classifier reviews the rest.",
  },
  {
    key: "unsafe",
    label: "Bypass permissions",
    detail: "Nothing asks. Deny rules still apply. For isolated environments.",
  },
];

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
      } satisfies OpenedSession as T;
    // The composer strip. Deliberately not the careful default: the amber mode
    // chip and a model with an effort dial are the states worth looking at.
    case "picker_state":
      return {
        models: [
          { profile: "anthropic", label: "Opus 5", efforts: ["low", "medium", "high"] },
          { profile: "anthropic", label: "Sonnet 5", efforts: ["low", "medium", "high"] },
          { profile: "openai", label: "gpt-5.1-codex", efforts: ["medium", "high"] },
        ],
        model: 0,
        effort: "high",
        presets: [
          { key: "quant", label: "quant" },
          { key: "cheap", label: "cheap" },
        ],
        preset: 0,
        modes: MODES,
        mode: "accept-edits",
        mode_staged: false,
      } as T;
    case "choose_model":
    case "choose_preset":
    case "choose_mode":
      return undefined as T;
    case "shown_file": {
      const path = String(args?.path ?? "");
      const body = SHOWN[path.slice(path.lastIndexOf(".") + 1)] ?? "no fixture for this file";
      return { body, bytes: body.length, truncated: false } as T;
    }
    default:
      return undefined as T;
  }
}
