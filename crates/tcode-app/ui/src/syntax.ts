/**
 * Syntax highlighting, hand-rolled and deliberately small.
 *
 * The obvious move is to install Shiki or highlight.js. Both were rejected for
 * the same reason: they carry their own themes, so their colours arrive as
 * literal values that no theme pack can reassign. That breaks the rule the
 * whole visual system rests on (`DESIGN.md` § Theme packs — a component may
 * only reference `var(--token)`), and it would put a second, competing palette
 * inside a surface whose entire premise is that chroma means state.
 *
 * So this emits *semantic classes* and the theme decides what they look like.
 * Porcelain renders them as a printed listing — graphite at four weights with
 * olive for keywords — rather than as a terminal, which is the app's stated
 * anti-reference.
 *
 * It is a scanner, not a parser: it knows comments, strings, numbers, keywords
 * and call sites, and it is wrong at the edges (a `#` inside a shell string, a
 * Rust lifetime that looks like a char literal). That is an acceptable trade
 * for ~150 lines and no dependency, because the job here is making a snippet
 * scannable, not compiling it.
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
  | "punct";

type Language = {
  /** Line comment openers, longest first. */
  line: string[];
  /** `[open, close]` block comment pairs. */
  block: [string, string][];
  /** Quote characters that start a string. */
  quotes: string[];
  keywords: Set<string>;
  /** Words that read as types rather than control flow. */
  types: Set<string>;
};

const words = (list: string) => new Set(list.split(/\s+/).filter(Boolean));

const RUST: Language = {
  line: ["//"],
  block: [["/*", "*/"]],
  quotes: ['"'],
  keywords: words(`as async await break const continue crate dyn else enum extern
    fn for if impl in let loop match mod move mut pub ref return self Self static
    struct super trait type unsafe use where while yield union`),
  types: words(`bool char str String Vec Option Result Some None Ok Err u8 u16 u32
    u64 u128 usize i8 i16 i32 i64 i128 isize f32 f64 Box Arc Rc RefCell HashMap
    BTreeMap Path PathBuf`),
};

const TS: Language = {
  line: ["//"],
  block: [["/*", "*/"]],
  quotes: ['"', "'", "`"],
  keywords: words(`as async await break case catch class const continue debugger
    default delete do else enum export extends finally for from function get if
    implements import in instanceof interface let new of return satisfies set
    static super switch this throw try typeof var void while yield readonly
    keyof infer declare namespace type`),
  types: words(`any boolean never null number object string symbol undefined
    unknown Array Promise Record Partial Readonly Map Set true false`),
};

const PYTHON: Language = {
  line: ["#"],
  block: [],
  quotes: ['"', "'"],
  keywords: words(`and as assert async await break class continue def del elif
    else except finally for from global if import in is lambda nonlocal not or
    pass raise return try while with yield match case`),
  types: words(`bool bytes dict float int list None object set str tuple True
    False self cls`),
};

const SHELL: Language = {
  line: ["#"],
  block: [],
  quotes: ['"', "'"],
  keywords: words(`if then elif else fi for while do done case esac in function
    return export local source alias set unset trap exit`),
  types: new Set<string>(),
};

const JSON_LANG: Language = {
  line: [],
  block: [],
  quotes: ['"'],
  keywords: words("true false null"),
  types: new Set<string>(),
};

const TOML: Language = {
  line: ["#"],
  block: [],
  quotes: ['"', "'"],
  keywords: words("true false"),
  types: new Set<string>(),
};

const GO: Language = {
  line: ["//"],
  block: [["/*", "*/"]],
  quotes: ['"', "`"],
  keywords: words(`break case chan const continue default defer else fallthrough
    for func go goto if import interface map package range return select struct
    switch type var`),
  types: words(`bool byte error float32 float64 int int8 int16 int32 int64 rune
    string uint uint8 uint16 uint32 uint64 nil true false`),
};

