//! The window's terminals: real PTYs, one per tab.
//!
//! ## Whose terminal this is
//!
//! **It is the user's, and the model cannot reach it.** No tool writes here;
//! there is no command that lets a turn type into a tab. That is deliberate and
//! it is the whole security posture of this file: the agent's way to run a
//! command is the `shell` tool, which goes through the approval panel, and a
//! terminal the model could drive would be a second way in with no panel on it.
//! Anything added here that takes bytes from somewhere other than a keystroke
//! in the terminal pane breaks that, however convenient it looks.
//!
//! ## Rule 2, honoured rather than excepted
//!
//! `browser.rs` had to take an exception — a child webview *is* a window's
//! child, so there is no headless version of it. A PTY is an ordinary child
//! process, so everything here is written against [`Emit`] and `tests/terminal.rs`
//! drives a real shell with a collector and no window at all.
//!
//! ## Bytes, and why they are never a `String` on this side
//!
//! A PTY hands back whatever the program wrote, and a read boundary lands
//! wherever the kernel put it — routinely in the middle of a UTF-8 sequence,
//! and for something like `less` or a progress bar, in the middle of an escape
//! sequence too. `from_utf8_lossy` here would replace the split half with `�`
//! and the character would be destroyed rather than delayed. So chunks cross
//! the bridge base64-encoded and are fed to xterm as bytes, which is the layer
//! that already knows how to hold half a code point until the rest arrives.
//!
//! ## Why a coalescing pump
//!
//! `yes`, a `cargo build`, or anything with a progress bar produces thousands
//! of reads a second. One IPC event per read locks the webview solid — the
//! failure is not "slow output", it is a window that stops repainting. So the
//! reader thread only ever hands chunks to a pump, and the pump emits at most
//! one event per [`FLUSH_WINDOW`], or sooner when [`MAX_CHUNK`] has piled up.
//! The first byte still leaves within one window, so typing still echoes
//! immediately; it is only floods that get batched, which is exactly the case
//! nobody can read anyway.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use serde_json::json;

use crate::bridge::Emit;

/// A chunk of a terminal's output, base64 in `data`. Named as a constant
/// because `ui/src/types.ts` hard-codes the same string (AGENTS.md rule 5).
pub const TERMINAL_OUTPUT: &str = "tcode://terminal-output";
/// The program in a terminal exited. The tab stays and stays readable — the
/// scrollback is the record of what happened, and closing it is the user's
/// call, not the shell's.
pub const TERMINAL_EXIT: &str = "tcode://terminal-exit";

/// How long the pump waits for more output before sending what it has. Just
/// over one frame at 60Hz: long enough to swallow a flood, short enough that a
/// keystroke's echo is not something anybody can perceive as delayed.
const FLUSH_WINDOW: Duration = Duration::from_millis(16);

/// The most that may pile up before it is sent regardless of the window. A
/// flood at full speed would otherwise grow one event without bound.
const MAX_CHUNK: usize = 64 * 1024;

/// What one `read` asks for. Bigger than a line and smaller than the pipe
/// buffer, so an interactive shell takes one read and a flood takes few.
const READ_CHUNK: usize = 8 * 1024;

/// One live terminal.
///
/// The child process itself is *not* here: it is owned by the pump thread,
/// which is the one place that waits on it. What stays behind is a killer —
/// `wait` blocks for as long as the program runs, so a `close` that had to take
/// the same lock would block for exactly as long.
struct Term {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    killer: Box<dyn ChildKiller + Send + Sync>,
}

/// Every terminal this window holds, by id.
#[derive(Default)]
pub struct Terminals {
    live: Mutex<HashMap<String, Term>>,
}

