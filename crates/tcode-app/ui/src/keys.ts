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
