/**
 * The plan, as data: what the backend sends, what the editor holds, and the
 * pure operations between them.
 *
 * Two rules shape this file.
 *
 * **The markdown grammar of a progress file is core's, and it stays there.** A
 * plan travels here as a *breakdown* — phases with a status and prose — and goes
 * back the same way, so nothing on this side has to know that a phase is a
 * `## [x] 2. Title` heading. That is why "what changed" below is a structural
 * comparison rather than a text diff of two rendered documents: rendering one
 * would mean owning the grammar, and a second implementation of it would drift
 * on the first schema change.
 *
 * **Editing is identity-preserving.** A draft phase carries an `id` the moment
 * the draft is made, which is what lets a retitled phase read as *this phase,
 * renamed* rather than as one phase deleted and another invented. Matching by
 * title — the only alternative — gets that wrong exactly when the user is doing
 * the most careful work.
 */

export type PhaseStatus = "pending" | "in_progress" | "completed";

/** Mirrors `tcode_core::progress::Phase`, as `PlanView` serializes it. */
export type PlanPhase = {
  phase: string;
  status: PhaseStatus;
  detail: string;
  phases: PlanPhase[];
};

/** Mirrors `PlanView` in `src/commands.rs`. */
export type Plan = {
  path: string;
  file: string;
  title: string;
  /** One line: what this plan is for. */
  description: string;
  /** The plan's prose — everything that belongs to no single phase. Shown but
   *  not edited here; core carries it through a structural revision untouched
   *  (`revise_plan_body`), which is what keeps this editor from deleting it. */
  background: string;
  state: "draft" | "active" | "done";
  done: number;
  total: number;
  phases: PlanPhase[];
};

/** A phase while it is being edited. Same shape plus the identity above. */
export type DraftPhase = {
  id: string;
  phase: string;
  status: PhaseStatus;
  detail: string;
  phases: DraftPhase[];
};

/** Which text of a phase a comment is about. */
export type PhaseField = "phase" | "detail";

/**
 * One comment the reviewer left, anchored to the passage it is about.
 *
 * `quote` is a snapshot taken when the comment was made, and it is what the
 * model receives (`tcode_core::progress::PlanNote`). Deliberately not an offset:
 * the text under it is being edited in the same sitting, and an offset would
 * silently come to point at something else. `path`/`field` only decide where the
 * comment is drawn.
 */
export type PlanComment = {
  id: string;
  path: PhasePath;
  field: PhaseField;
  quote: string;
  text: string;
};

/** Everything the review panel and the plan pane hold between them. */
export type PlanDraft = {
  /** The plan this draft was made from, so a plan replaced underneath the
   *  editor is noticed rather than silently written over. */
  path: string;
  /** The breakdown as it stood when editing began, for "what changed". */
  base: DraftPhase[];
  phases: DraftPhase[];
  comments: PlanComment[];
};

/** Where a phase sits: `[i]` for a top-level phase, `[i, j]` for a sub-phase.
 *  Two levels is core's hard cap, so this is never longer than two. */
export type PhasePath = number[];

let counter = 0;

/** Ids are per-session and never persisted: they identify a row inside one
 *  editing sitting, which is exactly as long as they mean anything. */
function nextId(): string {
  counter += 1;
  return `p${counter}`;
}

export function toDraft(phases: PlanPhase[]): DraftPhase[] {
  return phases.map((phase) => ({
    id: nextId(),
    phase: phase.phase,
    status: phase.status,
    detail: phase.detail,
    phases: toDraft(phase.phases ?? []),
  }));
}

/** The wire shape: ids are ours, and the backend validates what it gets. */
export function fromDraft(phases: DraftPhase[]): PlanPhase[] {
  return phases.map((phase) => ({
    phase: phase.phase.trim(),
    status: phase.status,
    detail: phase.detail.trim(),
    phases: fromDraft(phase.phases),
  }));
}

export function draftOf(plan: Plan): PlanDraft {
  const base = toDraft(plan.phases);
  return { path: plan.path, base, phases: base, comments: [] };
}

export function phaseAt(phases: DraftPhase[], path: PhasePath): DraftPhase | undefined {
  const [head, ...rest] = path;
  const found = phases[head];
  if (!found) return undefined;
  return rest.length === 0 ? found : phaseAt(found.phases, rest);
}

