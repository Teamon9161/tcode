import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

import {
  AGENT_EVENT,
  APPROVAL_REQUEST,
  TURN_FINISHED,
  type ApprovalRequest,
  type Decision,
  type SessionEvent,
  type SessionInfo,
  type TurnFinished,
} from "./types";
import { applyEvent, errorBlock, userBlock, type Block } from "./transcript";
import { Transcript } from "./Transcript";
import { ApprovalDialog } from "./ApprovalDialog";

export function App() {
  const [session, setSession] = useState<SessionInfo | null>(null);
  const [blocks, setBlocks] = useState<Block[]>([]);
  const [running, setRunning] = useState(false);
  const [approval, setApproval] = useState<ApprovalRequest | null>(null);
  const [draft, setDraft] = useState("");
  const [fault, setFault] = useState<string | null>(null);

  // The listeners must be registered before the first turn can start, and they
  // must be registered exactly once — a second subscription would double every
  // delta in the transcript.
  //
  // A rejection here is fatal and must say so. `listen()` goes through the core
  // event plugin, which the app's capabilities have to grant; when that grant
  // is missing the promise rejects, and an unhandled rejection would leave a
  // window that accepts messages and renders nothing back.
  useEffect(() => {
    const subscriptions = [
      listen<SessionEvent>(AGENT_EVENT, ({ payload }) => {
        setBlocks((current) => applyEvent(current, payload.event));
      }),
      listen<ApprovalRequest>(APPROVAL_REQUEST, ({ payload }) => {
        setApproval(payload);
      }),
      listen<TurnFinished>(TURN_FINISHED, ({ payload }) => {
        setRunning(false);
        setApproval(null);
        if (payload.error) {
          setBlocks((current) => [...current, errorBlock(payload.error!)]);
        }
      }),
    ];
    Promise.all(subscriptions).catch((error) =>
      setFault(`cannot listen for agent events: ${String(error)}`),
    );
    invoke<SessionInfo[]>("sessions")
      .then((open) => setSession(open[0] ?? null))
      .catch((error) => setFault(String(error)));
    return () => {
      subscriptions.forEach((pending) =>
        pending.then((unlisten) => unlisten()).catch(() => {}),
      );
    };
  }, []);

  const send = useCallback(async () => {
    const text = draft.trim();
    if (!text || !session || running) return;
    setDraft("");
    setBlocks((current) => [...current, userBlock(text)]);
    setRunning(true);
    try {
      await invoke("send_message", { session: session.id, text });
    } catch (error) {
      setRunning(false);
      setBlocks((current) => [...current, errorBlock(String(error))]);
    }
  }, [draft, session, running]);

  const answer = useCallback(
    async (decision: Decision, comment: string) => {
      if (!approval) return;
      const pending = approval;
      setApproval(null);
      try {
        await invoke("respond_approval", {
          session: pending.session,
          answer: { id: pending.id, decision, comment: comment || null },
        });
      } catch (error) {
        setBlocks((current) => [...current, errorBlock(String(error))]);
      }
    },
    [approval],
  );

  const interrupt = useCallback(async () => {
    if (!session) return;
    await invoke("interrupt", { session: session.id }).catch(() => {});
  }, [session]);

  if (fault) {
    return (
      <div className="fault">
        <h1>tcode could not start</h1>
        <p>{fault}</p>
      </div>
    );
  }

  return (
    <div className="app">
      <header>
        <span className="mark">tcode</span>
        <span className="cwd">{session?.cwd ?? "opening…"}</span>
        {running && (
          <button className="interrupt" onClick={interrupt}>
            stop
          </button>
        )}
      </header>

      <Transcript blocks={blocks} running={running} />

      <Composer
        value={draft}
        onChange={setDraft}
        onSubmit={send}
        disabled={!session || running}
      />

      {approval && <ApprovalDialog request={approval} onAnswer={answer} />}
    </div>
  );
}

function Composer({
  value,
  onChange,
  onSubmit,
  disabled,
}: {
  value: string;
  onChange: (value: string) => void;
  onSubmit: () => void;
  disabled: boolean;
}) {
  const field = useRef<HTMLTextAreaElement>(null);

  // Focus returns to the field whenever the turn ends, so a conversation is
  // typed without ever reaching for the mouse.
  useEffect(() => {
    if (!disabled) field.current?.focus();
  }, [disabled]);

  return (
    <form
      className="composer"
      onSubmit={(event) => {
        event.preventDefault();
        onSubmit();
      }}
    >
      <textarea
        ref={field}
        value={value}
        rows={3}
        placeholder={disabled ? "running…" : "ask for something"}
        disabled={disabled}
        onChange={(event) => onChange(event.target.value)}
        onKeyDown={(event) => {
          // Enter sends; Shift+Enter is a newline. Same as the TUI.
          if (event.key === "Enter" && !event.shiftKey) {
            event.preventDefault();
            onSubmit();
          }
        }}
      />
      <button type="submit" disabled={disabled || !value.trim()}>
        send
      </button>
    </form>
  );
}
