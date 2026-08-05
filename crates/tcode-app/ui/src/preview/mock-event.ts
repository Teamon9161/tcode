/**
 * Stand-in for `@tauri-apps/api/event`.
 *
 * Most of the preview drives state directly, so most listeners never fire. The
 * exception is the terminal: what that pane looks like *is* what a shell wrote
 * into it, and no other fixture shape can say so — a terminal with nothing in
 * it demonstrates only that the pane exists, and the thing worth looking at is
 * the app's ANSI palette rendered on paper. So this keeps a registry and
 * `mock-core.ts` plays a canned session through it.
 */

type Handler = (event: { payload: unknown }) => void;

const listeners = new Map<string, Set<Handler>>();

export async function listen<T>(
  name: string,
  handler: (event: { payload: T }) => void,
): Promise<() => void> {
  const known = listeners.get(name) ?? new Set<Handler>();
  known.add(handler as Handler);
  listeners.set(name, known);
  return () => {
    known.delete(handler as Handler);
  };
}

/** Preview only: deliver an event the way the backend would have. */
export function deliver(name: string, payload: unknown) {
  for (const handler of listeners.get(name) ?? []) handler({ payload });
}
