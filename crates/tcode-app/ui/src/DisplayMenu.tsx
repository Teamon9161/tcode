import { useCallback, useEffect, useRef, useState, type ReactNode } from "react";
import { createPortal } from "react-dom";
import { invoke } from "@ipc";

import { APP_THEMES, loadAppTheme, setAppTheme, type AppTheme } from "./appTheme";
import { CODE_THEMES, loadCodeTheme, setCodeTheme, type CodeTheme } from "./codeTheme";
import type { Display } from "./display";
import { useSeat } from "./seat";
import { SettingsIcon, ChevronRight } from "./components/Icons";

type DesktopSettings = { terminal_shell: string };

/**
 * The one app-level settings surface in the title bar.
 *
 * It deliberately remains a compact popover rather than a preferences window:
 * all of these choices apply to this whole window, are safe to reverse, and are
 * most useful while the effect remains visible. Code palette and transcript
 * switches are presentational. The shell is the only setting that crosses IPC,
 * because it changes what future terminal tabs execute.
 */
export function SettingsPanel({
  display,
  onChange,
}: {
  display: Display;
  onChange: (next: Display) => void;
}) {
  const [open, setOpen] = useState(false);
  const [appTheme, setCurrentAppTheme] = useState<AppTheme>(() => loadAppTheme());
  const [codeTheme, setCurrentCodeTheme] = useState<CodeTheme>(() => loadCodeTheme());
  const [shell, setShell] = useState("");
  const [saving, setSaving] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const [failure, setFailure] = useState<string | null>(null);
  const trigger = useRef<HTMLButtonElement>(null);
  const box = useRef<HTMLDivElement>(null);

  const close = useCallback(() => {
    setOpen(false);
    setNotice(null);
    setFailure(null);
    trigger.current?.focus();
  }, []);
  useSeat({ open, trigger, box, onEscape: close, onOutside: () => setOpen(false) });

  useEffect(() => {
    if (!open) return;
    let current = true;
    setNotice(null);
    setFailure(null);
    invoke<DesktopSettings>("desktop_settings")
      .then((settings) => {
        if (current) setShell(settings.terminal_shell);
      })
      .catch((error) => {
        if (current) setFailure(`Could not read terminal settings: ${String(error)}`);
      });
    return () => {
      current = false;
    };
  }, [open]);

  const applyShell = (next: string) => {
    if (saving) return;
    setSaving(true);
    setFailure(null);
    invoke<DesktopSettings>("set_terminal_shell", { shell: next })
      .then((settings) => {
        setShell(settings.terminal_shell);
        setNotice(
          settings.terminal_shell
            ? "Saved. New terminals will use this shell."
            : "Saved. New terminals will detect a shell automatically.",
        );
      })
      .catch((error) => setFailure(`Could not save terminal shell: ${String(error)}`))
      .finally(() => setSaving(false));
  };

  return (
    <>
      <button
        ref={trigger}
        type="button"
        className="icon-btn"
        aria-expanded={open}
        aria-label="Settings"
        title="Settings"
        onClick={() => setOpen((was) => !was)}
      >
        <SettingsIcon size={15} />
      </button>

      {open &&
        createPortal(
          <div
            className="seated settings-panel"
            ref={box}
            role="dialog"
            aria-label="Settings"
          >
            <section className="settings-section" aria-labelledby="settings-appearance">
              <h2 className="settings-head" id="settings-appearance">
                Appearance
              </h2>
              <p className="settings-note">App theme</p>
              <div className="settings-choices">
                {APP_THEMES.map((theme) => (
                  <button
                    key={theme.id}
                    type="button"
                    className={`settings-choice${appTheme === theme.id ? " is-on" : ""}`}
                    aria-pressed={appTheme === theme.id}
                    onClick={() => {
                      setAppTheme(theme.id);
                      setCurrentAppTheme(theme.id);
                    }}
                  >
                    <span className="chip-tick" aria-hidden="true" />
                    <span className="settings-choice-lines">
                      <span className="settings-choice-label">{theme.label}</span>
                      <span className="settings-choice-detail">{theme.detail}</span>
                    </span>
                  </button>
                ))}
              </div>

            </section>

            <SettingsDisclosure
              label="Code theme"
              summary={CODE_THEMES.find((t) => t.id === codeTheme)?.label ?? "Porcelain"}
            >
              <div className="settings-choices">
                {CODE_THEMES.map((theme) => (
                  <button
                    key={theme.id}
                    type="button"
                    className={`settings-choice${codeTheme === theme.id ? " is-on" : ""}`}
                    aria-pressed={codeTheme === theme.id}
                    onClick={() => {
                      setCodeTheme(theme.id);
                      setCurrentCodeTheme(theme.id);
                    }}
                  >
                    <span className="chip-tick" aria-hidden="true" />
                    <span className="settings-choice-lines">
                      <span className="settings-choice-label">{theme.label}</span>
                      <span className="settings-choice-detail">{theme.detail}</span>
                    </span>
                  </button>
                ))}
              </div>
            </SettingsDisclosure>

            <section className="settings-section" aria-labelledby="settings-conversation">
              <h2 className="settings-head" id="settings-conversation">
                Conversation
              </h2>
              <Switch
                label="Reasoning"
                hint="The model's thinking, as prose between the steps."
                on={display.thinking}
                onToggle={() => onChange({ ...display, thinking: !display.thinking })}
              />
              <Switch
                label="Edit details"
                hint="Show file changes in the conversation by default."
                on={display.editDetails}
                onToggle={() => onChange({ ...display, editDetails: !display.editDetails })}
              />
            </section>

            <SettingsDisclosure
              label="Terminal"
              summary={shell ? "Custom shell" : "Automatic shell"}
            >
              <label className="settings-field-label" htmlFor="terminal-shell">
                Shell command
              </label>
              <input
                id="terminal-shell"
                className="settings-field"
                value={shell}
                placeholder="Automatic shell"
                spellCheck={false}
                autoComplete="off"
                onChange={(event) => setShell(event.target.value)}
                onKeyDown={(event) => {
                  if (event.key !== "Enter") return;
                  event.preventDefault();
                  applyShell(shell);
                }}
              />
              <p className="settings-help">
                Applies to terminals opened after saving. Leave empty to detect automatically.
              </p>
              <div className="settings-actions">
                <button
                  type="button"
                  className="settings-action"
                  disabled={saving}
                  onClick={() => applyShell("")}
                >
                  Use automatic shell
                </button>
                <button
                  type="button"
                  className="settings-save"
                  disabled={saving}
                  onClick={() => applyShell(shell)}
                >
                  {saving ? "Saving…" : "Save shell"}
                </button>
              </div>
              {notice && <p className="settings-notice">{notice}</p>}
              {failure && (
                <p className="settings-failure" role="alert">
                  {failure}
                </p>
              )}
            </SettingsDisclosure>
          </div>,
          document.body,
        )}
    </>
  );
}

