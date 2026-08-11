import { useEffect, useSyncExternalStore } from "react";
import type { HighlighterCore, ThemeRegistrationRaw } from "shiki/core";

/**
 * Syntax highlighting: a real grammar, still no second palette.
 *
 * The rule this has to satisfy has never changed (`DESIGN.md` § Theme packs — a
 * component may only reference `var(--token)`), and it is why a highlighter was
 * hand-rolled here first: every library ships a *theme*, so its colours arrive
 * as literal values no theme pack can reassign. What that bought was ~150 lines
 * and no dependency; what it cost was a scanner that knew twelve languages and
 * eight token shapes, and was wrong at every edge it did not know about. On a
 * snippet that is a fair trade. On a whole file open in a pane it is not: the
 * errors stop being occasional and become the texture of the page.
 *
 * So the split is drawn one level down. Shiki does the *classification* — real
 * TextMate grammars, the same ones the editor beside this app uses — and the
 * theme it is handed is not a palette at all: `mark()` paints each of our eight
 * kinds a sentinel colour, and `kindOf()` reads that sentinel straight back
 * out. Nothing but an index survives the trip. What reaches the DOM is what
 * always reached it, a `tok-*` class, and the values stay in `base.css`.
 *
 * Two consequences worth knowing before changing anything here:
 *
 *  - **Tokens are data, never markup.** `codeToTokens` returns an array of
 *    `{content, color}`; the HTML-producing side of shiki (`codeToHtml`) is
 *    never called, so rule 10 holds by construction rather than by review.
 *  - **Everything here loads on demand** — shiki itself, and then one chunk per
 *    grammar — so `highlight` answers `null` until what it needs has arrived.
 *    Callers hold `useGrammar` and draw plain text in the meantime: a code
 *    block is readable unhighlighted, and a loading state for one would be
 *    worse than the wait it announces. It is also what keeps the cost off the
 *    window that never opens a file.
 */

export type Token = { text: string; kind: TokenKind };
export type TokenKind =
  | "plain"
  | "comment"
  | "string"
  | "number"
  | "keyword"
  | "type"
  | "call"
  | "punct"
  | "heading"
  | "link"
  | "emphasis";

const KINDS: TokenKind[] = [
  "plain",
  "comment",
  "string",
  "number",
  "keyword",
  "type",
  "call",
  "punct",
  "heading",
  "link",
  "emphasis",
];

/** `plain` is `#000001`, `comment` is `#000002`, and so on: a colour a theme
 *  would never contain, carrying an index rather than a hue. */
const mark = (kind: TokenKind) => `#${String(KINDS.indexOf(kind) + 1).padStart(6, "0")}`;

function kindOf(color: string | undefined): TokenKind {
  const at = color ? Number.parseInt(color.slice(1), 10) - 1 : -1;
  return KINDS[at] ?? "plain";
}

const paints = (kind: TokenKind, ...scope: string[]) => ({
  scope,
  settings: { foreground: mark(kind) },
});

const THEME_NAME = "tcode";

/**
 * Which TextMate scopes are which of our eight kinds.
 *
 * This is the whole mapping, and it is ours rather than a library's on purpose
 * — shiki's own `css-variables` theme would have worked and buckets scopes
 * differently than this app draws them (object properties with numeric
 * constants, tag names with keywords). Order matters: within TextMate theme
 * resolution the more specific scope wins, so the broad catch-alls go first and
 * the narrow rules below them take it back.
 */
const THEME: ThemeRegistrationRaw = {
  name: THEME_NAME,
  type: "dark",
  colors: { "editor.foreground": mark("plain"), "editor.background": "#000000" },
  // `settings` rather than `tokenColors`: both name the same list, and this is
  // the one TextMate itself reads.
  settings: [
    paints("plain", "source", "meta", "variable", "text"),
    paints("punct", "punctuation", "keyword.operator"),
    paints(
      "keyword",
      "keyword",
      "storage",
      "constant.language",
      "variable.language",
      "entity.name.tag",
    ),
    paints(
      "type",
      "entity.name.type",
      "entity.name.class",
      "entity.name.namespace",
      "entity.other.inherited-class",
      "entity.other.attribute-name",
      "support.type",
      "support.class",
    ),
    paints(
      "call",
      "entity.name.function",
      "support.function",
      "meta.function-call",
      "variable.function",
    ),
    paints("number", "constant.numeric", "constant.other"),
    paints(
      "string",
      "string",
      "punctuation.definition.string",
      "constant.character.escape",
      "markup.inline.raw",
      "markup.fenced_code",
    ),
    paints("comment", "comment", "punctuation.definition.comment"),
    paints(
      "heading",
      "markup.heading",
      "punctuation.definition.heading",
    ),
    paints(
      "link",
      "markup.underline.link",
      "string.other.link.title",
      "string.other.link.description",
    ),
    paints(
      "emphasis",
      "markup.bold",
      "markup.italic",
      "punctuation.definition.bold",
      "punctuation.definition.italic",
      "markup.strikethrough",
    ),
  ],
};

