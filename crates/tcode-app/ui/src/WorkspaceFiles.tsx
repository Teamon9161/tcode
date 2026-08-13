import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type CSSProperties,
  type MouseEvent as ReactMouseEvent,
} from "react";
import { createPortal } from "react-dom";
import { invoke } from "@ipc";

import {
  ChevronDown,
  ChevronRight,
  FileIcon,
  FolderIcon,
  PlusIcon,
  RefreshIcon,
  SearchIcon,
} from "./components/Icons";
import { MOD } from "./keys";
import { useSeat } from "./seat";
import { useSession } from "./session";
import {
  WORKSPACE_ROOT,
  workspaceHostPath,
  createWorkspaceEntry,
  deleteWorkspaceEntry,
  emptyWorkspaceTree,
  isWorkspaceDirectoryLoaded,
  renameWorkspaceEntry,
  replaceWorkspaceChildren,
  toggleWorkspaceDirectory,
  visibleWorkspaceTree,
  type WorkspaceEntry,
  type WorkspaceList,
  type WorkspaceTreeNode,
} from "./workspaceTree";

type Creating = { parent: string; kind: "file" | "directory" };

/** One external program this machine actually has, as `openers.rs` reports it.
 *  The id is the backend's own; the webview never names a command line. */
type Opener = { id: string; name: string };

/** Where a menu was asked for and what it acts on. A `null` entry is the
 *  workspace root: the tree's own empty space, and the toolbar's `+`. `asking`
 *  opens it straight on the delete question, which is what the Delete key does. */
type Menu = {
  entry: WorkspaceTreeNode | null;
  at: { x: number; y: number };
  asking?: boolean;
};

/** The folder a new thing goes in when a row is current: that row if it is a
 *  folder, otherwise the folder holding it — the same answer a file manager
 *  gives, and the reason `Mod+N` on a file does not create at the root. */
function folderOf(node: WorkspaceTreeNode | null): string {
  if (!node) return WORKSPACE_ROOT;
  if (node.kind === "directory") return node.path;
  const cut = node.path.lastIndexOf("/");
  return cut === -1 ? WORKSPACE_ROOT : node.path.slice(0, cut);
}

/**
 * A live, lazy view of one session's workspace.
 *
 * Unlike FilePanel's transcript-derived “Files” index, this calls the confined
 * workspace commands for the session provided by the surrounding pane. Its tree
 * state is intentionally local: two panes may browse separate sessions without
 * making one session's open directories or filter become the other's state.
 *
 * **A row is a name and nothing else.** Acting on a file is a right-click, the
 * way it is in every file tree anyone has used — familiarity is the feature
 * (PRODUCT.md § Design Principles), and it is also the only shape that scales:
 * four hover-revealed word buttons per row put more control than content on a
 * line whose whole job is to be scanned, and they arrived exactly when the
 * pointer was over the name somebody was reading.
 *
 * **Lazy loading is ours to keep quiet about.** Every unopened folder used to
 * carry a `not loaded` tag, which is bookkeeping from this file leaking onto the
 * screen: a closed folder is closed, and that is already drawn by the chevron.
 * The one place the laziness is a fact the reader needs is a filter that found
 * nothing — collapsed folders were never searched — so it is said there, once,
 * instead of on every row forever.
 */
