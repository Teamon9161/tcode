import { useEffect, useState } from "react";

import type { ApprovalRequest, Queued, SessionInfo, Status } from "../types";
import type { Block } from "../blocks";
import type { TouchedFile } from "../files";
import type { Pasted } from "../paste";
import { BLANK, LimitsContext, type SessionState } from "../session";
import { NO_METER, type Limits, type Meter } from "../usage";
import {
  EMPTY,
  openAside,
  openInspect,
  openWeb,
  panes,
  single,
  split,
  toggleTerminal,
  type Tiling,
} from "../layout";
import { ToolMetaProvider, type ToolMeta } from "../toolViews";
import { DisplayContext, DISPLAY_DEFAULT, type Display } from "../display";
import type { RewindTarget } from "../rewind";
import { draftOf, type Plan, type PlanComment } from "../plan";
import { Workspace } from "../Workspace";

/**
 * The design preview: every screen and state, side by side, in a browser.
 *
 * It renders the real components against fixture data — not a mock-up of them —
 * so what is looked at here is what ships. Reachable with `npm run preview:ui`.
 *
 * The fixtures are deliberately the hard cases, because the easy ones never
 * needed looking at: a conversation carrying every kind of rich block, two
 * sub-agents at once, a batch, a queued message, a diff worth folding.
 */

const HOME = "/home/teamon";
const ROOT = "/home/teamon/code/rust/tcode";

/** `a`'s log is the first entry in `mock-core`'s history on purpose: it is the
 *  one state the merged rail had to get right — a conversation that is open
 *  *and* has a log on disk must appear once, not as a live row plus a resume
 *  target that would put a second ledger on the same file. */
const SESSIONS: SessionInfo[] = [
  { id: "a", cwd: ROOT, name: "tcode", home: HOME, log_id: "0193f0" },
  {
    id: "b",
    cwd: "/home/teamon/code/py/duck_ext",
    name: "duck_ext",
    home: HOME,
    log_id: "0193d2",
  },
  {
    id: "c",
    cwd: "/home/teamon/code/rust/pybond",
    name: "pybond",
    home: HOME,
    log_id: null,
  },
];

const STATUS: Record<string, Status> = {
  a: "running",
  b: "waiting",
  c: "idle",
};

/**
 * The usage strip's world. Deliberately the states worth looking at rather than
 * the comfortable ones: a window a third full with a real receipt behind it, a
 * second conversation whose figure is still an estimate because it was resumed,
 * and a subscription whose weekly window is tight enough to have climbed onto
 * the strip on its own.
 */
const METER: Meter = {
  context: 68_400,
  estimated: false,
  turn: {
    input_tokens: 1_178,
    output_tokens: 3_902,
    cache_read_tokens: 61_200,
    cache_write_tokens: 4_100,
  },
};

const OTHER_METER: Meter = { ...NO_METER, context: 21_500, estimated: true };

const LIMITS: Limits = {
  primary: {
    used_percent: 34,
    window_minutes: 300,
    resets_at: Math.floor(Date.now() / 1000) + 4_800,
  },
  secondary: {
    used_percent: 91,
    window_minutes: 10_080,
    resets_at: Math.floor(Date.now() / 1000) + 259_200,
  },
};

/**
 * What each conversation is doing, for the rail row and the finder.
 *
 * One map for both, because it is one fact. The rail needs it for the case the
 * fixture is built around: two of these are the same folder name away from being
 * indistinguishable rows, and the activity line is the only thing that tells them
 * apart.
 */
/* Phases as `activity.ts` produces them, not prose about them: the running one
   is what the live line and the rail both draw, so a fixture that writes a
    sentence here shows a line the app can never actually be in. `a` is at the
    generic tool-execution phase, which deliberately leaves the file name in
    the trace instead of repeating it by the composer. */
const ACTIVITY: Record<string, string> = {
  a: "calling a tool",
  b: "waiting on shell",
  c: "done",
};

