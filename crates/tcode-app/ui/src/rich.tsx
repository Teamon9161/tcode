import { Fragment, type ReactNode } from "react";
import { Marked, type Token, type Tokens } from "marked";

import { renderFence } from "./fences";
// Aliased: a component called `Math` would shadow the global inside this module.
import { Math as TeX } from "./math";

/**
 * Model output, as a document.
 *
 * Two rules define this file, and neither is negotiable.
 *
 * **Nothing here ever becomes markup.** The lexer produces tokens; this module
 * turns the token types it recognises into React elements and turns everything
 * else into text. There is no `innerHTML` on this path and no sanitiser to
 * trust, because a whitelist that can only *construct* known elements has no
 * failure mode where unexpected input becomes a node. Model output is data: it
 * routinely carries file contents, fetched pages and MCP results, and in this
 * window a script that runs reaches `window.__TAURI__`.
 *
 * **Unknown is not passed through.** A raw HTML token, an unrecognised token
 * type, a link with a scheme that is not on the list — each renders as its own
 * literal text. Falling back to "render it anyway" is the one change that would
 * quietly undo the rule above.
 *
 * Rich blocks (charts, diagrams, artifacts, math) are not exceptions to this;
 * they are delegated to `fences.tsx`, which either builds nodes or hands the
 * source to an opaque-origin frame that cannot reach this realm.
 */

const marked = new Marked({ gfm: true, breaks: true });

/** `$$…$$` on its own, and `$…$` inline.
 *
 *  Inline math is a heuristic and is guarded accordingly: a run of digits is
 *  money, not algebra, and `costs $5 and $10 total` must not typeset. The
 *  unambiguous forms — a ```math fence and `$$…$$` — carry no such guess. */
marked.use({
  extensions: [
    {
      name: "blockMath",
      level: "block",
      start: (src: string) => src.indexOf("$$"),
      tokenizer(src: string) {
        const match = /^\$\$([\s\S]+?)\$\$(?:\n+|$)/.exec(src);
        if (match) return { type: "blockMath", raw: match[0], text: match[1].trim() };
        return undefined;
      },
    },
    {
      name: "inlineMath",
      level: "inline",
      start: (src: string) => src.indexOf("$"),
      tokenizer(src: string) {
        const match = /^\$(?!\s)([^$\n]+?)(?<!\s)\$(?![\d$])/.exec(src);
        if (!match) return undefined;
        const body = match[1];
        if (/^[\d,.\s]+$/.test(body)) return undefined; // currency, not math
        return { type: "inlineMath", raw: match[0], text: body };
      },
    },
  ],
});

/**
 * Documents already built, by their source text.
 *
 * Every assistant message in a conversation is lexed and rebuilt on every
 * render of the transcript, and a transcript re-renders for reasons that have
 * nothing to do with it — a stream delta three messages down, a pane opening
 * beside it. At a hundred turns that was measured at ~12ms of pure lexing per
 * render, on top of the reconciliation it feeds.
 *
 * Sound because the output is a function of the input and React elements are
 * immutable: handing the same tree back is what React does with an unchanged
 * subtree anyway. The one moving part is the streaming message at the end,
 * whose text differs every delta — it misses, builds, and evicts a step later,
 * which is the behaviour a cache should have for it.
 *
 * Capped, and the cap is on entries rather than characters: the values are
 * element trees whose size is not knowable from here, and the eviction that
 * matters is of the long tail nobody is looking at.
 */
const DOCUMENTS = new Map<string, ReactNode[]>();
const MAX_DOCUMENTS = 400;

export function rich(text: string): ReactNode[] {
  const found = DOCUMENTS.get(text);
  if (found) {
    // Insertion order is recency: re-seating a hit keeps a message that is
    // still on screen out of the eviction path.
    DOCUMENTS.delete(text);
    DOCUMENTS.set(text, found);
    return found;
  }
  const built = blocks(marked.lexer(text), "r");
  DOCUMENTS.set(text, built);
  if (DOCUMENTS.size > MAX_DOCUMENTS) {
    const oldest = DOCUMENTS.keys().next();
    if (!oldest.done) DOCUMENTS.delete(oldest.value);
  }
  return built;
}

function blocks(tokens: Token[], seed: string): ReactNode[] {
  const out: ReactNode[] = [];
  tokens.forEach((token, index) => {
    const node = block(token, `${seed}-${index}`);
    if (node !== null) out.push(<Fragment key={`${seed}-${index}`}>{node}</Fragment>);
  });
  return out;
}

