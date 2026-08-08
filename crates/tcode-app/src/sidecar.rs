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
//!
//! out  {"call": 3, "method": "browser_open", "args": {}}
//! in   {"call": 3, "ok": {...}}        | {"call": 3, "error": "..."}
//! ```
//!
//! **Two id spaces, and that is why the key differs.** `id` is the shell's
//! numbering for the requests it makes; `call` is this process's numbering for
//! the requests it makes. They are allocated independently and would collide if
//! they shared a key, so which field a frame carries *is* the answer to "who
//! asked". See [`Shell`] for why this direction exists at all.
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

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

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

/// Ask the shell to do something only the shell can do.
///
/// The counterpart of [`Emit`], and the other half of the pipe. `Emit` is
/// enough for everything the backend has needed so far — it produces events and
/// something draws them — but a native view cannot be *read* that way: a
/// browser snapshot has to come back. So this is a request with an answer,
/// travelling in the direction the frames never used to go.
///
/// **Deliberately narrow.** The one intended consumer is the browser tool
/// (`AGENT-BROWSER.md`): a `WebContentsView` and its debugger live in the
/// Electron main process, and nothing in Rust can reach them. A second
/// consumer should be read as a question — "why is this logic not in Rust?" —
/// because the rule it lives next to (`AGENTS.md`: no business logic in the
/// shell) does not weaken just because a channel now exists.
///
/// A trait rather than the concrete client so `tests/bridge.rs` can drive the
/// whole browser path with no window and no Electron, which is hard rule 2.
#[async_trait::async_trait]
pub trait Shell: Send + Sync {
    async fn call(&self, method: &str, args: Value) -> Result<Value, String>;
}

/// How long a shell call may take before it is abandoned.
///
/// **Not optional, and not per-call.** The spike that measured CDP against a
/// hidden `WebContentsView` hung twice on `Page.captureScreenshot` — the call
/// simply never came back — and each time the symptom was a window on screen
/// with no output, indistinguishable from a broken build. A shell call that can
/// hang forever is a turn that can hang forever, so the bound lives here where
/// no caller can forget it rather than in each of them.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// [`Shell`] over the pipe: write a `call` frame, wait for the reply the read
/// loop routes back.
pub struct ShellClient {
    out: Outbound,
    next: AtomicU64,
    /// Calls awaiting a reply. Emptied — with an error each — when the shell
    /// goes away, for the same reason `main.js` rejects its own pending map:
    /// a promise that neither resolves nor rejects is the failure nobody can
    /// see.
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>,
}

impl ShellClient {
    pub fn new(out: Outbound) -> Self {
        Self {
            out,
            next: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
        }
    }

    /// Route one `{"call": n, …}` reply. Returns whether the frame was one.
    ///
    /// An unknown `call` id is dropped rather than treated as an error: it is
    /// what a reply to an abandoned (timed out) call looks like, and the caller
    /// has already been told what happened.
    fn deliver(&self, frame: &Value) -> bool {
        let Some(id) = frame.get("call").and_then(Value::as_u64) else {
            return false;
        };
        let waiting = self.pending.lock().expect("pending").remove(&id);
        if let Some(waiting) = waiting {
            let _ = waiting.send(match frame.get("error") {
                Some(error) => Err(error.as_str().unwrap_or("the shell failed").to_string()),
                None => Ok(frame.get("ok").cloned().unwrap_or(Value::Null)),
            });
        }
        true
    }

    /// Fail every outstanding call. Called when stdin closes: the shell has
    /// exited, so no reply is ever coming.
    fn abandon(&self, why: &str) {
        for (_, waiting) in self.pending.lock().expect("pending").drain() {
            let _ = waiting.send(Err(why.to_string()));
        }
    }
}

#[async_trait::async_trait]
impl Shell for ShellClient {
    async fn call(&self, method: &str, args: Value) -> Result<Value, String> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().expect("pending").insert(id, tx);
        self.out
            .send(json!({ "call": id, "method": method, "args": args }));

        match tokio::time::timeout(CALL_TIMEOUT, rx).await {
            Ok(Ok(reply)) => reply,
            // The sender was dropped without answering: `abandon`, or a bug.
            Ok(Err(_)) => Err(format!("the shell went away during '{method}'")),
            Err(_) => {
                self.pending.lock().expect("pending").remove(&id);
                Err(format!(
                    "the shell did not answer '{method}' within {}s",
                    CALL_TIMEOUT.as_secs()
                ))
            }
        }
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
    shell: Arc<ShellClient>,
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
        // A reply to something *this* process asked for, not a request. Checked
        // before the request shape because the two are told apart by which id
        // field they carry, and only one of them has a `method`.
        if shell.deliver(&request) {
            continue;
        }
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
    // Anything still waiting on the shell will wait forever otherwise — stdin
    // closing is exactly the moment no reply can arrive.
    shell.abandon("the shell exited before answering");
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