/** Mirrors what `tool_views()` returns for the real tool set. */
const TOOL_META = new Map<string, ToolMeta>(
  (
    [
      // `display_name` is core's own (`Tool::display_name`), so these are the
      // real answers and not prettier ones: `shell` really does show as "Run".
      {
        name: "read",
        display_name: "Read",
        route: "transcript",
        quiet_output: true,
        hide_success_result: false,
      },
      {
        name: "edit",
        display_name: "Edit",
        route: "transcript",
        quiet_output: false,
        hide_success_result: true,
      },
      {
        name: "write",
        display_name: "Write",
        route: "transcript",
        quiet_output: false,
        hide_success_result: true,
      },
      {
        name: "shell",
        display_name: "Run",
        route: "transcript",
        quiet_output: false,
        hide_success_result: false,
      },
      {
        name: "grep",
        display_name: "Search",
        route: "transcript",
        quiet_output: true,
        hide_success_result: false,
      },
      {
        name: "agent",
        display_name: "Agent",
        route: "transcript",
        quiet_output: false,
        hide_success_result: false,
      },
      {
        name: "skill",
        display_name: "Skill",
        route: "transcript",
        quiet_output: false,
        hide_success_result: false,
      },
      // Every field here is what the backend really derives for `ShowTool`, not
      // a plausible-looking set: `quiet_output` follows from its
      // `BatchPolicy::ParallelReadOnly`, and `hide_success_result` is false
      // because `BODY_IS_THE_RESULT` lists only the four editing tools.
      {
        name: "show",
        display_name: "Show",
        route: "transcript",
        quiet_output: true,
        hide_success_result: false,
      },
      {
        name: "progress",
        display_name: "Progress",
        route: "progress",
        quiet_output: false,
        hide_success_result: false,
      },
      // The retired name a resumed session still holds, aliased onto the live
      // tool exactly as `RETIRED_NAMES` in `commands.rs` does it. In the fixture
      // for the same reason it is in the backend: without it this scene showed a
      // phase flip as a tool card in the transcript, which is what a resumed
      // conversation used to do.
      {
        name: "update_progress",
        display_name: "Progress",
        route: "progress",
        quiet_output: false,
        hide_success_result: false,
      },
      {
        name: "ask_user",
        display_name: "Ask_user",
        route: "silent",
        quiet_output: false,
        hide_success_result: false,
      },
    ] as ToolMeta[]
  ).map((meta) => [meta.name, meta]),
);

const AGENT_FILE = `${ROOT}/crates/tcode-core/src/agent/mod.rs`;

/** A real image, small enough to sit in the source: what a pasted screenshot
 *  looks like as a thumbnail, and what the pane enlarges. */
const PASTED = `data:image/svg+xml;base64,${btoa(
  `<svg xmlns="http://www.w3.org/2000/svg" width="480" height="300" viewBox="0 0 480 300">
     <rect width="480" height="300" fill="#eceadf"/>
     <path d="M40 250 L140 190 L240 210 L340 120 L440 70" fill="none" stroke="#3f5d3a" stroke-width="4"/>
     <path d="M40 40 V260 H450" fill="none" stroke="#9a9887" stroke-width="2"/>
     <text x="52" y="34" font-family="sans-serif" font-size="16" fill="#4a4838">retry latency, 3 attempts</text>
   </svg>`,
)}`;

