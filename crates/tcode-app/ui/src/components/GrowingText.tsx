import { useCallback, useEffect, useRef } from "react";

/**
 * A text field the height of the text in it.
 *
 * A `<textarea>` rather than `contenteditable`, which is not a detail: what
 * gets typed and pasted in here ends up beside model output, and a plain
 * textarea has no path from text to markup (AGENTS.md rule 10).
 *
 * It exists as a shared component because every place in this window that
 * takes more than a few words has the same shape of problem and the same two
 * traps. A plan's prose is two lines for one phase and eight for another; an
 * approval note is "yes but skip the tests" one time and a paragraph of
 * reasoning the next. A fixed box clips the long one — and a single-line
 * `<input>` clips it *invisibly*, scrolling sideways so the writer cannot read
 * back what they are about to send.
 *
 * The two traps:
 *
 *  - **Measuring costs a reflow of the caret.** Reading `scrollHeight` means
 *    clearing the height to `auto` first, which collapses the field to one row
 *    for an instant. Invisible on its own, but the IME candidate window is
 *    positioned from the caret rect, so during a composition that is two jumps
 *    per keystroke — the flicker `Composer.tsx` documents at length. While a
 *    composition is open the height is therefore only ever raised, never
 *    remeasured from scratch.
 *  - **`overflow: auto` is not the same as "scroll when it must".** A field
 *    sized to exactly its content still draws a track down its side in this
 *    webview, so overflow is switched with the height instead.
 */
export function GrowingText({
  value,
  onChange,
  className,
  placeholder,
  rows = 1,
  maxHeight = 240,
  ...rest
}: {
  value: string;
  onChange: (value: string) => void;
  className: string;
  placeholder: string;
  rows?: number;
  /** Where growing stops and scrolling starts, in px. */
  maxHeight?: number;
} & { [attribute: `data-${string}`]: string | undefined } & {
  "aria-label"?: string;
}) {
  const field = useRef<HTMLTextAreaElement>(null);
  const composing = useRef(false);

  const resize = useCallback(
    (grow: boolean) => {
      const node = field.current;
      if (!node) return;
      if (!grow) node.style.height = "auto";
      const content = node.scrollHeight;
      if (grow && content <= node.clientHeight) return;
      // `scrollHeight` is content plus padding; under `border-box` the height
      // we set has to carry the borders as well, or the field ends up exactly
      // its border width too short and scrolls — clipping the first two pixels
      // of the first line, which reads as a font rendering bug rather than as
      // a sizing one. (Measured: a 1px border each side left `scrollTop` at 2.)
      const style = getComputedStyle(node);
      const frame =
        style.boxSizing === "border-box"
          ? parseFloat(style.borderTopWidth) + parseFloat(style.borderBottomWidth)
          : 0;
      const wanted = content + frame;
      node.style.height = `${Math.min(wanted, maxHeight)}px`;
      node.style.overflowY = wanted > maxHeight ? "auto" : "hidden";
    },
    [maxHeight],
  );

  useEffect(() => resize(composing.current), [value, resize]);

  return (
    <textarea
      {...rest}
      ref={field}
      className={className}
      value={value}
      rows={rows}
      placeholder={placeholder}
      spellCheck={false}
      onCompositionStart={() => {
        composing.current = true;
      }}
      onCompositionEnd={() => {
        composing.current = false;
        resize(false);
      }}
      onChange={(event) => onChange(event.target.value)}
    />
  );
}
