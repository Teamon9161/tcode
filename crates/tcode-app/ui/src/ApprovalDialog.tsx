import { useEffect, useRef, useState } from "react";

import type { ApprovalRequest, Decision } from "./types";
import { Diff, isEditShape } from "./components/Diff";

/**
 * The consent surface. It shows the exact call — tool, target and raw input —
 * because approving something you were not shown is not consent.
 *
 * Two properties are load-bearing and must survive any restyling: the denying
 * button holds focus so a stray Enter cannot approve anything, and Escape
 * denies rather than dismissing. A modal that could be closed *without* an
 * answer would leave the turn parked with no way back to it.
 */
export function ApprovalDialog({
  request,
  onAnswer,
}: {
  request: ApprovalRequest;
  onAnswer: (decision: Decision, comment: string) => void;
}) {
  const [comment, setComment] = useState("");
  const [showInput, setShowInput] = useState(false);
  const deny = useRef<HTMLButtonElement>(null);
  const diffable = isEditShape(request.input);

  useEffect(() => {
    setComment("");
    setShowInput(false);
    deny.current?.focus();
  }, [request.id]);

  return (
    <div
      className="scrim"
      onKeyDown={(event) => {
        if (event.key === "Escape") {
          event.stopPropagation();
          onAnswer("no", comment);
        }
      }}
    >
      <div className="dialog" role="dialog" aria-modal="true" aria-labelledby="approval-title">
        <div className="dialog-head">
          <h2 id="approval-title">{request.is_edit ? "Change a file?" : "Run this?"}</h2>
          <span className="dialog-tool">{request.tool}</span>
        </div>

        <p className="dialog-target">{request.descriptor}</p>
        <p className="dialog-summary">{request.summary}</p>

        {diffable && <Diff input={request.input} />}

        <button
          className="link-btn dialog-toggle"
          onClick={() => setShowInput((was) => !was)}
          aria-expanded={showInput}
        >
          {showInput ? "hide the raw call" : diffable ? "show the raw call" : "show the exact call"}
        </button>
        {showInput && (
          <pre className="dialog-input">{JSON.stringify(request.input, null, 2)}</pre>
        )}

        <input
          className="dialog-comment"
          value={comment}
          placeholder="Optional note — guidance for a yes, a reason for a no"
          onChange={(event) => setComment(event.target.value)}
        />

        <div className="dialog-actions">
          <button
            ref={deny}
            className="btn btn-secondary"
            onClick={() => onAnswer("no", comment)}
          >
            No
          </button>
          <div className="dialog-yes">
            {request.allows_project && (
              <button className="btn btn-ghost" onClick={() => onAnswer("yes-project", comment)}>
                Always here
              </button>
            )}
            <button className="btn btn-ghost" onClick={() => onAnswer("yes-session", comment)}>
              This session
            </button>
            <button className="btn btn-primary" onClick={() => onAnswer("yes", comment)}>
              Yes, once
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}
