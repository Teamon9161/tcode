import { useState } from "react";

import type { ApprovalRequest, SessionInfo, Status } from "../types";
import type { Block } from "../transcript";
import type { TouchedFile } from "../files";
import { Launchpad } from "../Launchpad";
import { Workspace } from "../Workspace";

/**
 * The design preview: every screen and state, side by side, in a browser.
 *
 * It renders the real components against fixture data — not a mock-up of them —
 * so what is looked at here is what ships. Reachable with `npm run preview:ui`.
 */

const HOME = "/home/teamon";

const SESSIONS: SessionInfo[] = [
  { id: "a", cwd: "/home/teamon/code/rust/tcode", name: "tcode", home: HOME },
  { id: "b", cwd: "/home/teamon/code/py/duck_ext", name: "duck_ext", home: HOME },
  { id: "c", cwd: "/home/teamon/code/rust/pybond", name: "pybond", home: HOME },
];

const STATUS: Record<string, Status> = { a: "running", b: "waiting", c: "idle" };

const BLOCKS: Block[] = [
  { kind: "user", text: "Make the retry path testable — right now the backoff sleeps for real." },
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
    result: { preview: "537 lines", content: "…", isError: false },
  },
  {
    kind: "assistant",
    text: "Found it. The sleep is on line 604:\n\n```rust\ntokio::time::sleep(Duration::from_millis(delay)).await;\n```\n\nI'll thread a `Sleeper` through so tests can pass an instant one.",
  },
  {
    kind: "tool",
    callId: "t2",
    name: "edit",
    summary: "crates/tcode-core/src/agent/mod.rs",
    result: { preview: "3 hunks", content: "@@ -601,7 +601,7 @@\n-    tokio::time::sleep(…)\n+    self.sleeper.sleep(…)", isError: false },
  },
  { kind: "note", text: "retrying (1/3): connection reset by peer" },
  {
    kind: "tool",
    callId: "t3",
    name: "shell",
    summary: "cargo test -p tcode-core retry",
  },
];

const FILES: TouchedFile[] = [
  {
    path: "/home/teamon/code/rust/tcode/crates/tcode-core/src/agent/mod.rs",
    action: "edited",
    calls: ["t1", "t2"],
    pending: false,
    failed: false,
  },
  {
    path: "/home/teamon/code/rust/tcode/crates/tcode-core/src/agent/retry.rs",
    action: "created",
    calls: ["t4"],
    pending: false,
    failed: false,
  },
  {
    path: "/home/teamon/code/rust/tcode/crates/tcode-core/src/session.rs",
    action: "read",
    calls: ["t5"],
    pending: false,
    failed: false,
  },
];

const APPROVAL: ApprovalRequest = {
  session: "a",
  id: "ap1",
  tool: "edit",
  summary: "Replace the inline sleep with the injected clock, three hunks.",
  descriptor: "crates/tcode-core/src/agent/mod.rs",
  is_edit: true,
  allows_project: true,
  input: {
    file_path: "crates/tcode-core/src/agent/mod.rs",
    old_string: "tokio::time::sleep(Duration::from_millis(delay)).await;",
    new_string: "self.sleeper.sleep(Duration::from_millis(delay)).await;",
  },
};

type Scene = "launchpad" | "session" | "approval" | "empty";

export function Preview() {
  const [scene, setScene] = useState<Scene>("launchpad");
  const [draft, setDraft] = useState("");

  return (
    <div className="preview">
      <nav className="preview-bar">
        {(["launchpad", "session", "approval", "empty"] as Scene[]).map((name) => (
          <button
            key={name}
            className={scene === name ? "is-on" : undefined}
            onClick={() => setScene(name)}
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
              ({ a: "edit crates/tcode-core/src/agent/mod.rs", b: "waiting on shell", c: "done" })[id] ?? ""
            }
            onEnter={() => setScene("session")}
            onOpenFolder={async () => setScene("session")}
          />
        )}
        {(scene === "session" || scene === "approval" || scene === "empty") && (
          <Workspace
            session={SESSIONS[0]}
            sessions={SESSIONS}
            blocks={scene === "empty" ? [] : BLOCKS}
            files={scene === "empty" ? [] : FILES}
            running={scene === "session"}
            approval={scene === "approval" ? APPROVAL : null}
            statusOf={(id) => STATUS[id] ?? "idle"}
            draft={draft}
            onDraft={setDraft}
            onSend={() => {}}
            onInterrupt={() => {}}
            onAnswer={() => setScene("session")}
            onSelect={() => {}}
            onClose={() => {}}
            onHome={() => setScene("launchpad")}
          />
        )}
      </div>
    </div>
  );
}
