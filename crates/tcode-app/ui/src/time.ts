/**
 * Relative time against the backend's clock.
 *
 * `now` comes from Rust rather than `Date.now()` because the webview's clock
 * and the backend's can disagree after a suspend, and a session stamped "in 3
 * minutes" reads as a bug in the app rather than in the clock.
 */
export function ago(unix: number | null, now: number): string {
  if (unix === null) return "";
  const seconds = Math.max(0, now - unix);
  if (seconds < 60) return "just now";
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  if (days === 1) return "yesterday";
  if (days < 30) return `${days}d ago`;
  const months = Math.floor(days / 30);
  if (months < 12) return `${months}mo ago`;
  return `${Math.floor(months / 12)}y ago`;
}

/** `/home/teamon/code/rust/tcode` → `~/code/rust/tcode`. */
export function tilde(path: string, home: string | null): string {
  if (home && path.startsWith(home)) return `~${path.slice(home.length)}`;
  return path;
}
