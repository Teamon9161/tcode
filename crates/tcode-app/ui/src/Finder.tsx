import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { invoke } from "@tauri-apps/api/core";

import type { ProjectList } from "./types";
import { useSeat } from "./seat";
import { find, foundKey, type Found, type FoundSession } from "./rail";
import { Path } from "./components/Path";
import { StatusDot } from "./components/Status";
import { SearchIcon } from "./components/Icons";
import { statusLabel } from "./activity";

/**
 * Getting to a conversation or a folder by typing its name.
 *
 * The rail shows what is happening and the folders you were in lately; this is
 * how you reach everything else. That division is what lets the rail stay a
 * column you can read at a glance instead of growing into an index of a hundred
 * folders — the long tail is here, one keystroke away, rather than pushed into
 * the rail where it would sit on top of the one question the rail exists to
 * answer.
 *
 * Two kinds of result, and they do different things, which the rows say rather
 * than leave to be guessed. A **conversation** is one that is open right now:
 * picking it shows it in a pane. A **folder** is a place: picking it starts a
 * conversation there, the same meaning `FolderPicker` gives the act everywhere
 * else in the app.
 *
 * Stored conversations are deliberately not searched. Building a preview for one
 * means replaying its log, so searching them means replaying every log in every
 * project on every keystroke; they are reached by opening the project that holds
 * them, where the replay is one folder's worth and already paid for. Saying so
 * beats a search that quietly answers for a third of the corpus.
 *
 * A popover on the shared `.seated` frame rather than a centred command
 * palette: a modal over the window would hide the conversations the window is
 * holding, which is the same reason the approval dock is not one (rule 9b).
 */
