/**
 * Reading what the user selected, wherever the selection lives.
 *
 * The plan editor has no read mode: a phase's title and its prose are editable
 * text, which in this app means a `<textarea>` (never `contenteditable` — see
 * `boundary.test.ts` and rule 10; a plain textarea cannot turn model output into
 * markup). That splits "what is selected" in two, because a textarea's selection
 * is its own `selectionStart`/`selectionEnd` and does not appear in
 * `window.getSelection()` at all.
 *
 * So this module answers one question — *what text is selected* — from either
 * source, as a pure function of values a caller can hand it. The bubble's
 * position comes from the pointer instead of from range geometry: measuring a
 * caret rect inside a textarea means mirroring the field into a hidden div, and
 * the place the user's hand already is happens to be the better anchor anyway.
 */

/** The parts of a text field this needs; keeps the function testable without a
 *  DOM, and without pretending to be a `HTMLTextAreaElement`. */
export type TextFieldSelection = {
  value: string;
  selectionStart: number | null;
  selectionEnd: number | null;
};

/**
 * A quote long enough to identify a passage and no longer.
 *
 * The quote is sent to the model, so it is *trimmed* rather than reflowed —
 * altering the words would make the anchor a paraphrase. Very long selections
 * are cut at a paragraph-ish length with an ellipsis: the point of the quote is
 * to say which passage, and a comment carrying a whole plan back has stopped
 * being an anchor.
 */
const MAX_QUOTE = 600;

export function normalizeQuote(text: string): string {
  const quote = text.trim();
  if (quote.length <= MAX_QUOTE) return quote;
  return `${quote.slice(0, MAX_QUOTE).trimEnd()}…`;
}

/**
 * The selected text: a field's own selection when the event came from one, else
 * the document's.
 *
 * A collapsed selection (a click, a caret move) is not a selection — it returns
 * empty, which is what keeps a bare click from offering to comment on nothing.
 */
export function selectedText(
  field: TextFieldSelection | null,
  documentSelection: string,
): string {
  if (field) {
    const { value, selectionStart, selectionEnd } = field;
    if (selectionStart !== null && selectionEnd !== null && selectionEnd > selectionStart) {
      return normalizeQuote(value.slice(selectionStart, selectionEnd));
    }
    // The pointer went down in a text field with nothing selected in it. The
    // document selection belongs to something else, so there is no quote here.
    return "";
  }
  return normalizeQuote(documentSelection);
}

/** A text field, if that is what this event came from. */
export function fieldOf(target: EventTarget | null): TextFieldSelection | null {
  if (target instanceof HTMLTextAreaElement || target instanceof HTMLInputElement) {
    return {
      value: target.value,
      selectionStart: target.selectionStart,
      selectionEnd: target.selectionEnd,
    };
  }
  return null;
}

/** Where the comment bubble goes, in viewport coordinates.
 *
 *  Viewport, not page: the bubble is `position: fixed`, because a pane's body is
 *  a scroll container and an absolutely positioned bubble inside one gets
 *  clipped at its edge — exactly where a selection near the bottom of a long
 *  plan would put it. */
export type BubbleAt = { x: number; y: number };

/** The quote and the place to offer commenting on it, from one pointer event.
 *  `null` when nothing is selected, which is most clicks. */
export function commentTarget(event: {
  target: EventTarget | null;
  clientX: number;
  clientY: number;
}): { quote: string; at: BubbleAt } | null {
  const quote = selectedText(
    fieldOf(event.target),
    typeof window === "undefined" ? "" : (window.getSelection()?.toString() ?? ""),
  );
  if (!quote) return null;
  return { quote, at: { x: event.clientX, y: event.clientY } };
}
