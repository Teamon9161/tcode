//! The terminal pane's backend, driven with no window at all.
//!
//! This is what rule 2 buys: a real shell, in a real PTY, asserted on through
//! the same `Emit` the webview would receive. `browser.rs` could not be tested
//! this way — a child webview needs a window — and it would have been easy to
//! assume a terminal was the same kind of thing. It is not: a PTY is an
//! ordinary child process, so the whole path is exercised here.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::Value;

use tcode_app::bridge::Emit;
use tcode_app::terminal::{Terminals, TERMINAL_EXIT, TERMINAL_OUTPUT};

/// The keystroke that ends a line differs between a Unix PTY in canonical
/// mode (`\n`) and a Windows console (the Enter key is `\r`).
#[cfg(windows)]
const ENTER: &str = "\r";
#[cfg(not(windows))]
const ENTER: &str = "\n";

/// The test's stand-in for the webview: records what the backend emitted, and
/// answers the shell's cursor-position query the way xterm.js does.
///
/// cmd.exe sends `ESC [ 6 n` (DSR) as its first act on a fresh console and
/// blocks until the terminal replies with `ESC [ <row> ; <col> R`. The real
/// pane answers that through xterm.js; a recorder that never answers leaves
/// the shell sitting at the query, so no keystroke ever reaches it and the
/// echo round trip never happens on Windows.
struct Collector {
    events: Mutex<Vec<(String, Value)>>,
    terminals: Arc<Terminals>,
}

impl Collector {
    fn new(terminals: Arc<Terminals>) -> Self {
        Self {
            events: Mutex::new(Vec::new()),
            terminals,
        }
    }
}

impl Emit for Collector {
    fn emit(&self, event: &str, payload: Value) {
        self.events
            .lock()
            .unwrap()
            .push((event.to_string(), payload.clone()));
        if event == TERMINAL_OUTPUT
            && payload["data"].as_str().is_some_and(|data| {
                base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .is_ok_and(|bytes| bytes.windows(4).any(|w| w == b"\x1b[6n"))
            })
        {
            let Some(id) = payload["id"].as_str() else {
                return;
            };
            // A freshly opened terminal starts at row 1, column 1.
            let _ = self.terminals.write(
                id,
                &base64::engine::general_purpose::STANDARD.encode("\x1b[1;1R"),
            );
        }
    }
}

impl Collector {
    /// Everything one terminal has written so far, decoded and joined.
    ///
    /// Joined rather than asserted chunk by chunk on purpose: where the pump
    /// draws its boundaries is its own business (and depends on how fast the
    /// shell was), so a test that pinned them would fail for reasons that are
    /// not bugs.
    fn output(&self, id: &str) -> String {
        let bytes: Vec<u8> = self
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|(name, payload)| name == TERMINAL_OUTPUT && payload["id"] == id)
            .flat_map(|(_, payload)| {
                base64::engine::general_purpose::STANDARD
                    .decode(payload["data"].as_str().unwrap_or_default())
                    .unwrap_or_default()
            })
            .collect();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    fn exit_code(&self, id: &str) -> Option<u32> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .find(|(name, payload)| name == TERMINAL_EXIT && payload["id"] == id)
            .and_then(|(_, payload)| payload["code"].as_u64())
            .map(|code| code as u32)
    }
}

/// Polls rather than sleeps a fixed time: a shell's startup is not something
/// this test gets to predict, and a sleep long enough to be safe is a sleep
/// long enough to be annoying.
fn until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if done() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for {what}");
}

fn send(terminals: &Terminals, id: &str, text: &str) {
    terminals
        .write(id, &base64::engine::general_purpose::STANDARD.encode(text))
        .expect("write to the terminal");
}

/// The round trip the pane is: type something, see what the program wrote.
#[test]
fn a_shell_echoes_what_it_is_told_and_reports_how_it_ended() {
    let dir = tempfile::tempdir().expect("temp dir");
    let terminals = Terminals::new();
    let collector = Arc::new(Collector::new(terminals.clone()));

    let id = terminals
        .open(collector.clone(), dir.path(), 80, 24)
        .expect("open a terminal");

    send(&terminals, &id, &format!("echo tcode-terminal-ok{ENTER}"));
    until("the shell to echo", || {
        collector.output(&id).contains("tcode-terminal-ok")
    });

    // A specific code, not merely "it ended": the tab shows this, and a status
    // that is always zero is a status nobody can act on.
    send(&terminals, &id, &format!("exit 7{ENTER}"));
    until("the shell to exit", || collector.exit_code(&id).is_some());
    assert_eq!(collector.exit_code(&id), Some(7));
}

/// Resizing is what a program learns its shape from. Nothing here can see the
/// `SIGWINCH`, but a terminal that has gone away must say so rather than
/// pretending the resize landed — a silent success here is a full-screen
/// program drawing at the wrong size forever.
#[test]
fn resizing_a_terminal_that_is_gone_is_an_error_not_a_shrug() {
    let terminals = Terminals::new();
    assert!(terminals.resize("nobody", 80, 24).is_err());
    assert!(terminals.write("nobody", "").is_err());
    // Closing one is not, though: the pane closes tabs it has already forgotten
    // (a program that exited, then the tab dismissed), and that is not a fault.
    assert!(terminals.close("nobody").is_ok());
}

/// Closing a tab ends the program in it. Left undone, a `npm run dev` started
/// in a terminal outlives every trace of the terminal it was started in.
#[test]
fn closing_a_tab_kills_what_was_running_in_it() {
    let dir = tempfile::tempdir().expect("temp dir");
    let terminals = Terminals::new();
    let collector = Arc::new(Collector::new(terminals.clone()));

    let id = terminals
        .open(collector.clone(), dir.path(), 80, 24)
        .expect("open a terminal");
    send(&terminals, &id, &format!("sleep 600{ENTER}"));
    // Wait until the shell is really up before killing it, so what is under
    // test is the kill rather than a race with the spawn.
    until("the shell to start", || !collector.output(&id).is_empty());

    terminals.close(&id).expect("close the terminal");
    assert_eq!(terminals.count(), 0);
    until("the program to be reaped", || {
        collector.exit_code(&id).is_some()
    });
}
