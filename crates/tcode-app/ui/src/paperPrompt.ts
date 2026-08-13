import { basename } from "./show";

export type PaperAction = "translate" | "explain" | "ask";

export function paperPrompt(action: PaperAction, path: string, page: number, text: string): string {
  const source = `@paper(${JSON.stringify(basename(path))}, page ${page})`;
  const selected = text.trim();
  switch (action) {
    case "translate":
      return `Translate the selected text from ${source}:\n\n${selected}`;
    case "explain":
      return `Explain the selected text from ${source}:\n\n${selected}`;
    case "ask":
      return `I have a question about the selected text from ${source}:\n\n${selected}\n\n`;
  }
}

export function summarizePaperPrompt(path: string): string {
  const quoted = /\s/.test(path) ? `@"${path}"` : `@${path}`;
  return (
    `${quoted} Summarize this paper: core thesis, method, key findings, limitations, and important terms.`
  );
}
