import { Fragment, type ReactNode } from "react";

/**
 * The little of Markdown that earns its place in a transcript.
 *
 * Fenced blocks and inline code, and nothing else. Those two carry almost all
 * of the readability difference in agent output — a diff or a command wrapped
 * in body prose is genuinely hard to read — while headings and emphasis add
 * decoration this surface does not need.
 *
 * Prose is emitted as paragraphs rather than left to `white-space: pre-wrap`.
 * Pre-wrap looks like the cheaper option until a fenced block is involved: the
 * blank lines that separate the fence from the prose around it then render as
 * literal empty lines *on top of* the block's own margins, and every code
 * sample ends up marooned in whitespace.
 *
 * Everything is built as React nodes; no HTML is ever parsed or injected. Model
 * output is data, and the one way to be sure it cannot become markup is to
 * never give it a path to `innerHTML`.
 */
export function rich(text: string): ReactNode[] {
  const nodes: ReactNode[] = [];
  const fence = /```([\w+-]*)[ \t]*\n?([\s\S]*?)(?:```|$)/g;
  let cursor = 0;
  let match: RegExpExecArray | null;
  let key = 0;

  while ((match = fence.exec(text)) !== null) {
    nodes.push(...prose(text.slice(cursor, match.index), key++));
    const [, language, body] = match;
    nodes.push(
      <pre className="code-block" key={`code-${key++}`}>
        {language && <span className="code-lang">{language}</span>}
        <code>{body.replace(/\n+$/, "")}</code>
      </pre>,
    );
    cursor = match.index + match[0].length;
  }
  nodes.push(...prose(text.slice(cursor), key++));
  return nodes;
}

/** A run of prose between fences, as paragraphs. */
function prose(text: string, seed: number): ReactNode[] {
  return text
    .split(/\n{2,}/)
    .map((block) => block.replace(/^\n+|\n+$/g, ""))
    .filter((block) => block.length > 0)
    .map((block, index) => (
      <p className="para" key={`p-${seed}-${index}`}>
        {inline(block, `${seed}-${index}`)}
      </p>
    ));
}

/** Splits a paragraph on backticks, keeping single newlines as line breaks. */
function inline(text: string, seed: string): ReactNode[] {
  return text.split(/`([^`\n]+)`/g).map((part, index) => {
    if (index % 2 === 1) {
      return (
        <code className="code-inline" key={`c-${seed}-${index}`}>
          {part}
        </code>
      );
    }
    const lines = part.split("\n");
    return (
      <Fragment key={`t-${seed}-${index}`}>
        {lines.map((line, at) => (
          <Fragment key={at}>
            {at > 0 && <br />}
            {line}
          </Fragment>
        ))}
      </Fragment>
    );
  });
}
