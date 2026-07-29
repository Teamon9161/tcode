import { useState } from "react";

import type { ApprovalRequest, SessionInfo, Status } from "../types";
import type { Block } from "../blocks";
import type { TouchedFile } from "../files";
import type { Pasted } from "../paste";
import { BLANK, type SessionState } from "../session";
import { openInspect, panes, single, split, type Tiling } from "../layout";
import { ToolMetaProvider, type ToolMeta } from "../toolViews";
import { Launchpad } from "../Launchpad";
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

const SESSIONS: SessionInfo[] = [
  { id: "a", cwd: ROOT, name: "tcode", home: HOME },
  { id: "b", cwd: "/home/teamon/code/py/duck_ext", name: "duck_ext", home: HOME },
  { id: "c", cwd: "/home/teamon/code/rust/pybond", name: "pybond", home: HOME },
];

const STATUS: Record<string, Status> = { a: "running", b: "waiting", c: "idle" };

/** Mirrors what `tool_views()` returns for the real tool set. */
const TOOL_META = new Map<string, ToolMeta>(
  (
    [
      { name: "read", route: "transcript", quiet_output: true, hide_success_result: false },
      { name: "edit", route: "transcript", quiet_output: false, hide_success_result: true },
      { name: "write", route: "transcript", quiet_output: false, hide_success_result: true },
      { name: "shell", route: "transcript", quiet_output: false, hide_success_result: false },
      {
        name: "update_progress",
        route: "progress",
        quiet_output: false,
        hide_success_result: false,
      },
      { name: "ask_user", route: "silent", quiet_output: false, hide_success_result: false },
    ] as ToolMeta[]
  ).map((meta) => [meta.name, meta]),
);

const AGENT_FILE = `${ROOT}/crates/tcode-core/src/agent/mod.rs`;

const BLOCKS: Block[] = [
  {
    kind: "user",
    text: "Make the retry path testable — right now the backoff sleeps for real.",
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
        { phase: "Phase 2 — thread an injectable clock", status: "in_progress" },
        { phase: "Phase 3 — cover the retry path with a test", status: "pending" },
      ],
    },
    result: { preview: "progress updated", content: "", isError: false },
  },
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

See [the retry notes](https://example.com/retry) for the original reasoning.`,
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
  {
    kind: "batch",
    label: "reading 3 files",
    blocks: [
      {
        kind: "tool",
        callId: "b1",
        name: "read",
        summary: "crates/tcode-core/src/session.rs",
        input: { file_path: `${ROOT}/crates/tcode-core/src/session.rs` },
        result: { preview: "212 lines", content: "pub struct Session { /* … */ }", isError: false },
      },
      {
        kind: "tool",
        callId: "b2",
        name: "read",
        summary: "crates/tcode-core/src/provider.rs",
        input: { file_path: `${ROOT}/crates/tcode-core/src/provider.rs` },
        result: { preview: "398 lines", content: "pub trait Provider { /* … */ }", isError: false },
      },
      {
        kind: "tool",
        callId: "b3",
        name: "read",
        summary: "crates/tcode-core/src/types.rs",
        input: { file_path: `${ROOT}/crates/tcode-core/src/types.rs` },
        result: { preview: "94 lines", content: "pub enum ContentBlock { /* … */ }", isError: false },
      },
    ],
  },
  {
    kind: "run",
    run: "r1",
    meta: {
      kind: "explore",
      model: "claude-opus-5",
      summary: "Find every other place that sleeps inline",
      prompt: "Search the workspace for direct calls to tokio::time::sleep outside tests.",
    },
    blocks: [
      { kind: "assistant", text: "Searching for direct sleeps outside test modules." },
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
    kind: "run",
    run: "r2",
    meta: {
      kind: "general",
      model: "claude-sonnet-5",
      summary: "Draft the Sleeper trait and its test double",
      prompt: "Write a Sleeper trait with a real and an instant implementation.",
      status: "ok",
      toolCalls: 4,
    },
    blocks: [
      {
        kind: "assistant",
        text: "Added `Sleeper` with `RealSleeper` and `InstantSleeper`.\n\n```rust\npub trait Sleeper: Send + Sync {\n    /// Instant in tests, real in production.\n    fn sleep(&self, delay: Duration) -> BoxFuture<'_, ()>;\n}\n```",
      },
    ],
  },
  { kind: "note", text: "retrying (1/3): connection reset by peer" },
  {
    kind: "tool",
    callId: "t3",
    name: "shell",
    summary: "cargo test -p tcode-core retry",
    input: { command: "cargo test -p tcode-core retry" },
  },
  { kind: "queued", text: "also add a test for the cap at 30s", attachments: [], entryIndex: 12 },
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

const APPROVAL: ApprovalRequest = {
  session: "a",
  id: "ap1",
  tool: "edit",
  summary: "Replace the inline sleep with the injected clock.",
  descriptor: "crates/tcode-core/src/agent/mod.rs",
  is_edit: true,
  allows_project: true,
  input: {
    file_path: "crates/tcode-core/src/agent/mod.rs",
    old_string: "                tokio::time::sleep(Duration::from_millis(delay)).await;",
    new_string: "                self.sleeper.sleep(Duration::from_millis(delay)).await;",
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
            description: "Monomorphized, no dynamic dispatch, but it spreads through the type.",
            preview: "pub struct Agent<S: Sleeper> {\n    sleeper: S,\n}",
          },
        ],
      },
      {
        question: "What should the tests cover?",
        multiSelect: true,
        options: [
          { label: "The backoff curve", description: "Delays grow and then cap." },
          { label: "The cancel path", description: "A cancelled retry stops sleeping." },
          { label: "The error passthrough", description: "The last error survives the retries." },
        ],
      },
    ],
  },
};

