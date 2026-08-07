import { invoke, listen } from "@ipc";
import { Terminal, type ITheme } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";

import {
  addTab,
  closeTab,
  endTab,
  NO_TABS,
  renameTab,
  selectTab,
  stepTab,
  type Tabs,
} from "./terminal";
import { TERMINAL_EXIT, TERMINAL_OUTPUT, type TerminalExit, type TerminalOutput } from "./types";
import { appOwnedInTerminal } from "./keys";
import { asColor, tokenValue } from "./color";

/**
 * The live terminals: the xterm instances, the PTY bridge, and the tab list
 * they belong to.
 *
 * ## Why this is a module and not a component's state
 *
 * `Mod+J` closes the pane, and closing the pane unmounts the component. If the
 * emulators lived in React state, that keystroke would take the scrollback of
 * every tab with it — while the shells kept running, because they are child
 * processes and know nothing about a pane. What came back on the next `Mod+J`
 * would be a blank screen in front of a live `npm run dev`, which is worse than
 * either closing it or keeping it.
 *
 * So the terminals live here, outside React entirely, and each one keeps a host
 * element that is *moved* between the DOM and nowhere rather than rebuilt. The
 * pane is a window onto this, exactly as `WebPane` is a window onto a webview
 * the backend owns. `layout.ts` says the same thing from its side: the tiling
 * tree holds `{kind: "terminal"}` and no tab list.
 *
 * React sees it through `useSyncExternalStore`, which also means a tab opening
 * or a title changing re-renders the terminal pane and nothing else — the
 * transcripts beside it are not participants in this.
 *
 * ## The one race worth knowing about
 *
 * The backend starts reading the moment the PTY exists, which is *before*
 * `terminal_open` has returned an id to this side. A shell prints its prompt
 * within a millisecond or two, so the first chunk routinely arrives while the
 * `invoke` promise is still pending and there is nothing to route it to yet.
 * Early output is therefore held in `pending` and flushed when the id lands.
 * Both halves run on the same thread, so this is exact rather than hopeful.
 */

/** One terminal: the emulator, the fitter, and the element it draws into. */
type Live = { term: Terminal; fit: FitAddon; host: HTMLDivElement };

type Snapshot = {
  tabs: Tabs;
  /** Anything that failed on the bridge, shown in the pane rather than left as
   *  a rejected promise nobody sees (AGENTS.md rule 7). */
  failure: string | null;
};

const live = new Map<string, Live>();
/** Output that arrived before its terminal was registered. See the note above. */
const pending = new Map<string, Uint8Array[]>();
const watchers = new Set<() => void>();

let state: Snapshot = { tabs: NO_TABS, failure: null };
let mount: HTMLElement | null = null;
let listening = false;

// ------------------------------------------------------------------ the store

export function subscribe(watcher: () => void): () => void {
  watchers.add(watcher);
  return () => {
    watchers.delete(watcher);
  };
}

/** Referentially stable between changes, which `useSyncExternalStore` requires
 *  — a fresh object every call is an infinite render. */
export function snapshot(): Snapshot {
  return state;
}

function publish(next: Partial<Snapshot>) {
  state = { ...state, ...next };
  for (const watcher of watchers) watcher();
}

function failed(what: string, error: unknown) {
  publish({ failure: `${what}: ${String(error)}` });
}

// ------------------------------------------------------------------ the panel

/**
 * The pane has appeared. Every existing terminal's host goes back into the DOM,
 * with its scrollback exactly as it was left.
 *
 * The re-fit matters: the pane it is coming back into is rarely the size it
 * left, and a terminal whose emulator and PTY disagree about the width wraps
 * every line in the wrong place until something else resizes it.
 */
export function attach(element: HTMLElement) {
  mount = element;
  for (const { host } of live.values()) element.appendChild(host);
  show();
  fitCurrent();
  focusCurrent();
}

/** The pane went away. The hosts come out of the document and stay alive —
 *  this is the whole reason the module exists. */
export function detach(element: HTMLElement) {
  // Only if this is still the pane we are mounted in: React can commit the new
  // pane's effects before the old one's cleanup in a re-parenting render, and
  // detaching then would empty the pane that just arrived.
  if (mount !== element) return;
  for (const { host } of live.values()) host.remove();
  mount = null;
}

/** Only the current tab is displayed; the rest keep their state off-screen. */
function show() {
  for (const [id, { host }] of live) {
    host.style.display = id === state.tabs.current ? "block" : "none";
  }
}

