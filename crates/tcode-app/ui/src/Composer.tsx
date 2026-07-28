import { useEffect, useRef, useState } from "react";

import { imagesFrom, isImagePaste, type Pasted } from "./paste";
import { ArrowUp, CloseIcon, StopIcon } from "./components/Icons";

/**
 * The input. One control that changes intent with the turn: send while idle,
 * stop while running — the same place your hand already is, rather than a stop
 * button that appears somewhere else on the screen mid-turn.
 *
 * It also takes images, by paste or by drop. They ride as chips above the text
 * rather than as a token inside it (the TUI's `[Image #1]` exists because a
 * terminal cannot show a thumbnail); removing one is the chip's own ×, and what
 * is on screen is exactly what will be sent.
 */
export function Composer({
  value,
  running,
  disabled,
  attachments,
  onChange,
  onAttach,
  onDetach,
  onSubmit,
  onInterrupt,
}: {
  value: string;
  running: boolean;
  disabled: boolean;
  attachments: Pasted[];
  onChange: (value: string) => void;
  onAttach: (items: Pasted[]) => void;
  onDetach: (id: string) => void;
  onSubmit: () => void;
  onInterrupt: () => void;
}) {
  const field = useRef<HTMLTextAreaElement>(null);
  const [dropping, setDropping] = useState(false);
  const [failure, setFailure] = useState<string | null>(null);

  // Focus returns whenever the turn ends, so a conversation is typed without
  // ever reaching for the mouse.
  useEffect(() => {
    if (!running && !disabled) field.current?.focus();
  }, [running, disabled]);

  // Grow with the content up to a ceiling, then scroll. Set before paint so a
  // pasted block never flashes at one row first.
  useEffect(() => {
    const node = field.current;
    if (!node) return;
    node.style.height = "auto";
    node.style.height = `${Math.min(node.scrollHeight, 220)}px`;
  }, [value]);

  const take = (transfer: DataTransfer | null) => {
    setFailure(null);
    imagesFrom(transfer)
      .then((images) => images.length > 0 && onAttach(images))
      .catch((error) => setFailure(`could not read that image: ${String(error)}`));
  };

  const sendable = (value.trim().length > 0 || attachments.length > 0) && !disabled;

  return (
    <form
      className={`composer${dropping ? " is-dropping" : ""}`}
      onSubmit={(event) => {
        event.preventDefault();
        if (sendable) onSubmit();
      }}
      onDragOver={(event) => {
        if (!isImagePaste(event.dataTransfer)) return;
        event.preventDefault();
        setDropping(true);
      }}
      onDragLeave={() => setDropping(false)}
      onDrop={(event) => {
        if (!isImagePaste(event.dataTransfer)) return;
        event.preventDefault();
        setDropping(false);
        take(event.dataTransfer);
      }}
    >
      {attachments.length > 0 && (
        <ul className="attachments">
          {attachments.map((item) => (
            <li key={item.id} className="attachment">
              <img className="attachment-thumb" src={item.url} alt={item.name} />
              <span className="attachment-name">{item.name}</span>
              <button
                type="button"
                className="attachment-remove"
                onClick={() => onDetach(item.id)}
                aria-label={`Remove ${item.name}`}
              >
                <CloseIcon size={12} />
              </button>
            </li>
          ))}
        </ul>
      )}

      {failure && (
        <p className="composer-note" role="alert">
          {failure}
        </p>
      )}

      <div className="composer-row">
        <textarea
          ref={field}
          value={value}
          rows={1}
          placeholder={running ? "running — Esc to stop" : "Ask for something in this folder"}
          disabled={disabled}
          onChange={(event) => onChange(event.target.value)}
          onPaste={(event) => {
            // Only intercept images; a text paste stays the textarea's.
            if (!isImagePaste(event.clipboardData)) return;
            event.preventDefault();
            take(event.clipboardData);
          }}
          onKeyDown={(event) => {
            if (event.key === "Enter" && !event.shiftKey) {
              event.preventDefault();
              if (!running && sendable) onSubmit();
              return;
            }
            if (event.key === "Escape" && running) {
              event.preventDefault();
              onInterrupt();
            }
          }}
        />
        {running ? (
          <button
            type="button"
            className="btn btn-icon btn-stop"
            onClick={onInterrupt}
            aria-label="Stop this turn"
            title="Stop (Esc)"
          >
            <StopIcon size={15} />
          </button>
        ) : (
          <button
            type="submit"
            className="btn btn-icon btn-primary"
            disabled={!sendable}
            aria-label="Send"
            title="Send (Enter)"
          >
            <ArrowUp size={16} />
          </button>
        )}
      </div>
    </form>
  );
}
