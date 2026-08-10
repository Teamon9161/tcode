import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@ipc";

import { FileBody } from "./FileBody";
import { Prose } from "./Prose";
import { useSession } from "./session";
import { basename, workspaceRouteOf } from "./show";
import type { WorkspaceBinaryView, WorkspaceTextView } from "./types";
import {
  canSaveWorkspaceText,
  conflictedWorkspaceFileSession,
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

  useLayoutEffect(() => {
    if (!document || document.mode !== "preview" || !preview.current) return;
    preview.current.scrollTop = document.previewScroll;
  }, [document?.generation, document?.mode]);

  const reload = useCallback(() => {
    if (
      reloadNeedsConfirmation(dirty) &&
      !window.confirm("Discard your unsaved changes and reload this file?")
    ) {
      return;
    }
    void read(true);
  }, [dirty, read]);

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

  const save = useCallback(() => {
    if (!document) return;
    const submitted = document.text;
    if (
      !canSaveWorkspaceText({
        dirty,
        truncated: !document.complete,
        conflicted: document.conflicted,
      })
    ) {
      if (!document.complete) {
        setFailure(
          "This response is only the file prefix. It cannot be saved; reload cannot recover the full content, so use another editor for this file.",
        );
      }
      return;
    }

    setSaving(true);
    setFailure(null);
    invoke<WorkspaceTextView>("workspace_write_text", {
      session,
      path,
      text: submitted,
      revision: document.file.revision,
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
        if (message.toLocaleLowerCase().includes("revision conflict")) {
          setDocument((current) => {
            if (!current) return current;
            const next = conflictedWorkspaceFileSession(current);
            rememberWorkspaceFileSession(session, path, next);
            return next;
          });
          setFailure(null);
        } else {
          setFailure(`could not save this file: ${message}`);
        }
      })
      .finally(() => setSaving(false));
  }, [dirty, document, path, session]);

  const saveEnabled =
    document !== null &&
    canSaveWorkspaceText({
      dirty,
      truncated: !document.complete,
      conflicted: document.conflicted,
    });
  const editing = route.as === "editor" || document?.mode === "edit";

  const controls = useMemo<WorkspaceFileControls>(
    () => ({
      session,
      path,
      mode: route.as === "markdown" ? (document?.mode ?? "preview") : null,
      onMode: route.as === "markdown" ? changeMode : null,
      onReload: reload,
      onSave: route.load === "text" ? save : null,
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
      save,
      saveEnabled,
      saving,
      session,
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

      {document?.conflicted && (
        <p className="workspace-file-conflict">
          The file changed outside this editor. Reload to discard this draft and read the current
          file; this editor will not overwrite it.
        </p>
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