const BLOCKS: Block[] = [
  // The scene opens after a compaction, because that boundary is the one thing
  // in a transcript that has no other way to be looked at: it only exists in a
  // conversation long enough to have run out of window.
  {
    kind: "compact",
    summary:
      "The conversation began with a survey of the retry path. `user_turn` was found to sleep inline via `tokio::time::sleep`, `step` was ruled out as the place to fix it, and two designs were weighed: threading a `Duration` through every call, or a `Sleeper` trait with a test double. The second was chosen. Nothing has been written to disk yet.",
  },
  {
    kind: "user",
    text: "Make the retry path testable — right now the backoff sleeps for real.",
    images: [PASTED],
  },
  {
    kind: "thinking",
    text: "The retry loop sleeps inline, so a test has to wait out the real delay. Injecting a clock is the smallest change that makes it observable.",
  },
  {
    kind: "assistant",
    text: "The backoff calls `tokio::time::sleep` directly inside `user_turn`, so any test that exercises a retry pays the real delay. I'll take the delay as a parameter instead.\n\nReading the loop first.",
  },
  {
    kind: "tool",
    callId: "t1",
    name: "read",
    summary: "crates/tcode-core/src/agent/mod.rs",
    input: { file_path: AGENT_FILE },
    result: {
      preview: "537 lines",
      content: `pub async fn user_turn(&self, session: &mut Session) -> Result<(), AgentError> {
    let mut attempt = 0;
    loop {
        match self.step(session).await {
            Ok(done) => return Ok(done),
            Err(error) if attempt < self.watchdog.max_retries => {
                attempt += 1;
                let delay = backoff(attempt);
                // The sleep under test.
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
            Err(error) => return Err(error),
        }
    }
}`,
      isError: false,
    },
  },
  {
    kind: "tool",
    callId: "p1",
    name: "update_progress",
    summary: "3 phases",
    input: {
      phases: [
        { phase: "Phase 1 — locate the inline sleep", status: "completed" },
        {
          phase: "Phase 2 — thread an injectable clock",
          status: "in_progress",
        },
        {
          phase: "Phase 3 — cover the retry path with a test",
          status: "pending",
        },
      ],
    },
    result: { preview: "progress updated", content: "", isError: false },
  },
  // The long answer: every rich block the transcript can draw, and — at the end
  // — one link of each kind there is. Both are here because a link is the one
  // affordance whose failure is silence: it looked identical before anything
  // answered a click, so a scene without one could not tell the two apart. The
  // page goes to the window's browser, the path to the same viewer `show` opens
  // (`links.ts`).
  {
    kind: "assistant",
    text: `Found it on line 604. The fix threads a \`Sleeper\` through so tests can pass an instant one.

### What changes

| piece | before | after |
|---|---|---|
| \`Agent\` | — | holds a \`Sleeper\` |
| retry loop | \`tokio::time::sleep\` | \`self.sleeper.sleep\` |
| tests | real delay | instant |

The backoff itself is unchanged — exponential with a cap of $30000$ ms:

$$d_n = \\min\\!\\left(2^{n} \\cdot 100,\\; 30000\\right)$$

Growth over the first eight attempts, so the cap is visible:

\`\`\`echarts
{
  "height": 240,
  "grid": { "left": 52, "right": 20, "top": 24, "bottom": 36 },
  "xAxis": { "type": "category", "data": ["1","2","3","4","5","6","7","8"], "name": "attempt" },
  "yAxis": { "type": "value", "name": "ms" },
  "series": [{ "type": "line", "smooth": true, "areaStyle": {}, "data": [200,400,800,1600,3200,6400,12800,30000] }]
}
\`\`\`

And where the clock enters:

\`\`\`mermaid
flowchart LR
  A[user_turn] --> B{step ok?}
  B -- yes --> C[return]
  B -- no --> D[backoff n]
  D --> E[Sleeper::sleep]
  E --> A
\`\`\`

- [x] locate the sleep
- [ ] thread the clock
- [ ] cover it with a test

See [the retry notes](https://example.com/retry) for the original reasoning, and
[the measurements](out/carry.csv) for what the delay costs today.`,
  },
  {
    kind: "tool",
    callId: "t2",
    name: "edit",
    summary: "crates/tcode-core/src/agent/mod.rs",
    input: {
      file_path: AGENT_FILE,
      old_string: `                attempt += 1;
                let delay = backoff(attempt);
                // The sleep under test.
                tokio::time::sleep(Duration::from_millis(delay)).await;`,
      new_string: `                attempt += 1;
                let delay = backoff(attempt);
                // Injected so a test can pass an instant clock.
                self.sleeper.sleep(Duration::from_millis(delay)).await;`,
    },
    result: { preview: "1 hunk", content: "", isError: false },
  },
  // A rejected edit, which is the shape the transcript got wrong for longest: it
  // drew the diff anyway, so a change that never touched the file was rendered
  // in the same red and green as one that did, under a message the disclosure
  // then repeated verbatim (`preview` is the first line of `content`, and a tool
  // error is one line). Nothing in the fixtures failed, so nothing showed it.
  {
    kind: "tool",
    callId: "t3",
    name: "edit",
    summary: "crates/tcode-app/tests/bridge.rs",
    input: {
      file_path: `${ROOT}/crates/tcode-app/tests/bridge.rs`,
      old_string: "    let first_request = collector",
      new_string:
        '    let first_request = collector\n        .wait_for(APPROVAL_REQUEST, |payload| payload["session"] == "s1")',
    },
    result: {
      preview:
        "target_line 696 does not contain an exact old_string occurrence",
      content:
        "target_line 696 does not contain an exact old_string occurrence",
      isError: true,
    },
  },
  {
    kind: "batch",
    // Core's own `batch_label`, verbatim: it title-cases the tool, so a fixture
    // that wrote "reading 3 files" was the reason two casings for one phrase went
    // unnoticed sitting in the same column.
    label: "Read 3 files",
    blocks: [
      {
        kind: "tool",
        callId: "b1",
        name: "read",
        summary: "crates/tcode-core/src/session.rs",
        input: { file_path: `${ROOT}/crates/tcode-core/src/session.rs` },
        result: {
          preview: "212 lines",
          content: "pub struct Session { /* … */ }",
          isError: false,
        },
      },
      {
        kind: "tool",
        callId: "b2",
        name: "read",
        summary: "crates/tcode-core/src/provider.rs",
        input: { file_path: `${ROOT}/crates/tcode-core/src/provider.rs` },
        result: {
          preview: "398 lines",
          content: "pub trait Provider { /* … */ }",
          isError: false,
        },
      },
      {
        kind: "tool",
        callId: "b3",
        name: "read",
        summary: "crates/tcode-core/src/types.rs",
        input: { file_path: `${ROOT}/crates/tcode-core/src/types.rs` },
        result: {
          preview: "94 lines",
          content: "pub enum ContentBlock { /* … */ }",
          isError: false,
        },
      },
    ],
  },
  // A shown report, at its call site — the shape `show` actually produces most
  // of the time, since an artifact draws where the conversation is and the
  // pop-out is what moves it (rule 13). It is a `.html`, so this is the inline
  // half of the framed path: the one place `.shown.is-inline.is-framed` is on
  // screen, and the one that has to prove a page in the reading column takes a
  // band of it rather than a scrollbar inside a scrollbar.
  {
    kind: "tool",
    callId: "s1",
    name: "show",
    summary: "out/carry.html",
    input: {
      path: "/home/teamon/code/py/duck_ext/out/carry.html",
      label: "Carry report",
    },
    result: {
      preview:
        "shown: /home/teamon/code/py/duck_ext/out/carry.html (html, 4.1 KB)",
      content:
        "shown: /home/teamon/code/py/duck_ext/out/carry.html (html, 4.1 KB)",
      isError: false,
    },
  },
  // A skill call, which used to reach the transcript as the bare word `skill`:
  // core's generic summary has no key for this tool's argument.
  {
    kind: "tool",
    callId: "k1",
    name: "skill",
    summary: "skill",
    input: { name: "impeccable", arguments: "audit the trace column" },
    result: {
      preview: "# Skill: impeccable",
      content: "# Skill: impeccable\n\n…",
      isError: false,
    },
  },
  // A delegating call and its run, paired by `parent_call` the way the wire pairs
  // them. Both are here because the transcript's job is to draw them as one step:
  // with the call's row alone the reader got `agent · agent(explore)` above a row
  // that already said everything.
  {
    kind: "tool",
    callId: "a1",
    name: "agent",
    summary: "agent(explore)",
    input: {
      agent: "explore",
      prompt: "Search the workspace for direct calls to sleep.",
    },
  },
  {
    kind: "run",
    run: "r1",
    meta: {
      kind: "explore",
      model: "claude-opus-5",
      summary: "Find every other place that sleeps inline",
      prompt:
        "Search the workspace for direct calls to tokio::time::sleep outside tests.\n\nReport, for each hit:\n\n- the file and line\n- whether it is inside a retry loop\n- whether a test already covers it\n\nDo not edit anything.",
      parentCall: "a1",
    },
    blocks: [
      {
        kind: "assistant",
        text: "Searching for direct sleeps outside test modules.",
      },
      {
        kind: "tool",
        callId: "r1t1",
        name: "shell",
        summary: "rg -n 'tokio::time::sleep' crates/",
        input: { command: "rg -n 'tokio::time::sleep' crates/" },
      },
    ],
  },
  {
    kind: "tool",
    callId: "a2",
    name: "agent",
    summary: "agent(general)",
    input: { agent: "general", prompt: "Write a Sleeper trait." },
    result: {
      preview: "Added `Sleeper` with two implementations.",
      content:
        "Added `Sleeper` in `agent/retry.rs` with two implementations:\n\n- `RealSleeper` — delegates to `tokio::time::sleep`.\n- `InstantSleeper` — returns immediately and records the delay it was asked for.\n\nThe retry loop takes `&dyn Sleeper`, so no type parameter spreads through `Agent`.",
      isError: false,
    },
  },
  {
    kind: "run",
    run: "r2",
    meta: {
      kind: "general",
      model: "claude-sonnet-5",
      summary: "Draft the Sleeper trait and its test double",
      prompt:
        "Write a Sleeper trait with a real and an instant implementation.",
      parentCall: "a2",
      // The real wire value (`TaskRunStatus::Done`). It said "ok" here, which no
      // status has ever been — so the transcript's `status === "ok"` test looked
      // right in the preview and drew a red cross on every finished run in the
      // actual app.
      status: "done",
      toolCalls: 4,
    },
    blocks: [
      {
        kind: "assistant",
        text: "Added `Sleeper` with `RealSleeper` and `InstantSleeper`.\n\n```rust\npub trait Sleeper: Send + Sync {\n    /// Instant in tests, real in production.\n    fn sleep(&self, delay: Duration) -> BoxFuture<'_, ()>;\n}\n```",
      },
    ],
  },
  // The other two endings, which are neither a success nor an error and had no
  // way to say so: both used to wear the failure cross.
  {
    kind: "run",
    run: "r3",
    meta: {
      kind: "explore",
      model: "claude-haiku-4-5",
      summary: "Survey the watchdog's other timers",
      prompt: "List every timer the watchdog owns.",
      parentCall: "",
      status: "interrupted",
      toolCalls: 2,
    },
    blocks: [
      { kind: "assistant", text: "Found two so far — the step deadline and…" },
    ],
  },
  { kind: "note", text: "retrying (1/3): connection reset by peer" },
  // A folded harness note, in the shape core actually emits it: headline first,
  // the model-facing remainder after the newline. Without one here the fold has
  // no state to be seen in, which is how it went un-designed for so long.
  {
    kind: "note",
    text:
      "Monitor m1 (test failures in the watch log): 2 new event lines:\n" +
      "FAILED tcode_core::watchdog::step_deadline_trips\n" +
      "FAILED tcode_core::watchdog::cap_is_thirty_seconds\n" +
      "Full log: /home/me/.tcode/projects/tcode/scratchpad/tool-output/m1.log",
  },
  {
    kind: "tool",
    callId: "t3",
    name: "shell",
    summary: "cargo test -p tcode-core retry",
    input: { command: "cargo test -p tcode-core retry" },
  },
  { kind: "user", text: "also add a test for the cap at 30s", entryIndex: 12 },
];

