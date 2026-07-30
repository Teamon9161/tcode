import type { Queued } from "./types";
import { CloseIcon, ReturnIcon } from "./components/Icons";

/**
 * What you said while it was working, and what you can still do about it.
 *
 * Typing during a turn is the most ordinary thing there is — you see where it is
 * going before it gets there — and until now the composer simply refused. The
 * prompt has to go *somewhere*, and the one place it must not go is invisibly
 * into a queue: a message that was accepted and is not on screen is a message
 * you will send twice.
 *
 * So it sits between the conversation and the composer, in the order it will be
 * delivered, and every row can be taken back. Above the composer rather than in
 * the transcript because it has not happened yet: the transcript is a record,
 * and putting a thing that might never be sent into it would make the record
 * conditional. When core delivers one at a safe boundary, `QueuedInput` puts it
 * in the transcript for real and it leaves here.
 *
 * "Send now" stops the turn. It is the one control here that destroys something
 * — a turn mid-flight — so it is worded as what it does rather than as a
 * direction, and it is not the row's default action.
 */
export function QueueStrip({
  queued,
  onWithdraw,
  onSendNow,
}: {
  queued: Queued[];
  onWithdraw: (index: number, text: string) => void;
  onSendNow: () => void;
}) {
  if (queued.length === 0) return null;

  return (
    <section className="queue-strip" aria-label="Waiting to be sent">
      <div className="queue-body">
        <div className="queue-head">
          <span className="queue-title">
            {queued.length === 1 ? "queued" : `queued · ${queued.length}`}
          </span>
          <span className="queue-note">sent when this turn reaches a safe point</span>
          <button type="button" className="queue-now" onClick={onSendNow}>
            <ReturnIcon size={12} />
            Stop and send now
          </button>
        </div>

        <ol className="queue-list">
          {queued.map((item, index) => (
            <li key={index} className="queue-item">
              <span className="queue-text">{item.text}</span>
              {item.attachments.length > 0 && (
                <span className="queue-attachments">
                  {item.attachments.length} attached
                </span>
              )}
              <button
                type="button"
                className="queue-drop"
                onClick={() => onWithdraw(index, item.text)}
                title="Take this back"
                aria-label={`Take back: ${item.text}`}
              >
                <CloseIcon size={12} />
              </button>
            </li>
          ))}
        </ol>
      </div>
    </section>
  );
}
