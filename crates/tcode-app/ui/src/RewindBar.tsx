import { useState } from "react";

import type { RewindPreview } from "./types";
import { RewindIcon } from "./components/Icons";

/**
 * "Go back to here?" — the one question worth stopping for.
 *
 * Rewinding is the only operation in this app that destroys conversation, and
 * the only one that can undo work on disk. So it is asked rather than done, and
 * the two halves are asked separately because they are separate: dropping
 * messages is recoverable by retyping, while rolling files back throws away
 * edits that may have been made by hand since. The file box is unticked, and it
 * is absent entirely when nothing changed — an option that never applies is a
 * decision nobody should be made to read.
 *
 * A docked bar, not a modal, for the same reason `Approval` is one (AGENTS.md
 * rule 9b): the other panes are other conversations, and a question in this one
 * must not freeze them. It sits above the composer, on the same `--measure` axis
 * as everything else on the conversation's column.
 *
 * What it says is the count, in words, because that is the part no click takes
 * back. It deliberately does not preview the messages — they are on screen,
 * directly above, which is where the button was pressed.
 */
export function RewindBar({
  preview,
  busy,
  onConfirm,
  onCancel,
}: {
  preview: RewindPreview;
  /** The command is in flight; the answer must not be given twice. */
  busy: boolean;
  onConfirm: (restoreFiles: boolean) => void;
  onCancel: () => void;
}) {
  const [restoreFiles, setRestoreFiles] = useState(false);
  const messages = preview.dropped === 1 ? "1 message" : `${preview.dropped} messages`;

  return (
    <section className="rewind-bar" aria-label="Go back to an earlier message">
      <div className="rewind-body">
        <p className="rewind-question">
          <RewindIcon size={13} />
          Go back to this message? This drops the {messages} after it, and returns the
          prompt to the composer to edit.
        </p>

        {preview.dirty && (
          <button
            type="button"
            className={`rewind-files${restoreFiles ? " is-on" : ""}`}
            role="switch"
            aria-checked={restoreFiles}
            onClick={() => setRestoreFiles((was) => !was)}
          >
            <span className="chip-tick" aria-hidden="true" />
            <span className="rewind-lines">
              <span className="rewind-label">Also roll back the files it changed</span>
              <span className="rewind-hint">
                Anything edited since — by the agent or by you — goes back to how it was.
              </span>
            </span>
          </button>
        )}

        <div className="rewind-actions">
          <button type="button" className="btn btn-ghost" onClick={onCancel} disabled={busy}>
            Keep it
          </button>
          <button
            type="button"
            className="btn btn-primary"
            onClick={() => onConfirm(restoreFiles)}
            disabled={busy}
          >
            {busy ? "Going back…" : "Go back"}
          </button>
        </div>
      </div>
    </section>
  );
}
