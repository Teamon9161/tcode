/**
 * What the caret is part-way through typing, and what finishing it would mean.
 *
 * Two things in a prompt are not prose: a leading `/` names a command, and an
 * `@` names a file in this folder. Both are typed from memory today — the
 * terminal completes them and this window did not, so the desktop app was the
 * one place where you had to already know the name of the file you wanted to
 * point at.
 *
 * All of it is pure and lives apart from the composer for the usual reason:
 * "where does this token start" is arithmetic with edge cases (an `@` inside a
 * word is an email address, not a path; a `/` after the first word is a path
 * separator, not a command), and arithmetic with edge cases wants tests rather
 * than a running app and an IME.
 */

export type TokenKind = "command" | "mention";

export type Token = {
  kind: TokenKind;
  /** Index of the `/` or `@` that opened it. */
  start: number;
  /** What has been typed since, up to the caret — never past it, so completing
   *  can only ever add text. */
  query: string;
};

/**
 * The token the caret sits in, if it is in one.
 *
 * A command is only ever the first thing in the message, because that is the
 * only place the window will read one (`App.tsx` sends a draft starting with
 * `/` to `slash_command` and everything else to the model). So `cd /usr/bin`
 * offers nothing, and neither does `/compact the parts about caching` once the
 * caret has passed the command's own word.
 *
 * A mention opens at an `@` that starts the text or follows whitespace, which
 * is what keeps `me@example.com` from becoming a file path. Deliberately not
 * "anything that is not a letter": every other rule admits some address, and
 * silently turning one into a path reference would put a stranger's mail
 * domain into the request as a file the model then goes looking for.
 */
export function tokenAt(text: string, caret: number): Token | null {
  const before = text.slice(0, caret);
  if (text.startsWith("/") && !/\s/.test(before) && before.length > 0) {
    return { kind: "command", start: 0, query: before.slice(1) };
  }
  const at = before.lastIndexOf("@");
  if (at === -1) return null;
  // Whitespace anywhere between the `@` and the caret means the token ended
  // before the caret got here.
  if (/\s/.test(before.slice(at))) return null;
  if (at > 0 && !/\s/.test(text[at - 1])) return null;
  return { kind: "mention", start: at, query: before.slice(at + 1) };
}

/**
 * The draft with this token finished, and where the caret goes.
 *
 * The token is replaced whole — from its opening character to the whitespace
 * that ends it — even when the caret is sitting in the middle of it. Stopping
 * at the caret instead is the version that looks safer and is not: going back
 * to fix `@src/ma|in.rs` and accepting a suggestion left `@src/main.rs in.rs`,
 * two paths where the typist was correcting one. Nothing outside the token is
 * touched either way, which is the property that actually matters.
 */
export function complete(
  text: string,
  token: Token,
  caret: number,
  insert: string,
): { text: string; caret: number } {
  const after = text.slice(caret).search(/\s/);
  const end = after === -1 ? text.length : caret + after;
  // A finished suggestion carries the space that ends it, so the next word can
  // be typed straight away. Mid-sentence there is already one there, and two
  // is a gap somebody has to go back and close.
  const written =
    insert.endsWith(" ") && /\s/.test(text[end] ?? "") ? insert.slice(0, -1) : insert;
  const next = text.slice(0, token.start) + written + text.slice(end);
  return { text: next, caret: token.start + written.length };
}

/** One `@path` written in a draft: where it is, and what it names. */
export type Mention = { start: number; end: number; path: string };

/**
 * Every `@path` in a draft, in order.
 *
 * Same rule as `tokenAt` for what opens one, because a mention that highlights
 * differently from the way it completes would be two answers to one question.
 * The path may or may not exist — that is the caller's to find out, and the
 * whole reason these are located at all.
 */
export function mentions(text: string): Mention[] {
  const found: Mention[] = [];
  for (let at = text.indexOf("@"); at !== -1; at = text.indexOf("@", at + 1)) {
    if (at > 0 && !/\s/.test(text[at - 1])) continue;
    const rest = text.slice(at + 1);
    const stop = rest.search(/\s/);
    const path = stop === -1 ? rest : rest.slice(0, stop);
    if (!path) continue;
    found.push({ start: at, end: at + 1 + path.length, path });
  }
  return found;
}

/** A draft split into what to draw plainly and what to draw as a mention.
 *  Text first, alternating, so the caller can map straight onto nodes. */
export type Segment = { text: string; mention: string | null };

export function segments(text: string): Segment[] {
  const out: Segment[] = [];
  let at = 0;
  for (const found of mentions(text)) {
    if (found.start > at) out.push({ text: text.slice(at, found.start), mention: null });
    out.push({ text: text.slice(found.start, found.end), mention: found.path });
    at = found.end;
  }
  if (at < text.length) out.push({ text: text.slice(at), mention: null });
  return out;
}
