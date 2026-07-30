import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

import { Code } from "./components/Code";
import { languageOf } from "./diff";
import { rich } from "./rich";
import { useSession } from "./session";
import type { WorkspaceTextView } from "./types";
import {
  canSaveWorkspaceText,
  discardWorkspaceDraft,
  reloadNeedsConfirmation,
  rememberWorkspaceDraft,
  workspaceDraft,
} from "./workspaceDrafts";

type View = "source" | "preview";

/**
 * A live text editor for one file inside the session-confined workspace.
 *
 * Workspace file bodies are untrusted data just like transcript output: source
 * stays in a textarea, Markdown goes through `rich`, and all other text goes
 * through the existing safe code renderer. This deliberately does not share
 * Shown's image or sandbox dispatch, because these commands return text only.
 */
export function WorkspaceEditor({ path }: { path: string }) {
  const session = useSession();
  const [file, setFile] = useState<WorkspaceTextView | null>(null);
  const [text, setText] = useState("");
  const [complete, setComplete] = useState(false);
  const [view, setView] = useState<View>("source");
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [failure, setFailure] = useState<string | null>(null);
  const [conflicted, setConflicted] = useState(false);

  const dirty = file !== null && text !== file.text;
  const language = languageOf(path);
  const markdown = ["md", "markdown", "mdx"].includes(language);

  const read = useCallback(
    (discard: boolean) => {
      if (discard) discardWorkspaceDraft(session, path);
      setLoading(true);
      setFailure(null);
      setConflicted(false);
      return invoke<WorkspaceTextView>("workspace_read_text", { session, path })
        .then((loaded) => {
          setFile(loaded);
          setText(loaded.text);
          setComplete(!loaded.truncated);
        })
        .catch((error) => setFailure(`could not read this file: ${String(error)}`))
        .finally(() => setLoading(false));
    },
    [path, session],
  );

  useEffect(() => {
    const cached = workspaceDraft(session, path);
    if (cached) {
      setFile(cached.file);
      setText(cached.text);
      setComplete(cached.complete);
      setLoading(false);
      setFailure(null);
      setConflicted(false);
      return;
    }
    void read(false);
  }, [path, read, session]);

  useEffect(() => {
    if (!dirty) return;
    const warn = (event: BeforeUnloadEvent) => {
      event.preventDefault();
      event.returnValue = "";
    };
    window.addEventListener("beforeunload", warn);
    return () => window.removeEventListener("beforeunload", warn);
  }, [dirty]);

  const reload = () => {
    if (reloadNeedsConfirmation(dirty) && !window.confirm("Discard your unsaved changes and reload this file?")) {
      return;
    }
    void read(true);
  };

  const updateText = (next: string) => {
    setText(next);
    if (file) rememberWorkspaceDraft(session, path, { file, text: next, complete });
  };

  const save = () => {
    if (!file) return;
    if (!canSaveWorkspaceText({ dirty, truncated: !complete, conflicted })) {
      if (!complete) {
        setFailure("This response is only the file prefix. It cannot be saved; reload cannot recover the full content, so use another editor for this file.");
      }
      return;
    }

    setSaving(true);
    setFailure(null);
    invoke<WorkspaceTextView>("workspace_write_text", {
      session,
      path,
      text,
      revision: file.revision,
    })
      .then((saved) => {
        // The write received `text` in full even when its bounded echo is a
        // prefix, so the local baseline remains the complete submitted text.
        setFile({ ...saved, text });
        setComplete(true);
        discardWorkspaceDraft(session, path);
        setConflicted(false);
      })
      .catch((error) => {
        const message = String(error);
        if (message.toLocaleLowerCase().includes("revision conflict")) {
          setConflicted(true);
          setFailure(null);
        } else {
          setFailure(`could not save this file: ${message}`);
        }
      })
      .finally(() => setSaving(false));
  };

  const saveEnabled = file !== null && canSaveWorkspaceText({ dirty, truncated: !complete, conflicted });

  return (
    <section className="workspace-editor" aria-label={`Editor for ${path}`}>
      <header className="workspace-editor-bar">
        <p className="inspect-path" title={path}>{path}</p>
        <div className="workspace-editor-actions">
          <div className="segmented segmented-xs" role="group" aria-label="File view">
            <button
              className={view === "source" ? "is-on" : undefined}
              onClick={() => setView("source")}
              aria-pressed={view === "source"}
            >
              source
            </button>
            <button
              className={view === "preview" ? "is-on" : undefined}
              onClick={() => setView("preview")}
              aria-pressed={view === "preview"}
            >
              preview
            </button>
          </div>
          <button className="link-btn" onClick={reload} disabled={loading || saving}>
            reload
          </button>
          <button className="btn btn-primary workspace-editor-save" onClick={save} disabled={!saveEnabled || saving}>
            {saving ? "saving…" : "save"}
          </button>
        </div>
      </header>

      {file && !complete && (
        <p className="workspace-editor-warning">
          This is only the first part of a {file.bytes.toLocaleString()} byte file. Saving is disabled: reload cannot recover the full content; use another editor for this file.
        </p>
      )}

      {conflicted && (
        <p className="workspace-editor-conflict">
          The file changed outside this editor. Reload to discard this draft and read the current file; this editor will not overwrite it.
        </p>
      )}

      {failure && <p className="workspace-editor-error">{failure}</p>}

      {loading && !file ? (
        <p className="inspect-empty">loading file…</p>
      ) : file ? (
        view === "source" ? (
          <textarea
            className="workspace-editor-source"
            value={text}
            onChange={(event) => updateText(event.target.value)}
            spellCheck={false}
            aria-label={`Source for ${path}`}
            readOnly={!complete}
          />
        ) : markdown ? (
          <div className="workspace-editor-preview doc">{rich(text)}</div>
        ) : (
          <div className="workspace-editor-preview">
            <Code source={text} language={language} />
          </div>
        )
      ) : null}
    </section>
  );
}
