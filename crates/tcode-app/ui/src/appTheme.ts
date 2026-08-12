export const APP_THEMES = [
  { id: "system", label: "System", detail: "Follow the operating system preference." },
  { id: "light", label: "Light", detail: "Porcelain — white paper with graphite marks." },
  { id: "dark", label: "Dark", detail: "Ink — dark ground with light chalk marks." },
] as const;

export type AppTheme = (typeof APP_THEMES)[number]["id"];

const KEY = "tcode.app-theme";
const FALLBACK: AppTheme = "system";

function isAppTheme(value: unknown): value is AppTheme {
  return APP_THEMES.some((t) => t.id === value);
}

export function loadAppTheme(): AppTheme {
  try {
    const stored = window.localStorage.getItem(KEY);
    return isAppTheme(stored) ? stored : FALLBACK;
  } catch {
    return FALLBACK;
  }
}

function resolveEffective(preference: AppTheme): "light" | "dark" {
  if (preference !== "system") return preference;
  return window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
}

function apply(effective: "light" | "dark") {
  if (effective === "dark") {
    document.documentElement.dataset.theme = "dark";
  } else {
    delete document.documentElement.dataset.theme;
  }
  window.dispatchEvent(new CustomEvent("tcode:theme-changed"));
}

export function setAppTheme(theme: AppTheme): void {
  apply(resolveEffective(theme));
  try {
    window.localStorage.setItem(KEY, theme);
  } catch {}
}

export function initAppTheme(): void {
  apply(resolveEffective(loadAppTheme()));

  window
    .matchMedia("(prefers-color-scheme: dark)")
    .addEventListener("change", () => {
      if (loadAppTheme() === "system") apply(resolveEffective("system"));
    });
}
