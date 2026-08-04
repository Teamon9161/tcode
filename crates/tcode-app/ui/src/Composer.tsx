import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

import {
  imageFromNativeClipboard,
  imagesFrom,
  isImagePaste,
  needsNativeImageRead,
  type NativeClipboardImage,
  type Pasted,
} from "./paste";
import { Chips } from "./Chips";
import { CloseIcon, ReturnIcon, StopIcon } from "./components/Icons";
import type { Meter } from "./usage";

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

/**
 * How long a keystroke stays local before the window is told about it.
 *
 * The draft lives here, not in the window's state, and this is why. A
 * controlled field whose value round-trips through `App` re-renders every pane
 * on every keystroke, and with a long conversation on screen that is tens of
 * milliseconds of markdown re-lexing before the character appears — but the
 * cost that actually broke input is the round-trip itself: reassigning
 * `textarea.value` during an IME preedit cancels the composition, so the
 * candidate window flickers on every keystroke in Chinese, Japanese and Korean.
 *
 * The window still needs the draft — the file tree appends `@path` to it, a
 * rewind puts a prompt back into it — so it is published on an idle, on blur,
 * and on submit, rather than never. Nothing reads it faster than that: sending
 * carries the text with it (`onSubmit`), and blur lands before the click that
 * would mention a file.
 */
const PUBLISH_IDLE = 200;