const SQL: Language = {
  line: ["--"],
  block: [["/*", "*/"]],
  quotes: ["'", '"'],
  keywords: words(`select from where group by having order limit offset join left
    right inner outer on as insert into values update set delete create table
    drop alter index view with union all distinct case when then else end`),
  types: words("int integer text varchar boolean date timestamp numeric null"),
};

const CSS_LANG: Language = {
  line: [],
  block: [["/*", "*/"]],
  quotes: ['"', "'"],
  keywords: words("important from to and not or only media supports keyframes"),
  types: new Set<string>(),
};

const BY_NAME: Record<string, Language> = {
  rust: RUST,
  rs: RUST,
  ts: TS,
  tsx: TS,
  typescript: TS,
  js: TS,
  jsx: TS,
  javascript: TS,
  json: JSON_LANG,
  jsonc: JSON_LANG,
  py: PYTHON,
  python: PYTHON,
  sh: SHELL,
  bash: SHELL,
  zsh: SHELL,
  shell: SHELL,
  console: SHELL,
  toml: TOML,
  ini: TOML,
  go: GO,
  sql: SQL,
  css: CSS_LANG,
  scss: CSS_LANG,
};

export function isHighlightable(language: string): boolean {
  return language.toLowerCase() in BY_NAME;
}

export function highlight(source: string, language: string): Token[] {
  const spec = BY_NAME[language.toLowerCase()];
  if (!spec) return [{ text: source, kind: "plain" }];

  const tokens: Token[] = [];
  let plain = "";
  let at = 0;

  const flush = () => {
    if (plain) tokens.push({ text: plain, kind: "plain" });
    plain = "";
  };
  const emit = (text: string, kind: TokenKind) => {
    flush();
    tokens.push({ text, kind });
  };

  while (at < source.length) {
    const rest = source.slice(at);

    const line = spec.line.find((opener) => rest.startsWith(opener));
    if (line) {
      const end = source.indexOf("\n", at);
      const stop = end === -1 ? source.length : end;
      emit(source.slice(at, stop), "comment");
      at = stop;
      continue;
    }

    const block = spec.block.find(([open]) => rest.startsWith(open));
    if (block) {
      const close = source.indexOf(block[1], at + block[0].length);
      const stop = close === -1 ? source.length : close + block[1].length;
      emit(source.slice(at, stop), "comment");
      at = stop;
      continue;
    }

    const quote = spec.quotes.find((mark) => rest.startsWith(mark));
    if (quote) {
      const stop = closingQuote(source, at, quote);
      emit(source.slice(at, stop), "string");
      at = stop;
      continue;
    }

    const digits = /^\d[\d_]*(\.\d+)?([eE][+-]?\d+)?[a-zA-Z]*/.exec(rest);
    if (digits && !isWordChar(source[at - 1] ?? "")) {
      emit(digits[0], "number");
      at += digits[0].length;
      continue;
    }

    const word = /^[A-Za-z_$][\w$]*/.exec(rest);
    if (word) {
      const text = word[0];
      const after = rest.slice(text.length);
      const kind: TokenKind = spec.keywords.has(text)
        ? "keyword"
        : spec.types.has(text)
          ? "type"
          : /^\s*\(/.test(after)
            ? "call"
            : /^[A-Z]/.test(text)
              ? "type"
              : "plain";
      if (kind === "plain") plain += text;
      else emit(text, kind);
      at += text.length;
      continue;
    }

    const punct = /^[{}[\]()<>;,.:=+\-*/%!&|^~?@#]+/.exec(rest);
    if (punct) {
      emit(punct[0], "punct");
      at += punct[0].length;
      continue;
    }

    plain += source[at];
    at += 1;
  }

  flush();
  return tokens;
}

/** Walks to the closing quote, honouring backslash escapes. An unterminated
 *  string runs to end of input, which is what a half-streamed snippet needs. */
function closingQuote(source: string, start: number, quote: string): number {
  let at = start + quote.length;
  while (at < source.length) {
    if (source[at] === "\\") {
      at += 2;
      continue;
    }
    if (source.startsWith(quote, at)) return at + quote.length;
    at += 1;
  }
  return source.length;
}

const isWordChar = (character: string) => /[\w$]/.test(character);
