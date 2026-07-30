import { useCallback, useEffect, useState, type CSSProperties } from "react";
import { invoke } from "@tauri-apps/api/core";

import { ChevronDown, ChevronRight, FileIcon, FolderIcon, PlusIcon } from "./components/Icons";
import { useSession } from "./session";
import {
  WORKSPACE_ROOT,
  createWorkspaceEntry,
  deleteWorkspaceEntry,
  emptyWorkspaceTree,
  isWorkspaceDirectoryLoaded,
  renameWorkspaceEntry,
  replaceWorkspaceChildren,
  toggleWorkspaceDirectory,
  visibleWorkspaceTree,
  type WorkspaceEntry,
  type WorkspaceTreeNode,
} from "./workspaceTree";

type Creating = { parent: string; kind: "file" | "directory" };

/**
 * A live, lazy view of one session's workspace.
 *
 * Unlike FilePanel's transcript-derived “Files” index, this calls the confined
 * workspace commands for the session provided by the surrounding pane. Its tree
 * state is intentionally local: two panes may browse separate sessions without
 * making one session's open directories or filter become the other's state.
 */
export function WorkspaceFiles({ onOpenFile }: { onOpenFile: (path: string) => void }) {
  const session = useSession();
  const [tree, setTree] = useState(emptyWorkspaceTree);
  const [filter, setFilter] = useState("");
  const [loading, setLoading] = useState<ReadonlySet<string>>(new Set());
  const [failure, setFailure] = useState<string | null>(null);
  const [creating, setCreating] = useState<Creating | null>(null);
  const [renaming, setRenaming] = useState<string | null>(null);

  const loadDirectory = useCallback(
    (path: string) => {
      setFailure(null);
      setLoading((current) => new Set(current).add(path));
      return invoke<WorkspaceEntry[]>("workspace_list", { session, path: path || null })
        .then((entries) => setTree((current) => replaceWorkspaceChildren(current, path, entries)))
        .catch((error) => setFailure(`could not load this folder: ${String(error)}`))
        .finally(() => setLoading((current) => {
          const next = new Set(current);
          next.delete(path);
          return next;
        }));
    },
    [session],
  );

  // The root is the one eager request. Every other directory stays unknown
  // until selected, which keeps a large repository from becoming a startup job.
  useEffect(() => {
    setTree(emptyWorkspaceTree());
    setFilter("");
    setCreating(null);
    setRenaming(null);
    void loadDirectory(WORKSPACE_ROOT);
  }, [loadDirectory]);

  const refresh = () => {
    const known = [WORKSPACE_ROOT, ...tree.expanded].filter((path) => isWorkspaceDirectoryLoaded(tree, path));
    for (const path of known) void loadDirectory(path);
  };

  const toggleDirectory = (entry: WorkspaceTreeNode) => {
    const opening = !tree.expanded.has(entry.path);
    setTree((current) => toggleWorkspaceDirectory(current, entry.path));
    if (opening && !entry.loaded) void loadDirectory(entry.path);
  };

  const create = (parent: string, kind: Creating["kind"], name: string) => {
    setFailure(null);
    return invoke<WorkspaceEntry>("workspace_create", {
      session,
      parent: parent || null,
      name,
      kind,
    })
      .then((entry) => {
        setTree((current) => createWorkspaceEntry(current, parent, entry));
        setCreating(null);
      })
      .catch((error) => setFailure(`could not create ${kind}: ${String(error)}`));
  };

  const rename = (entry: WorkspaceTreeNode, name: string) => {
    setFailure(null);
    return invoke<WorkspaceEntry>("workspace_rename", { session, path: entry.path, name })
      .then((renamed) => {
        setTree((current) => renameWorkspaceEntry(current, entry.path, renamed));
        setRenaming(null);
      })
      .catch((error) => setFailure(`could not rename ${entry.name}: ${String(error)}`));
  };

  const remove = (entry: WorkspaceTreeNode) => {
    const noun = entry.kind === "directory" ? "empty directory" : "file";
    if (!window.confirm(`Delete ${noun} “${entry.name}”? This cannot be undone.`)) return;
    setFailure(null);
    void invoke<void>("workspace_delete", { session, path: entry.path })
      .then(() => {
        setTree((current) => deleteWorkspaceEntry(current, entry.path));
        setRenaming((current) => (current === entry.path ? null : current));
      })
      .catch((error) => setFailure(`could not delete ${entry.name}: ${String(error)}`));
  };

  const rows = visibleWorkspaceTree(tree, filter);
  const rootLoaded = isWorkspaceDirectoryLoaded(tree, WORKSPACE_ROOT);

  return (
    <section className="workspace-files" aria-label="Workspace files">
      <header className="workspace-tree-toolbar">
        <label className="workspace-filter">
          <span>filter</span>
          <input
            value={filter}
            onChange={(event) => setFilter(event.target.value)}
            placeholder="files loaded here"
            aria-label="Filter loaded workspace files"
          />
        </label>
        <button className="link-btn" onClick={refresh} disabled={loading.size > 0}>
          refresh
        </button>
        <button className="workspace-new" onClick={() => setCreating({ parent: WORKSPACE_ROOT, kind: "file" })}>
          <PlusIcon size={13} />
          file
        </button>
        <button className="workspace-new" onClick={() => setCreating({ parent: WORKSPACE_ROOT, kind: "directory" })}>
          <PlusIcon size={13} />
          folder
        </button>
      </header>

      {failure && <p className="workspace-tree-error">{failure}</p>}

      {creating?.parent === WORKSPACE_ROOT && (
        <WorkspaceNameEntry
          key={`create-root-${creating.kind}`}
          depth={0}
          label={`Name the new ${creating.kind}`}
          onSave={(name) => void create(creating.parent, creating.kind, name)}
          onCancel={() => setCreating(null)}
        />
      )}

      {!rootLoaded && !failure ? (
        <p className="inspect-empty">loading workspace…</p>
      ) : rows.length === 0 && !creating ? (
        <p className="inspect-empty">{filter ? "no loaded files match this filter" : "this workspace is empty"}</p>
      ) : (
        <ul className="workspace-tree" role="tree" aria-label="Workspace files">
          {rows.map((entry) => (
            <li key={entry.path}>
              {renaming === entry.path ? (
                <WorkspaceNameEntry
                  key={`rename-${entry.path}`}
                  depth={entry.depth}
                  label={`Rename ${entry.name}`}
                  initial={entry.name}
                  onSave={(name) => void rename(entry, name)}
                  onCancel={() => setRenaming(null)}
                />
              ) : (
                <WorkspaceRow
                  entry={entry}
                  creating={creating}
                  onToggle={() => toggleDirectory(entry)}
                  onOpen={() => onOpenFile(entry.path)}
                  onCreate={(kind) => setCreating({ parent: entry.path, kind })}
                  onRename={() => setRenaming(entry.path)}
                  onDelete={() => remove(entry)}
                />
              )}
              {creating?.parent === entry.path && (
                <WorkspaceNameEntry
                  key={`create-${entry.path}-${creating.kind}`}
                  depth={entry.depth + 1}
                  label={`Name the new ${creating.kind} in ${entry.name}`}
                  onSave={(name) => void create(creating.parent, creating.kind, name)}
                  onCancel={() => setCreating(null)}
                />
              )}
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}

function WorkspaceRow({
  entry,
  creating,
  onToggle,
  onOpen,
  onCreate,
  onRename,
  onDelete,
}: {
  entry: WorkspaceTreeNode;
  creating: Creating | null;
  onToggle: () => void;
  onOpen: () => void;
  onCreate: (kind: Creating["kind"]) => void;
  onRename: () => void;
  onDelete: () => void;
}) {
  const style = { "--workspace-depth": entry.depth } as CSSProperties;
  const isDirectory = entry.kind === "directory";
  const isLink = entry.kind === "link";

  return (
    <div className="workspace-row" style={style} role="treeitem" aria-level={entry.depth + 1} aria-expanded={isDirectory ? entry.expanded : undefined}>
      {isLink ? (
        <span className="workspace-row-main is-link" title={entry.path}>
          <FileIcon size={14} />
          <span>{entry.name}</span>
          <span className="workspace-link-kind">link</span>
        </span>
      ) : (
        <button
          className="workspace-row-main"
          onClick={isDirectory ? onToggle : onOpen}
          title={entry.path}
          aria-label={isDirectory ? `${entry.expanded ? "Collapse" : "Expand"} ${entry.name}` : `Open ${entry.name}`}
        >
          {isDirectory ? entry.expanded ? <ChevronDown size={13} /> : <ChevronRight size={13} /> : <span className="workspace-chevron-gap" />}
          {isDirectory ? <FolderIcon size={14} /> : <FileIcon size={14} />}
          <span>{entry.name}</span>
          {isDirectory && !entry.loaded && <span className="workspace-row-note">not loaded</span>}
          {isDirectory && entry.loaded && creating?.parent === entry.path && <span className="workspace-row-note">new</span>}
        </button>
      )}
      {!isLink && (
        <span className="workspace-row-actions">
          {isDirectory && (
            <>
              <button className="workspace-row-action" onClick={() => onCreate("file")}>new file</button>
              <button className="workspace-row-action" onClick={() => onCreate("directory")}>new folder</button>
            </>
          )}
          <button className="workspace-row-action" onClick={onRename}>rename</button>
          <button className="workspace-row-action is-delete" onClick={onDelete}>delete</button>
        </span>
      )}
    </div>
  );
}

function WorkspaceNameEntry({
  depth,
  label,
  initial = "",
  onSave,
  onCancel,
}: {
  depth: number;
  label: string;
  initial?: string;
  onSave: (name: string) => void;
  onCancel: () => void;
}) {
  const [name, setName] = useState(initial);
  const style = { "--workspace-depth": depth } as CSSProperties;

  return (
    <form
      className="workspace-name-entry"
      style={style}
      onSubmit={(event) => {
        event.preventDefault();
        const next = name.trim();
        if (next) onSave(next);
      }}
    >
      <input value={name} onChange={(event) => setName(event.target.value)} aria-label={label} autoFocus />
      <button className="workspace-row-action" type="submit">save</button>
      <button className="workspace-row-action" type="button" onClick={onCancel}>cancel</button>
    </form>
  );
}