/** Replace one phase, keeping every other row identical. */
export function editAt(
  phases: DraftPhase[],
  path: PhasePath,
  change: (phase: DraftPhase) => DraftPhase,
): DraftPhase[] {
  const [head, ...rest] = path;
  return phases.map((phase, index) => {
    if (index !== head) return phase;
    if (rest.length === 0) return change(phase);
    return { ...phase, phases: editAt(phase.phases, rest, change) };
  });
}

export function removeAt(phases: DraftPhase[], path: PhasePath): DraftPhase[] {
  const [head, ...rest] = path;
  if (rest.length === 0) return phases.filter((_, index) => index !== head);
  return phases.map((phase, index) =>
    index === head ? { ...phase, phases: removeAt(phase.phases, rest) } : phase,
  );
}

/** A new, empty phase — at the end of the list, or of one phase's sub-phases.
 *  `parent` of `null` adds a top-level phase. */
export function addPhase(phases: DraftPhase[], parent: PhasePath | null): DraftPhase[] {
  const blank: DraftPhase = {
    id: nextId(),
    phase: "",
    status: "pending",
    detail: "",
    phases: [],
  };
  if (parent === null) return [...phases, blank];
  return editAt(phases, parent, (phase) => ({
    ...phase,
    phases: [...phase.phases, { ...blank, id: nextId() }],
  }));
}

/**
 * Move a phase among its own siblings.
 *
 * Only among siblings: a sub-phase dragged out to the top level, or a phase with
 * sub-phases dragged into one, would silently reshape the plan — and the second
 * would breach core's two-level cap, which is a rejected write rather than a
 * rendering problem. Order within a level is what a reviewer is actually
 * adjusting.
 */
export function movePhase(phases: DraftPhase[], path: PhasePath, to: number): DraftPhase[] {
  const parent = path.slice(0, -1);
  const from = path[path.length - 1];
  const reorder = (list: DraftPhase[]): DraftPhase[] => {
    if (to < 0 || to >= list.length || to === from) return list;
    const next = [...list];
    const [moved] = next.splice(from, 1);
    next.splice(to, 0, moved);
    return next;
  };
  if (parent.length === 0) return reorder(phases);
  return editAt(phases, parent, (phase) => ({ ...phase, phases: reorder(phase.phases) }));
}

/** Click a phase's box to advance it. The file is the user's; marking a phase
 *  done by hand is a legitimate edit, and the model is handed their version. */
export function nextStatus(status: PhaseStatus): PhaseStatus {
  return status === "pending" ? "in_progress" : status === "in_progress" ? "completed" : "pending";
}

export const STATUS_MARK: Record<PhaseStatus, string> = {
  pending: "○",
  in_progress: "●",
  completed: "✓",
};

/** One row of the strip's phase list. */
export type PhaseRow = {
  path: PhasePath;
  phase: string;
  status: PhaseStatus;
  detail: string;
  depth: number;
};

/**
 * The phase list as the strip shows it: every top-level phase, and sub-phases
 * only under the one actually running.
 *
 * What a finished or not-yet-started phase breaks down into is detail nobody is
 * reading at a glance — the same judgement the TUI's pane makes, for the same
 * reason. The editor shows everything; this is the glance.
 */
export function phaseRows(phases: DraftPhase[] | PlanPhase[]): PhaseRow[] {
  const rows: PhaseRow[] = [];
  phases.forEach((phase, index) => {
    rows.push({
      path: [index],
      phase: phase.phase,
      status: phase.status,
      detail: phase.detail,
      depth: 0,
    });
    if (phase.status !== "in_progress") return;
    (phase.phases ?? []).forEach((child, at) => {
      rows.push({
        path: [index, at],
        phase: child.phase,
        status: child.status,
        detail: child.detail,
        depth: 1,
      });
    });
  });
  return rows;
}

/**
 * The phase the plan is on, for the collapsed strip's one line.
 *
 * A running sub-phase wins over its running parent: it is the more specific
 * answer to "where is this", and the parent is already named by the title.
 * With nothing running, the next pending phase is what a reader wants; with
 * nothing pending either, the plan is finished and there is no current phase.
 */