    /// A call goes out under `call`, never `id`.
    ///
    /// The two directions number their requests independently, so a frame that
    /// used `id` for both would let the shell's reply to its own request #3 be
    /// read as an answer to this process's request #3. The field name *is* the
    /// disambiguator, which makes it worth an assertion rather than a comment.
    #[tokio::test]
    async fn a_call_frame_is_told_apart_from_a_request_by_its_id_field() {
        let (out, mut rx) = channel();
        let shell = Arc::new(ShellClient::new(out));

        let calling = {
            let shell = shell.clone();
            tokio::spawn(async move { shell.call("browser_open", json!({ "rect": {} })).await })
        };

        let frame = loop {
            match rx.try_recv() {
                Ok(frame) => break frame,
                Err(_) => tokio::task::yield_now().await,
            }
        };
        assert_eq!(frame["method"], "browser_open");
        assert!(
            frame["call"].is_u64(),
            "a call frame carries `call`: {frame}"
        );
        assert!(
            frame.get("id").is_none(),
            "a call frame must not carry `id`"
        );

        shell.deliver(&json!({ "call": frame["call"], "ok": { "tab": "t1" } }));
        assert_eq!(calling.await.unwrap().unwrap()["tab"], "t1");
    }

    /// An error from the shell arrives as an error, not as a `null` answer.
    #[tokio::test]
    async fn a_shell_error_reaches_the_caller() {
        let (out, mut rx) = channel();
        let shell = Arc::new(ShellClient::new(out));

        let calling = {
            let shell = shell.clone();
            tokio::spawn(async move { shell.call("browser_snapshot", json!({})).await })
        };
        let frame = loop {
            match rx.try_recv() {
                Ok(frame) => break frame,
                Err(_) => tokio::task::yield_now().await,
            }
        };

        shell.deliver(&json!({ "call": frame["call"], "error": "that browser tab is not open" }));
        assert_eq!(
            calling.await.unwrap(),
            Err("that browser tab is not open".into())
        );
    }

    /// The shell exiting must fail every outstanding call.
    ///
    /// This is the same guarantee `main.js`'s `die()` gives the other
    /// direction, and it exists for the same reason: a request that neither
    /// succeeds nor fails is a turn that hangs with nothing on screen to say
    /// why. `CALL_TIMEOUT` would eventually catch it, but thirty seconds of
    /// silence for a shell that is already gone is not an answer.
    #[tokio::test]
    async fn a_dead_shell_fails_its_pending_calls() {
        let (out, _rx) = channel();
        let shell = Arc::new(ShellClient::new(out));

        let calling = {
            let shell = shell.clone();
            tokio::spawn(async move { shell.call("browser_open", json!({})).await })
        };
        // Let the call register itself before the shell is declared gone;
        // abandoning an empty map would pass without testing anything.
        while shell.pending.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
        shell.abandon("the shell exited before answering");

        assert!(calling.await.unwrap().is_err());
    }

    /// A reply to a call that already gave up is dropped, not treated as an
    /// error or matched to whoever holds that id next. `next` never reuses an
    /// id, so "unknown" here can only mean "abandoned".
    #[test]
    fn a_late_reply_to_an_abandoned_call_is_harmless() {
        let (out, _rx) = channel();
        let shell = ShellClient::new(out);
        assert!(shell.deliver(&json!({ "call": 99, "ok": null })));
    }

    /// And the frames going the other way are still requests. `deliver` has to
    /// say no to those or the read loop would swallow every command the shell
    /// sends.
    #[test]
    fn a_request_frame_is_not_mistaken_for_a_reply() {
        let (out, _rx) = channel();
        let shell = ShellClient::new(out);
        assert!(!shell.deliver(&json!({ "id": 1, "method": "sessions", "args": {} })));
    }

    /// The shell holds up its end of the `call` frame.
    ///
    /// Both halves were written from this module's header and **nothing else
    /// compares them** — the same gap `bridge.rs::the_event_names_match_the_
    /// frontend` and `dispatch.rs::the_shell_answers_every_shell_owned_verb`
    /// exist to close. The failure without this check is silent in the worst
    /// way: every call the backend makes waits out `CALL_TIMEOUT` and comes
    /// back "the shell did not answer", which reads as a hung shell rather
    /// than as a missing branch in a line handler.
    ///
    /// Matched on `frame.call` rather than on a function name, because the
    /// field is the contract and the function around it may be renamed.
    #[test]
    fn the_shell_routes_call_frames() {
        let main =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/electron/main.js"))
                .expect("electron/main.js");

        assert!(
            main.contains("frame.call !== undefined"),
            "the Electron shell no longer recognizes a `call` frame — every request \
             this process makes of it would time out instead of being answered"
        );
        assert!(
            main.contains("call: frame.call"),
            "the shell recognizes a `call` frame but does not answer under the same \
             id — `ShellClient::deliver` has nothing to match the reply to"
        );
    }
}
