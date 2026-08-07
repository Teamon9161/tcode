import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@ipc";

import { complete, mentions, tokenAt, type Token, type TokenKind } from "./completion";
import type { Suggestion } from "./Completions";
import type { WorkspaceEntry } from "./workspaceTree";

/**
 * Where the composer's two menus get their contents.
 *
 * Kept out of `Composer.tsx` because it is all asynchronous bookkeeping — a
 * debounce, a stale-response guard, a cache — and the composer already carries
 * the one thing in this window that must not be disturbed while it runs (an
 * IME composition). Nothing here reaches the field: the hooks answer questions
 * about a string and the composer decides what to do with the answer.
 *
 * Both requests are handled rather than escalated when they fail (AGENTS.md
 * rule 7 asks for no *silent* rejection, which this is not). A folder that was
 * renamed under a session should cost a menu, not the window: the draft is
 * still typeable, and the fatal screen exists for the case where nothing is.
 */

/** How long the typing settles before a directory is read. Short enough to feel
 *  immediate, long enough that a fast typist reads one directory and not six. */
const SETTLE = 80;

/** The commands this window can run. One request per process: the registry is
 *  compiled in, so the answer cannot change while it runs. */
let commandsOnce: Promise<Suggestion[]> | null = null;

function commands(): Promise<Suggestion[]> {
  commandsOnce ??= invoke<{ name: string; help: string }[]>("slash_commands")
    .then((list) =>
      list.map((command) => ({
        insert: `${command.name} `,
        label: command.name,
        hint: command.help,
      })),
    )
    .catch(() => []);
  return commandsOnce;
}

/**
 * The menu for whatever the caret is part-way through, and the keys that drive
 * it.
 *
 * `caret` being `null` means the field does not have it — no caret, no token,
 * no menu. `enabled` is the composition guard: a preedit is not a draft, so
 * nothing is queried and nothing pops up over the candidate window while one
 * is open.
 */
export function useCompletions({
  session,
  text,
  caret,
  enabled,
}: {
  session: string;
  text: string;
  caret: number | null;
  enabled: boolean;
}) {
  const token = useMemo(
    () => (enabled && caret !== null ? tokenAt(text, caret) : null),
    [enabled, text, caret],
  );
  // The answer, and which kind of token it answers. Holding the kind is what
  // stops one menu's contents from being shown under the other's question: a
  // draft that goes from `/co` to `... @s` changes what is being asked between
  // the request and the reply, and for one beat the file menu would have been
  // a list of commands. Within a kind the previous answer stays up while the
  // next one is read, so the list refines rather than blinking every keystroke.
  const [answer, setAnswer] = useState<{ kind: TokenKind; items: Suggestion[] } | null>(null);
  const [active, setActive] = useState(0);
  // Dismissed by hand. Cleared as soon as the token itself changes, so Escape
  // hides this menu rather than turning completion off for the message.
  const [dismissed, setDismissed] = useState<string | null>(null);
  const key = token ? `${token.kind}:${token.query}` : null;

  useEffect(() => {
    setActive(0);
  }, [key]);

  useEffect(() => {
    let live = true;
    if (!token) {
      setAnswer(null);
      return;
    }
    if (token.kind === "command") {
      void commands().then((list) => {
        if (!live) return;
        const wanted = token.query.toLowerCase();
        setAnswer({
          kind: "command",
          items: list.filter((item) => item.label.slice(1).toLowerCase().startsWith(wanted)),
        });
      });
      return () => {
        live = false;
      };
    }
    const timer = setTimeout(() => {
      invoke<WorkspaceEntry[]>("workspace_complete", { session, prefix: token.query })
        .then((entries) => {
          if (!live) return;
          setAnswer({
            kind: "mention",
            items: entries.map((entry) => {
              const directory = entry.kind === "directory";
              return {
                // A directory continues the path, so it is completed with the
                // separator already typed — the next keystroke lists what is
                // inside it rather than repeating the slash by hand.
                insert: `@${entry.path}${directory ? "/" : " "}`,
                label: entry.name,
                hint: entry.path === entry.name ? undefined : entry.path,
                directory,
              };
            }),
          });
        })
        .catch(() => live && setAnswer({ kind: "mention", items: [] }));
    }, SETTLE);
    return () => {
      live = false;
      clearTimeout(timer);
    };
  }, [session, token]);

  const shown = token && key !== dismissed && answer?.kind === token.kind ? answer.items : [];

  const move = useCallback(
    (delta: number) =>
      setActive((was) => (shown.length === 0 ? 0 : (was + delta + shown.length) % shown.length)),
    [shown.length],
  );

  const close = useCallback(() => setDismissed(key), [key]);

  /** The draft with one of these accepted. `null` when there is nothing to
   *  accept, which is what tells the caller to let the key through. */
  const choose = useCallback(
    (item: Suggestion) => {
      if (!token || caret === null) return null;
      return complete(text, token, caret, item.insert);
    },
    [token, text, caret],
  );

  const accept = useCallback(
    () => (shown.length === 0 ? null : choose(shown[Math.min(active, shown.length - 1)])),
    [shown, active, choose],
  );

  return { items: shown, active: Math.min(active, Math.max(shown.length - 1, 0)), move, choose, accept, close };
}

/**
 * Which `@path`s in the draft name something that is really here.
 *
 * One request for the whole draft, settled the same way, so a sentence with
 * three mentions is one directory walk rather than three. Paths that have not
 * been answered about yet are simply not in the set: the tint arrives when the
 * answer does, and a path drawn as ordinary text for one beat is a far smaller
 * lie than one drawn as resolved before anybody checked.
 */
export function useKnownMentions(session: string, text: string): Set<string> {
  const paths = useMemo(() => mentions(text).map((found) => found.path), [text]);
  const wanted = paths.join("\n");
  const [known, setKnown] = useState<Set<string>>(new Set());
  // Read inside the effect so a new array of the same paths does not re-ask.
  const held = useRef(paths);
  held.current = paths;

  useEffect(() => {
    if (wanted === "") {
      setKnown(new Set());
      return;
    }
    let live = true;
    const timer = setTimeout(() => {
      invoke<string[]>("workspace_present", { session, paths: held.current })
        .then((found) => live && setKnown(new Set(found)))
        .catch(() => live && setKnown(new Set()));
    }, SETTLE);
    return () => {
      live = false;
      clearTimeout(timer);
    };
  }, [session, wanted]);

  return known;
}

export type { Token };
