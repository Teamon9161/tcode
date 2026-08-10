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
    id: "github-light",
    label: "GitHub Light",
    detail: "Clear blue, red, purple, and green roles on paper.",
  },
  {
    id: "one-light",
    label: "One Light",
    detail: "Atom's balanced violet, blue, and green syntax palette.",
  },
  {
    id: "solarized-light",
    label: "Solarized Light",
    detail: "The classic low-glare blue, cyan, green, and magenta palette.",
  },
] as const;

export type CodeTheme = (typeof CODE_THEMES)[number]["id"];

const KEY = "tcode.code-theme";
const FALLBACK: CodeTheme = "porcelain";

function isCodeTheme(value: unknown): value is CodeTheme {
  return CODE_THEMES.some((theme) => theme.id === value);
}

export function loadCodeTheme(): CodeTheme {
  try {
    const stored = window.localStorage.getItem(KEY);
    return isCodeTheme(stored) ? stored : FALLBACK;
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