/**
 * Language tags this app answers to, and the grammar each one wants.
 *
 * Aliases are spelled out rather than guessed because both entrances here are
 * user-facing strings that will not be corrected: a fence's language tag, which
 * the model wrote, and a file's extension, which somebody else chose. An entry
 * costs nothing until something asks for it — the import is a chunk of its own.
 */
const LANGS: Record<string, () => Promise<unknown>> = {
  rust: () => import("@shikijs/langs/rust"),
  ts: () => import("@shikijs/langs/tsx"),
  tsx: () => import("@shikijs/langs/tsx"),
  js: () => import("@shikijs/langs/tsx"),
  jsx: () => import("@shikijs/langs/tsx"),
  py: () => import("@shikijs/langs/python"),
  sh: () => import("@shikijs/langs/bash"),
  json: () => import("@shikijs/langs/json"),
  jsonc: () => import("@shikijs/langs/jsonc"),
  toml: () => import("@shikijs/langs/toml"),
  ini: () => import("@shikijs/langs/ini"),
  yaml: () => import("@shikijs/langs/yaml"),
  go: () => import("@shikijs/langs/go"),
  sql: () => import("@shikijs/langs/sql"),
  css: () => import("@shikijs/langs/css"),
  scss: () => import("@shikijs/langs/scss"),
  html: () => import("@shikijs/langs/html"),
  xml: () => import("@shikijs/langs/xml"),
  md: () => import("@shikijs/langs/markdown"),
  c: () => import("@shikijs/langs/c"),
  cpp: () => import("@shikijs/langs/cpp"),
  java: () => import("@shikijs/langs/java"),
  kotlin: () => import("@shikijs/langs/kotlin"),
  swift: () => import("@shikijs/langs/swift"),
  ruby: () => import("@shikijs/langs/ruby"),
  php: () => import("@shikijs/langs/php"),
  lua: () => import("@shikijs/langs/lua"),
  zig: () => import("@shikijs/langs/zig"),
  dockerfile: () => import("@shikijs/langs/docker"),
  make: () => import("@shikijs/langs/make"),
  diff: () => import("@shikijs/langs/diff"),
};

const ALIASES: Record<string, keyof typeof LANGS> = {
  rs: "rust",
  typescript: "ts",
  javascript: "js",
  mjs: "js",
  cjs: "js",
  mts: "ts",
  cts: "ts",
  python: "py",
  bash: "sh",
  zsh: "sh",
  shell: "sh",
  console: "sh",
  fish: "sh",
  yml: "yaml",
  golang: "go",
  markdown: "md",
  mdx: "md",
  svg: "xml",
  htm: "html",
  h: "c",
  hpp: "cpp",
  cc: "cpp",
  cxx: "cpp",
  rb: "ruby",
  kt: "kotlin",
  patch: "diff",
  makefile: "make",
};

/** The grammar a tag names, or null when nothing here draws it. */
export function languageId(language: string): string | null {
  const tag = language.toLowerCase();
  const resolved = ALIASES[tag] ?? tag;
  return resolved in LANGS ? resolved : null;
}

export function isHighlightable(language: string): boolean {
  return languageId(language) !== null;
}

// ------------------------------------------------------- the loaded grammars
//
// One highlighter for the window, because a grammar is a large, immutable thing
// and every pane wants the same handful. The store below is what turns "it
// arrived" into a re-render: components read `ready` through
// `useSyncExternalStore`, so a grammar that lands while three panes are open
// repaints all three and nothing polls.

let core: HighlighterCore | null = null;
let starting: Promise<HighlighterCore> | null = null;
const ready = new Set<string>();
const loading = new Map<string, Promise<void>>();
const listeners = new Set<() => void>();