impl Terminals {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Starts a shell in `cwd` and returns the id its tab will use.
    ///
    /// `emit` rather than an `AppHandle`: this is where rule 2 is paid for, and
    /// it is what lets a test assert on the byte stream of a real shell.
    pub fn open(
        &self,
        emit: Arc<dyn Emit>,
        cwd: &std::path::Path,
        cols: u16,
        rows: u16,
    ) -> Result<String, String> {
        let pair = native_pty_system()
            .openpty(size(cols, rows))
            .map_err(|error| format!("cannot open a terminal: {error}"))?;

        let mut command = CommandBuilder::new(shell());
        command.cwd(cwd);
        // What a program asks the terminal for. xterm.js is a 256-colour,
        // truecolor-capable terminal, and a shell that is not told inherits
        // whatever this process was started with — which for a desktop app
        // launched from a file manager is nothing at all, and `vim` then draws
        // in two colours.
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");

        let child = pair
            .slave
            .spawn_command(command)
            .map_err(|error| format!("cannot start a shell: {error}"))?;
        // The slave end must go now. It is the other side of the connection,
        // and while this process still holds it the reader below never sees
        // EOF — the tab would sit there looking live for a shell that exited.
        drop(pair.slave);

        let killer = child.clone_killer();
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|error| format!("cannot read the terminal: {error}"))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|error| format!("cannot write to the terminal: {error}"))?;

        let id = uuid::Uuid::new_v4().to_string();
        pump(id.clone(), emit, reader, child);

        self.live.lock().expect("terminals lock").insert(
            id.clone(),
            Term {
                master: pair.master,
                writer,
                killer,
            },
        );
        Ok(id)
    }

    /// Keystrokes and pastes, exactly as the terminal produced them.
    ///
    /// Base64 for the same reason output is: what a key produces is bytes (a
    /// pasted `é` is two of them, `Alt` prefixes an ESC), and a round trip
    /// through a lossy string would corrupt the ones that are not text.
    pub fn write(&self, id: &str, data: &str) -> Result<(), String> {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|error| format!("bad terminal input: {error}"))?;
        let mut live = self.live.lock().expect("terminals lock");
        let term = live.get_mut(id).ok_or("that terminal is not open")?;
        term.writer
            .write_all(&bytes)
            .and_then(|()| term.writer.flush())
            .map_err(|error| format!("cannot write to the terminal: {error}"))
    }

    /// The pane was resized, so the program has to be told: `SIGWINCH` is how
    /// anything full-screen learns its own shape.
    pub fn resize(&self, id: &str, cols: u16, rows: u16) -> Result<(), String> {
        let live = self.live.lock().expect("terminals lock");
        let term = live.get(id).ok_or("that terminal is not open")?;
        term.master
            .resize(size(cols, rows))
            .map_err(|error| format!("cannot resize the terminal: {error}"))
    }

    /// Closes one tab: the program is killed and the connection dropped, which
    /// is what gives the pump its EOF and lets it retire.
    pub fn close(&self, id: &str) -> Result<(), String> {
        let Some(mut term) = self.live.lock().expect("terminals lock").remove(id) else {
            return Ok(());
        };
        // Killed first, then disconnected. The other order leaves a program
        // that ignores EOF running with nothing attached to it.
        let _ = term.killer.kill();
        Ok(())
    }

    /// Every terminal, on the app's own exit.
    ///
    /// The window closing is not a frontend event any pane can intercept (the
    /// caption is non-client area), so this is the only place the shells get
    /// shut down — the same reason `main.rs` closes the browser there.
    pub fn close_all(&self) {
        let ids: Vec<String> = self
            .live
            .lock()
            .expect("terminals lock")
            .keys()
            .cloned()
            .collect();
        for id in ids {
            let _ = self.close(&id);
        }
    }

    /// How many terminals are live. For tests and for nothing else.
    pub fn count(&self) -> usize {
        self.live.lock().expect("terminals lock").len()
    }
}

