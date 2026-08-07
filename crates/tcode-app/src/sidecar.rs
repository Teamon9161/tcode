//! The backend as a child process: one JSON object per line, both directions.
//!
//! This is the second consumer of [`crate::dispatch::Registry`] and the reason
//! that table exists. A Tauri `invoke` and a line on stdin carry exactly the
//! same three things — a method name, an argument object, and somewhere to put
//! the answer — so the shell that owns the window is free to be written in
//! another language without any of the 50 commands noticing.
//!
//! ## The frame
//!
//! ```text
//! in   {"id": 7, "method": "sessions", "args": {}}
//! out  {"id": 7, "ok": {...}}          | {"id": 7, "error": "..."}
//! out  {"event": "tcode://agent-event", "payload": {...}}
//! ```
//!
//! **Not JSON-RPC 2.0**, and that is a decision rather than an omission.
//! `tcode-acp` speaks JSON-RPC because Zed is on the other end of that pipe and
//! the protocol is somebody else's; here both ends ship in the same repository
//! and the registry's error type is already a `String`. Spelling `"jsonrpc":
//! "2.0"` on every frame and wrapping every message in an error object with a
//! numeric code would buy compatibility with a peer that does not exist. What
//! *is* copied from `tcode-acp` is the part that matters: line-delimited JSON,
//! **stdout reserved entirely for frames**, diagnostics on stderr.
//!
//! ## Why stdout is untouchable
//!
//! One stray `println!` anywhere in the process — a dependency's debug print, a
//! panic message routed wrong — lands in the middle of a frame and the shell
//! sees a parse error for a message it did not send. Everything in this crate
//! already uses `eprintln!`; that is not a style preference, it is this.

use std::sync::Arc;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use crate::bridge::Emit;
use crate::dispatch::{Ctx, Registry};

/// The write end of the pipe, cloneable so the emitter and the request tasks
/// share one serialized stdout.
#[derive(Clone)]
pub struct Outbound(mpsc::UnboundedSender<Value>);

impl Outbound {
    fn send(&self, frame: Value) {
        // The receiver is gone only when the writer task has stopped, which
        // means stdout is closed, which means the shell exited. Nothing to
        // report to and nobody to report it to.
        let _ = self.0.send(frame);
    }
}

/// Make the outbound channel.
///
/// **Unbounded, deliberately.** [`Emit::emit`] is synchronous — it is called
/// from inside the agent loop — so the two alternatives are `blocking_send`
/// (which parks a Tokio worker, and panics if it is a runtime thread) and
/// `try_send` (which drops events when the channel fills). A dropped
/// `AgentEvent` is invisible: the transcript is simply missing a piece and
/// nothing anywhere says so. Growing memory while a local child process is
/// behind on reads is the better failure, and it is bounded in practice by the
/// shell being a pipe reader that does nothing else.
pub fn channel() -> (Outbound, mpsc::UnboundedReceiver<Value>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (Outbound(tx), rx)
}

/// [`Emit`] over the pipe. The counterpart of `impl Emit for tauri::AppHandle`,
/// and the whole of what the shell swap costs the event side.
pub struct StdioEmitter(Outbound);

impl StdioEmitter {
    pub fn new(out: Outbound) -> Self {
        Self(out)
    }
}

impl Emit for StdioEmitter {
    fn emit(&self, event: &str, payload: Value) {
        self.0.send(json!({ "event": event, "payload": payload }));
    }
}

/// Read requests until stdin closes, then shut down what this process owns.
///
/// Returns when the shell's end of the pipe goes away — the app quitting, or
/// crashing. Both are the same event from here and both mean the same thing:
/// the child processes this one started (rule 9i's PTYs) have nobody left to
/// talk to and must not outlive the window.
pub async fn serve(
    ctx: Arc<Ctx>,
    registry: Arc<Registry>,
    out: Outbound,
    outbound: mpsc::UnboundedReceiver<Value>,
) -> anyhow::Result<()> {
    let writer = tokio::spawn(write_frames(outbound));

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(error) => {
                // No id to answer under, so this can only go to the log. A
                // frame this side cannot parse is a bug in the shell, and the
                // shell is the thing with a console.
                eprintln!("tcode-sidecar: unreadable frame: {error}");
                continue;
            }
        };
        let (Some(id), Some(method)) = (request.get("id").cloned(), request["method"].as_str())
        else {
            eprintln!("tcode-sidecar: frame without an id or a method: {line}");
            continue;
        };
        let method = method.to_string();
        let args = request
            .get("args")
            .cloned()
            .unwrap_or_else(|| Value::Object(Default::default()));

        // One task per request, because that is what a Tauri `invoke` already
        // is: `send_message` returns immediately but `project_list` reads every
        // log in a folder, and running it inline would stop this loop — so a
        // pane opening the folder menu would freeze every other pane until the
        // disk was done. Replies are correlated by id, so they may return in
        // any order.
        let (ctx, registry, out) = (ctx.clone(), registry.clone(), out.clone());
        tokio::spawn(async move {
            out.send(match registry.call(&ctx, &method, &args).await {
                Ok(value) => json!({ "id": id, "ok": value }),
                Err(error) => json!({ "id": id, "error": error }),
            });
        });
    }

    // Same teardown the Tauri shell does on `CloseRequested`, for the same
    // reason: a terminal holds a live child process and the window closing is
    // not something any pane can intercept.
    ctx.terminals.close_all();
    writer.abort();
    Ok(())
}

/// Serialize the outbound channel onto stdout, one line each.
async fn write_frames(mut outbound: mpsc::UnboundedReceiver<Value>) {
    let mut stdout = tokio::io::stdout();
    while let Some(frame) = outbound.recv().await {
        let mut line = frame.to_string();
        line.push('\n');
        // A closed stdout is the shell having exited. Stop writing; `serve`'s
        // read loop is about to end for the same reason.
        if stdout.write_all(line.as_bytes()).await.is_err() || stdout.flush().await.is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The event frame is the shape `preload.js` destructures. Pinned here
    /// because both ends were written from this comment and nothing else
    /// compares them — a renamed field is a UI that silently receives nothing,
    /// exactly the failure `bridge.rs::the_event_names_match_the_frontend`
    /// exists to catch for the names themselves.
    #[test]
    fn an_event_frame_carries_its_name_and_payload() {
        let (out, mut rx) = channel();
        StdioEmitter::new(out).emit("tcode://turn-started", json!({ "session": "s" }));

        let frame = rx.try_recv().expect("the emitter wrote a frame");
        assert_eq!(frame["event"], "tcode://turn-started");
        assert_eq!(frame["payload"]["session"], "s");
    }

    /// Frames are line-delimited, so a payload containing a newline must not
    /// become two frames. `serde_json` escapes it, and this is the assertion
    /// that keeps anyone from "simplifying" the writer into something that
    /// does not — a tool result with a newline in it is the common case, not
    /// the exotic one.
    #[test]
    fn a_newline_in_a_payload_stays_inside_one_frame() {
        let (out, mut rx) = channel();
        StdioEmitter::new(out).emit("tcode://agent-event", json!({ "text": "one\ntwo" }));

        let line = rx.try_recv().unwrap().to_string();
        assert!(
            !line.contains('\n'),
            "the frame broke into two lines: {line}"
        );
    }
}
