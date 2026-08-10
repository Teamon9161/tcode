import { LanguageDescription, type LanguageSupport } from "@codemirror/language";
import { languages } from "@codemirror/language-data";

/** Match the complete filename before its extension, then lazily load only the
 * parser that won. Unknown names and failed dynamic imports remain plain text. */
export function loadEditorLanguage(path: string): Promise<LanguageSupport | null> {
  const description = LanguageDescription.matchFilename(languages, basename(path));
  return description ? description.load().catch(() => null) : Promise.resolve(null);
}

function basename(path: string): string {
  const cut = Math.max(path.lastIndexOf("/"), path.lastIndexOf("\\"));
  return cut === -1 ? path : path.slice(cut + 1);
}
