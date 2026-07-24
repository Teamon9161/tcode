import { useEffect, useRef } from "react";

import { ArrowUp, StopIcon } from "./components/Icons";

/**
 * The input. One control that changes intent with the turn: send while idle,
 * stop while running — the same place your hand already is, rather than a stop
 * button that appears somewhere else on the screen mid-turn.
 */
export function Composer({
  value,
  running,
  disabled,
  onChange,
  onSubmit,
  onInterrupt,
}: {
  value: string;
  running: boolean;
  disabled: boolean;
  onChange: (value: string) => void;
  onSubmit: () => void;
  onInterrupt: () => void;
}) {
  const field = useRef<HTMLTextAreaElement>(null);

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

  return (
    <form
      className="composer"
      onSubmit={(event) => {
        event.preventDefault();
        onSubmit();
      }}
    >
      <textarea
        ref={field}
        value={value}
        rows={1}
        placeholder={running ? "running — Esc to stop" : "Ask for something in this folder"}
        disabled={disabled}
        onChange={(event) => onChange(event.target.value)}
        onKeyDown={(event) => {
          if (event.key === "Enter" && !event.shiftKey) {
            event.preventDefault();
            if (!running) onSubmit();
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
          disabled={disabled || !value.trim()}
          aria-label="Send"
          title="Send (Enter)"
        >
          <ArrowUp size={16} />
        </button>
      )}
    </form>
  );
}
