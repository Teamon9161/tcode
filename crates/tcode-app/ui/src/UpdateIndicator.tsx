import { useEffect, useState } from "react";
import { invoke, listen } from "@ipc";

const UPDATE_STATE = "tcode://update-state";

type UpdatePayload =
  | { state: "downloading"; version: string | null; percent: number }
  | { state: "ready"; version: string | null };

export function UpdateIndicator() {
  const [update, setUpdate] = useState<UpdatePayload | null>(null);

  useEffect(() => {
    const pending = listen<UpdatePayload | null>(UPDATE_STATE, ({ payload }) => {
      setUpdate(payload);
    });
    return () => {
      pending.then((unlisten) => unlisten()).catch(() => {});
    };
  }, []);

  if (!update) return null;

  if (update.state === "downloading") {
    return <DownloadRing percent={update.percent} version={update.version} />;
  }

  return (
    <button
      type="button"
      className="update-pill"
      onClick={() => invoke("update_restart").catch(console.warn)}
      title={`tcode${update.version ? ` ${update.version}` : ""} is ready — restart to install`}
    >
      Restart to update
    </button>
  );
}

const R = 5;
const C = 2 * Math.PI * R;

function DownloadRing({ percent, version }: { percent: number; version: string | null }) {
  const offset = C - (percent / 100) * C;

  return (
    <span
      className="update-ring"
      title={`Downloading${version ? ` ${version}` : ""}${percent > 0 ? ` — ${percent}%` : ""}`}
    >
      <svg width="14" height="14" viewBox="0 0 14 14" aria-hidden="true">
        <circle cx="7" cy="7" r={R} className="update-track" />
        <circle
          cx="7"
          cy="7"
          r={R}
          className="update-fill"
          strokeDasharray={C}
          strokeDashoffset={offset}
          transform="rotate(-90 7 7)"
        />
      </svg>
    </span>
  );
}
