import { LanguageDescription, type LanguageSupport } from "@codemirror/language";
import { languages } from "@codemirror/language-data";

/** Match the complete filename before its extension, then lazily load only the
 * parser that won. Unknown names and failed dynamic imports remain plain text. */
export function loadEditorLanguage(path: string): Promise<LanguageSupport | null> {
  const name = basename(path);
  const description = LanguageDescription.matchFilename(languages, name);
  if (!description) return Promise.resolve(null);
  if (isMarkdown(description)) return loadMarkdownLanguage();
  return description.load().catch(() => null);
}

function loadMarkdownLanguage(): Promise<LanguageSupport | null> {
  return import("@codemirror/lang-markdown")
    .then((module) => module.markdown({ codeLanguages: languages }))
    .catch(() => null);
}

function isMarkdown(description: LanguageDescription): boolean {
  return description.name === "Markdown";
}

function basename(path: string): string {
  const cut = Math.max(path.lastIndexOf("/"), path.lastIndexOf("\\"));
  return cut === -1 ? path : path.slice(cut + 1);
}
