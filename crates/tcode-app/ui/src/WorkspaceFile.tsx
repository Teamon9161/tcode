import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@ipc";

import { FileBody } from "./FileBody";
import { Prose } from "./Prose";
import { useSession } from "./session";
import { basename, workspaceRouteOf } from "./show";
import type { WorkspaceBinaryView, WorkspaceStatView, WorkspaceTextView } from "./types";
import {
  canForceSaveWorkspaceText,
  canSaveWorkspaceText,
  diskChangedWorkspaceFileSession,
  newWorkspaceFileSession,
  reloadNeedsConfirmation,
  reloadedWorkspaceFileSession,
  rememberWorkspaceFileSession,
  savedWorkspaceFileSession,
  workspaceFileDirty,
  workspaceFileSession,
  type WorkspaceFileSession,
  type WorkspaceMode,
} from "./workspaceDrafts";
import {
  useWorkspaceFileControls,
  type WorkspaceFileControls,
} from "./workspaceFileControls";
import {
  WorkspaceEditor,
  type WorkspaceEditorSnapshot,
} from "./WorkspaceEditor";

/** How often the open file is checked against disk. The check is metadata-only
 *  (`workspace_stat`), so a poll is the same order of cost as a stat — and it
 *  is how the editor notices an agent rewriting the file out from under it. */
const POLL_MS = 2000;

/** One file from the live workspace tree. Unlike a `show` artifact, every UTF-8
 * non-Markdown file opens as source; Markdown alone has preview/edit modes, and
 * image extensions remain render-only. `workspaceRouteOf` owns that distinction
 * so the component never grows a second extension table. */