/** The second conversation in the split scene. Short on purpose: the point of
 *  that scene is the layout, and a second full transcript would only compete
 *  with the first for attention. */
const OTHER_BLOCKS: Block[] = [
  { kind: "user", text: "Why is the bond curve loader re-fetching on every call?" },
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

const SCENES = [
  "launchpad",
  "session",
  "approval",
  "question",
  "split",
  "shown",
  "empty",
] as const;
type Scene = (typeof SCENES)[number];

/** One conversation, filling the window. */
const solo = (): Tiling => single({ kind: "session", session: "a" });

/** Two conversations, one of them looking into a change: `row(row(a, diff), b)`.
 *  Three panes and a nested split, which is the case worth looking at — an even
 *  two-up never shows whether the recursion reads correctly. */
function tiled(): Tiling {
  const one = solo();
  const two = split(one, panes(one)[0].id, "row", { kind: "session", session: "b" });
  return openInspect(two, panes(two)[0].id, "a", { kind: "diff", callId: "t2" });
}

/** A conversation with a file the model put on screen beside it — the state
 *  `show` produces, which is otherwise reachable only by running a script that
 *  writes one. The fixture bodies live in `mock-core.ts`. */
function showing(): Tiling {
  const one = solo();
  return openInspect(one, panes(one)[0].id, "a", {
    kind: "shown",
    path: "/home/teamon/code/py/duck_ext/out/carry.csv",
    label: "Carry by tenor",
  });
}

export function Preview() {
  const [scene, setScene] = useState<Scene>("launchpad");
  const [tiling, setTiling] = useState<Tiling>(solo);
  const [draft, setDraft] = useState("");
  const [attachments, setAttachments] = useState<Pasted[]>([]);

  const pick = (name: Scene) => {
    setScene(name);
    setTiling(name === "split" ? tiled() : name === "shown" ? showing() : solo());
  };

  const stateOf = (id: string): SessionState =>
    id === "a"
      ? {
          ...BLANK,
          blocks: scene === "empty" ? [] : BLOCKS,
          files: scene === "empty" ? [] : FILES,
          running: scene === "session",
          approval:
            scene === "approval" ? APPROVAL : scene === "question" ? QUESTION : null,
          draft,
          attachments,
        }
      : { ...BLANK, blocks: OTHER_BLOCKS, running: true };

  return (
    <ToolMetaProvider meta={TOOL_META}>
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
          {scene === "launchpad" && (
            <Launchpad
              open={SESSIONS}
              statusOf={(id) => STATUS[id] ?? "idle"}
              activityOf={(id) =>
                ({
                  a: "edit crates/tcode-core/src/agent/mod.rs",
                  b: "waiting on shell",
                  c: "done",
                })[id] ?? ""
              }
              onEnter={() => pick("session")}
              onOpenFolder={async () => pick("session")}
            />
          )}
          {scene !== "launchpad" && (
            <Workspace
              tiling={tiling}
              sessions={SESSIONS}
              stateOf={stateOf}
              statusOf={(id) => STATUS[id] ?? "idle"}
              onTiling={(step) => setTiling(step)}
              onDraft={(_, value) => setDraft(value)}
              onAttach={(_, items) => setAttachments((was) => [...was, ...items])}
              onDetach={(_, id) => setAttachments((was) => was.filter((item) => item.id !== id))}
              onSend={() => {}}
              onInterrupt={() => {}}
              onAnswer={() => pick("session")}
              onCloseSession={() => {}}
              onHome={() => pick("launchpad")}
            />
          )}
        </div>
      </div>
    </ToolMetaProvider>
  );
}
