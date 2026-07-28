import { useState } from "react";

import { highlight, isHighlightable } from "../syntax";
import { CheckIcon, CopyIcon } from "./Icons";

/**
 * A fenced code block.
 *
 * Tokens become spans with semantic classes; the theme decides how they read
 * (see `syntax.ts` for why the highlighter is local rather than a library).
 * Nothing here is ever markup — the token text goes in as a child, so a snippet
 * that happens to contain `<script>` is a string that says `<script>`.
 */
export function Code({ source, language }: { source: string; language: string }) {
  const body = source.replace(/\n+$/, "");
  const tokens = isHighlightable(language) ? highlight(body, language) : null;

  return (
    <div className="code-block">
      <div className="code-bar">
        {language && <span className="code-lang">{language}</span>}
        <Copy text={body} />
      </div>
      <pre className="code-body">
        <code>
          {tokens
            ? tokens.map((token, index) =>
                token.kind === "plain" ? (
                  token.text
                ) : (
                  <span className={`tok-${token.kind}`} key={index}>
                    {token.text}
                  </span>
                ),
              )
            : body}
        </code>
      </pre>
    </div>
  );
}

/** Copying a command out of a transcript is the single most common thing done
 *  with one, and selecting it by hand in a scrolling view is fiddly. */
export function Copy({ text }: { text: string }) {
  const [done, setDone] = useState(false);

  return (
    <button
      className={`code-copy${done ? " is-done" : ""}`}
      onClick={() => {
        navigator.clipboard
          .writeText(text)
          .then(() => {
            setDone(true);
            setTimeout(() => setDone(false), 1200);
          })
          .catch(() => {});
      }}
      aria-label={done ? "Copied" : "Copy"}
      title={done ? "Copied" : "Copy"}
    >
      {done ? <CheckIcon size={13} /> : <CopyIcon size={13} />}
    </button>
  );
}
