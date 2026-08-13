import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@ipc";
import { renderAsync } from "docx-preview";

import { useSession } from "./session";
import { basename } from "./show";

type Status =
  | { kind: "loading"; detail: string }
  | { kind: "ready" }
  | { kind: "failed"; detail: string };

export function DocumentView({ path }: { path: string }) {
  const session = useSession();
  const body = useRef<HTMLDivElement>(null);
  const style = useRef<HTMLStyleElement>(null);
  const [status, setStatus] = useState<Status>({ kind: "loading", detail: "preparing document" });

  useEffect(() => {
    let live = true;
    setStatus({ kind: "loading", detail: "loading document" });
    if (body.current) body.current.replaceChildren();

    invoke<string>("serve_url", { session, path })
      .then((url) => {
        if (!live) return;
        setStatus({ kind: "loading", detail: "rendering document" });
        return fetch(url);
      })
      .then((response) => {
        if (!live || !response) return;
        if (!response.ok) throw new Error(`fetch failed: ${response.status}`);
        return response.arrayBuffer();
      })
      .then((buffer) => {
        if (!live || !buffer || !body.current) return;
        return renderAsync(buffer, body.current, style.current ?? undefined, {
          className: "docx-content",
          inWrapper: true,
          ignoreWidth: false,
          ignoreHeight: true,
          ignoreFonts: false,
          breakPages: true,
          renderHeaders: true,
          renderFooters: true,
          renderFootnotes: true,
          renderEndnotes: true,
        });
      })
      .then(() => {
        if (live) setStatus({ kind: "ready" });
      })
      .catch((error) => {
        if (live) setStatus({ kind: "failed", detail: String(error) });
      });

    return () => {
      live = false;
    };
  }, [path, session]);

  const openExternal = useCallback(() => {
    invoke<void>("workspace_open_external", { session, path, opener: "system" }).catch(() => {});
  }, [session, path]);

  return (
    <div className="document-view">
      <div className="document-bar">
        <p className="document-title" title={path}>{basename(path)}</p>
        <div className="document-controls">
          <button type="button" className="chip" onClick={openExternal}>
            Open external
          </button>
        </div>
      </div>

      {status.kind === "failed" && <p className="inspect-empty">{status.detail}</p>}
      {status.kind === "loading" && <p className="inspect-empty">{status.detail}…</p>}

      <style ref={style} />
      <div className="document-body" ref={body} />
    </div>
  );
}
