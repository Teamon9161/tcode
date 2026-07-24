import { useEffect, useRef, useState } from "react";
import type { ApprovalRequest, Decision } from "./types";

/**
 * The consent surface. It shows the exact call — tool, target and raw input —
 * because approving something you were not shown is not consent.
 */
export function ApprovalDialog({
  request,
  onAnswer,
}: {
  request: ApprovalRequest;
  onAnswer: (decision: Decision, comment: string) => void;
}) {
  const [comment, setComment] = useState("");
  const deny = useRef<HTMLButtonElement>(null);

  // The denying button takes focus: a stray Enter must not approve anything.
  useEffect(() => {
    deny.current?.focus();
  }, [request.id]);

  return (
    <div className="scrim">
      <div className="dialog" role="dialog" aria-modal="true">
        <h2>
          {request.tool}
          <span className="descriptor">{request.descriptor}</span>
        </h2>
        <p className="summary">{request.summary}</p>
        <pre className="input">{JSON.stringify(request.input, null, 2)}</pre>
        <input
          value={comment}
          placeholder="optional note — guidance for a yes, a reason for a no"
          onChange={(event) => setComment(event.target.value)}
        />
        <div className="actions">
          <button ref={deny} onClick={() => onAnswer("no", comment)}>
            no
          </button>
          <button onClick={() => onAnswer("yes", comment)}>yes, once</button>
          <button onClick={() => onAnswer("yes-session", comment)}>
            yes, this session
          </button>
          {request.allows_project && (
            <button onClick={() => onAnswer("yes-project", comment)}>
              yes, always in this project
            </button>
          )}
        </div>
      </div>
    </div>
  );
}