/** What the backend would answer for `BLOCKS`: its one real prompt, at the
 *  ledger index that prompt actually sits at, and an era that touched files. */
const REWIND_TARGETS: RewindTarget[] = [
  {
    index: 1,
    text: "Make the retry path testable — right now the backoff sleeps for real.",
    dirty: true,
  },
];

const FILES: TouchedFile[] = [
  {
    path: AGENT_FILE,
    action: "edited",
    calls: ["t1", "t2"],
    pending: false,
    failed: false,
    run: null,
  },
  {
    path: `${ROOT}/crates/tcode-core/src/agent/retry.rs`,
    action: "created",
    calls: ["t4"],
    pending: false,
    failed: false,
    run: "r2",
  },
  {
    path: `${ROOT}/crates/tcode-core/src/session.rs`,
    action: "read",
    calls: ["b1"],
    pending: false,
    failed: false,
    run: null,
  },
];

/* The descriptor and summary are core's own, verbatim (`edit.rs::permission`).
   They used to read as a path and a sentence somebody wrote, which is exactly
   the pair of strings the panel is built to reconcile — a fixture that softens
   them hides the thing under design. */
const APPROVAL: ApprovalRequest = {
  session: "a",
  id: "ap1",
  tool: "edit",
  summary: "edit crates/tcode-core/src/agent/mod.rs",
  descriptor: "edit(crates/tcode-core/src/agent/mod.rs)",
  is_edit: true,
  allows_project: true,
  input: {
    file_path: "crates/tcode-core/src/agent/mod.rs",
    old_string:
      "                tokio::time::sleep(Duration::from_millis(delay)).await;",
    new_string:
      "                self.sleeper.sleep(Duration::from_millis(delay)).await;",
  },
};

