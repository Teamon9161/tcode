import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

import {
  AGENT_EVENT,
  APPROVAL_REQUEST,
  TURN_FINISHED,
  type AgentEvent,
  type ApprovalRequest,
  type Decision,
  type SessionEvent,
  type SessionInfo,
  type Status,
  type TurnFinished,
} from "./types";
import { applyEvent, errorBlock, userBlock, type Block } from "./transcript";
import { applyFileEvent, type TouchedFile } from "./files";
import { Launchpad } from "./Launchpad";
import { Workspace } from "./Workspace";
import { Mark } from "./components/Mark";

/** Everything the UI knows about one open conversation. */
type SessionState = {
  blocks: Block[];
  files: TouchedFile[];
  running: boolean;
  approval: ApprovalRequest | null;
  failed: boolean;
  draft: string;
  /** One line for the launchpad card: the last thing that happened. */
  activity: string;
};

const BLANK: SessionState = {
  blocks: [],
  files: [],
  running: false,
  approval: null,
  failed: false,
  draft: "",
  activity: "not started",
};

export function App() {
  const [sessions, setSessions] = useState<SessionInfo[]>([]);
  const [states, setStates] = useState<Record<string, SessionState>>({});
  const [view, setView] = useState<string | null>(null);
  const [fault, setFault] = useState<string | null>(null);

  // Sessions that are not on screen still receive events — that is the whole
  // point of the app — so the reducers are keyed by session id and run
  // regardless of which one is in view.
  const patch = useCallback(
    (id: string, change: (state: SessionState) => SessionState) => {
      setStates((current) => ({
        ...current,
        [id]: change(current[id] ?? BLANK),
      }));
    },
    [],
  );

  // The listeners must be registered before the first turn can start, and
  // exactly once — a second subscription would double every delta.
  //
  // A rejection here is fatal and must say so. `listen()` goes through the core
  // event plugin, which the app's capabilities have to grant; when that grant
  // is missing the promise rejects, and an unhandled rejection would leave a
  // window that accepts messages and renders nothing back.
  useEffect(() => {
    const subscriptions = [
      listen<SessionEvent>(AGENT_EVENT, ({ payload }) => {
        patch(payload.session, (state) => ({
          ...state,
          blocks: applyEvent(state.blocks, payload.event),
          files: applyFileEvent(state.files, payload.event),
          activity: describe(payload.event) ?? state.activity,
        }));
      }),
      listen<ApprovalRequest>(APPROVAL_REQUEST, ({ payload }) => {
        patch(payload.session, (state) => ({
          ...state,
          approval: payload,
          activity: `waiting on ${payload.tool}`,
        }));
      }),
      listen<TurnFinished>(TURN_FINISHED, ({ payload }) => {
        patch(payload.session, (state) => ({
          ...state,
          running: false,
          approval: null,
          failed: payload.error !== null,
          activity: payload.error ? "failed" : "done",
          blocks: payload.error
            ? [...state.blocks, errorBlock(payload.error)]
            : state.blocks,
        }));
      }),
    ];
    Promise.all(subscriptions).catch((error) =>
      setFault(`cannot listen for agent events: ${String(error)}`),
    );

    invoke<SessionInfo[]>("sessions")
      .then(setSessions)
      .catch((error) => setFault(String(error)));

    return () => {
      subscriptions.forEach((pending) =>
        pending.then((unlisten) => unlisten()).catch(() => {}),
      );
    };
  }, [patch]);

  const statusOf = useCallback(
    (id: string): Status => {
      const state = states[id];
      if (!state) return "idle";
      if (state.approval) return "waiting";
      if (state.running) return "running";
      if (state.failed) return "failed";
      return "idle";
    },
    [states],
  );

  const openFolder = useCallback(async (path: string, resume?: string) => {
    const info = await invoke<SessionInfo>("open_folder", {
      path,
      resume: resume ?? null,
    });
    setSessions((current) =>
      current.some((open) => open.id === info.id) ? current : [...current, info],
    );
    setView(info.id);
  }, []);

  const closeSession = useCallback(
    async (id: string) => {
      await invoke("close_session", { session: id }).catch(() => {});
      setSessions((current) => {
        const left = current.filter((open) => open.id !== id);
        setView((showing) =>
          showing === id ? (left[0]?.id ?? null) : showing,
        );
        return left;
      });
      setStates((current) => {
        const { [id]: _gone, ...rest } = current;
        return rest;
      });
    },
    [],
  );

  const send = useCallback(
    async (id: string) => {
      const text = (states[id]?.draft ?? "").trim();
      if (!text || states[id]?.running) return;
      patch(id, (state) => ({
        ...state,
        draft: "",
        running: true,
        failed: false,
        blocks: [...state.blocks, userBlock(text)],
        activity: text,
      }));
      try {
        await invoke("send_message", { session: id, text });
      } catch (error) {
        patch(id, (state) => ({
          ...state,
          running: false,
          failed: true,
          blocks: [...state.blocks, errorBlock(String(error))],
        }));
      }
    },
    [states, patch],
  );

  const answer = useCallback(
    async (id: string, decision: Decision, comment: string) => {
      const pending = states[id]?.approval;
      if (!pending) return;
      patch(id, (state) => ({ ...state, approval: null }));
      try {
        await invoke("respond_approval", {
          session: id,
          answer: { id: pending.id, decision, comment: comment || null },
        });
      } catch (error) {
        patch(id, (state) => ({
          ...state,
          blocks: [...state.blocks, errorBlock(String(error))],
        }));
      }
    },
    [states, patch],
  );

  // A session that needs an answer pulls the view to itself, but only from the
  // launchpad: yanking someone out of a conversation they are reading would be
  // worse than the delay in noticing.
  const waiting = sessions.find((open) => states[open.id]?.approval);
  const alerted = useRef<string | null>(null);
  useEffect(() => {
    if (!waiting || view !== null) return;
    if (alerted.current === waiting.id) return;
    alerted.current = waiting.id;
    setView(waiting.id);
  }, [waiting, view]);

  const current = useMemo(
    () => sessions.find((open) => open.id === view) ?? null,
    [sessions, view],
  );

  if (fault) return <Fault reason={fault} />;

  if (!current) {
    return (
      <Launchpad
        open={sessions}
        statusOf={statusOf}
        activityOf={(id) => states[id]?.activity ?? BLANK.activity}
        onEnter={setView}
        onOpenFolder={openFolder}
      />
    );
  }

  const state = states[current.id] ?? BLANK;
  return (
    <Workspace
      session={current}
      sessions={sessions}
      blocks={state.blocks}
      files={state.files}
      running={state.running}
      approval={state.approval}
      statusOf={statusOf}
      draft={state.draft}
      onDraft={(draft) => patch(current.id, (was) => ({ ...was, draft }))}
      onSend={() => send(current.id)}
      onInterrupt={() => {
        invoke("interrupt", { session: current.id }).catch(() => {});
      }}
      onAnswer={(decision, comment) => answer(current.id, decision, comment)}
      onSelect={setView}
      onClose={closeSession}
      onHome={() => setView(null)}
    />
  );
}

/** The one-line summary the launchpad card shows for a session. */
function describe(event: AgentEvent): string | null {
  switch (event.type) {
    case "ToolStart": {
      const data = event.data as { name: string; summary: string };
      return `${data.name} ${data.summary}`.trim();
    }
    case "Compacting":
      return "compacting history";
    case "Interrupted":
      return "interrupted";
    default:
      return null;
  }
}

/**
 * Startup failures get the whole window, not a toast. Every one of them means
 * the app cannot do its job, and the alternative — a window that looks fine and
 * silently does nothing — is the failure mode this screen exists to prevent.
 */
function Fault({ reason }: { reason: string }) {
  return (
    <div className="fault">
      <Mark size={22} state="failed" />
      <h1>tcode could not start</h1>
      <p>{reason}</p>
      <p className="fault-hint">
        If no provider is configured yet, run <code>tcode</code> in a terminal
        once to set one up.
      </p>
    </div>
  );
}
