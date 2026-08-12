/**
 * Syntax-colour preferences are window presentation, like the transcript display
 * switches. They never change an agent turn, so browser storage is the right
 * scope; terminal and app-surface colours remain the selected app theme.
 */
export const CODE_THEMES = [
  {
    id: "porcelain",
    label: "Porcelain",
    detail: "The app's olive-and-graphite code palette.",
  },
  {
    id: "github",
    label: "GitHub",
    detail: "Clear blue, red, purple, and green roles.",
  },
  {
    id: "one",
    label: "One",
    detail: "Atom's balanced violet, blue, and green syntax palette.",
  },
  {
    id: "solarized",
    label: "Solarized",
    detail: "The classic low-glare blue, cyan, green, and magenta palette.",
  },
] as const;

export type CodeTheme = (typeof CODE_THEMES)[number]["id"];

const KEY = "tcode.code-theme";
const FALLBACK: CodeTheme = "porcelain";

const LEGACY: Record<string, CodeTheme> = {
  "github-light": "github",
  "one-light": "one",
  "solarized-light": "solarized",
};

function isCodeTheme(value: unknown): value is CodeTheme {
  return CODE_THEMES.some((theme) => theme.id === value);
}

export function loadCodeTheme(): CodeTheme {
  try {
    const stored = window.localStorage.getItem(KEY);
    if (isCodeTheme(stored)) return stored;
    if (stored && stored in LEGACY) return LEGACY[stored];
    return FALLBACK;
  } catch {
    return FALLBACK;
  }
}

/** Apply before React's first paint, then remember the choice best-effort. */
export function setCodeTheme(theme: CodeTheme): void {
  document.documentElement.dataset.codeTheme = theme;
  try {
    window.localStorage.setItem(KEY, theme);
  } catch {
    // A webview with disabled storage still keeps the choice for this window.
  }
}
