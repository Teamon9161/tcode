import { useEffect, useRef, useState } from "react";

import { imagesFrom, isImagePaste, type Pasted } from "./paste";
import { Chips } from "./Chips";
import { CloseIcon, ReturnIcon, StopIcon } from "./components/Icons";

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
/** Roughly ten lines: past that, scrolling the draft beats losing the
 *  conversation behind it. */
const MAX_HEIGHT = 220;

export function Composer({
  value,
  running,
  disabled,
  attachments,
  planFirst,
  onPlanFirst,
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
  /** Whether this message asks for a plan before anything is changed. */
  planFirst: boolean;
  onPlanFirst: (on: boolean) => void;
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
  //
  // The overflow is switched with the height rather than left on `auto`: a
  // field sized to exactly its content still renders a scrollbar under `auto`
  // in this webview, and a one-line prompt box with a scroll track down its
  // side is the kind of detail that makes an app look assembled rather than
  // built.
  useEffect(() => {
    const node = field.current;
    if (!node) return;
    node.style.height = "auto";
    const wanted = node.scrollHeight;
    node.style.height = `${Math.min(wanted, MAX_HEIGHT)}px`;
    node.style.overflowY = wanted > MAX_HEIGHT ? "auto" : "hidden";
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
          placeholder={
            running
              ? "running — Esc to stop"
              : planFirst
                ? "Describe what to plan"
                : "Ask for something in this folder"
          }
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
        {/* Inside the field's own box, not a filled button beside it. What sends
            a message here is the return key; the glyph is a reminder of that
            and a target for the pointer, so it shows the key rather than
            inventing a second control with its own colour and weight. It is
            `--faint` with nothing to send and `--ink` once there is — the state
            is carried by the same mark getting darker, which is as quiet as a
            control can be while still being one. */}
        {running ? (
          <button
            type="button"
            className="composer-key is-stop"
            onClick={onInterrupt}
            aria-label="Stop this turn"
            title="Stop (Esc)"
          >
            <StopIcon size={13} />
          </button>
        ) : (
          <button
            type="submit"
            className="composer-key"
            disabled={!sendable}
            aria-label="Send"
            title="Send (Enter)"
          >
            <ReturnIcon size={16} />
          </button>
        )}
      </div>

      {/* Under the field, not above it and not in a settings screen: these are
          properties of the message about to be sent — and so is this one.
          Planning used to be a permission mode; it is a property of a request
          now, which is exactly what this row holds. It clears when the message
          is sent, like an attachment does, because it described that message. */}
      {/* The one control on this row that is a switch rather than a menu, so it
          is the one that has to look like it: an outline and a box that fills.
          Bare like its neighbours, it read as a label announcing that planning
          was already on — the opposite of its actual state. */}
      <div className="composer-row-chips">
        <button
          type="button"
          className={`chip is-toggle${planFirst ? " is-on" : ""}`}
          aria-pressed={planFirst}
          onClick={() => onPlanFirst(!planFirst)}
          title="Investigate and write a plan for your approval before changing anything"
        >
          <span className="chip-tick" aria-hidden="true" />
          plan first
        </button>
        <Chips />
      </div>
    </form>
  );
}