/** A low-frequency setting stays out of the first read, yet remains one hover,
 * click, or keyboard focus away. */
function SettingsDisclosure({
  label,
  summary,
  children,
}: {
  label: string;
  summary: string;
  children: ReactNode;
}) {
  const [open, setOpen] = useState(false);
  return (
    <section
      className={`settings-section settings-secondary${open ? " is-open" : ""}`}
      onMouseEnter={() => setOpen(true)}
      onMouseLeave={() => setOpen(false)}
      onFocusCapture={() => setOpen(true)}
      onBlurCapture={(event) => {
        if (!event.currentTarget.contains(event.relatedTarget as Node | null)) setOpen(false);
      }}
    >
      <h2 className="settings-head">
        <button
          type="button"
          className="settings-secondary-trigger"
          aria-expanded={open}
          onClick={() => setOpen((was) => !was)}
        >
          <span>{label}</span>
          <span className="settings-secondary-summary">{summary}</span>
          <ChevronRight size={13} />
        </button>
      </h2>
      <div className="settings-secondary-content">
        <div className="settings-secondary-inner">{children}</div>
      </div>
    </section>
  );
}

/** A concise transcript switch, at list width. */
function Switch({
  label,
  hint,
  on,
  onToggle,
}: {
  label: string;
  hint: string;
  on: boolean;
  onToggle: () => void;
}) {
  return (
    <button
      type="button"
      className={`dmenu-switch${on ? " is-on" : ""}`}
      role="switch"
      aria-checked={on}
      onClick={onToggle}
    >
      <span className="chip-tick" aria-hidden="true" />
      <span className="dmenu-lines">
        <span className="dmenu-label">{label}</span>
        <span className="dmenu-hint">{hint}</span>
      </span>
    </button>
  );
}