export function Composer({
  value,
  running,
  disabled,
  attachments,
  meter,
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
  /** What this conversation occupies and what its last turn cost, for the ring
   *  on the strip below the field. */
  meter: Meter;
  /** Whether this message asks for a plan before anything is changed. */
  planFirst: boolean;
  onPlanFirst: (on: boolean) => void;
  onChange: (value: string) => void;
  onAttach: (items: Pasted[]) => void;
  onDetach: (id: string) => void;
  /** The text goes with it. Reading it back out of the window's state would
   *  race the publish above: a prompt typed and sent inside one idle window
   *  would send whatever the state still held. */
  onSubmit: (text: string) => void;
  onInterrupt: () => void;
}) {
  const field = useRef<HTMLTextAreaElement>(null);
  const [dropping, setDropping] = useState(false);
  const [failure, setFailure] = useState<string | null>(null);

  // What is in the field, and what the window was last told is in it. They
  // differ only between a keystroke and the publish that follows it.
  const [text, setText] = useState(value);
  const latest = useRef(value);
  const published = useRef(value);
  const idle = useRef<ReturnType<typeof setTimeout> | null>(null);
  // True between `compositionstart` and `compositionend`. A preedit is not a
  // draft — it is the IME's working area — so nothing leaves this component
  // and nothing resizes underneath it until the candidate is accepted.
  const composing = useRef(false);

  const publish = useCallback(() => {
    if (idle.current) {
      clearTimeout(idle.current);
      idle.current = null;
    }
    if (latest.current === published.current) return;
    published.current = latest.current;
    onChange(latest.current);
  }, [onChange]);

  // Someone else wrote the draft: a rewind putting a prompt back, the file tree
  // appending `@path`, a send clearing it. What we published ourselves comes
  // back identical and must not clobber anything typed since.
  useEffect(() => {
    if (value === published.current) return;
    if (idle.current) {
      clearTimeout(idle.current);
      idle.current = null;
    }
    published.current = value;
    latest.current = value;
    setText(value);
  }, [value]);

  // An unmount with a keystroke still pending would lose it — closing a pane
  // must not throw away what was typed into it.
  useEffect(() => () => publish(), [publish]);

  const change = (next: string) => {
    latest.current = next;
    setText(next);
    if (composing.current) return;
    if (idle.current) clearTimeout(idle.current);
    idle.current = setTimeout(publish, PUBLISH_IDLE);
  };

  /**
   * Grow with the content up to a ceiling, then scroll.
   *
   * `grow` is the IME's version. Measuring normally means clearing the height
   * to `auto` first, which collapses the field to one row for the instant it
   * takes to read `scrollHeight` — invisible at 60fps, but it moves the caret
   * rect twice per keystroke, and the candidate window is positioned from that
   * rect. That is the flicker. While a composition is open the height is left
   * where it is and only ever raised, so the caret does not move.
   *
   * The overflow is switched with the height rather than left on `auto`: a
   * field sized to exactly its content still renders a scrollbar under `auto`
   * in this webview, and a one-line prompt box with a scroll track down its
   * side is the kind of detail that makes an app look assembled rather than
   * built.
   */
  const resize = useCallback((grow: boolean) => {
    const node = field.current;
    if (!node) return;
    if (!grow) node.style.height = "auto";
    const wanted = node.scrollHeight;
    if (grow && wanted <= node.clientHeight) return;
    node.style.height = `${Math.min(wanted, MAX_HEIGHT)}px`;
    node.style.overflowY = wanted > MAX_HEIGHT ? "auto" : "hidden";
  }, []);

  // Focus returns whenever the turn ends, so a conversation is typed without
  // ever reaching for the mouse.
  useEffect(() => {
    if (!running && !disabled) field.current?.focus();
  }, [running, disabled]);

  useEffect(() => {
    resize(composing.current);
  }, [text, resize]);

  const take = (transfer: DataTransfer | null) => {
    setFailure(null);
    imagesFrom(transfer)
      .then((images) => images.length > 0 && onAttach(images))
      .catch((error) => setFailure(`could not read that image: ${String(error)}`));
  };

  const takeClipboardImage = (transfer: DataTransfer | null) => {
    setFailure(null);
    // Prefer the browser's File when it has one. WebKitGTK sometimes advertises
    // an image MIME but provides no File, in which case the native clipboard is
    // the only source that can still fulfill this user-initiated paste.
    imagesFrom(transfer)
      .catch(() => [])
      .then(async (images) => {
        if (images.length > 0) {
          onAttach(images);
          return;
        }
        const image = await invoke<NativeClipboardImage | null>("clipboard_image");
        if (!image) throw new Error("the system clipboard did not provide an image");
        onAttach([imageFromNativeClipboard(image)]);
      })
      .catch((error) => setFailure(`could not read that image: ${String(error)}`));
  };

  const sendable = (text.trim().length > 0 || attachments.length > 0) && !disabled;

  /** Hand the text over and empty the field in the same beat. The window
   *  clears the draft too, which arrives as an external write this has already
   *  agreed with. */
  const submit = () => {
    if (idle.current) {
      clearTimeout(idle.current);
      idle.current = null;
    }
    const sending = latest.current;
    latest.current = "";
    published.current = "";
    setText("");
    onSubmit(sending);
  };

  return (
    <form
      className={`composer${dropping ? " is-dropping" : ""}`}
      onSubmit={(event) => {
        event.preventDefault();
        if (sendable) submit();
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
          value={text}
          rows={1}
          placeholder={
            running
              ? "running — type to queue, Esc to stop"
              : planFirst
                ? "Describe what to plan"
                : "Ask for something in this folder"
          }
          disabled={disabled}
          onChange={(event) => change(event.target.value)}
          onCompositionStart={() => {
            composing.current = true;
            if (idle.current) {
              clearTimeout(idle.current);
              idle.current = null;
            }
          }}
          onCompositionEnd={(event) => {
            composing.current = false;
            // The accepted candidate is the first thing about this word that is
            // a draft, so it is the first thing published and the first thing
            // the field is measured for.
            change(event.currentTarget.value);
            resize(false);
          }}
          // Leaving the field settles it: whatever reads the draft next — the
          // file tree's mention, a slash command in another pane — sees what is
          // on screen rather than what was on screen 200ms ago.
          onBlur={publish}
          onPaste={(event) => {
            // Text stays native. The final branch catches the empty WebKitGTK
            // clipboard event shape, which can still be a system image paste.
            if (!needsNativeImageRead(event.clipboardData)) return;
            event.preventDefault();
            takeClipboardImage(event.clipboardData);
          }}
          onKeyDown={(event) => {
            // An IME uses Enter to accept the current candidate. That key must
            // stay native: cancelling it or sending here resets composition and
            // makes Chinese, Japanese, and Korean input visibly flicker.
            // WebKit can report composition after the keydown, where 229 is the
            // compatible signal for the same event.
            if (event.nativeEvent.isComposing || event.nativeEvent.keyCode === 229) return;

            if (event.key === "Enter" && !event.shiftKey) {
              event.preventDefault();
              // Enter sends while idle and queues while running — the same key
              // for the same act, because "say this" is one intention and which
              // of the two happens is the backend's answer, not the typist's.
              // This used to refuse outright while a turn ran, which made the
              // most ordinary thing in the app (seeing where it is going and
              // saying one more thing) impossible.
              if (sendable) submit();
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

      {/* Under the field, not above it and not in a settings screen: everything
          on this row is a property of the message about to be sent or of what
          will answer it. Planning used to be a permission mode; it is a property
          of a request now, and it clears when the message is sent, like an
          attachment does, because it described that message.

          One row, one height, one baseline. Every control on it is the same
          22px box whether or not it draws a border (`.chip` reserves the border
          it may not use), and the row's own inset puts the first label on the
          field's text column — the strip reads as a caption under the input
          rather than as a toolbar of differently-sized parts, which is what it
          looked like when a bordered switch stood beside bare labels. */}
      <div className="composer-strip">
        {/* The one control here that is a switch rather than a menu or a
            reading, so it is the one that has to look like it: an outline and a
            box that fills. Bare like its neighbours it read as a caption saying
            planning was already on — the opposite of its actual state. */}
        <button
          type="button"
          className={`chip is-toggle${planFirst ? " is-on" : ""}`}
          aria-pressed={planFirst}
          onClick={() => onPlanFirst(!planFirst)}
          title="Investigate and write a plan for your approval before changing anything"
        >
          <span className="chip-tick" aria-hidden="true" />
          plan
        </button>
        <Chips meter={meter} />
      </div>
    </form>
  );
}