export function WorkspaceFile({ path }: { path: string }) {
  const session = useSession();
  const route = useMemo(() => workspaceRouteOf(path), [path]);
  const [document, setDocument] = useState<WorkspaceFileSession | null>(() =>
    route.load === "text" ? workspaceFileSession(session, path) : null,
  );
  const [binary, setBinary] = useState<WorkspaceBinaryView | null>(null);
  const [loading, setLoading] = useState(() => document === null);
  const [saving, setSaving] = useState(false);
  const [failure, setFailure] = useState<string | null>(null);
  const preview = useRef<HTMLDivElement>(null);
  // The poll reads the current baseline from a ref rather than from `document`
  // in a closure, so the 2s interval never has to be re-created on every
  // keystroke; `saving` is read the same way so a poll cannot flag the disk
  // for the write that is in flight (the save refreshes the baseline itself).
  const documentRef = useRef<WorkspaceFileSession | null>(null);
  const savingRef = useRef(false);
  useEffect(() => {
    documentRef.current = document;
  });

  const remember = useCallback(
    (next: WorkspaceFileSession) => {
      rememberWorkspaceFileSession(session, path, next);
      setDocument(next);
    },
    [path, session],
  );

  const read = useCallback(
    (reload: boolean) => {
      setLoading(true);
      setFailure(null);
      const request =
        route.load === "bytes"
          ? invoke<WorkspaceBinaryView>("workspace_read_binary", { session, path }).then(
              (loaded) => setBinary(loaded),
            )
          : invoke<WorkspaceTextView>("workspace_read_text", { session, path }).then(
              (loaded) => {
                setDocument((current) => {
                  const next =
                    reload && current
                      ? reloadedWorkspaceFileSession(current, loaded)
                      : newWorkspaceFileSession(loaded, route.as === "markdown");
                  rememberWorkspaceFileSession(session, path, next);
                  return next;
                });
              },
            );

      return request
        .catch((error) => setFailure(`could not read this file: ${String(error)}`))
        .finally(() => setLoading(false));
    },
    [path, route, session],
  );

  useEffect(() => {
    if (route.load === "text" && workspaceFileSession(session, path)) return;
    void read(false);
  }, [path, read, route.load, session]);

  const dirty = document ? workspaceFileDirty(document) : false;
  useEffect(() => {
    if (!dirty) return;
    const warn = (event: BeforeUnloadEvent) => {
      event.preventDefault();
      event.returnValue = "";
    };
    window.addEventListener("beforeunload", warn);
    return () => window.removeEventListener("beforeunload", warn);
  }, [dirty]);

  // The disk-change check. Runs while the file is shown; the immediate call
  // covers a session that was cached by an earlier navigation, the interval the
  // agent editing the file while it is on screen.
  useEffect(() => {
    if (route.load !== "text") return;
    const check = () => {
      const baseline = documentRef.current?.file.fingerprint;
      if (!baseline || savingRef.current) return;
      invoke<WorkspaceStatView>("workspace_stat", { session, path })
        .then((stat) => {
          if (stat.fingerprint === baseline) return;
          // The baseline guard drops a response that raced a save or reload:
          // the file has since been re-read, so this sighting is stale.
          setDocument((current) =>
            current && !current.diskChanged && current.file.fingerprint === baseline
              ? diskChangedWorkspaceFileSession(current)
              : current,
          );
        })
        .catch(() => {
          // A transient read error is not worth a banner; the next poll retries.
        });
    };
    check();
    const id = setInterval(check, POLL_MS);
    return () => clearInterval(id);
  }, [path, route.load, session]);

  useLayoutEffect(() => {
    if (!document || document.mode !== "preview" || !preview.current) return;
    preview.current.scrollTop = document.previewScroll;
  }, [document?.generation, document?.mode]);

  // `confirmed` is the banner's reload button: the banner itself is the choice
  // to discard, so it must not ask again. The header's refresh icon still asks
  // whenever reloading would throw away edits.
  const reload = useCallback(
    (confirmed: boolean) => {
      if (
        !confirmed &&
        reloadNeedsConfirmation(dirty) &&
        !window.confirm("Discard your unsaved changes and reload this file?")
      ) {
        return;
      }
      void read(true);
    },
    [dirty, read],
  );

  const changeMode = useCallback((mode: WorkspaceMode) => {
    if (!document || document.mode === mode) return;
    remember({
      ...document,
      mode,
      previewScroll:
        document.mode === "preview" ? (preview.current?.scrollTop ?? 0) : document.previewScroll,
    });
  }, [document, remember]);

  const updateEditor = useCallback(
    (snapshot: WorkspaceEditorSnapshot) => {
      setDocument((current) => {
        if (!current) return current;
        const next = {
          ...current,
          text: snapshot.state.doc.toString(),
          editorState: snapshot.state,
          editorScroll: snapshot.scroll,
        };
        rememberWorkspaceFileSession(session, path, next);
        return next;
      });
    },
    [path, session],
  );

  // `force` is the banner's overwrite answer: write over whatever is on disk
  // now, skipping the revision guard the reader was just told about. A
  // revision conflict on a plain save lands in the same banner instead of a
  // dead end — the file changed, and the choice is reload or overwrite.
  const submit = useCallback(
    (force: boolean) => {
      if (!document) return;
      const submitted = document.text;
      const allowed = force
        ? canForceSaveWorkspaceText({ dirty, truncated: !document.complete })
        : canSaveWorkspaceText({
            dirty,
            truncated: !document.complete,
            diskChanged: document.diskChanged,
          });
      if (!allowed) {
        if (!document.complete) {
          setFailure(
            "This response is only the file prefix. It cannot be saved; reload cannot recover the full content, so use another editor for this file.",
          );
        }
        return;
      }

      setSaving(true);
      savingRef.current = true;
      setFailure(null);
      invoke<WorkspaceTextView>("workspace_write_text", {
        session,
        path,
        text: submitted,
        revision: document.file.revision,
        force,
      })
        .then((saved) => {
          setDocument((current) => {
            if (!current) return current;
            const next = savedWorkspaceFileSession(current, saved, submitted);
            rememberWorkspaceFileSession(session, path, next);
            return next;
          });
        })
        .catch((error) => {
          const message = String(error);
          if (!force && message.toLocaleLowerCase().includes("revision conflict")) {
            setDocument((current) => {
              if (!current) return current;
              const next = diskChangedWorkspaceFileSession(current);
              rememberWorkspaceFileSession(session, path, next);
              return next;
            });
            setFailure(null);
          } else {
            setFailure(`could not save this file: ${message}`);
          }
        })
        .finally(() => {
          setSaving(false);
          savingRef.current = false;
        });
    },
    [dirty, document, path, session],
  );

  const saveEnabled =
    document !== null &&
    canSaveWorkspaceText({
      dirty,
      truncated: !document.complete,
      diskChanged: document.diskChanged,
    });
  const editing = route.as === "editor" || document?.mode === "edit";

  const controls = useMemo<WorkspaceFileControls>(
    () => ({
      session,
      path,
      mode: route.as === "markdown" ? (document?.mode ?? "preview") : null,
      onMode: route.as === "markdown" ? changeMode : null,
      onReload: () => reload(false),
      onSave: route.load === "text" ? () => submit(false) : null,
      dirty,
      loading,
      saving,
      saveDisabled: !saveEnabled,
    }),
    [
      changeMode,
      dirty,
      document?.mode,
      loading,
      path,
      reload,
      route.as,
      route.load,
      saveEnabled,
      saving,
      session,
      submit,
    ],
  );
  useWorkspaceFileControls(controls);

  return (
    <section className="workspace-file" aria-label={`File ${path}`}>

      {document && !document.complete && (
        <p className="workspace-file-warning">
          This is only the first part of a {document.file.bytes.toLocaleString()} byte file. Saving
          is disabled: reload cannot recover the full content; use another editor for this file.
        </p>
      )}

      {document?.diskChanged && (
        <div className="workspace-file-disk">
          <p>
            This file changed on disk.
            {dirty
              ? " Your edits are still here — reload to discard them, or overwrite the file."
              : ""}
          </p>
          <div className="workspace-file-disk-actions">
            <button type="button" className="btn" onClick={() => reload(true)}>
              {dirty ? "Reload and discard my changes" : "Reload"}
            </button>
            {dirty && (
              <button
                type="button"
                className="btn btn-primary"
                onClick={() => submit(true)}
                disabled={
                  !canForceSaveWorkspaceText({ dirty, truncated: !document.complete }) ||
                  saving
                }
              >
                Overwrite the file
              </button>
            )}
          </div>
        </div>
      )}

      {failure && <p className="workspace-file-error">{failure}</p>}

      {loading && !document && !binary ? (
        <p className="inspect-empty">loading file…</p>
      ) : route.as === "image" && binary ? (
        <div className="workspace-file-body">
          <FileBody path={path} label={basename(path)} body={binary.url} />
        </div>
      ) : document && editing ? (
        <WorkspaceEditor
          key={document.generation}
          path={path}
          initialDoc={document.text}
          initialState={document.editorState}
          initialScroll={document.editorScroll}
          readOnly={!document.complete}
          onSnapshot={updateEditor}
        />
      ) : document ? (
        <div
          className="workspace-file-preview"
          ref={preview}
          onScroll={(event) => {
            const top = event.currentTarget.scrollTop;
            rememberWorkspaceFileSession(session, path, { ...document, previewScroll: top });
          }}
        >
          <Prose className="doc" text={document.text} />
        </div>
      ) : null}
    </section>
  );
}
