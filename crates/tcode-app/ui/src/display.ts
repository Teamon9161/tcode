import { createContext, useContext } from "react";

/**
 * What the window shows, as opposed to what it holds.
 *
 * These are the reader's preferences about the *view*, not the agent's
 * behaviour and not any conversation's state — which is why they live in the
 * webview rather than in `[tcode_state]`. Nothing here changes what a turn does,
 * what is sent, or what is recorded; a wrong value costs a click, not a run. The
 * config file stays the place for things that change what the agent does.
 *
 * One flag today, and the shape is the point: the panel that edits this is where
 * the next display switch goes, instead of a second control somewhere else.
 */
export type Display = {
  /**
   * Whether reasoning appears in the transcript at all.
   *
   * Off by default. Folded behind a disclosure it was the worst of both: a row
   * in the trace column that looked exactly like a step the agent took, holding
   * text nobody had asked for. Shown, it is prose and reads as prose; hidden, the
   * column is only things that happened.
   */
  thinking: boolean;
};

export const DISPLAY_DEFAULT: Display = { thinking: false };

const KEY = "tcode.display";

/**
 * Read from storage, and tolerant on purpose: a stored value is data (an older
 * build's shape, a hand-edited entry), so every field is checked and anything
 * unrecognized falls back to the default rather than reaching a component.
 */
export function loadDisplay(): Display {
  try {
    const raw = window.localStorage.getItem(KEY);
    if (!raw) return DISPLAY_DEFAULT;
    const stored = JSON.parse(raw) as unknown;
    if (typeof stored !== "object" || stored === null) return DISPLAY_DEFAULT;
    const record = stored as Record<string, unknown>;
    return {
      thinking:
        typeof record.thinking === "boolean" ? record.thinking : DISPLAY_DEFAULT.thinking,
    };
  } catch {
    // A webview with storage disabled still has to open.
    return DISPLAY_DEFAULT;
  }
}

export function saveDisplay(display: Display): void {
  try {
    window.localStorage.setItem(KEY, JSON.stringify(display));
  } catch {
    // Losing the preference on the next launch beats failing the click.
  }
}

/** Read by the transcript at every depth, so a sub-agent's reasoning obeys the
 *  same switch as the conversation holding it — one setting, not one per
 *  surface. */
export const DisplayContext = createContext<Display>(DISPLAY_DEFAULT);

export function useDisplay(): Display {
  return useContext(DisplayContext);
}
