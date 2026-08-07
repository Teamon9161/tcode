import { useCallback, useEffect, useState } from "react";
import { invoke } from "@ipc";

import { PencilIcon, RefreshIcon } from "./components/Icons";
import { Path } from "./components/Path";
import { FileBody } from "./FileBody";
import { MOD } from "./keys";
import { useSession } from "./session";
import { basename, isBinary } from "./show";
import type { WorkspaceBinaryView, WorkspaceTextView } from "./types";
import {
  canSaveWorkspaceText,
  discardWorkspaceDraft,
  reloadNeedsConfirmation,
  rememberWorkspaceDraft,
  workspaceDraft,
} from "./workspaceDrafts";

/**
 * One file from the live workspace tree: shown as what it is, edited on request.
 *
 * The two states are **read** and **edit**, and choosing those two words is the
 * whole design. This pane used to offer `source` and `preview` instead, which
 * is a distinction only Markdown has: it opened every file — a `.rs`, a `.png`,
 * an `.html` — in a grey textarea, and offered to "preview" things that have no
 * second form. So the file was hardest to read in the state it always started
 * in, and images could not be opened at all.
 *
 * Now the file draws through `FileBody`, the same table `show` uses, and the
 * one control asks the only question with two answers: are you reading this or
 * changing it. Rendering is the resting state because a pane that opened a file
 * is a pane that was asked to show it; editing is a click because editing is a
 * decision. Which files have no such click is decided by the same table: bytes
 * that arrive as a `data:` URL are a picture, and a picture has no source to
 * put in a textarea.
 *
 * Bodies here are untrusted exactly as transcript output is — a file in the
 * project was not written by the person in the conversation. Nothing on this
 * path becomes markup: `FileBody` either constructs nodes or hands the source
 * to an opaque-origin frame.
 */
export function WorkspaceFile({ path }: { path: string }) {
  const session = useSession();
  const drawn = isBinary(path);

  const [file, setFile] = useState<WorkspaceTextView | null>(null);
  const [text, setText] = useState("");
  const [complete, setComplete] = useState(false);
  const [editing, setEditing] = useState(false);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [failure, setFailure] = useState<string | null>(null);
  const [conflicted, setConflicted] = useState(false);

  const dirty = file !== null && text !== file.text;
  const label = basename(path);

  const read = useCallback(
    (discard: boolean) => {
      if (discard) discardWorkspaceDraft(session, path);
      setLoading(true);
      setFailure(null);
      setConflicted(false);
      // Which of the two doors this file comes through is `show.ts`'s answer,
      // not this component's and not the backend's (see `commands.rs`).
      const request = drawn
        ? invoke<WorkspaceBinaryView>("workspace_read_binary", { session, path }).then(
            (loaded): WorkspaceTextView => ({
              path: loaded.path,
              text: loaded.url,
              revision: "",
              bytes: loaded.bytes,
              truncated: false,
            }),
          )
        : invoke<WorkspaceTextView>("workspace_read_text", { session, path });

      return request
        .then((loaded) => {
          setFile(loaded);
          setText(loaded.text);
          setComplete(!loaded.truncated);
        })
        .catch((error) => setFailure(`could not read this file: ${String(error)}`))
        .finally(() => setLoading(false));
    },
    [drawn, path, session],
  );

  useEffect(() => {
    const cached = workspaceDraft(session, path);
    if (cached) {
      setFile(cached.file);
      setText(cached.text);
      setComplete(cached.complete);
      // Unsaved text is the reason this pane opens on the editor rather than on
      // the rendered file: text nobody has written yet must not be somewhere you
      // have to go looking for.
      setEditing(true);
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
    if (
      reloadNeedsConfirmation(dirty) &&
      !window.confirm("Discard your unsaved changes and reload this file?")
    ) {
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

  const saveEnabled =
    file !== null && canSaveWorkspaceText({ dirty, truncated: !complete, conflicted });

  return (
    <section
      className="workspace-file"
      aria-label={`File ${path}`}
      // The key everyone's hands already know. On the section rather than the
      // textarea so it works from the rendered view too, and stopped here so it
      // cannot reach anything else that might one day want it.
      onKeyDown={(event) => {
        if (!(event.ctrlKey || event.metaKey) || event.key !== "s") return;
        event.preventDefault();
        event.stopPropagation();
        if (saveEnabled && !saving) save();
      }}
    >
      <header className="workspace-file-bar">
        <Path className="inspect-path" path={path} keep={2} />
        <div className="workspace-file-actions">
          {/* Nothing here says "preview": there is no second reading of a file,
              only the file and the text behind it. A picture has neither, so it
              gets no control rather than a disabled one. */}
          {/* The mode is said in a word, not spelled by dimming the file.
              The two states used to be told apart by the body: rendered and
              legible, or a grey well in a smaller face. That reads as "this
              file is off" rather than "you are editing it" — and for a source
              file, whose read view is already its text, the difference between
              the two states looked like nothing happened at all, which is why
              only Markdown and CSV seemed editable. The body is now the same
              in both; this control is what changes. */}
          {!drawn && (
            <button
              type="button"
              className={`chip is-toggle${editing ? " is-on" : ""}`}
              onClick={() => setEditing((was) => !was)}
              aria-pressed={editing}
              title={editing ? "Stop editing" : "Edit this file"}
            >
              <PencilIcon size={12} />
              {editing ? "editing" : "edit"}
            </button>
          )}
          {/* Reading the file again is a repeatable action with nothing to
              report, so it is the same glyph the file tree refreshes with.
              Saving keeps its word: it is the primary action, it is the only
              control here with states to say out loud, and a floppy disk is a
              picture of a thing this app's user has never owned. */}
          <button
            className="icon-btn"
            onClick={reload}
            disabled={loading || saving}
            aria-label="Read this file again"
            title="Read this file again"
          >
            <RefreshIcon size={14} />
          </button>
          {editing && (
            <button
              className="btn btn-primary workspace-file-save"
              onClick={save}
              disabled={!saveEnabled || saving}
              title={`Save (${MOD}+S)`}
            >
              {saving ? "saving…" : "save"}
            </button>
          )}
        </div>
      </header>

      {file && !complete && (
        <p className="workspace-file-warning">
          This is only the first part of a {file.bytes.toLocaleString()} byte file. Saving is
          disabled: reload cannot recover the full content; use another editor for this file.
        </p>
      )}

      {conflicted && (
        <p className="workspace-file-conflict">
          The file changed outside this editor. Reload to discard this draft and read the current
          file; this editor will not overwrite it.
        </p>
      )}

      {failure && <p className="workspace-file-error">{failure}</p>}

      {loading && !file ? (
        <p className="inspect-empty">loading file…</p>
      ) : file ? (
        editing ? (
          <textarea
            className="workspace-file-source"
            value={text}
            onChange={(event) => updateText(event.target.value)}
            spellCheck={false}
            aria-label={`Source for ${path}`}
            readOnly={!complete}
          />
        ) : (
          <div className="workspace-file-body">
            {/* `revision` is the backend's, so saving this file reloads the
                frame that is displaying it. Without it a rendered report would
                keep showing the version from before the save — the one view
                here that holds a reference rather than the bytes. */}
            <FileBody path={path} label={label} body={text} revision={file.revision} />
          </div>
        )
      ) : null}
    </section>
  );
}
