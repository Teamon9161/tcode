/** Stand-in for `@tauri-apps/api/event`: the preview drives state directly. */
export async function listen<T>(
  _name: string,
  _handler: (event: { payload: T }) => void,
): Promise<() => void> {
  return () => {};
}