function block(token: Token, key: string): ReactNode {
  switch (token.type) {
    case "space":
    case "def":
      return null;

    case "paragraph":
      return <p className="para">{inlines((token as Tokens.Paragraph).tokens, key)}</p>;

    case "text": {
      const item = token as Tokens.Text;
      return <p className="para">{item.tokens ? inlines(item.tokens, key) : item.text}</p>;
    }

    case "heading": {
      const item = token as Tokens.Heading;
      // Transcript headings are structure inside someone else's page, so they
      // stay in the body's own scale rather than competing with the app's.
      const Tag = (["h3", "h4", "h5", "h6", "h6", "h6"] as const)[
        Math.min(Math.max(item.depth, 1), 6) - 1
      ];
      return <Tag className={`prose-h prose-h${item.depth}`}>{inlines(item.tokens, key)}</Tag>;
    }

    case "code": {
      const item = token as Tokens.Code;
      return renderFence(item.text, item.lang ?? "");
    }

    case "blockquote":
      return (
        <blockquote className="prose-quote">
          {blocks((token as Tokens.Blockquote).tokens, key)}
        </blockquote>
      );

    case "list": {
      const item = token as Tokens.List;
      const Tag = item.ordered ? "ol" : "ul";
      return (
        <Tag
          className="prose-list"
          start={item.ordered && item.start !== 1 ? Number(item.start) : undefined}
        >
          {item.items.map((entry, index) => (
            <li className={entry.task ? "is-task" : undefined} key={index}>
              {entry.task && (
                <input
                  type="checkbox"
                  className="prose-check"
                  checked={Boolean(entry.checked)}
                  readOnly
                  aria-label={entry.checked ? "done" : "not done"}
                />
              )}
              {blocks(entry.tokens, `${key}-${index}`)}
            </li>
          ))}
        </Tag>
      );
    }

    case "table": {
      const item = token as Tokens.Table;
      return (
        <div className="prose-table-scroll">
          <table className="prose-table">
            <thead>
              <tr>
                {item.header.map((cell, index) => (
                  <th key={index} style={alignOf(item.align[index])}>
                    {inlines(cell.tokens, `${key}-h${index}`)}
                  </th>
                ))}
              </tr>
            </thead>
            <tbody>
              {item.rows.map((row, rowIndex) => (
                <tr key={rowIndex}>
                  {row.map((cell, index) => (
                    <td key={index} style={alignOf(item.align[index])}>
                      {inlines(cell.tokens, `${key}-${rowIndex}-${index}`)}
                    </td>
                  ))}
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      );
    }

    case "hr":
      return <hr className="prose-rule" />;

    case "blockMath":
      return <TeX tex={(token as unknown as { text: string }).text} display />;

    // `html` lands here, and so does anything marked learns to lex after this
    // file was written. Both render as what they literally are.
    default:
      return <p className="para">{(token as { raw?: string }).raw ?? ""}</p>;
  }
}

function inlines(tokens: Token[] | undefined, seed: string): ReactNode[] {
  if (!tokens) return [];
  return tokens.map((token, index) => (
    <Fragment key={`${seed}-${index}`}>{inline(token, `${seed}-${index}`)}</Fragment>
  ));
}

function inline(token: Token, key: string): ReactNode {
  switch (token.type) {
    case "text":
    case "escape": {
      const item = token as Tokens.Text;
      return item.tokens ? inlines(item.tokens, key) : item.text;
    }

    case "strong":
      return <strong>{inlines((token as Tokens.Strong).tokens, key)}</strong>;

    case "em":
      return <em>{inlines((token as Tokens.Em).tokens, key)}</em>;

    case "del":
      return <del>{inlines((token as Tokens.Del).tokens, key)}</del>;

    case "codespan":
      return <code className="code-inline">{(token as Tokens.Codespan).text}</code>;

    case "br":
      return <br />;

    case "link": {
      const item = token as Tokens.Link;
      const href = safeHref(item.href);
      // A rejected scheme keeps its text and loses its affordance. Rendering it
      // as a dead link would hide that something was refused.
      if (!href) return <span className="link-refused">{inlines(item.tokens, key)}</span>;
      return (
        <a className="prose-link" href={href} target="_blank" rel="noreferrer noopener">
          {inlines(item.tokens, key)}
        </a>
      );
    }

    case "image": {
      const item = token as Tokens.Image;
      // The webview has no entitlement to fetch remote pictures and no asset
      // protocol yet, so an embedded one would be a broken icon every time. The
      // reference is shown as a reference until there is a real file to load.
      return (
        <span className="prose-image" title={item.href}>
          {item.text || "image"}
        </span>
      );
    }

    case "inlineMath":
      return <TeX tex={(token as unknown as { text: string }).text} display={false} />;

    default:
      return (token as { raw?: string }).raw ?? "";
  }
}

/** Schemes a transcript link may carry. Everything else — `javascript:`,
 *  `data:`, `vbscript:`, anything invented later — is refused by omission. */
const SCHEMES = ["http:", "https:", "mailto:"];

function safeHref(href: string): string | null {
  const trimmed = href.trim();
  // A relative path or fragment carries no scheme and cannot execute.
  if (/^[.#/?]/.test(trimmed)) return trimmed;
  const colon = trimmed.indexOf(":");
  if (colon === -1) return trimmed;
  const scheme = trimmed.slice(0, colon + 1).toLowerCase();
  return SCHEMES.includes(scheme) ? trimmed : null;
}

function alignOf(align: "center" | "left" | "right" | null) {
  return align ? { textAlign: align } : undefined;
}
