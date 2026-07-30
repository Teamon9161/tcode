import { useCallback, useEffect, useRef, useState } from "react";
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
  type OpenedSession,
  type Queued,
  type RewindPreview,
  type Rewound,
  type SessionInfo,
  type Status,
  type TurnFinished,
} from "./types";
import { applyEvent, errorBlock, noteBlock, userBlock } from "./blocks";
import type { RewindTarget } from "./rewind";
import { applyFileEvent } from "./files";
import {
  EMPTY,
  closeSession as closePanesOf,
  show,
  showBeside,
  type Tiling,
} from "./layout";
import { draftOf, fromDraft, type Plan, type PlanDraft } from "./plan";
import type { PlanDecision } from "./PlanEditor";
import { BLANK, LimitsContext, type SessionState } from "./session";
import { replayLedger } from "./replay";
import { adoptContext, applyUsage, limitsFrom, NO_USAGE, type Limits } from "./usage";
import { displayToolSummary, ToolMetaProvider, type ToolMeta } from "./toolViews";
import { DisplayContext, loadDisplay, saveDisplay, type Display } from "./display";
import { Launchpad } from "./Launchpad";
import { Workspace } from "./Workspace";
import { Mark } from "./components/Mark";

export function App() {
  const [sessions, setSessions] = useState<SessionInfo[]>([]);
  const [states, setStates] = useState<Record<string, SessionState>>({});
  // What the window is showing, as a pane tree (`layout.ts`). Today it only
  // ever holds a single conversation pane, so this renders exactly as a plain
  // `view: string | null` did; it is the seat the split view sits down in.
  const [tiling, setTiling] = useState<Tiling>(EMPTY);
  const [fault, setFault] = useState<string | null>(null);
  // One budget for the window, not one per conversation: the 5-hour and weekly
  // windows belong to the account, and every session open here spends the same
  // one. Whichever session's turn hears about it last is the one that is true.
  const [limits, setLimits] = useState<Limits | null>(null);
  const [toolMeta, setToolMeta] = useState<Map<string, ToolMeta>>(new Map());
  // Read once at mount, not on every render: it is the reader's setting rather
  // than the window's state, and it outlives the window.
  const [display, setDisplay] = useState<Display>(loadDisplay);
  const changeDisplay = useCallback((next: Display) => {
    setDisplay(next);
    saveDisplay(next);
  }, []);

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

  /**
   * Re-read one conversation's plan.
   *
   * Called at the three moments a plan can have changed — a `progress` call
   * finished, a review arrived, a turn ended — rather than polled: nothing about
   * a plan changes on its own, and a timer would ask a hundred times to catch
   * the two answers that differ. The draft follows the file unless the user has
   * unsaved edits in it, which are theirs to keep.
   */
  const readPlan = useCallback(
    (id: string) => {
      invoke<Plan | null>("plan", { session: id })
        .then((plan) =>
          patch(id, (state) => ({
            ...state,
            plan,
            planDraft: rebase(state.planDraft, plan),
          })),
        )
        .catch((error) => console.warn("plan unavailable:", error));
    },
    [patch],
  );

  /**
   * Re-read the two things only the backend can answer about a settled session:
   * what is still queued, and where it can be rewound to.
   *
   * Both are read rather than mirrored. The queue is shared state a running turn
   * drains without telling anyone, and the rewind points are ledger indices —
   * the one number this side must never invent, since it is what truncation acts
   * on. A failure leaves the last answer standing, which costs an affordance and
   * never a wrong one.
   */
  const refreshTurnState = useCallback(
    (id: string) => {
      Promise.all([
        invoke<Queued[]>("queued", { session: id }).catch(() => null),
        invoke<RewindTarget[]>("rewind_targets", { session: id }).catch(() => null),
      ]).then(([queued, rewindTargets]) =>
        patch(id, (state) => ({
          ...state,
          queued: queued ?? state.queued,
          rewindTargets: rewindTargets ?? state.rewindTargets,
        })),
      );
    },
    [patch],
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
          meter: applyUsage(state.meter, payload.event),
          activity: describe(payload.event) ?? state.activity,
        }));
        const reported = limitsFrom(payload.event);
        if (reported) setLimits(reported);
        // A `progress` call is the one event that changes a file the panel is
        // showing, so it is the one event that asks the backend to read it again.
        if (touchedPlan(payload.event)) readPlan(payload.session);
      }),
      listen<ApprovalRequest>(APPROVAL_REQUEST, ({ payload }) => {
        patch(payload.session, (state) => ({
          ...state,
          approval: payload,
          activity: `waiting on ${payload.tool}`,
        }));
        // A plan review's draft was saved to disk before this arrived, which is
        // where the editor gets the phases — with every phase's detail, not just
        // the ones the model happened to resend in this call.
        readPlan(payload.session);
      }),
      listen<TurnFinished>(TURN_FINISHED, ({ payload }) => {
        patch(payload.session, (state) => ({
          ...state,
          running: false,
          approval: null,
          failed: payload.error !== null,
          activity: payload.error ? "failed" : "done",
          // One authoritative reading per turn. A turn is where the running
          // total can have gone wrong and cannot recover on its own: an
          // auto-compaction rewrites the window after the last `Usage` event
          // the webview saw, and no event says how big the summary came out.
          meter: adoptContext(state.meter, payload.context_tokens, payload.context_estimated),
          blocks: payload.error
            ? [...state.blocks, errorBlock(payload.error)]
            : state.blocks,
        }));
        readPlan(payload.session);
        // Both are answers only the backend has, and a turn boundary is when
        // both change: the queue was drained into the turn (or into the one this
        // is about to start), and the prompts that can be gone back to are
        // whatever the ledger now holds. Reading them here is also what makes
        // rewinding available at all — the targets are empty while a turn owns
        // the session, which is exactly when the control must not be offered.
        refreshTurnState(payload.session);
      }),
    ];
    Promise.all(subscriptions).catch((error) =>
      setFault(`cannot listen for agent events: ${String(error)}`),
    );

    invoke<SessionInfo[]>("sessions")
      .then(setSessions)
      .catch((error) => setFault(String(error)));

    // Tool routing is static for the process, so it is fetched once. Failing to
    // get it is not fatal: every tool then falls back to the ordinary transcript
    // treatment, which is a duller conversation rather than a broken one.
    invoke<ToolMeta[]>("tool_views")
      .then((list) => setToolMeta(new Map(list.map((meta) => [meta.name, meta]))))
      .catch((error) => console.warn("tool_views unavailable:", error));

    return () => {
      subscriptions.forEach((pending) =>
        pending.then((unlisten) => unlisten()).catch(() => {}),
      );
    };
  }, [patch, readPlan, refreshTurnState]);

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
    const opened = await invoke<OpenedSession>("open_folder", {
      path,
      resume: resume ?? null,
    });
    const { session, history } = opened;
    const replayed = replayLedger(history);
    setSessions((current) =>
      current.some((open) => open.id === session.id) ? current : [...current, session],
    );
    setStates((current) => ({
      ...current,
      [session.id]: {
        ...BLANK,
        ...replayed,
        // The backend measured the prompt it would actually send. Sizing this
        // from `history` was wrong in both directions at once: short by the
        // system prompt and the tool schemas, and long by every entry a
        // compaction had already moved out of the window.
        meter: adoptContext(BLANK.meter, opened.context_tokens, opened.context_estimated),
        activity: history.length > 0 ? "resumed" : BLANK.activity,
      },
    }));
    setTiling((current) => show(current, session.id));
    // A resumed conversation may already be working through a plan — it is on
    // disk, and the session adopted it — so the strip must not wait for the next
    // turn to find that out.
    readPlan(session.id);
    // Same reasoning for going back: a resumed conversation's earlier prompts
    // are rewind points from the moment it opens, not once a turn has run.
    refreshTurnState(session.id);
  }, [readPlan, refreshTurnState]);

  const closeSession = useCallback(
    async (id: string) => {
      await invoke("close_session", { session: id }).catch(() => {});
      const left = sessions.filter((open) => open.id !== id);
      setSessions(left);
      setStates((current) => {
        const { [id]: _gone, ...rest } = current;
        return rest;
      });
      setTiling((current) => {
        const next = closePanesOf(current, id);
        // Losing the last pane falls back to another open conversation rather
        // than to the launchpad — the rail is not empty, so neither is the
        // window.
        return next.root || !left[0] ? next : show(next, left[0].id);
      });
    },
    [sessions],
  );

  /**
   * Send, or queue when a turn is already running.
   *
   * Which of the two happened is the backend's answer, not a guess made here:
   * `send_message` returns the queue as it now stands, so an empty array means
   * the turn started and anything else is exactly what to draw above the
   * composer. Asking `running` first and acting on it would drop the prompt typed
   * in the same moment a turn ended.
   *
   * The optimistic user block is therefore appended only on the sending path.
   * A queued prompt has not happened yet — the transcript is a record, and a
   * message that might still be taken back does not belong in one. Core puts it
   * there for real (`QueuedInput`) at the boundary where it is delivered.
   */
  const send = useCallback(
    async (id: string) => {
      const text = (states[id]?.draft ?? "").trim();
      const attachments = states[id]?.attachments ?? [];
      if (!text && attachments.length === 0) return;
      const planFirst = states[id]?.planFirst ?? false;
      patch(id, (state) => ({
        ...state,
        draft: "",
        attachments: [],
        planFirst: false,
      }));
      try {
        // `plan` is a flag, never the instruction text: the words come from core
        // (the same ones `/plan` submits), so the webview cannot author model
        // context that claims to be the harness speaking.
        const queued = await invoke<Queued[]>("send_message", {
          session: id,
          text,
          plan: planFirst,
          images: attachments.map((item) => ({
            media_type: item.mediaType,
            data: item.data,
          })),
        });
        patch(id, (state) =>
          queued.length > 0
            ? { ...state, queued }
            : {
                ...state,
                running: true,
                failed: false,
                // The receipt is per turn, so it is zeroed where the turn is
                // submitted. Not on `AgentEvent::Started`: core emits that per
                // model request, and a six-step turn would end up reporting
                // only its last step's cost.
                meter: { ...state.meter, turn: NO_USAGE },
                blocks: [
                  ...state.blocks,
                  userBlock(
                    text,
                    attachments.map((item) => item.url),
                  ),
                ],
                activity: text || `${attachments.length} image(s)`,
              },
        );
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

  const withdrawQueued = useCallback(
    async (id: string, index: number, text: string) => {
      const queued = await invoke<Queued[]>("withdraw_queued", { session: id, index, text }).catch(
        () => null,
      );
      // A refusal is not an error worth a banner: the queue is re-read either
      // way, and the answer to "why is it still there" is that it was already
      // delivered.
      if (queued) patch(id, (state) => ({ ...state, queued }));
    },
    [patch],
  );

  const sendQueuedNow = useCallback((id: string) => {
    invoke("interrupt_and_send", { session: id }).catch(() => {});
  }, []);

  /**
   * Ask what going back to a point would cost — or withdraw the question.
   *
   * The preview is a command rather than something worked out here: how many
   * messages stop existing, and whether any file changed in that era, are facts
   * about the ledger and the checkpoint store.
   */
  const askRewind = useCallback(
    async (id: string, target: RewindTarget | null) => {
      if (!target) {
        patch(id, (state) => ({ ...state, rewindAsk: null }));
        return;
      }
      try {
        const preview = await invoke<RewindPreview>("rewind_preview", {
          session: id,
          entryIndex: target.index,
        });
        patch(id, (state) => ({ ...state, rewindAsk: { ...preview, index: target.index } }));
      } catch (error) {
        patch(id, (state) => ({
          ...state,
          rewindAsk: null,
          blocks: [...state.blocks, errorBlock(String(error))],
        }));
      }
    },
    [patch],
  );

  /**
   * Go back, and rebuild everything derived from what is now a shorter ledger.
   *
   * The backend returns the whole conversation, and it is replayed rather than
   * truncated in place. Four separate reductions of the event stream live here —
   * the transcript, the file index, the meter, the rewind points — and
   * hand-truncating each at the right spot is four chances to leave one showing
   * work that no longer happened. Replay is a path that already exists and is
   * already right.
   *
   * The prompt comes back to the composer, which is the whole point of going
   * back: you are here to say it differently.
   */
  const rewind = useCallback(
    async (id: string, restoreFiles: boolean) => {
      const ask = states[id]?.rewindAsk;
      if (!ask || states[id]?.rewinding) return;
      patch(id, (state) => ({ ...state, rewinding: true }));
      try {
        const done = await invoke<Rewound>("rewind", {
          session: id,
          entryIndex: ask.index,
          restoreFiles,
        });
        const replayed = replayLedger(done.session.history);
        const targets = await invoke<RewindTarget[]>("rewind_targets", { session: id }).catch(
          () => [],
        );
        patch(id, (state) => ({
          ...state,
          ...replayed,
          rewindAsk: null,
          rewinding: false,
          rewindTargets: targets,
          draft: done.text,
          failed: false,
          activity: "rewound",
          meter: adoptContext(
            { ...state.meter, turn: NO_USAGE },
            done.session.context_tokens,
            done.session.context_estimated,
          ),
          blocks: [
            ...replayed.blocks,
            ...(done.restored.length > 0
              ? [
                  noteBlock(
                    `files rolled back — ${done.restored
                      .map((file) => `${file.path}: ${file.outcome}`)
                      .join(", ")}`,
                  ),
                ]
              : []),
          ],
        }));
      } catch (error) {
        patch(id, (state) => ({
          ...state,
          rewinding: false,
          rewindAsk: null,
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

  /**
   * Answer a plan review: the edits, the comments and the decision, in one call.
   *
   * A refused answer is the one case where the question comes *back*. The backend
   * rejects a breakdown it cannot apply without consuming the request — a turn is
   * parked on it — so the panel returns rather than leaving the conversation
   * waiting on a dialog nobody can see.
   */
  const decidePlan = useCallback(
    async (id: string, choice: PlanDecision) => {
      const pending = states[id]?.approval;
      if (!pending) return;
      patch(id, (state) => ({
        ...state,
        approval: null,
        handoffPending: choice.fresh && choice.decision === "yes",
      }));
      try {
        await invoke("respond_approval", {
          session: id,
          answer: {
            id: pending.id,
            decision: choice.decision,
            comment: choice.note || null,
            phases: choice.phases,
            notes: choice.comments.map((entry) => ({ quote: entry.quote, text: entry.text })),
            fresh_session: choice.fresh,
          },
        });
        patch(id, (state) => ({ ...state, planDraft: null }));
      } catch (error) {
        patch(id, (state) => ({
          ...state,
          approval: pending,
          handoffPending: false,
          blocks: [...state.blocks, errorBlock(String(error))],
        }));
      }
    },
    [states, patch],
  );

  const savePlan = useCallback(
    async (id: string) => {
      const draft = states[id]?.planDraft;
      if (!draft) return;
      try {
        const plan = await invoke<Plan>("write_plan", {
          session: id,
          phases: fromDraft(draft.phases),
        });
        patch(id, (state) => ({ ...state, plan, planDraft: draftOf(plan) }));
      } catch (error) {
        patch(id, (state) => ({
          ...state,
          blocks: [...state.blocks, errorBlock(String(error))],
        }));
      }
    },
    [states, patch],
  );

  /**
   * Hand an approved plan to a fresh conversation, once the planning turn has
   * ended.
   *
   * Waiting for the end is the whole correctness of it: the `progress` tool runs
   * *after* the approval is answered, and only then is the plan on disk marked
   * active. An effect rather than a line in the `TURN_FINISHED` listener because
   * this reads state and starts work, which is not a reducer's job.
   */
  useEffect(() => {
    for (const [id, state] of Object.entries(states)) {
      if (!state.handoffPending || state.running || state.approval) continue;
      patch(id, (was) => ({ ...was, handoffPending: false }));
      invoke<OpenedSession>("execute_plan_elsewhere", { session: id })
        .then((opened) => {
          const { session, history } = opened;
          const replayed = replayLedger(history);
          setSessions((current) =>
            current.some((open) => open.id === session.id) ? current : [...current, session],
          );
          setStates((current) => ({
            ...current,
            [session.id]: {
              ...BLANK,
              ...replayed,
              running: true,
              activity: "executing the plan",
              meter: adoptContext(BLANK.meter, opened.context_tokens, opened.context_estimated),
            },
          }));
          // Beside, not instead: the plan came from the conversation next to it,
          // and both are worth watching while one works.
          setTiling((current) => showBeside(current, session.id));
        })
        .catch((error) =>
          patch(id, (was) => ({
            ...was,
            blocks: [...was.blocks, errorBlock(String(error))],
          })),
        );
    }
  }, [states, patch]);

  // A session that needs an answer pulls the view to itself, but only from the
  // launchpad: yanking someone out of a conversation they are reading would be
  // worse than the delay in noticing.
  const waiting = sessions.find((open) => states[open.id]?.approval);
  const alerted = useRef<string | null>(null);
  useEffect(() => {
    if (!waiting || tiling.root !== null) return;
    if (alerted.current === waiting.id) return;
    alerted.current = waiting.id;
    setTiling((current) => show(current, waiting.id));
  }, [waiting, tiling.root]);

  const stateOf = useCallback((id: string) => states[id] ?? BLANK, [states]);

  if (fault) return <Fault reason={fault} />;

  if (!tiling.root) {
    return (
      <Launchpad
        open={sessions}
        statusOf={statusOf}
        activityOf={(id) => states[id]?.activity ?? BLANK.activity}
        onEnter={(id) => setTiling((current) => show(current, id))}
        onOpenFolder={openFolder}
      />
    );
  }

  return (
    <ToolMetaProvider meta={toolMeta}>
      {/* Wraps the whole window, which is what makes a sub-agent's reasoning obey
          the same switch as the conversation that delegated it: the transcript
          reads this at every depth it recurses to. */}
      <DisplayContext.Provider value={display}>
        <LimitsContext.Provider value={limits}>
          <Workspace
            tiling={tiling}
            display={display}
            onDisplay={changeDisplay}
            sessions={sessions}
            stateOf={stateOf}
            statusOf={statusOf}
            onTiling={(step) => setTiling(step)}
            onDraft={(id, draft) => patch(id, (was) => ({ ...was, draft }))}
            onAttach={(id, items) =>
              patch(id, (was) => ({ ...was, attachments: [...was.attachments, ...items] }))
            }
            onDetach={(id, item) =>
              patch(id, (was) => ({
                ...was,
                attachments: was.attachments.filter((entry) => entry.id !== item),
              }))
            }
            onSend={send}
            onInterrupt={(id) => {
              invoke("interrupt", { session: id }).catch(() => {});
            }}
            onWithdrawQueued={withdrawQueued}
            onSendQueuedNow={sendQueuedNow}
            onAskRewind={askRewind}
            onRewind={rewind}
            onAnswer={answer}
            onDecidePlan={decidePlan}
            onPlanDraft={(id, draft) => patch(id, (was) => ({ ...was, planDraft: draft }))}
            onSavePlan={savePlan}
            onPlanOpen={(id, open) => patch(id, (was) => ({ ...was, planOpen: open }))}
            onPlanFirst={(id, on) => patch(id, (was) => ({ ...was, planFirst: on }))}
            onCloseSession={closeSession}
            onHome={() => setTiling(EMPTY)}
            onOpenFolder={openFolder}
          />
        </LimitsContext.Provider>
      </DisplayContext.Provider>
    </ToolMetaProvider>
  );
}

/**
 * Whether this event could have changed the plan on disk.
 *
 * Only `progress` writes that file, and only when its call finishes: asking on
 * `ToolStart` would read the file the tool is still writing.
 */
function touchedPlan(event: AgentEvent): boolean {
  if (event.type !== "ToolEnd") return false;
  return (event.data as { name?: string }).name === "progress";
}

/**
 * Keep a draft across a re-read of the plan.
 *
 * A draft with edits in it survives — those are the user's words, and dropping
 * them because a phase flipped somewhere else would be the worst possible moment
 * to do it. An untouched draft is rebuilt from the new plan, and a draft for a
 * different plan file is abandoned: it was about something else.
 */
function rebase(draft: PlanDraft | null, plan: Plan | null): PlanDraft | null {
  if (!plan) return null;
  if (!draft || draft.path !== plan.path) return draftOf(plan);
  const untouched = draft.phases === draft.base && draft.comments.length === 0;
  return untouched ? draftOf(plan) : draft;
}

/**
 * The one-line summary the rail and the launchpad card show for a session.
 *
 * The tool's name and what the call is about, once each. Pasting core's summary
 * after the name printed the name twice for every tool whose summary *is* its
 * name — `progress progress`, `agent agent(explore)` — because `summarize_call`
 * falls back to the bare name when it recognizes none of the call's argument
 * keys. `displayToolSummary` is the registry's answer to exactly that, and the
 * transcript has been using it all along.
 */
function describe(event: AgentEvent): string | null {
  switch (event.type) {
    case "ToolStart": {
      const data = event.data as { name: string; summary: string; input: unknown };
      const about = displayToolSummary(data.name, data.summary, data.input);
      return about && about !== data.name ? `${data.name} · ${about}` : data.name;
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
