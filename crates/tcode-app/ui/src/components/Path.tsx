/**
 * A path, abbreviated by whole segments when it is long.
 *
 * The obvious CSS trick for front-truncation — `direction: rtl` plus an
 * ellipsis — is wrong for paths: bidi reordering moves the leading `/` to the
 * *end*, so `/home/me/code` renders as `home/me/code/`. That is not a cosmetic
 * glitch; it is a wrong path shown to someone about to approve a file change.
 *
 * Dropping whole leading segments keeps the tail — the part that identifies the
 * file — and never rewrites a character. The full path stays in the tooltip.
 */
export function Path({
  path,
  home,
  keep = 3,
  className,
}: {
  path: string;
  /** Home directory; the path is shown as `~/…` when it is inside. */
  home?: string | null;
  /** How many trailing segments to keep before eliding the front. */
  keep?: number;
  className?: string;
}) {
  return (
    <span className={className} title={path}>
      {shorten(path, home ?? null, keep)}
    </span>
  );
}

export function shorten(path: string, home: string | null, keep: number): string {
  const shown = home && path.startsWith(home) ? `~${path.slice(home.length)}` : path;
  const segments = shown.split("/").filter(Boolean);
  if (segments.length <= keep + 1) return shown;
  return `…/${segments.slice(-keep).join("/")}`;
}