/** The other authorization shape, and the one the panel was reworked for: a
 *  command, which core describes twice — `run(cmd)` as the rule and `run: cmd`
 *  as the sentence — and which arrives with its own line breaks that no
 *  one-line summary can carry. */
const COMMAND: ApprovalRequest = {
  session: "a",
  id: "ap4",
  tool: "shell",
  summary: `run: cargo test -p tcode-core --test agent_loop -- --nocapture \\
  | rg -v '^\\s*$' | tail -40`,
  descriptor: `run(cargo test -p tcode-core --test agent_loop -- --nocapture \\
  | rg -v '^\\s*$' | tail -40)`,
  is_edit: false,
  allows_project: true,
  input: {
    command: `cargo test -p tcode-core --test agent_loop -- --nocapture \\
  | rg -v '^\\s*$' | tail -40`,
    cwd: `${ROOT}/crates/tcode-core`,
    timeout_ms: 600000,
  },
};

/** The other approval shape: a question, which is not a yes/no at all. */
const QUESTION: ApprovalRequest = {
  session: "a",
  id: "ap2",
  tool: "ask_user",
  summary: "How should the clock be injected?",
  descriptor: "ask_user",
  is_edit: false,
  allows_project: false,
  input: {
    questions: [
      {
        question: "How should the clock be injected?",
        options: [
          {
            label: "Trait object",
            description: "A `dyn Sleeper` on the agent — swappable at runtime.",
            preview: "pub struct Agent {\n    sleeper: Arc<dyn Sleeper>,\n}",
          },
          {
            label: "Generic parameter",
            description:
              "Monomorphized, no dynamic dispatch, but it spreads through the type.",
            preview: "pub struct Agent<S: Sleeper> {\n    sleeper: S,\n}",
          },
        ],
      },
      {
        question: "What should the tests cover?",
        multiSelect: true,
        options: [
          {
            label: "The backoff curve",
            description: "Delays grow and then cap.",
          },
          {
            label: "The cancel path",
            description: "A cancelled retry stops sleeping.",
          },
          {
            label: "The error passthrough",
            description: "The last error survives the retries.",
          },
        ],
      },
    ],
  },
};

