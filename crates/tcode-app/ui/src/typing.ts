/** Whether moving focus away from this target would interrupt typing. This is
 * shared by composer focus restoration and pane-level Escape handling so a
 * contenteditable editor cannot fall through one list but not the other. */
export function isTyping(target: EventTarget | null): boolean {
  return (
    target instanceof HTMLElement &&
    (target instanceof HTMLInputElement ||
      target instanceof HTMLTextAreaElement ||
      Boolean(target.isContentEditable))
  );
}
