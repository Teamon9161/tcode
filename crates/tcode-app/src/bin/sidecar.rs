//! The backend as a child process, for the Electron shell.
//!
//! Deliberately the same shape as `main.rs`, minus a window: boot the agent,
//! open the working directory as the first session, build the one [`Ctx`], and
//! hand a registry to whatever is going to drive it. The difference is entirely
//! in the last line — a pipe instead of a webview.
//!
//! **The working directory is the folder to open**, exactly as under Tauri.
//! Which folder that is remains the shell's decision; it spawns this process
//! with the cwd it wants.
//!
//! Nothing here prints to stdout. See `sidecar.rs` for why that is a hard rule
//! rather than a habit.

use std::sync::Arc;

use anyhow::Context;

use tcode_app::bridge::Emit;
use tcode_app::dispatch::{Ctx, Registry};
use tcode_app::sidecar::{self, ShellClient, StdioEmitter};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cwd = tcode_app::paths::canonical_dir(
        &std::env::current_dir().context("cannot determine working directory")?,
    )
    .context("cannot canonicalize working directory")?;

    // The pipe comes before the agent now, and the ordering is load-bearing in
    // both directions: `Ctx` holds the emitter (the write end), and the
    // `browser` tool the agent is built with holds the shell client (the read
    // end's other half). Neither can be attached afterwards without a mutable
    // cell, and one channel created first removes the need for either.
    let (out, outbound) = sidecar::channel();
    let shell = Arc::new(ShellClient::new(out.clone()));

    let startup = tcode_app::boot::start(cwd, shell.clone()).await?;
    for warning in &startup.warnings {
        eprintln!("warning: {warning}");
    }
    eprintln!(
        "tcode-sidecar: session {} open on {}",
        startup.session.id,
        startup.session.cwd.display()
    );
    if let Ok(serve) = startup.serve.get() {
        eprintln!(
            "tcode-sidecar: viewer origin on http://127.0.0.1:{}",
            serve.port()
        );
    }

    let emit: Arc<dyn Emit> = Arc::new(StdioEmitter::new(out.clone()));
    startup.supervisor.attach_emitter(emit.clone());

    let ctx = Arc::new(Ctx {
        supervisor: startup.supervisor,
        serve: startup.serve,
        terminals: tcode_app::terminal::Terminals::with_shell(startup.terminal_shell),
        emit,
    });

    // `builtin()` and nothing else: `browser_*` belongs to whoever owns the
    // native views, and on this side of the pipe that is the Electron main
    // process. A browser verb invoked from the webview is answered there; one
    // this process needs goes out as a `call` frame through `shell`.
    sidecar::serve(ctx, Arc::new(Registry::builtin()), out, outbound, shell).await
}