/** The second conversation in the split scene. Short on purpose: the point of
 *  that scene is the layout, and a second full transcript would only compete
 *  with the first for attention. */
const OTHER_BLOCKS: Block[] = [
  {
    kind: "user",
    text: "Why is the bond curve loader re-fetching on every call?",
  },
  {
    kind: "assistant",
    text: "The cache key includes the request timestamp, so no two calls ever hit. Checking where it is built.",
  },
  {
    kind: "tool",
    callId: "d1",
    name: "shell",
    summary: "rg -n 'cache_key' src/duck_ext",
    input: { command: "rg -n 'cache_key' src/duck_ext" },
  },
];

/**
 * A plan, as the backend reads it off disk.
 *
 * Deliberately the hard case: prose on every phase (which is what the review
 * panel edits), a sub-phase under the running one, and a title long enough to
 * need the strip's ellipsis.
 */
const PLAN: Plan = {
  path: `${HOME}/.tcode/projects/c--code-rust-tcode/progress/20260730-101200-testable-retry-path.md`,
  file: "20260730-101200-testable-retry-path.md",
  title: "Make the retry path testable",
  description: "make the retry loop's waiting injectable so it can be tested",
  background:
    "## Decision\nWorth doing. The retry loop is the one path no test covers, and the reason is a single `tokio::time::sleep` on wall-clock time.\n\n## What the investigation established\n`agent/mod.rs` is the only place in the crate that waits on wall-clock time; nothing else has to move for a clock to be injectable.\n\n## Ruled out\nShortening the delays under `cfg(test)`: it makes the tests fast without making them deterministic, which is the half that matters.",
  state: "active",
  done: 1,
  total: 4,
  phases: [
    {
      phase: "locate every inline sleep in the retry loop",
      status: "completed",
      detail:
        "Read-only. `agent/mod.rs` sleeps inside the `'retry` loop; confirm nothing else in the crate waits on wall-clock time before the shape is chosen.",
      phases: [],
    },
    {
      phase: "thread an injectable clock through Agent",
      status: "in_progress",
      detail:
        "Add a `Sleeper` the agent holds and the retry loop calls. Risk: the watchdog also sleeps, on its own task — if both take the injected clock, a test that advances time affects the stall detector too.",
      phases: [
        {
          phase: "write the failing test first",
          status: "completed",
          detail:
            "One retry, instant clock, asserts the second attempt happened.",
          phases: [],
        },
        {
          phase: "replace the call in the retry loop",
          status: "in_progress",
          detail: "",
          phases: [],
        },
      ],
    },
    {
      phase: "cover cancellation while a retry is waiting",
      status: "pending",
      detail:
        "A cancelled turn must stop sleeping rather than finish its backoff. This is the phase most likely to find a real bug.",
      phases: [],
    },
  ],
};

/** The plan under review: same file, not yet approved. */
const DRAFT_PLAN: Plan = { ...PLAN, state: "draft", done: 0 };

/** A plan submitted for approval. The body is what core saved before asking. */
const PLAN_REVIEW: ApprovalRequest = {
  session: "a",
  id: "ap3",
  tool: "progress",
  summary: "Make the retry path testable",
  descriptor: "progress",
  is_edit: false,
  allows_project: false,
  input: {
    title: "Make the retry path testable",
    state: "active",
    plan: "\n## [ ] 1. locate every inline sleep in the retry loop\nRead-only.\n",
  },
};

const SCENES = [
  "rail",
  "session",
  "workspace",
  "approval",
  "command",
  "question",
  "plan",
  "model",
  "split",
  "shown",
  "web",
  "terminal",
  "empty",
] as const;
type Scene = (typeof SCENES)[number];

/** The scene named in the URL, so a state can be linked to and survives a
 *  reload — the one thing a scene switcher in component state cannot do. */
function wanted(): Scene {
  const asked = new URLSearchParams(window.location.search).get("scene");
  return SCENES.includes(asked as Scene) ? (asked as Scene) : "rail";
}

/** One conversation, filling the window. */
const solo = (): Tiling => single({ kind: "session", session: "a" });

/** Two conversations, one of them looking into a change: `row(row(a, diff), b)`.
 *  Three panes and a nested split, which is the case worth looking at — an even
 *  two-up never shows whether the recursion reads correctly. */
function tiled(): Tiling {
  const one = solo();
  const two = split(one, panes(one)[0].id, "row", {
    kind: "session",
    session: "b",
  });
  return openInspect(two, panes(two)[0].id, "a", {
    kind: "diff",
    callId: "t2",
  });
}

