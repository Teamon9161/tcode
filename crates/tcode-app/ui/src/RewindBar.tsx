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
  const n = preview.dropped;

  return (
    <section className="dock rewind-bar" aria-label="Go back to an earlier message">
      <div className="rewind-body">
        <div className="rewind-top">
          <span className="rewind-icon" aria-hidden="true">
            <RewindIcon size={14} />
          </span>
          <p className="rewind-question">
            Drop {n === 1 ? "1 message" : `${n} messages`} and return the prompt to the
            composer?
          </p>
          <div className="rewind-actions">
            <button type="button" className="btn btn-ghost btn-sm" onClick={onCancel} disabled={busy}>
              Cancel
            </button>
            <button
              type="button"
              className="btn btn-primary btn-sm"
              onClick={() => onConfirm(restoreFiles)}
              disabled={busy}
            >
              {busy ? "Going back…" : "Go back"}
            </button>
          </div>
        </div>

        {preview.dirty && (
          <button
            type="button"
            className={`rewind-files${restoreFiles ? " is-on" : ""}`}
            role="switch"
            aria-checked={restoreFiles}
            onClick={() => setRestoreFiles((was) => !was)}
          >
            <span className="chip-tick" aria-hidden="true" />
            <span className="rewind-label">Also roll back files changed since this point</span>
          </button>
        )}
      </div>
    </section>
  );
}
