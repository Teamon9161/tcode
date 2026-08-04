import type { StoredSession } from "./types";

/**
 * The desktop interpretation of bare `/resume`.
 *
 * It stays with its conversation above the composer rather than opening a
 * window-wide modal: other panes may be running and this choice only replaces
 * the ledger in this one pane.
 */
export function ResumePicker({
  sessions,
  onChoose,
  onCancel,
}: {
  sessions: StoredSession[];
  onChoose: (id: string) => void;
  onCancel: () => void;
}) {
  return (
    <section className="dock resume-strip" aria-label="Resume a conversation">
      <div className="resume-body">
        <div className="resume-head">
          <strong>Resume a conversation</strong>
          <span>Choose a saved conversation for this folder</span>
          <button type="button" className="resume-cancel" onClick={onCancel}>
            Cancel
          </button>
        </div>
        {sessions.length === 0 ? (
          <p className="resume-empty">No earlier conversation is available in this folder.</p>
        ) : (
          <ul className="resume-list">
            {sessions.map((entry) => (
              <li key={entry.id}>
                <button type="button" className="resume-item" onClick={() => onChoose(entry.id)}>
                  <span>{entry.preview || "(no prompt yet)"}</span>
                  <code>{entry.id.slice(0, 8)}</code>
                </button>
              </li>
            ))}
          </ul>
        )}
      </div>
    </section>
  );
}