/// Reads the terminal and emits it, coalesced.
///
/// Two threads rather than one, and that is forced: a read on a PTY blocks
/// until there is something to read, so a single thread cannot both wait for
/// output and honour a flush deadline. The reader does nothing but hand chunks
/// over; the pump owns the clock and the byte budget.
fn pump(
    id: String,
    emit: Arc<dyn Emit>,
    mut reader: Box<dyn Read + Send>,
    mut child: Box<dyn portable_pty::Child + Send + Sync>,
) {
    let (tx, rx) = mpsc::channel::<Vec<u8>>();

    std::thread::spawn(move || {
        let mut buffer = vec![0u8; READ_CHUNK];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    if tx.send(buffer[..read].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
        // Dropping `tx` is what tells the pump the program is gone.
    });

    std::thread::spawn(move || {
        // `recv` blocks until there is anything at all, so an idle terminal
        // costs nothing rather than waking up once a window.
        while let Ok(first) = rx.recv() {
            let mut batch = first;
            let deadline = Instant::now() + FLUSH_WINDOW;
            while batch.len() < MAX_CHUNK {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                match rx.recv_timeout(left) {
                    Ok(more) => batch.extend_from_slice(&more),
                    Err(RecvTimeoutError::Timeout) => break,
                    // The program ended mid-batch. Send what it wrote before
                    // retiring — the last line of a command's output is the one
                    // that says how it went.
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
            emit.emit(
                TERMINAL_OUTPUT,
                json!({
                    "id": id,
                    "data": base64::engine::general_purpose::STANDARD.encode(&batch),
                }),
            );
        }

        // `wait` is here rather than anywhere else because this thread is the
        // one with nothing left to do. It cannot deadlock a `close`: that path
        // holds a separate killer (see `Term`).
        let code = child.wait().map(|status| status.exit_code()).unwrap_or(0);
        emit.emit(TERMINAL_EXIT, json!({ "id": id, "code": code }));
    });
}

fn size(cols: u16, rows: u16) -> PtySize {
    PtySize {
        // A zero dimension is a legal CSS rect (a pane mid-collapse) and a
        // nonsense terminal: a shell told it has no rows draws nothing and
        // never recovers, because nothing tells it again until the next resize.
        rows: rows.max(1),
        cols: cols.max(1),
        pixel_width: 0,
        pixel_height: 0,
    }
}

/// The program a new tab runs.
///
/// The user's own shell, because a terminal that is not the shell you
/// configured is a terminal with your aliases, prompt and PATH missing. No
/// `-l` or `-i`: the child is attached to a tty, which is what every shell
/// actually tests to decide it is interactive, and passing the flags as well
/// starts a *login* shell that re-reads profiles the desktop session already
/// applied.
fn shell() -> String {
    #[cfg(windows)]
    {
        std::env::var("COMSPEC").unwrap_or_else(|_| "powershell.exe".into())
    }
    #[cfg(not(windows))]
    {
        std::env::var("SHELL")
            .ok()
            .filter(|shell| !shell.trim().is_empty())
            .unwrap_or_else(|| "/bin/sh".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pane mid-collapse reports zero, and a shell told it has zero rows
    /// never draws again — nothing resizes it back until the *next* layout
    /// change, which may never come.
    #[test]
    fn a_collapsed_pane_never_reaches_the_pty_as_zero() {
        let clamped = size(0, 0);
        assert_eq!((clamped.cols, clamped.rows), (1, 1));
    }

    /// The event names are a contract with a file in another language, so a
    /// typo on either side is a UI that silently receives nothing (AGENTS.md
    /// rule 5). Nothing else checks it: the listener is registered fine, the
    /// backend emits fine, and the terminal simply stays blank.
    #[test]
    fn the_event_names_match_the_frontends() {
        let types = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/ui/src/types.ts"
        ))
        .expect("ui/src/types.ts");

        for name in [TERMINAL_OUTPUT, TERMINAL_EXIT] {
            assert!(
                types.contains(&format!("\"{name}\"")),
                "ui/src/types.ts does not carry `{name}` — the frontend is listening on a \
                 different event and will never receive anything"
            );
        }
    }

    /// A command the webview calls but nobody registered rejects its promise
    /// and does nothing else. That is the failure mode rule 6 is about, and it
    /// is invisible from either side on its own.
    #[test]
    fn every_terminal_command_the_frontend_calls_is_registered() {
        let host = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/ui/src/termHost.ts"
        ))
        .expect("ui/src/termHost.ts");
        let main = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/main.rs"))
            .expect("src/main.rs");

        // Every `invoke("terminal_…")` in the store, whatever it is called.
        let called: Vec<&str> = host
            .match_indices("invoke")
            .filter_map(|(at, _)| {
                let rest = &host[at..];
                let open = rest.find("\"terminal_")? + 1;
                let end = rest[open..].find('"')? + open;
                Some(&rest[open..end])
            })
            .collect();
        assert!(
            !called.is_empty(),
            "no terminal commands found in termHost.ts — this test now pins nothing"
        );

        for command in called {
            assert!(
                main.contains(&format!("commands::{command},")),
                "termHost.ts calls `{command}` but main.rs does not register it — the call will \
                 reject and the terminal will look inert"
            );
        }
    }

    #[test]
    fn the_shell_is_the_users_own() {
        // Whatever this machine is, the fallback is a shell that exists.
        let picked = shell();
        assert!(!picked.trim().is_empty());
    }
}
