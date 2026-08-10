/**
 * Keep disk-backed plan refreshes ordered per session.
 *
 * A `progress` ToolEnd and the enclosing turn finish can both request the plan.
 * IPC replies may arrive in the opposite order, so an older response must not
 * restore an already superseded status to the prompt dock.
 */
export class PlanRefreshes {
  private readonly latest = new Map<string, number>();

  begin(session: string): number {
    const next = (this.latest.get(session) ?? 0) + 1;
    this.latest.set(session, next);
    return next;
  }

  isCurrent(session: string, request: number): boolean {
    return this.latest.get(session) === request;
  }
}
