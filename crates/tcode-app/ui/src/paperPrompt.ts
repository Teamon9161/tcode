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

const MAX_PAGES_FOR_SUMMARY = 30;
const TAIL_PAGES = 5;

export function summarizePaperPrompt(path: string, pageTexts: string[], totalPages: number): string {
  const name = basename(path);
  let body: string;
  if (pageTexts.length <= MAX_PAGES_FOR_SUMMARY + TAIL_PAGES) {
    body = pageTexts.map((t, i) => `--- Page ${i + 1} ---\n${t}`).join("\n\n");
  } else {
    const head = pageTexts.slice(0, MAX_PAGES_FOR_SUMMARY);
    const tail = pageTexts.slice(-TAIL_PAGES);
    body =
      head.map((t, i) => `--- Page ${i + 1} ---\n${t}`).join("\n\n") +
      `\n\n[... pages ${MAX_PAGES_FOR_SUMMARY + 1}–${totalPages - TAIL_PAGES} omitted ...]\n\n` +
      tail.map((t, i) => `--- Page ${totalPages - TAIL_PAGES + i + 1} ---\n${t}`).join("\n\n");
  }

  return (
    `Summarize this academic paper @paper(${JSON.stringify(name)}):\n\n` +
    `<paper-text>\n${body}\n</paper-text>\n\n` +
    `Please provide a structured summary:\n` +
    `1. **Core thesis** (one sentence)\n` +
    `2. **Motivation & problem**\n` +
    `3. **Method** (3-5 key points)\n` +
    `4. **Main findings & conclusions**\n` +
    `5. **Limitations & future work**\n` +
    `6. **Key terms** (if domain-specific)`
  );
}

export function summarizePagePrompt(path: string, page: number, pageText: string): string {
  const name = basename(path);
  return (
    `Summarize page ${page} of @paper(${JSON.stringify(name)}):\n\n` +
    `<page-text>\n${pageText.trim()}\n</page-text>\n\n` +
    `Explain what this page covers: key points, definitions, formulas if any, and how it connects to the paper's overall argument.`
  );
}
