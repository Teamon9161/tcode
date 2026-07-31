import { useMemo, useState } from "react";

import { highlight, useGrammar } from "../syntax";
import { CheckIcon, CopyIcon } from "./Icons";

/**
 * A fenced code block.
 *
 * Tokens become spans with semantic classes; the theme decides how they read
 * (see `syntax.ts` for why the grammar comes from a library and the colours do
 * not). Nothing here is ever markup — the token text goes in as a child, so a
 * snippet that happens to contain `<script>` is a string that says `<script>`.
 *
 * Until the grammar for this language has loaded, `highlight` answers null and
 * the body draws as plain text. That is the whole loading state on purpose: the
 * text is already correct and complete, and only its colouring is late.
 */
export function Code({ source, language }: { source: string; language: string }) {
  const body = source.replace(/\n+$/, "");
  const loaded = useGrammar(language);
  const tokens = useMemo(
    () => (loaded ? highlight(body, language) : null),
    [body, language, loaded],
  );

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