export function currentPhase(phases: PlanPhase[] | DraftPhase[]): PhaseRow | null {
  const flat: PhaseRow[] = [];
  phases.forEach((phase, index) => {
    flat.push({ path: [index], phase: phase.phase, status: phase.status, detail: phase.detail, depth: 0 });
    (phase.phases ?? []).forEach((child, at) => {
      flat.push({
        path: [index, at],
        phase: child.phase,
        status: child.status,
        detail: child.detail,
        depth: 1,
      });
    });
  });
  const running = flat.filter((row) => row.status === "in_progress");
  if (running.length > 0) return running[running.length - 1];
  return flat.find((row) => row.status === "pending") ?? null;
}

/** What the reviewer changed, phase by phase. */
export type PlanChange =
  | { kind: "added"; title: string }
  | { kind: "removed"; title: string }
  | { kind: "renamed"; title: string; from: string }
  | { kind: "moved"; title: string }
  | { kind: "status"; title: string; from: PhaseStatus; to: PhaseStatus }
  | { kind: "detail"; title: string; from: string; to: string };

/**
 * Compare the draft against the plan it was made from.
 *
 * Structural, not textual: it answers "what did I change" in the terms the
 * reviewer was working in, and a detail edit carries both texts so the panel can
 * show that one field as a diff. Comparing by `id` is what makes a rename a
 * rename; a moved phase reports the move and nothing else, because "it is
 * earlier now" is the whole change.
 */
export function planChanges(base: DraftPhase[], draft: DraftPhase[]): PlanChange[] {
  const flatten = (list: DraftPhase[]): DraftPhase[] =>
    list.flatMap((phase) => [phase, ...flatten(phase.phases)]);
  const before = flatten(base);
  const after = flatten(draft);
  const byId = new Map(before.map((phase) => [phase.id, phase]));
  const changes: PlanChange[] = [];

  // Position is compared among the phases that exist in *both*, so deleting the
  // first phase does not report every phase after it as moved.
  const survived = (phase: DraftPhase) => byId.has(phase.id);
  const wasKept = (phase: DraftPhase) => after.some((now) => now.id === phase.id);
  const orderBefore = before.filter(wasKept).map((phase) => phase.id);
  const orderAfter = after.filter(survived).map((phase) => phase.id);

  after.forEach((phase) => {
    const was = byId.get(phase.id);
    if (!was) {
      if (phase.phase.trim() || phase.detail.trim()) {
        changes.push({ kind: "added", title: title(phase) });
      }
      return;
    }
    if (was.phase !== phase.phase) {
      changes.push({ kind: "renamed", title: title(phase), from: was.phase });
    }
    if (was.status !== phase.status) {
      changes.push({ kind: "status", title: title(phase), from: was.status, to: phase.status });
    }
    if (was.detail !== phase.detail) {
      changes.push({ kind: "detail", title: title(phase), from: was.detail, to: phase.detail });
    }
    if (orderBefore.indexOf(phase.id) !== orderAfter.indexOf(phase.id)) {
      changes.push({ kind: "moved", title: title(phase) });
    }
  });

  before.forEach((phase) => {
    if (!after.some((now) => now.id === phase.id)) {
      changes.push({ kind: "removed", title: title(phase) });
    }
  });

  return changes;
}

export function isEdited(draft: PlanDraft): boolean {
  return planChanges(draft.base, draft.phases).length > 0;
}

function title(phase: DraftPhase): string {
  return phase.phase.trim() || "(untitled phase)";
}

/**
 * Whether a `progress` call is a plan the user is meant to read.
 *
 * Mirrors `tcode_core::progress::is_submission`: `state: "active"` is the
 * submission, and the tool's own route says the same thing per call. The backend
 * sends each tool's *default* route (`tool_views`), which for `progress` is the
 * plan surface, so the one call that belongs in the conversation instead is
 * recognized here — one field, kept next to the type it reads.
 */
export function isPlanSubmission(input: unknown): boolean {
  return record(input)?.state === "active";
}

/** Whether an approval request is a plan review. Recognized by the shape of the
 *  call — the saved body core attaches to the review copy — exactly as an
 *  `ask_user` question form is recognized by its `questions` array. */
export function isPlanReview(input: unknown): boolean {
  const body = record(input)?.plan;
  return typeof body === "string" && body.trim().length > 0;
}

/** The plan body a submitted call carries, for the transcript's own record of
 *  the document the user was shown. */
export function planBody(input: unknown): string | null {
  const body = record(input)?.plan;
  if (typeof body === "string" && body.trim()) return body;
  return null;
}

function record(value: unknown): Record<string, unknown> | null {
  if (typeof value !== "object" || value === null) return null;
  return value as Record<string, unknown>;
}
