import { useEffect, useState } from "react";

import type { ApprovalRequest, Decision, SessionInfo, Status } from "./types";
import type { Block } from "./blocks";
import type { TouchedFile } from "./files";
import { useInspector } from "./inspect";
import { Mark } from "./components/Mark";
import { Path } from "./components/Path";
import { StatusDot } from "./components/Status";
import { BackIcon, CloseIcon, PanelIcon, PlusIcon } from "./components/Icons";
import { Transcript } from "./Transcript";
import { Composer } from "./Composer";
import { Inspector } from "./Inspector";
import { ApprovalDialog } from "./ApprovalDialog";

/** Wide enough for a split diff without crowding the conversation. */
const DEFAULT_WIDTH = 380;
const MIN_WIDTH = 280;
const MAX_WIDTH = 900;

/**
 * One conversation, with the other open ones a click away on the left and one
 * inspectable thing on the right.
 *
 * The rail is always present rather than a drawer: knowing that another session
 * needs you is the reason this app exists, and information you have to open a
 * menu to see is information you will miss.
 *
 * The right region is a single slot (see `inspect.ts`). Its root is the file
 * index, and anything opened from the conversation — a diff, a snapshot, a
 * sub-agent's own turn — pushes onto a stack whose back button returns there.
 */
export function Workspace({
  session,
  sessions,
  blocks,
  files,
  running,
  approval,
  statusOf,
  draft,
  onDraft,
  onSend,
  onInterrupt,
  onAnswer,
  onSelect,
  onClose,
  onHome,
}: {
  session: SessionInfo;
  sessions: SessionInfo[];
  blocks: Block[];
  files: TouchedFile[];
  running: boolean;
  approval: ApprovalRequest | null;
  statusOf: (id: string) => Status;
  draft: string;
  onDraft: (value: string) => void;
  onSend: () => void;
  onInterrupt: () => void;
  onAnswer: (decision: Decision, comment: string) => void;
  onSelect: (id: string) => void;
  onClose: (id: string) => void;
  onHome: () => void;
}) {
  const nav = useInspector();
  const [width, setWidth] = useState(DEFAULT_WIDTH);
  const open = nav.value !== null;
  const { open: openPanel, close: closePanel } = nav;

  // The panel opens itself the first time a turn touches a file, then stays
  // wherever the user last put it — useful without being insistent.
  const touched = files.length > 0;
  useEffect(() => {
    if (touched) openPanel({ kind: "files" });
  }, [touched, openPanel]);

  // Esc closes the panel before it reaches the composer's interrupt: a panel
  // that ignores Esc is the one thing every panel is expected to handle.
  useEffect(() => {
    if (!open) return;
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.stopPropagation();
        closePanel();
      }
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [open, closePanel]);

  return (
    <div className={`workspace${open ? " has-panel" : ""}`}>
      <nav className="rail">
        <button className="rail-home" onClick={onHome} title="All projects">
          <Mark size={17} state={statusOf(session.id)} />
        </button>

        <ul className="rail-list">
          {sessions.map((entry) => (
            <li key={entry.id}>
              <button
                className={`rail-item${entry.id === session.id ? " is-current" : ""}`}
                onClick={() => onSelect(entry.id)}
                title={entry.cwd}
              >
                <StatusDot status={statusOf(entry.id)} />
                <span className="rail-name">{entry.name}</span>
              </button>
              <button
                className="rail-close"
                onClick={() => onClose(entry.id)}
                aria-label={`Close ${entry.name}`}
              >
                <CloseIcon size={13} />
              </button>
            </li>
          ))}
        </ul>

        <button className="rail-add" onClick={onHome}>
          <PlusIcon size={14} />
          <span className="rail-name">Open folder</span>
        </button>
      </nav>

      <div className="stage">
        <header className="topbar">
          <button className="icon-btn" onClick={onHome} aria-label="Back to all projects">
            <BackIcon size={15} />
          </button>
          <span className="stage-title">{session.name}</span>
          <Path className="stage-path" path={session.cwd} home={session.home} keep={4} />
          <button
            className={`icon-btn${open ? " is-on" : ""}`}
            onClick={() => (open ? closePanel() : openPanel({ kind: "files" }))}
            aria-pressed={open}
            aria-label={open ? "Hide the panel" : "Show the panel"}
          >
            <PanelIcon size={15} />
          </button>
        </header>

        <Transcript blocks={blocks} running={running} onOpen={openPanel} />

        <Composer
          value={draft}
          running={running}
          disabled={false}
          onChange={onDraft}
          onSubmit={onSend}
          onInterrupt={onInterrupt}
        />
      </div>

      <Inspector
        nav={nav}
        blocks={blocks}
        files={files}
        cwd={session.cwd}
        width={width}
        onWidth={(next) => setWidth(Math.min(Math.max(next, MIN_WIDTH), MAX_WIDTH))}
      />

      {approval && <ApprovalDialog request={approval} onAnswer={onAnswer} />}
    </div>
  );
}