export function Finder({
  sessions,
  home,
  onShow,
  onOpenFolder,
}: {
  /** Open conversations, already named and captioned by the rail's own rules —
   *  so the finder cannot end up calling a conversation something different
   *  from what the row two inches to the left calls it. */
  sessions: FoundSession[];
  home: string;
  onShow: (session: string) => void;
  onOpenFolder: (path: string) => Promise<void>;
}) {
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [data, setData] = useState<ProjectList | null>(null);
  const [failure, setFailure] = useState<string | null>(null);
  const [at, setAt] = useState(0);
  const trigger = useRef<HTMLButtonElement>(null);
  const box = useRef<HTMLDivElement>(null);
  const field = useRef<HTMLInputElement>(null);

  const close = useCallback(() => {
    setOpen(false);
    trigger.current?.focus();
  }, []);
  useSeat({
    open,
    trigger,
    box,
    onEscape: close,
    onOutside: () => setOpen(false),
  });

  // Opening clears the query. A finder that remembers what you last typed
  // greets you with somebody else's results — and the reason you opened it is
  // that you have a new thing in mind.
  useEffect(() => {
    if (!open) return;
    setQuery("");
    setAt(0);
    field.current?.focus();
    invoke<ProjectList>("project_list")
      .then((value) => {
        setData(value);
        setFailure(null);
      })
      .catch((error) => setFailure(String(error)));
  }, [open]);

  // Mod+P from anywhere. Not Mod+K: this goes to a place rather than running a
  // command, and the app's other jump — Mod+1…9 — is the same verb with the
  // list already memorised.
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (!(event.ctrlKey || event.metaKey) || event.altKey || event.shiftKey)
        return;
      if (event.key !== "p" && event.key !== "P") return;
      event.preventDefault();
      setOpen((was) => !was);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  const projects = useMemo(() => data?.projects ?? [], [data]);
  const results = useMemo(
    () => find(query, sessions, projects),
    [query, sessions, projects],
  );
  const here = Math.min(at, Math.max(0, results.length - 1));

  const choose = useCallback(
    (entry: Found) => {
      setOpen(false);
      if (entry.kind === "session") {
        onShow(entry.session.id);
        return;
      }
      onOpenFolder(entry.project.path).catch((error) => {
        setFailure(String(error));
        setOpen(true);
      });
    },
    [onShow, onOpenFolder],
  );

  const onFieldKey = (event: React.KeyboardEvent<HTMLInputElement>) => {
    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      event.preventDefault();
      const step = event.key === "ArrowDown" ? 1 : -1;
      // Wraps, because the list is short and running off the end of a short
      // list is a dead key rather than a boundary anyone wanted to feel.
      setAt((was) =>
        results.length ? (was + step + results.length) % results.length : 0,
      );
      return;
    }
    if (event.key === "Enter" && results[here]) {
      event.preventDefault();
      choose(results[here]);
    }
  };

  return (
    <>
      {/* Shaped like the field it opens, because that is what it stands in for.
          It is a button and not an input: the query lives in the popover, and a
          second field in the rail would be a second place to type the same
          thing. */}
      <button
        ref={trigger}
        type="button"
        className="rail-find"
        aria-expanded={open}
        onClick={() => setOpen((was) => !was)}
        title="Find a conversation or folder (Ctrl+P)"
      >
        <SearchIcon size={13} />
        <span className="rail-find-label">Find…</span>
        <kbd className="rail-find-key">^P</kbd>
      </button>

      {open &&
        createPortal(
          <div
            className="seated finder"
            ref={box}
            role="dialog"
            aria-label="Find"
          >
            <div className="finder-field">
              <SearchIcon size={14} />
              <input
                ref={field}
                value={query}
                onChange={(event) => {
                  setQuery(event.target.value);
                  setAt(0);
                }}
                onKeyDown={onFieldKey}
                placeholder="Conversations and folders"
                spellCheck={false}
                aria-label="Find a conversation or folder"
              />
            </div>

            {failure && (
              <p className="fmenu-note" role="alert">
                {failure}
              </p>
            )}

            <div className="finder-list">
              {!data && !failure && (
                <p className="fmenu-note">reading folders…</p>
              )}
              {data && results.length === 0 && (
                <p className="fmenu-note">
                  Nothing open or visited matches that. Earlier conversations
                  are inside their own folder in the rail.
                </p>
              )}
              {results.map((entry, index) => (
                <FoundRow
                  key={foundKey(entry)}
                  entry={entry}
                  home={data?.home ?? home}
                  marked={index === here}
                  onPick={() => choose(entry)}
                  onPoint={() => setAt(index)}
                />
              ))}
            </div>
          </div>,
          document.body,
        )}
    </>
  );
}

/**
 * One result. The two kinds are told apart by what they carry rather than by a
 * label on each row: a conversation has a status dot and the line it is on, a
 * folder has its path. The right edge says what picking will do, and only for
 * folders — "show" is what a list of open conversations does by default and
 * needs no saying, while "starts a new one" is the half of this list that could
 * be misread as going back to something.
 */
function FoundRow({
  entry,
  home,
  marked,
  onPick,
  onPoint,
}: {
  entry: Found;
  home: string;
  marked: boolean;
  onPick: () => void;
  onPoint: () => void;
}) {
  return (
    <button
      type="button"
      className={`finder-item${marked ? " is-marked" : ""}`}
      onClick={onPick}
      onMouseMove={onPoint}
    >
      {entry.kind === "session" ? (
        <>
          <StatusDot status={entry.status} />
          <span className="finder-lines">
            <span className="finder-name">{entry.title}</span>
            <span className="finder-sub">
              {entry.session.name}
              <span className="finder-dot">·</span>
              {statusLabel(entry.activity)}
            </span>
          </span>
        </>
      ) : (
        <>
          <span className="finder-gutter" aria-hidden="true" />
          <span className="finder-lines">
            <span className="finder-name">{entry.project.name}</span>
            <Path
              className="finder-sub"
              path={entry.project.path}
              home={home}
              keep={3}
            />
          </span>
          <span className="finder-verb">new</span>
        </>
      )}
    </button>
  );
}