export function focusCurrent() {
  live.get(state.tabs.current)?.term.focus();
}

/**
 * Re-measures the current terminal and tells the PTY.
 *
 * Only the current one: the others are `display: none`, and a hidden element
 * measures as zero — fitting one would tell its shell it is one column wide.
 * They are re-fitted when they are selected, which is the first moment their
 * size is knowable.
 */
export function fitCurrent() {
  const found = live.get(state.tabs.current);
  if (!found || !mount) return;
  try {
    found.fit.fit();
  } catch {
    // A pane mid-collapse measures as nothing and the fitter says so. The next
    // resize settles it; there is nothing to report to anybody here.
  }
}

// ------------------------------------------------------------------- the tabs

export function select(id: string) {
  publish({ tabs: selectTab(state.tabs, id) });
  show();
  fitCurrent();
  focusCurrent();
}

export function step(delta: number) {
  publish({ tabs: stepTab(state.tabs, delta) });
  show();
  fitCurrent();
  focusCurrent();
}

/**
 * Starts a shell in `cwd` and gives it a tab.
 *
 * The order is forced by what has to be true before what: the host must be in
 * the document before xterm can measure a character, the character must be
 * measured before there are rows and columns, and the rows and columns have to
 * be known before the PTY is created — a shell started at a guessed size draws
 * its first prompt at that size and only learns better on the next resize.
 */
export async function open(cwd: string) {
  if (!mount) return;
  listenOnce();

  const host = document.createElement("div");
  host.className = "term-host";
  // Visible from the start, because a `display: none` element has no metrics
  // and `fit` would size the shell to nothing. It is about to be the current
  // tab anyway — `open` always takes focus.
  for (const other of live.values()) other.host.style.display = "none";
  mount.appendChild(host);

  const term = new Terminal({
    theme: readTheme(),
    fontFamily: tokenValue("--font-mono") || "monospace",
    fontSize: Number.parseInt(tokenValue("--text-sm"), 10) || 13,
    lineHeight: 1.35,
    cursorBlink: true,
    // Deep enough that a build's output is still there when it finishes, and
    // bounded so a runaway process cannot grow the window's memory forever.
    scrollback: 10_000,
    // The shell decides what a word is; this is only how a double-click reads
    // one back. Paths are the thing most often double-clicked in here.
    allowProposedApi: false,
  });
  const fit = new FitAddon();
  term.loadAddon(fit);
  // Returning false means "not yours" — xterm neither sends the key nor
  // suppresses it, so it reaches the window's handlers as an ordinary event.
  // Without this, `Mod+J` would toggle the pane *and* send a line feed to the
  // shell, which for anything with a prompt means running whatever was typed.
  term.attachCustomKeyEventHandler((event) => !appOwnedInTerminal(event));
  term.open(host);
  try {
    fit.fit();
  } catch {
    // Sized in `attach`/`fitCurrent` once the pane has a size.
  }

  let id: string;
  try {
    id = await invoke<string>("terminal_open", {
      cwd,
      cols: term.cols,
      rows: term.rows,
    });
  } catch (error) {
    term.dispose();
    host.remove();
    show();
    failed("cannot start a terminal", error);
    return;
  }

  live.set(id, { term, fit, host });

  // Keystrokes. `onData` is text the terminal decided to send; `onBinary` is
  // the same channel for sequences that are not text (mouse reports), where
  // each unit is one byte and encoding it as UTF-8 would corrupt it.
  term.onData((data) => send(id, new TextEncoder().encode(data)));
  term.onBinary((data) => {
    const bytes = new Uint8Array(data.length);
    for (let at = 0; at < data.length; at += 1) bytes[at] = data.charCodeAt(at) & 0xff;
    send(id, bytes);
  });
  // The emulator resized itself, so the program has to hear about it — this is
  // where `SIGWINCH` comes from. Driven by the emulator rather than by the
  // fitter's caller, so a font change or a reflow reaches the shell too.
  term.onResize(({ cols, rows }) => {
    invoke("terminal_resize", { id, cols, rows }).catch((error) =>
      failed("cannot resize the terminal", error),
    );
  });
  term.onTitleChange((title) => publish({ tabs: renameTab(state.tabs, id, title) }));

  const held = pending.get(id);
  if (held) {
    pending.delete(id);
    for (const chunk of held) term.write(chunk);
  }

  publish({ tabs: addTab(state.tabs, { id, title: "", cwd, exit: null }), failure: null });
  show();
  fitCurrent();
  focusCurrent();
}

