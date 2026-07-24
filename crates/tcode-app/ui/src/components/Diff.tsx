/**
 * The change an edit approval is actually asking for.
 *
 * The raw tool input is still one click away, but reading a JSON blob with
 * escaped newlines is not how anyone decides whether an edit is right. The edit
 * tools carry `old_string` / `new_string`, so the removed and added lines can be
 * shown as themselves.
 *
 * Lines are marked with a leading `−` / `+` as well as a tint: the two are
 * distinguished by shape first, colour second.
 */
export function Diff({ input }: { input: unknown }) {
  const change = readEdit(input);
  if (!change) return null;

  return (
    <div className="diff">
      {change.removed.map((line, index) => (
        <div className="diff-line diff-out" key={`o${index}`}>
          <span className="diff-sign">−</span>
          <span className="diff-text">{line || " "}</span>
        </div>
      ))}
      {change.added.map((line, index) => (
        <div className="diff-line diff-in" key={`i${index}`}>
          <span className="diff-sign">+</span>
          <span className="diff-text">{line || " "}</span>
        </div>
      ))}
    </div>
  );
}

/** True when the input has an edit shape this can draw. */
export function isEditShape(input: unknown): boolean {
  return readEdit(input) !== null;
}

const LIMIT = 40;

function readEdit(input: unknown): { removed: string[]; added: string[] } | null {
  if (typeof input !== "object" || input === null) return null;
  const record = input as Record<string, unknown>;
  const before = record.old_string ?? record.old_str ?? record.old;
  const after = record.new_string ?? record.new_str ?? record.new ?? record.content;
  if (typeof after !== "string") return null;

  const removed = typeof before === "string" ? clip(before) : [];
  const added = clip(after);
  if (removed.length === 0 && added.length === 0) return null;
  return { removed, added };
}

/** A dialog is for deciding, not for reading a whole file. */
function clip(text: string): string[] {
  const lines = text.split("\n");
  if (lines.length <= LIMIT) return lines;
  return [...lines.slice(0, LIMIT), `… ${lines.length - LIMIT} more lines`];
}