function announce() {
  for (const listener of listeners) listener();
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

function engine(): Promise<HighlighterCore> {
  if (core) return Promise.resolve(core);
  // Imported here rather than at the top, so shiki itself is a chunk like its
  // grammars are: a window that opens on a conversation with no code in it
  // never fetches any of this.
  starting ??= Promise.all([import("shiki/core"), import("shiki/engine/javascript")])
    .then(([{ createHighlighterCore }, { createJavaScriptRegexEngine }]) =>
      createHighlighterCore({
        themes: [THEME],
        langs: [],
        // The JavaScript regex engine, not oniguruma's wasm. In a packaged
        // webview the wasm build is a second asset to fetch and a CSP allowance
        // to grant (`wasm-unsafe-eval`), and it buys nothing here: `forgiving`
        // covers the few grammar patterns the JS engine cannot compile by
        // leaving them unmatched.
        engine: createJavaScriptRegexEngine({ forgiving: true }),
      }),
    )
    .then((made) => {
      core = made;
      return made;
    });
  return starting;
}

function load(id: string): Promise<void> {
  const already = loading.get(id);
  if (already) return already;
  const request = engine()
    .then((highlighter) => highlighter.loadLanguage(LANGS[id]() as never))
    .then(() => {
      ready.add(id);
      announce();
    })
    // A grammar that will not load leaves its language unhighlighted forever,
    // which is exactly what an unknown language already does. It is not worth a
    // failure state on screen.
    .catch(() => {});
  loading.set(id, request);
  return request;
}

/** Loads the grammar a tag needs, resolving when `highlight` can answer for it.
 *  Components want `useGrammar` instead; this is for the callers that are not
 *  components — a test, or anything that wants a grammar warm before the pane
 *  that needs it exists. */
export function loadGrammar(language: string): Promise<void> {
  const id = languageId(language);
  return id === null ? Promise.resolve() : load(id);
}

/** Ensures the grammar for `language` is on its way, and re-renders when it
 *  lands. Returns whether `highlight` can answer yet. */
export function useGrammar(language: string): boolean {
  const id = languageId(language);
  const loaded = useSyncExternalStore(
    subscribe,
    () => id !== null && ready.has(id),
    () => false,
  );
  useEffect(() => {
    if (id !== null && !ready.has(id)) void load(id);
  }, [id]);
  return loaded;
}

/**
 * Snippets already tokenised, keyed by grammar and source.
 *
 * Running a TextMate grammar is the most expensive thing this app does inside a
 * render, and it was being done again on every render of anything that held
 * code: a diff calls this once per line (`components/Diff.tsx`), so a 200-line
 * change on screen cost ~20ms of tokenising per keystroke elsewhere in the
 * window. Every caller wanted a `useMemo` it did not have; one cache here fixes
 * all of them, including the ones added later.
 *
 * Only successes are stored. A miss before the grammar arrives means "ask
 * again once it has", and caching that null would leave the snippet plain
 * forever.
 */
const TOKENS = new Map<string, Token[]>();
const MAX_TOKENS = 4000;

/**
 * The tokens for a snippet, or null when there are none to draw yet — an
 * unknown language, or a grammar still arriving.
 *
 * Line breaks are tokens like any other text, so the result stays a flat run
 * that a `<code>` can hold directly. Callers that want lines can split on them;
 * nothing here does.
 */
export function highlight(source: string, language: string): Token[] | null {
  const id = languageId(language);
  if (id === null || !core || !ready.has(id)) return null;

  // A NUL cannot appear in either half, so the pair cannot be forged by a
  // snippet that happens to contain the separator.
  const key = `${id}\u0000${source}`;
  const found = TOKENS.get(key);
  if (found) {
    TOKENS.delete(key);
    TOKENS.set(key, found);
    return found;
  }

  // A grammar is a large piece of somebody else's software running inside a
  // render, and this app has no way to survive a throw there beyond `Boundary`
  // taking the whole window. Colouring is the most decorative thing on screen;
  // it does not get to be the thing that ends the session. Unhighlighted text
  // is the same text.
  let tokens;
  try {
    ({ tokens } = core.codeToTokens(source, { lang: id, theme: THEME_NAME }));
  } catch (failure) {
    console.warn(`could not highlight ${id}:`, failure);
    return null;
  }

  const out: Token[] = [];
  tokens.forEach((line, at) => {
    if (at > 0) out.push({ text: "\n", kind: "plain" });
    for (const token of line) out.push({ text: token.content, kind: kindOf(token.color) });
  });

  // Insertion order is recency: a hit is re-seated above, so what stays is
  // what is still being looked at.
  TOKENS.set(key, out);
  if (TOKENS.size > MAX_TOKENS) {
    const oldest = TOKENS.keys().next();
    if (!oldest.done) TOKENS.delete(oldest.value);
  }
  return out;
}
