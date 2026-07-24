import { useEffect, useState } from "react";

import type { ApprovalRequest, Decision, SessionInfo, Status } from "./types";
import type { Block } from "./transcript";
import type { TouchedFile } from "./files";
import { Mark } from "./components/Mark";
import { Path } from "./components/Path";
import { StatusDot } from "./components/Status";
import { BackIcon, CloseIcon, PanelIcon, PlusIcon } from "./components/Icons";
import { Transcript } from "./Transcript";
import { Composer } from "./Composer";
import { FilePanel } from "./FilePanel";
import { ApprovalDialog } from "./ApprovalDialog";

/**
 * One conversation, with the other open ones a click away on the left and the
 * files it has touched on the right.
 *
 * The rail is always present rather than a drawer: knowing that another session
 * needs you is the reason this app exists, and information you have to open a
 * menu to see is information you will miss.
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
  const [panelOpen, setPanelOpen] = useState(false);
  const [picked, setPicked] = useState<string | null>(null);

  // The panel opens itself the first time a turn touches a file, then stays
  // wherever the user last put it — useful without being insistent.
  const touched = files.length;
  useEffect(() => {
    if (touched > 0) setPanelOpen(true);
  }, [touched > 0]); // eslint-disable-line react-hooks/exhaustive-deps

  return (
    <div className={`workspace${panelOpen ? " has-panel" : ""}`}>
      <nav className="rail">
        <button className="rail-home" onClick={onHome} title="All projects">
          <Mark size={17} state={statusOf(session.id)} />
        </button>

        <ul className="rail-list">
          {sessions.map((open) => (
            <li key={open.id}>
              <button
                className={`rail-item${open.id === session.id ? " is-current" : ""}`}
                onClick={() => onSelect(open.id)}
                title={open.cwd}
              >
                <StatusDot status={statusOf(open.id)} />
                <span className="rail-name">{open.name}</span>
              </button>
              <button
                className="rail-close"
                onClick={() => onClose(open.id)}
                aria-label={`Close ${open.name}`}
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
            className={`icon-btn${panelOpen ? " is-on" : ""}`}
            onClick={() => setPanelOpen((was) => !was)}
            aria-pressed={panelOpen}
            aria-label={panelOpen ? "Hide the file panel" : "Show the file panel"}
          >
            <PanelIcon size={15} />
          </button>
        </header>

        <Transcript
          blocks={blocks}
          running={running}
          onPickFile={(path) => {
            const match = files.find((file) => file.path.endsWith(path)) ?? null;
            setPicked(match?.path ?? null);
            setPanelOpen(true);
          }}
        />

        <Composer
          value={draft}
          running={running}
          disabled={false}
          onChange={onDraft}
          onSubmit={onSend}
          onInterrupt={onInterrupt}
        />
      </div>

      {panelOpen && (
        <FilePanel
          files={files}
          blocks={blocks}
          cwd={session.cwd}
          selected={picked}
          onSelect={setPicked}
          onClose={() => setPanelOpen(false)}
        />
      )}

      {approval && <ApprovalDialog request={approval} onAnswer={onAnswer} />}
    </div>
  );
}