/**
 * The two things `show` produces, side by side — the state a script writing a
 * file gets to, which is otherwise reachable only by running one.
 *
 * Two panes rather than one because there are two *renderings*, and they are
 * nothing alike: a `.csv` is parsed on this side and drawn as DOM, while a
 * `.html` is a page loaded from the app's loopback origin (`Framed.tsx`) that
 * runs its own scripts. A scene showing only the table left the entire framed
 * path with no preview at all — which is how it came to be described in a
 * handover as something you could look at when you could not.
 *
 * The fixture bodies live in `mock-core.ts`, and the HTML one draws itself with
 * a script on purpose: if that paragraph still reads "replaced by script", the
 * page is being rendered as inert markup and this whole path has regressed.
 */
function showing(): Tiling {
  const one = solo();
  const table = openInspect(one, panes(one)[0].id, "a", {
    kind: "shown",
    path: "/home/teamon/code/py/duck_ext/out/carry.csv",
    label: "Carry by tenor",
  });
  return openAside(table, panes(table)[1].id, "a", {
    kind: "shown",
    path: "/home/teamon/code/py/duck_ext/out/carry.html",
    label: "Carry report",
  });
}

/** A real session beside its live workspace tree. From here, opening README.md
 * reaches Markdown preview, while the nested TypeScript fixture reaches the
 * highlighted source view; the inspect pane keeps both in its normal history. */
function workspace(): Tiling {
  const one = solo();
  return openInspect(one, panes(one)[0].id, "a", { kind: "workspace-tree" });
}

/**
 * A conversation beside the window's browser.
 *
 * Only the chrome is real here. The page is a native child webview the OS
 * composites over the pane body, so in a design preview that rectangle is
 * empty on purpose — see the `browser_*` cases in `mock-core.ts`. What this
 * scene is for is the part that *was* designed on this side: the address bar
 * against the pane header, and the browser sitting in the tiling as a pane
 * like any other.
 */
function web(): Tiling {
  return openWeb(solo());
}

/**
 * A conversation over the terminals — the shape `Mod+J` produces.
 *
 * Unlike the browser scene, everything here is real: the emulator is the one
 * that ships, drawing the app's own ANSI palette (`--term-*`), and `mock-core`
 * plays a session through it with colour in it. So this scene is where the
 * light-terminal palette is actually judged, which is the part of this pane
 * with a design decision in it.
 */
function terminal(): Tiling {
  return toggleTerminal(solo());
}

/** The window each scene wants. Every scene but the split views is one
 *  conversation — and `rail`, which is the window with no pane at all: the
 *  state that used to be a whole screen (the launchpad) and is now the field's
 *  empty state beside a rail that carries everything. */
function layoutFor(scene: Scene): Tiling {
  if (scene === "rail") return EMPTY;
  if (scene === "split") return tiled();
  if (scene === "shown") return showing();
  if (scene === "web") return web();
  if (scene === "terminal") return terminal();
  if (scene === "workspace") return workspace();
  return solo();
}

/** A comment already on the plan: the panel with nothing on it never shows what
 *  an anchored note looks like, and that is the state worth designing. */
function comments(plan: Plan): PlanComment[] {
  const detail = plan.phases[1]?.detail ?? "";
  return [
    {
      id: "c1",
      path: [1],
      field: "detail",
      quote: detail.slice(
        detail.indexOf("Risk:"),
        detail.indexOf("Risk:") + 96,
      ),
      text: "Split the watchdog's clock out — one injected clock for two waiters is the bug this would hide.",
    },
  ];
}