/** Closes a tab and the program in it. */
export function close(id: string) {
  const found = live.get(id);
  if (found) {
    found.term.dispose();
    found.host.remove();
    live.delete(id);
  }
  pending.delete(id);
  publish({ tabs: closeTab(state.tabs, id) });
  show();
  fitCurrent();
  focusCurrent();
  invoke("terminal_close", { id }).catch((error) => failed("cannot close the terminal", error));
}

/** How many tabs there are, for the pane deciding whether it has anything to
 *  show yet. */
export function isEmpty(): boolean {
  return state.tabs.list.length === 0;
}

function send(id: string, bytes: Uint8Array) {
  invoke("terminal_write", { id, data: base64(bytes) }).catch((error) =>
    failed("cannot write to the terminal", error),
  );
}

// ------------------------------------------------------------------ the bridge

/** Subscribed once for the whole app, not once per pane: `Mod+J` would
 *  otherwise add a listener every time the pane came back, and each chunk would
 *  be written to the terminal as many times as the pane had been opened. */
function listenOnce() {
  if (listening) return;
  listening = true;

  listen<TerminalOutput>(TERMINAL_OUTPUT, (event) => {
    const bytes = bytesOf(event.payload.data);
    const found = live.get(event.payload.id);
    if (found) found.term.write(bytes);
    // Not registered yet — the shell answered faster than `terminal_open`
    // returned. Hold it; `open` flushes.
    else pending.set(event.payload.id, [...(pending.get(event.payload.id) ?? []), bytes]);
  }).catch((error) => failed("cannot follow the terminal", error));

  listen<TerminalExit>(TERMINAL_EXIT, (event) => {
    // The tab stays and stays readable; only its status changes. A tab that
    // vanished when a command failed would take the error message with it.
    publish({ tabs: endTab(state.tabs, event.payload.id, event.payload.code) });
  }).catch((error) => failed("cannot follow the terminal", error));
}

function bytesOf(data: string): Uint8Array {
  const binary = atob(data);
  const bytes = new Uint8Array(binary.length);
  for (let at = 0; at < binary.length; at += 1) bytes[at] = binary.charCodeAt(at);
  return bytes;
}

function base64(bytes: Uint8Array): string {
  let binary = "";
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary);
}

// ------------------------------------------------------------------- the theme

/**
 * The app's tokens, as the colours xterm wants.
 *
 * This is the whole of "the terminal follows the theme". xterm cannot read a
 * CSS variable — it paints to a canvas — so the values have to be resolved on
 * this side, which is also why `base.css` names all sixteen ANSI slots. Read
 * once per terminal: the theme is a compile-time import (`main.tsx`), so there
 * is no runtime switch to follow.
 */
function readTheme(): ITheme {
  return {
    background: color("--term-bg"),
    foreground: color("--term-fg"),
    cursor: color("--term-cursor"),
    cursorAccent: color("--term-cursor-text"),
    selectionBackground: color("--term-selection"),
    black: color("--term-ansi-black"),
    red: color("--term-ansi-red"),
    green: color("--term-ansi-green"),
    yellow: color("--term-ansi-yellow"),
    blue: color("--term-ansi-blue"),
    magenta: color("--term-ansi-magenta"),
    cyan: color("--term-ansi-cyan"),
    white: color("--term-ansi-white"),
    brightBlack: color("--term-ansi-bright-black"),
    brightRed: color("--term-ansi-bright-red"),
    brightGreen: color("--term-ansi-bright-green"),
    brightYellow: color("--term-ansi-bright-yellow"),
    brightBlue: color("--term-ansi-bright-blue"),
    brightMagenta: color("--term-ansi-bright-magenta"),
    brightCyan: color("--term-ansi-bright-cyan"),
    brightWhite: color("--term-ansi-bright-white"),
  };
}


/** One `--term-*` token as something xterm can paint with, or nothing — a
 *  token that resolves to nothing is left to xterm's own default rather than
 *  turned into black, because black on a paper background is a legible mistake
 *  and `#000` on `#000` is an invisible one. */
function color(token: string): string | undefined {
  const value = tokenValue(token);
  if (!value) return undefined;
  return asColor(value) ?? value;
}