export function WorkspaceFiles({
  cwd,
  onOpenFile,
  onOpenAside,
  onMention,
}: {
  /** The session's folder, for the one thing the tree cannot say in relative
   *  terms: a path somebody is about to paste somewhere else. */
  cwd: string;
  onOpenFile: (path: string) => void;
  onOpenAside: (path: string) => void;
  onMention: (path: string) => void;
}) {
  const session = useSession();
  const [tree, setTree] = useState(emptyWorkspaceTree);
  const [filter, setFilter] = useState("");
  const [loading, setLoading] = useState<ReadonlySet<string>>(new Set());
  const [failure, setFailure] = useState<string | null>(null);
  const [warning, setWarning] = useState<string | null>(null);
  const [creating, setCreating] = useState<Creating | null>(null);
  const [renaming, setRenaming] = useState<string | null>(null);
  const [menu, setMenu] = useState<Menu | null>(null);
  // Which row the keyboard is on, so the keys below have something to act on.
  // Read from focus rather than kept as a selection: the tree has no selection —
  // one click opens — and a second highlight that only the keyboard moves would
  // be a second answer to "which row is current".
  const [active, setActive] = useState<WorkspaceTreeNode | null>(null);
  // Only the `+` button: a popover's trigger is exempt from dismiss-on-outside so
  // that pressing it again closes rather than reopens. A row must not be exempt —
  // it would keep its own menu open while toggling the folder underneath it.
  const adder = useRef<HTMLButtonElement>(null);

  const loadDirectory = useCallback(
    (path: string) => {
      setFailure(null);
      setWarning(null);
      setLoading((current) => new Set(current).add(path));
      return invoke<WorkspaceList>("workspace_list", { session, path: path || null })
        .then((listing) => {
          setTree((current) => replaceWorkspaceChildren(current, path, listing.entries));
          if (listing.warnings.length > 0) {
            setWarning(listing.warnings.join("; "));
          }
        })
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
    setMenu(null);
    setWarning(null);
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

  // Creating inside a folder has to open it, or the new file lands where nobody
  // can see it and the pane looks as though nothing happened.
  const startCreate = (parent: string, kind: Creating["kind"]) => {
    if (parent && !tree.expanded.has(parent)) {
      setTree((current) => toggleWorkspaceDirectory(current, parent));
      if (!isWorkspaceDirectoryLoaded(tree, parent)) void loadDirectory(parent);
    }
    setCreating({ parent, kind });
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
    setFailure(null);
    void invoke<void>("workspace_delete", { session, path: entry.path })
      .then(() => {
        setTree((current) => deleteWorkspaceEntry(current, entry.path));
        setRenaming((current) => (current === entry.path ? null : current));
      })
      .catch((error) => setFailure(`could not delete ${entry.name}: ${String(error)}`));
  };

  // Recoverable, so it needs no question — the same reason a file manager's
  // "Move to trash" does not confirm. The permanent Delete below still asks.
  const trash = (entry: WorkspaceTreeNode) => {
    setFailure(null);
    void invoke<void>("workspace_trash", { session, path: entry.path })
      .then(() => {
        setTree((current) => deleteWorkspaceEntry(current, entry.path));
        setRenaming((current) => (current === entry.path ? null : current));
      })
      .catch((error) =>
        setFailure(`could not move ${entry.name} to the trash: ${String(error)}`),
      );
  };

  // Failures are shown rather than swallowed (rule 7). A clipboard that refused
  // is worth a line: the next thing that happens is a paste of whatever was
  // there before, into something that matters.
  const copy = (text: string) => {
    setFailure(null);
    navigator.clipboard
      .writeText(text)
      .catch((error) => setFailure(`could not copy that: ${String(error)}`));
  };

  /**
   * The keys, on the pane rather than on the window.
   *
   * `Mod+N` is already the window's "start a conversation in this folder", and
   * it keeps that meaning everywhere except inside this tree, where it means
   * what it means in every file manager. Scoping it to the pane is what lets
   * both be true: this runs on the way up through React, before the window's own
   * listener, and stops the key there. Nothing has to know about the other.
   *
   * The bare keys are ignored while the caret is in the filter field — `Delete`
   * in a text field deletes a character, and it must not also delete a file.
   */
  const onKeys = (event: React.KeyboardEvent<HTMLElement>) => {
    const typing = event.target instanceof HTMLInputElement;
    const mod = event.ctrlKey || event.metaKey;

    if (mod && (event.key === "n" || event.key === "N")) {
      event.preventDefault();
      event.stopPropagation();
      startCreate(folderOf(active), event.shiftKey ? "directory" : "file");
      return;
    }
    if (typing || !active) return;

    if (event.key === "F2") {
      event.preventDefault();
      setRenaming(active.path);
      return;
    }
    if (event.key === "Delete") {
      event.preventDefault();
      // Asks rather than deletes. A single keystroke is exactly where a
      // destructive action must not carry one click's worth of certainty. The
      // question opens on the row it is about, which is where the eye already is.
      const box = (document.activeElement as HTMLElement | null)?.getBoundingClientRect();
      setMenu({ entry: active, at: { x: box?.left ?? 0, y: box?.bottom ?? 0 }, asking: true });
      return;
    }
    if (event.key === "@" && active.kind !== "directory") {
      event.preventDefault();
      onMention(active.path);
      return;
    }
    if (mod && event.key === "Enter" && active.kind === "file") {
      event.preventDefault();
      onOpenAside(active.path);
    }
  };

  const rows = visibleWorkspaceTree(tree, filter);
  const rootLoaded = isWorkspaceDirectoryLoaded(tree, WORKSPACE_ROOT);

  return (
    <section className="workspace-files" aria-label="Workspace files" onKeyDown={onKeys}>
      <header className="workspace-bar">
        <span className="workspace-find">
          <SearchIcon size={13} />
          <input
            value={filter}
            onChange={(event) => setFilter(event.target.value)}
            placeholder="filter"
            aria-label="Filter the folders you have opened"
          />
        </span>
        {/* One `+` rather than a pair of near-identical glyphs: “new file” and
            “new folder” differ by a detail you have to read a tooltip to see,
            and the menu they open is the same one every row already has. */}
        <button
          ref={adder}
          type="button"
          className="icon-btn"
          aria-expanded={menu !== null && menu.entry === null}
          aria-label="Add to this folder"
          title="Add to this folder"
          onClick={(event) => {
            const box = event.currentTarget.getBoundingClientRect();
            setMenu((was) => (was && was.entry === null ? null : { entry: null, at: { x: box.left, y: box.bottom } }));
          }}
        >
          <PlusIcon size={15} />
        </button>
        {/* No spinner while it runs. The pulsing status dot is the app's one
            continuous animation (DESIGN.md § Motion); a second one would be
            motion competing with the only motion that means something. */}
        <button
          type="button"
          className="icon-btn"
          onClick={refresh}
          disabled={loading.size > 0}
          aria-label="Read these folders again"
          title="Read these folders again"
        >
          <RefreshIcon size={15} />
        </button>
      </header>

      {failure && (
        <p className="workspace-error" role="alert">
          {failure}
        </p>
      )}

      {warning && <p className="workspace-note">{warning}</p>}

      {/* What the filter did and did not look at, said once beneath the field it
          qualifies rather than as a tag on every closed folder. It answers both
          questions a partial search raises — why a folder that plainly does not
          match is still listed, and why the thing you are sure exists is not.
          Under the field, not under the list: the list stretches to the bottom
          of the pane so that its empty space is still the root folder's, which
          would have left this stranded four hundred pixels from the rows it is
          about. */}
      {filter.trim() && <p className="workspace-note">only the folders you have opened were searched</p>}

      {creating?.parent === WORKSPACE_ROOT && (
        <WorkspaceNameEntry
          key={`create-root-${creating.kind}`}
          depth={0}
          kind={creating.kind}
          label={`Name the new ${creating.kind}`}
          onSave={(name) => void create(creating.parent, creating.kind, name)}
          onCancel={() => setCreating(null)}
        />
      )}

      {!rootLoaded && !failure ? (
        <p className="inspect-empty">reading the workspace…</p>
      ) : rows.length === 0 && !creating ? (
        <p className="inspect-empty">{filter ? "nothing matches" : "this folder is empty"}</p>
      ) : (
        <ul
          className="workspace-tree"
          role="tree"
          aria-label="Workspace files"
          // The empty space below the last row belongs to the folder the tree is
          // rooted at, which is what makes “new file” reachable in a workspace
          // with nothing in it yet.
          onContextMenu={(event) => {
            if (event.target !== event.currentTarget) return;
            event.preventDefault();
            setMenu({ entry: null, at: { x: event.clientX, y: event.clientY } });
          }}
        >
          {rows.map((entry) => (
            <li key={entry.path} role="none">
              {renaming === entry.path ? (
                <WorkspaceNameEntry
                  key={`rename-${entry.path}`}
                  depth={entry.depth}
                  kind={entry.kind}
                  label={`Rename ${entry.name}`}
                  initial={entry.name}
                  onSave={(name) => void rename(entry, name)}
                  onCancel={() => setRenaming(null)}
                />
              ) : (
                <WorkspaceRow
                  entry={entry}
                  targeted={menu?.entry?.path === entry.path}
                  onToggle={() => toggleDirectory(entry)}
                  onOpen={(aside) => (aside ? onOpenAside : onOpenFile)(entry.path)}
                  onFocus={() => setActive(entry)}
                  onMenu={(at) => setMenu({ entry, at })}
                />
              )}
              {creating?.parent === entry.path && (
                <WorkspaceNameEntry
                  key={`create-${entry.path}-${creating.kind}`}
                  depth={entry.depth + 1}
                  kind={creating.kind}
                  label={`Name the new ${creating.kind} in ${entry.name}`}
                  onSave={(name) => void create(creating.parent, creating.kind, name)}
                  onCancel={() => setCreating(null)}
                />
              )}
            </li>
          ))}
        </ul>
      )}

      {menu && (
        <WorkspaceMenu
          menu={menu}
          trigger={adder}
          onClose={() => setMenu(null)}
          onOpen={(aside) => menu.entry && (aside ? onOpenAside : onOpenFile)(menu.entry.path)}
          onCreate={(kind) => startCreate(menu.entry?.path ?? WORKSPACE_ROOT, kind)}
          onMention={() => menu.entry && onMention(menu.entry.path)}
          onCopy={(what) => {
            const entry = menu.entry;
            if (!entry) return;
            copy(
              what === "name"
                ? entry.name
                : what === "relative"
                  ? entry.path
                  : workspaceHostPath(cwd, entry.path),
            );
          }}
          onRename={() => menu.entry && setRenaming(menu.entry.path)}
          onTrash={() => menu.entry && trash(menu.entry)}
          onDelete={() => menu.entry && remove(menu.entry)}
        />
      )}
    </section>
  );
}

/**
 * One row: a chevron where there is something to open, the kind, the name.
 *
 * The whole row is the target and the whole row is the menu's, including the
 * indent — pointing at a name and getting nothing because the pointer was two
 * pixels left of the glyph is the failure mode of a tree drawn as buttons the
 * width of their text.
 */
function WorkspaceRow({
  entry,
  targeted,
  onToggle,
  onOpen,
  onFocus,
  onMenu,
}: {
  entry: WorkspaceTreeNode;
  /** A menu is open for this row. It keeps the row marked once the pointer
   *  leaves it for the menu, which is otherwise the moment a list of verbs
   *  stops saying which name it is about. */
  targeted: boolean;
  onToggle: () => void;
  onOpen: (aside: boolean) => void;
  onFocus: () => void;
  onMenu: (at: { x: number; y: number }) => void;
}) {
  const isDirectory = entry.kind === "directory";
  const isLink = entry.kind === "link";

  // A keyboard menu key reports the focused element's corner rather than a
  // pointer position; both arrive here as a viewport point.
  const ask = (event: ReactMouseEvent<HTMLElement>) => {
    event.preventDefault();
    const box = event.currentTarget.getBoundingClientRect();
    const off = event.clientX === 0 && event.clientY === 0;
    onMenu({ x: off ? box.left + box.width / 4 : event.clientX, y: off ? box.bottom : event.clientY });
  };

  const body = (
    <>
      {isDirectory ? (
        entry.expanded ? <ChevronDown size={13} /> : <ChevronRight size={13} />
      ) : (
        <span className="workspace-indent" />
      )}
      {isDirectory ? <FolderIcon size={14} /> : <FileIcon size={14} />}
      <span className="workspace-name">{entry.name}</span>
      {/* A symlink cannot be opened from here, and this is what says why the row
          does not answer. Rare enough to be worth a word rather than a glyph
          nobody has seen before. */}
      {isLink && <span className="workspace-tag">link</span>}
    </>
  );

  const shared = {
    className: `workspace-row${isLink ? " is-link" : ""}${targeted ? " is-target" : ""}`,
    style: { "--workspace-depth": entry.depth } as CSSProperties,
    title: entry.path,
    role: "treeitem",
    "aria-level": entry.depth + 1,
    onContextMenu: ask,
    onFocus,
  };

  // Focusable but with nothing to click: the menu is the only thing a link row
  // offers, and it has to be reachable without a pointer.
  if (isLink) {
    return (
      <span {...shared} tabIndex={0}>
        {body}
      </span>
    );
  }

  // Mod+click opens a file in a pane of its own, the way a modified click opens
  // a second thing rather than replacing the first everywhere else.
  return (
    <button
      {...shared}
      type="button"
      aria-expanded={isDirectory ? entry.expanded : undefined}
      onClick={(event) =>
        isDirectory ? onToggle() : onOpen(event.ctrlKey || event.metaKey)
      }
    >
      {body}
    </button>
  );
}

/**
 * What can be done to the thing under the pointer.
 *
 * Deleting asks in the menu itself rather than in `window.confirm`. A native
 * confirm freezes the whole webview, and this window is holding other
 * conversations that are still running — the same reason approvals are not
 * modal (rule 9b). Asking here also keeps the question next to the name it is
 * about, which is the fact the answer depends on.
 *
 * Every key that also does one of these things is printed beside it, dim. A
 * shortcut nothing ever mentions is a shortcut nobody uses — the same reason
 * the empty conversation lists the layout keys — and a menu is where somebody
 * already is when they want the thing the key is for.
 */
function WorkspaceMenu({
  menu,
  trigger,
  onClose,
  onOpen,
  onCreate,
  onMention,
  onCopy,
  onRename,
  onTrash,
  onDelete,
}: {
  menu: Menu;
  trigger: React.RefObject<HTMLButtonElement | null>;
  onClose: () => void;
  onOpen: (aside: boolean) => void;
  onCreate: (kind: Creating["kind"]) => void;
  onMention: () => void;
  onCopy: (what: "path" | "relative" | "name") => void;
  onRename: () => void;
  onTrash: () => void;
  onDelete: () => void;
}) {
  const session = useSession();
  const box = useRef<HTMLDivElement>(null);
  const [asking, setAsking] = useState(menu.asking ?? false);
  const entry = menu.entry;
  const isRoot = entry === null;
  const isDirectory = isRoot || entry.kind === "directory";
  // A link is listed but never followed, here or in the backend, so the two
  // things that would have to follow it are simply not offered.
  const external = entry !== null && entry.kind !== "link";

  const close = useCallback(() => {
    setAsking(false);
    onClose();
  }, [onClose]);
  useSeat({ open: true, trigger: isRoot ? trigger : NO_TRIGGER, at: menu.at, box, onEscape: close, onOutside: close });

  // The first item takes focus, on open and again when the delete question
  // replaces the list. One rule rather than an `autoFocus` on whichever item
  // happens to come first in each of the shapes this menu has — that was three
  // conditions and a directory ended up with none of them.
  useEffect(() => {
    box.current?.querySelector<HTMLButtonElement>("[role='menuitem']")?.focus();
  }, [asking]);

  const act = (run: () => void) => () => {
    run();
    close();
  };

  // Arrow keys walk the items; the first one takes focus on open, so the menu is
  // usable from the keyboard the moment it appears.
  const walk = (event: React.KeyboardEvent) => {
    if (event.key !== "ArrowDown" && event.key !== "ArrowUp") return;
    event.preventDefault();
    const items = [...(box.current?.querySelectorAll<HTMLButtonElement>("[role='menuitem']") ?? [])];
    const at = items.indexOf(document.activeElement as HTMLButtonElement);
    const step = event.key === "ArrowDown" ? 1 : -1;
    items[(at + step + items.length) % items.length]?.focus();
  };

  return createPortal(
    <div className="seated cmenu" ref={box} role="menu" aria-label="File actions" onKeyDown={walk}>
      {asking && entry ? (
        <>
          <p className="cmenu-ask">
            Delete <span className="cmenu-subject">{entry.name}</span>?
            {entry.kind === "directory" ? " It has to be empty." : ""} This cannot be undone.
          </p>
          {/* Keeping it comes first, and that is not politeness: this replaces a
              menu item that was in roughly this spot, so the pointer is already
              here — the answer under it has to be the harmless one. It is also
              the one that takes focus, being first. */}
          <button type="button" className="cmenu-item" role="menuitem" onClick={() => setAsking(false)}>
            Keep it
          </button>
          <button type="button" className="cmenu-item is-danger" role="menuitem" onClick={act(onDelete)}>
            Delete
          </button>
        </>
      ) : (
        <>
          {entry && entry.kind === "file" && (
            <Item onClick={act(() => onOpen(false))} keys="↵">
              Open
            </Item>
          )}
          {isDirectory && (
            <>
              <Item onClick={act(() => onCreate("file"))} keys={`${MOD} N`}>
                New file
              </Item>
              <Item onClick={act(() => onCreate("directory"))} keys={`${MOD} ⇧ N`}>
                New folder
              </Item>
            </>
          )}

          {external && (
            <Flyout
              parent={box}
              session={session}
              path={entry.path}
              onPane={entry.kind === "file" ? act(() => onOpen(true)) : null}
              onDone={close}
            />
          )}

          {entry && (
            <>
              <span className="cmenu-rule" role="separator" />
              <Item onClick={act(onMention)} keys="@">
                Mention in the message
              </Item>
              <Item onClick={act(() => onCopy("path"))}>Copy path</Item>
              <Item onClick={act(() => onCopy("relative"))}>Copy relative path</Item>
              <Item onClick={act(() => onCopy("name"))}>Copy name</Item>

              <span className="cmenu-rule" role="separator" />
              <Item onClick={act(onRename)} keys="F2">
                Rename
              </Item>
              <Item onClick={act(onTrash)}>Move to trash</Item>
              <Item danger onClick={() => setAsking(true)} keys="Del">
                Delete
              </Item>
            </>
          )}
        </>
      )}
    </div>,
    document.body,
  );
}

/** One line of the menu: what it does on the left, the key that also does it on
 *  the right. The key is `--faint` because it is a reminder, not a label — you
 *  read the verb and happen to see the key. */
function Item({
  children,
  keys,
  danger,
  onClick,
}: {
  children: React.ReactNode;
  keys?: string;
  danger?: boolean;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      className={`cmenu-item${danger ? " is-danger" : ""}`}
      role="menuitem"
      onClick={onClick}
    >
      <span>{children}</span>
      {/* Out of the accessible name: read aloud, "Delete Del" is the item's own
          label with a syllable of noise stapled to it. */}
      {keys && (
        <span className="cmenu-key" aria-hidden="true">
          {keys}
        </span>
      )}
    </button>
  );
}

/**
 * “Open in ▸” and the panel that comes out of it.
 *
 * The list is read from the backend when the flyout is first pointed at, and it
 * holds only programs actually installed on this machine (`openers.rs`). An
 * editor that is not here is absent rather than present and failing, which is
 * the difference between a menu that knows the machine and one that guesses at
 * it.
 *
 * It is drawn inside the parent menu's own element rather than in a second
 * portal. That is load-bearing: `seat.ts` dismisses a popover when a pointer
 * goes down outside its box, and a submenu in its own portal is outside that box
 * — so picking an editor would have closed the menu before the click landed.
 */
function Flyout({
  parent,
  session,
  path,
  onPane,
  onDone,
}: {
  parent: React.RefObject<HTMLDivElement | null>;
  session: string;
  path: string;
  /** The one destination that is not an external program: a pane of its own.
   *  `null` for a folder, which this app has nowhere to put. */
  onPane: (() => void) | null;
  onDone: () => void;
}) {
  const [open, setOpen] = useState(false);
  const [left, setLeft] = useState(false);
  const [openers, setOpeners] = useState<Opener[] | null>(null);
  const [failure, setFailure] = useState<string | null>(null);

  // Read on first reveal, not when the menu opens: most menus are opened to
  // rename or delete something, and those should not cost a process probe.
  useEffect(() => {
    if (!open || openers) return;
    invoke<Opener[]>("workspace_openers")
      .then(setOpeners)
      .catch((error) => setFailure(String(error)));
  }, [open, openers]);

  const reveal = (on: boolean) => {
    if (on) {
      const room = parent.current?.getBoundingClientRect();
      setLeft(room ? room.right + SUBMENU_WIDTH > window.innerWidth : false);
    }
    setOpen(on);
  };

  // The menu closes once the other program has been handed the file. It stays
  // open on failure, with the reason where the item was — nowhere else on screen
  // would be a sensible place to report that Explorer would not start.
  const send = (opener: string) => {
    invoke<void>("workspace_open_external", { session, path, opener })
      .then(onDone)
      .catch((error) => setFailure(String(error)));
  };

  return (
    <div
      className="cmenu-nest"
      onPointerEnter={() => reveal(true)}
      onPointerLeave={() => reveal(false)}
    >
      <button
        type="button"
        className="cmenu-item"
        role="menuitem"
        aria-haspopup="menu"
        aria-expanded={open}
        onClick={() => reveal(!open)}
        onKeyDown={(event) => {
          if (event.key === "ArrowRight") reveal(true);
          if (event.key === "ArrowLeft") reveal(false);
        }}
      >
        <span>Open in</span>
        <span className="cmenu-key" aria-hidden="true">
          ▸
        </span>
      </button>

      {open && (
        <div className={`seated cmenu cmenu-sub${left ? " is-left" : ""}`} role="menu" aria-label="Open in">
          {onPane && (
            <Item onClick={onPane} keys={`${MOD} ↵`}>
              A pane beside this
            </Item>
          )}
          {onPane && <span className="cmenu-rule" role="separator" />}
          {failure && <p className="cmenu-ask">{failure}</p>}
          {!openers && !failure && <p className="cmenu-ask">looking for editors…</p>}
          {openers?.map((opener) => (
            <Item key={opener.id} onClick={() => send(opener.id)}>
              {opener.name}
            </Item>
          ))}
        </div>
      )}
    </div>
  );
}

/** Kept in step with `--cmenu-w` in the stylesheet: it is only used to decide
 *  which side of the parent has room, so being a few pixels out is harmless. */
const SUBMENU_WIDTH = 190;

/** No trigger to exempt: a row's menu is dismissed by any click outside it,
 *  including one on the row that opened it. */
const NO_TRIGGER: React.RefObject<HTMLElement | null> = { current: null };

/**
 * Naming a new thing, or renaming an old one, on the row it will occupy.
 *
 * In place rather than in a dialog: the name's neighbours are what you check it
 * against, and a box in the middle of the window hides them. Enter saves, Escape
 * abandons — the field itself is the whole control, so it carries no buttons.
 *
 * Clicking away abandons it too, rather than committing. The two mistakes are
 * not the same size: a name you have to type again costs a moment, while a file
 * created from half a name is a file on disk you now have to find and remove.
 */
function WorkspaceNameEntry({
  depth,
  kind,
  label,
  initial = "",
  onSave,
  onCancel,
}: {
  depth: number;
  kind: WorkspaceEntry["kind"];
  label: string;
  initial?: string;
  onSave: (name: string) => void;
  onCancel: () => void;
}) {
  const [name, setName] = useState(initial);
  const style = { "--workspace-depth": depth } as CSSProperties;

  // Not a `<form>`, and the two keys are read here rather than left to implicit
  // submission. A form's Enter behaviour depends on it having a submit button
  // this row deliberately does not have, and a stray submit inside a webview is
  // the same hazard that keeps every popover in this window out of the
  // composer's form.
  return (
    <div className="workspace-naming" style={style}>
      <span className="workspace-indent" />
      {kind === "directory" ? <FolderIcon size={14} /> : <FileIcon size={14} />}
      <input
        value={name}
        onChange={(event) => setName(event.target.value)}
        aria-label={label}
        placeholder="name"
        autoFocus
        onBlur={onCancel}
        onKeyDown={(event) => {
          if (event.key === "Enter") {
            event.preventDefault();
            const next = name.trim();
            if (next) onSave(next);
            return;
          }
          if (event.key !== "Escape") return;
          // Ahead of the pane's own Escape, which would close the pane out from
          // under a field that was the only thing meant to go.
          event.stopPropagation();
          onCancel();
        }}
      />
    </div>
  );
}