export function Preview() {
  const [scene, setScene] = useState<Scene>(wanted);
  const [tiling, setTiling] = useState<Tiling>(() => layoutFor(wanted()));
  const [draft, setDraft] = useState("");
  const [attachments, setAttachments] = useState<Pasted[]>([]);
  // Live, like the draft and the picker fixtures: this is a switch whose whole
  // acceptance test is "flip it and the column changes", which a fixed value
  // cannot show. The `session` scene starts it on for the same reason the `model`
  // scene opens its panel — reasoning-as-prose is a shape worth seeing, and the
  // default state is "absent", which needs no demonstration.
  const [display, setDisplay] = useState<Display>(() => ({
    ...DISPLAY_DEFAULT,
    thinking: wanted() === "session",
  }));
  // Live, like the draft: a queue whose rows cannot be taken back demonstrates
  // half of the thing. The `session` scene starts with one waiting, because a
  // running turn is the only state in which the strip exists at all.
  const [queued, setQueued] = useState<Queued[]>(() =>
    wanted() === "session"
      ? [
          {
            text: "also add a test for the cap at 30s",
            attachments: [],
            turn: 1,
          },
        ]
      : [],
  );
  // The rewind question, opened by the control on any prompt. Its numbers are
  // the ones a real preview would return for that point.
  const [rewindAsk, setRewindAsk] = useState<SessionState["rewindAsk"]>(null);

  const pick = (name: Scene) => {
    setScene(name);
    setTiling(layoutFor(name));
    window.history.replaceState(null, "", `?scene=${name}`);
  };

  // The model panel is a click away from any conversation, and clicking is the
  // one thing a look-through cannot do for you. This scene opens it, so the
  // surface with the most design in it is a state you can just look at.
  //
  // Across frames rather than in this one: the strip reads its state through the
  // command bridge, so the chip does not exist until that promise has resolved.
  useEffect(() => {
    if (scene !== "model") return;
    let alive = true;
    let tries = 0;
    const tick = () => {
      if (!alive) return;
      const chip = document.querySelector<HTMLButtonElement>(".chip.is-model");
      if (chip) chip.click();
      else if (tries++ < 20) requestAnimationFrame(tick);
    };
    requestAnimationFrame(tick);
    return () => {
      alive = false;
    };
  }, [scene]);

  const plan = scene === "plan" ? DRAFT_PLAN : scene === "empty" ? null : PLAN;

  const stateOf = (id: string): SessionState =>
    id === "a"
      ? {
          ...BLANK,
          blocks: scene === "empty" ? [] : BLOCKS,
          files: scene === "empty" ? [] : FILES,
          running: scene === "session",
          approval:
            scene === "approval"
              ? APPROVAL
              : scene === "command"
                ? COMMAND
                : scene === "question"
                  ? QUESTION
                  : scene === "plan"
                    ? PLAN_REVIEW
                    : null,
          draft,
          attachments,
          queued,
          // Matched onto the fixture's own prompts by text, exactly as the real
          // backend's targets are. A hard-coded index here would demonstrate a
          // mapping that does not exist.
          rewindTargets: REWIND_TARGETS,
          rewindAsk,
          rewinding: false,
          planFirst: scene === "empty",
          plan,
          planDraft: plan
            ? { ...draftOf(plan), comments: comments(plan) }
            : null,
          planOpen: scene === "session",
          meter: METER,
          activity: ACTIVITY.a,
        }
      : {
          ...BLANK,
          blocks: OTHER_BLOCKS,
          running: true,
          meter: OTHER_METER,
          activity: ACTIVITY[id] ?? BLANK.activity,
        };

  return (
    <ToolMetaProvider meta={TOOL_META}>
      <DisplayContext.Provider value={display}>
        {/* A conversation with nothing in it yet is also the honest place to show
          the other half of the usage panel: a provider that reports no
          subscription windows at all. */}
        <LimitsContext.Provider value={scene === "empty" ? null : LIMITS}>
          <div className="preview">
            <nav className="preview-bar">
              {SCENES.map((name) => (
                <button
                  key={name}
                  className={scene === name ? "is-on" : undefined}
                  onClick={() => pick(name)}
                >
                  {name}
                </button>
              ))}
            </nav>
            <div className="preview-stage">
              <Workspace
                tiling={tiling}
                display={display}
                onDisplay={setDisplay}
                sessions={SESSIONS}
                stateOf={stateOf}
                statusOf={(id) => STATUS[id] ?? "idle"}
                onTiling={(step) => setTiling(step)}
                onDraft={(_, value) => setDraft(value)}
                onAttach={(_, items) =>
                  setAttachments((was) => [...was, ...items])
                }
                onDetach={(_, id) =>
                  setAttachments((was) => was.filter((item) => item.id !== id))
                }
                onSend={() => {}}
                onInterrupt={() => {}}
                onWithdrawQueued={(_, index) =>
                  setQueued((was) => was.filter((_item, at) => at !== index))
                }
                onSendQueuedNow={() => setQueued([])}
                onResume={() => {}}
                onDismissResume={() => {}}
                onAskRewind={(_, target) =>
                  setRewindAsk(
                    target
                      ? {
                          index: target.index,
                          text: target.text,
                          dirty: target.dirty,
                          // The count the backend works out from its own list.
                          dropped:
                            REWIND_TARGETS.length -
                            REWIND_TARGETS.findIndex(
                              (entry) => entry.index === target.index,
                            ),
                        }
                      : null,
                  )
                }
                onRewind={() => setRewindAsk(null)}
                onAnswer={() => pick("session")}
                onDecidePlan={() => pick("session")}
                onPlanDraft={() => {}}
                onSavePlan={() => {}}
                onPlanOpen={() => {}}
                onPlanFirst={() => {}}
                onCloseSession={() => {}}
                onOpenFolder={async () => pick("session")}
              />
            </div>
          </div>
        </LimitsContext.Provider>
      </DisplayContext.Provider>
    </ToolMetaProvider>
  );
}
