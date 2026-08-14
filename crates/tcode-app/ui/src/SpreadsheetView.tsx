import { useCallback, useEffect, useState } from "react";
import { invoke } from "@ipc";
import { init, Model, IronCalc } from "@ironcalc/workbook";
import "@ironcalc/workbook/style.css";

import { useSession } from "./session";
import { basename, extensionOf } from "./show";

type Status =
  | { kind: "loading"; detail: string }
  | { kind: "ready" }
  | { kind: "failed"; detail: string };

let wasmReady: Promise<unknown> | null = null;
function ensureWasm(): Promise<unknown> {
  if (!wasmReady) wasmReady = init();
  return wasmReady;
}

function useResizeInvalidation(element: HTMLElement | null): void {
  const [, setRevision] = useState(0);

  useEffect(() => {
    if (!element) return;

    let frame = 0;
    let lastSize = "";
    const schedule = (width: number, height: number) => {
      const nextSize = `${Math.round(width)}x${Math.round(height)}`;
      if (nextSize === lastSize) return;
      lastSize = nextSize;
      if (frame) cancelAnimationFrame(frame);
      frame = requestAnimationFrame(() => {
        frame = 0;
        setRevision((at) => at + 1);
      });
    };
    const measure = () => {
      const box = element.getBoundingClientRect();
      schedule(box.width, box.height);
    };

    if (typeof ResizeObserver === "undefined") {
      measure();
      window.addEventListener("resize", measure);
      return () => {
        window.removeEventListener("resize", measure);
        if (frame) cancelAnimationFrame(frame);
      };
    }

    const observer = new ResizeObserver((entries) => {
      const box = entries[0]?.contentRect;
      if (box) schedule(box.width, box.height);
      else measure();
    });
    observer.observe(element);
    measure();

    return () => {
      observer.disconnect();
      if (frame) cancelAnimationFrame(frame);
    };
  }, [element]);
}

const THEME: Record<string, string> = {
  "--typography-font-family": "var(--font-ui)",
  "--typography-font-size": "13px",
  "--palette-common-black": "var(--ink)",
  "--palette-common-white": "var(--bg)",
  "--palette-primary-main": "var(--brand)",
  "--palette-primary-dark": "var(--brand)",
  "--palette-primary-light": "var(--brand-wash)",
  "--palette-primary-contrast-text": "var(--bg)",
  "--palette-grey-50": "var(--surface-1)",
  "--palette-grey-100": "var(--surface-2)",
  "--palette-grey-200": "var(--line)",
  "--palette-grey-300": "var(--border)",
  "--palette-grey-400": "var(--muted)",
  "--palette-grey-500": "var(--muted)",
  "--palette-grey-600": "var(--subtle)",
  "--palette-grey-700": "var(--ink)",
  "--palette-grey-800": "var(--ink)",
  "--palette-grey-900": "var(--ink)",
  "--palette-sheet-header-background": "var(--surface-1)",
  "--palette-sheet-header-text-color": "var(--subtle)",
  "--palette-sheet-header-border-color": "var(--line)",
  "--palette-sheet-header-selected-background": "var(--brand-wash)",
  "--palette-sheet-header-selected-color": "var(--brand)",
  "--palette-sheet-grid-color": "var(--line)",
  "--palette-sheet-default-text-color": "var(--ink)",
  "--palette-sheet-outline-color": "var(--brand)",
  "--palette-sheet-default-cell-font-family": "var(--font-mono)",
};

export function SpreadsheetView({ path }: { path: string }) {
  const session = useSession();
  const [model, setModel] = useState<Model | null>(null);
  const [dirty, setDirty] = useState(false);
  const [saving, setSaving] = useState(false);
  const [status, setStatus] = useState<Status>({ kind: "loading", detail: "preparing spreadsheet" });
  const extension = extensionOf(path);
  const convertedPreview = extension === "xls" || extension === "csv" || extension === "tsv";
  const [containerElement, setContainerElement] = useState<HTMLDivElement | null>(null);
  useResizeInvalidation(containerElement);

  useEffect(() => {
    let live = true;
    setModel(null);
    setDirty(false);
    setStatus({ kind: "loading", detail: "loading WASM engine" });

    ensureWasm()
      .then(() => {
        if (!live) return;
        setStatus({ kind: "loading", detail: "loading spreadsheet" });
        return invoke<string>("spreadsheet_load", { session, path });
      })
      .then((base64) => {
        if (!live || !base64) return;
        const binary = atob(base64);
        const bytes = new Uint8Array(binary.length);
        for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
        const loaded = Model.from_bytes(bytes, "en");
        if (!live) {
          loaded.free();
          return;
        }
        setModel(loaded);
        setStatus({ kind: "ready" });
      })
      .catch((error) => {
        if (live) setStatus({ kind: "failed", detail: String(error) });
      });

    return () => {
      live = false;
    };
  }, [path, session]);

  useEffect(() => {
    return () => {
      model?.free();
    };
  }, [model]);

  const editable = !convertedPreview;

  const save = useCallback(() => {
    if (!model || saving || !editable) return;
    setSaving(true);
    const bytes = model.toBytes();
    const binary = Array.from(bytes, (b) => String.fromCharCode(b)).join("");
    const base64 = btoa(binary);
    invoke<void>("spreadsheet_save", { session, path, data: base64 })
      .then(() => setDirty(false))
      .catch((error) => setStatus({ kind: "failed", detail: `save failed: ${error}` }))
      .finally(() => setSaving(false));
  }, [model, saving, editable, session, path]);

  const openExternal = useCallback(() => {
    invoke<void>("workspace_open_external", { session, path, opener: "system" }).catch(() => {});
  }, [session, path]);

  return (
    <div className="spreadsheet-view">
      <div className="spreadsheet-bar">
        <p className="spreadsheet-title" title={path}>{basename(path)}</p>
        <div className="spreadsheet-controls">
          {dirty && editable && (
            <button type="button" className="chip" onClick={save} disabled={saving}>
              {saving ? "Saving…" : "Save"}
            </button>
          )}
          <button type="button" className="chip" onClick={openExternal}>
            Open external
          </button>
        </div>
      </div>

      {convertedPreview && status.kind === "ready" && (
        <p className="inspect-empty">This file is converted for spreadsheet preview and is read-only here.</p>
      )}
      {status.kind === "failed" && <p className="inspect-empty">{status.detail}</p>}
      {status.kind === "loading" && <p className="inspect-empty">{status.detail}…</p>}

      {model && status.kind === "ready" && (
        <div className="spreadsheet-body" ref={setContainerElement}>
          {containerElement && (
            <IronCalc
              model={model}
              canEdit={editable}
              themeVariables={THEME}
              rootContainer={containerElement}
            />
          )}
        </div>
      )}
    </div>
  );
}
