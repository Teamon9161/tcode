/**
 * What the platform calls the modifier every shortcut in this app carries.
 *
 * Named from the user agent because this app ships on all three desktops and
 * "Ctrl" shown to a Mac user is simply wrong. It lives in its own module rather
 * than beside the first thing that needed it: the empty conversation lists the
 * layout keys, and menus print their own beside each item, and two copies of
 * this test is one place for the answer to drift.
 */
export const MOD = /mac/i.test(navigator.platform || navigator.userAgent) ? "Cmd" : "Ctrl";

/** True when this event carries the platform's own command modifier. */
export function modKey(event: { ctrlKey: boolean; metaKey: boolean }): boolean {
  return event.ctrlKey || event.metaKey;
}

/** Whether a keystroke happened inside a terminal, which is the one surface in
 *  this window where the app is not entitled to most of the keyboard. */
export function inTerminal(target: EventTarget | null): boolean {
  return target instanceof Element && !!target.closest(".pane-body.is-term");
}

/**
 * The short list of chords the app keeps even while a shell has the keyboard.
 *
 * Everything else in a terminal is the shell's, and that is not generosity: a
 * terminal you cannot press `Ctrl+C`, `Ctrl+D`, `Ctrl+R`, `Ctrl+W` or `Ctrl+U`
 * in is not a terminal. So the app's own `Mod+W` (close pane), `Mod+N` (new
 * conversation) and `Mod+1…9` are given up in here, and what stays is only what
 * has no readline meaning at all:
 *
 *  - `Mod+J`, because it is the way *out* — a toggle you can enter but not
 *    leave is a trap. The cost is that a terminal can no longer be sent a bare
 *    `^J` (line feed, which is what `Enter` sends anyway); that is the price of
 *    the one binding this pane exists behind.
 *  - `Mod+Alt+←↑↓→` and `Mod+Alt+R`, which move between panes and turn a seam.
 *    Nothing in a shell uses `Ctrl+Alt`.
 *  - `Mod+Shift+T` / `Mod+Shift+W`, the tab verbs, spelled the way every
 *    terminal emulator already spells them.
 *
 * Both sides read this one predicate: xterm is told not to handle these
 * (`termHost.ts`), and the window's own handlers act on them. Two lists would
 * mean a key that is both sent to the shell and acted on by the app.
 */
export function appOwnedInTerminal(event: KeyboardEvent): boolean {
  if (!modKey(event)) return false;
  if (event.altKey) {
    return /^(Arrow(Left|Right|Up|Down)|r|R)$/.test(event.key);
  }
  if (event.shiftKey) return /^[tTwW]$/.test(event.key);
  return event.key === "j" || event.key === "J";
}
