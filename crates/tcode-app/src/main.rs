//! The Tauri shell. Opens the current directory as one session and hands the
//! webview a single [`rpc`] door onto [`tcode_app::dispatch::Registry`].
//!
//! This file is one of the two things the migration to Electron replaces (the
//! other is the browser pane); everything it reaches is written against
//! abstractions that do not know a window exists. See `MIGRATION-ELECTRON.md`.

// Release builds must not also spawn a console window on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use anyhow::Context;
use std::sync::Arc;
use tauri::Manager;
use tauri_plugin_dialog::DialogExt;

use tcode_app::bridge::{Emit, WINDOW_STATE};
use tcode_app::dispatch::{handler, Ctx, Registry};

/// Every command, through one door.
///
/// It replaced fifty generated `#[tauri::command]` wrappers, and the reason is
/// not brevity: those wrappers were the only part of the backend that knew what
/// a Tauri `invoke` is. With the argument-by-name and serialization moved into
/// `dispatch`, a shell has exactly one thing left to do — hand a method name and
/// a JSON object to the registry — which is also all the Electron main process
/// will do with a line off the sidecar's pipe.
///
/// `async` is load-bearing beyond ergonomics: it puts every command body inside
/// the Tokio runtime, which is what makes the plain `tokio::spawn` in
/// `commands::deliver` safe (see the comment there).
#[tauri::command]
async fn rpc(
    ctx: tauri::State<'_, Arc<Ctx>>,
    registry: tauri::State<'_, Registry>,
    method: String,
    args: serde_json::Value,
) -> Result<serde_json::Value, String> {
    registry.call(&ctx, &method, &args).await
}

/// The verbs only a window can answer.
///
/// They go in the same table as everything else, and that is the point: the
/// frontend has one `invoke`, and *who answers* is the shell's business — the
/// same arrangement `browser::commands::register` already uses. The alternative
/// was a second import in the frontend for the two panes that need a window,
/// which would have meant every consumer learning which door to knock on.
///
/// The title bar is app-drawn (rule 9c), so these are not conveniences: without
/// them the three buttons in the corner do nothing at all.
fn register_shell(registry: &mut Registry, app: tauri::AppHandle) {
    /// One window verb: look the window up, act on it, serialize the answer.
    ///
    /// The window is fetched per call rather than captured because `AppHandle`
    /// outlives it — a verb arriving during teardown must report that the
    /// window is gone, not hold a stale one open.
    fn verb<T, F>(registry: &mut Registry, app: &tauri::AppHandle, name: &'static str, act: F)
    where
        T: serde::Serialize + Send + 'static,
        F: Fn(&tauri::Window) -> tauri::Result<T> + Clone + Send + Sync + 'static,
    {
        let app = app.clone();
        registry.add(
            name,
            handler(move |_ctx, _args| {
                let (app, act) = (app.clone(), act.clone());
                Box::pin(async move {
                    let window = app.get_window("main").ok_or("the window is gone")?;
                    let out = act(&window).map_err(|error| error.to_string())?;
                    serde_json::to_value(out).map_err(|error| error.to_string())
                })
            }),
        );
    }

    verb(registry, &app, "window_minimize", |window| {
        window.minimize()
    });
    verb(registry, &app, "window_close", |window| window.close());
    verb(registry, &app, "window_is_maximized", |window| {
        window.is_maximized()
    });
    // Read-then-act rather than a single call, because `Window` has no
    // `toggle_maximize` — the JS API's one is a command implemented this way.
    verb(registry, &app, "window_toggle_maximize", |window| {
        if window.is_maximized()? {
            window.unmaximize()
        } else {
            window.maximize()
        }
    });

    // The folder dialog. `blocking_pick_folder` is the plugin's synchronous
    // API, and it is synchronous the only way a native dialog can be: it posts
    // to the main thread and waits. So it must not be awaited on a runtime
    // thread — hence `spawn_blocking`, and `None` for a cancelled dialog rather
    // than an error, because cancelling is an answer.
    registry.add(
        "dialog_open_folder",
        handler(move |_ctx, _args| {
            let app = app.clone();
            Box::pin(async move {
                let chosen =
                    tokio::task::spawn_blocking(move || app.dialog().file().blocking_pick_folder())
                        .await
                        .map_err(|error| format!("the folder picker could not run: {error}"))?;
                let path = chosen.map(|folder| folder.to_string());
                serde_json::to_value(path).map_err(|error| error.to_string())
            })
        }),
    );
}

fn main() -> anyhow::Result<()> {
    let cwd = tcode_app::paths::canonical_dir(
        &std::env::current_dir().context("cannot determine working directory")?,
    )
    .context("cannot canonicalize working directory")?;

    // `tauri::async_runtime` rather than `#[tokio::main]`: it is the runtime
    // Tauri spawns command tasks on, and having a second one only invites the
    // question of which context a given `spawn` lands in.
    let startup = tauri::async_runtime::block_on(tcode_app::boot::start(cwd))?;
    for warning in &startup.warnings {
        eprintln!("warning: {warning}");
    }
    eprintln!(
        "tcode-app: session {} open on {}",
        startup.session.id,
        startup.session.cwd.display()
    );
    // Named in the log because it is the first thing to check when a shown
    // report is blank: a port here means the origin is up and the question is
    // the frame, not the server.
    if let Ok(serve) = startup.serve.get() {
        eprintln!(
            "tcode-app: viewer origin on http://127.0.0.1:{}",
            serve.port()
        );
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(startup.supervisor)
        .manage(startup.serve)
        .manage(tcode_app::browser::Browser::new())
        .manage(tcode_app::terminal::Terminals::new())
        .setup(|app| {
            // Turns nobody typed — a monitor firing, a background sub-agent
            // reporting back — need somewhere to send their events, and the
            // supervisor was built before this window existed. This is where it
            // gets one, and where every open conversation starts watching.
            let emit: Arc<dyn Emit> = Arc::new(app.handle().clone());
            // Inside the runtime, and that is load-bearing rather than tidy.
            // `attach_emitter` starts a monitor watch per open session with a
            // plain `tokio::spawn` — correct, because the backend must not know
            // which runtime a shell happens to use — and `setup` runs on the
            // main thread before the event loop, where no runtime is entered.
            // Calling it directly panics with "there is no reactor running".
            //
            // `block_on` rather than `spawn` so the emitter is in place before
            // the first command can arrive: it does no I/O, and a window that
            // briefly answered turns with nowhere to send their events would be
            // a race nobody could reproduce.
            tauri::async_runtime::block_on(async {
                app.state::<Arc<tcode_app::state::Supervisor>>()
                    .attach_emitter(emit.clone());
            });

            // The composition root, and the only place a `Ctx` is built. It is
            // assembled here rather than in `boot` because `emit` is the one
            // field that cannot exist before the window does.
            app.manage(Arc::new(Ctx {
                supervisor: app
                    .state::<Arc<tcode_app::state::Supervisor>>()
                    .inner()
                    .clone(),
                serve: app.state::<tcode_app::boot::ServeHandle>().inner().clone(),
                terminals: app
                    .state::<Arc<tcode_app::terminal::Terminals>>()
                    .inner()
                    .clone(),
                emit,
            }));
            // The registry, plus the verbs only a shell can answer. This is the
            // seam the migration turns: `builtin()` is portable, `register` is
            // this window's, and after Electron there is a different `register`
            // and the same `builtin()`.
            let mut registry = Registry::builtin();
            register_shell(&mut registry, app.handle().clone());
            tcode_app::browser::commands::register(
                &mut registry,
                app.handle().clone(),
                app.state::<Arc<tcode_app::browser::Browser>>()
                    .inner()
                    .clone(),
            );
            app.manage(registry);
            // The window's widget tree is rearranged now, before anything is on
            // screen, so the browser's tabs have somewhere positionable to live
            // (`browser::place`). It moves the app's own webview, which is why
            // it happens at startup rather than when the first tab opens — and
            // it is a no-op on every platform that positions child webviews by
            // itself. A failure here is not fatal: everything except the
            // browser pane's geometry still works, and a window that refuses to
            // open would be the worse answer.
            if let Err(error) = tcode_app::browser::install(app.handle()) {
                eprintln!("tcode-app: could not prepare the browser layer: {error}");
            }

            // The browser's tabs are child webviews of the main window.
            // WebView2 hangs when the parent HWND is destroyed while a
            // controller is still alive (tauri#13534 is the same teardown for
            // owned windows), so the children must be closed while the window
            // is still whole, before the window's own teardown drops them with
            // their parent already gone. This is also the one place that closes
            // the *last* webview — `browser_close` deliberately keeps it, and
            // the app's own exit is not something frontend code can intercept
            // (the caption close is non-client area).
            let browser = app
                .state::<Arc<tcode_app::browser::Browser>>()
                .inner()
                .clone();
            // The same reason, one layer along: a terminal holds a child
            // process, and the window closing is not something any pane can
            // intercept. Without this the shells — and anything they were
            // running — outlive the app that started them.
            let terminals = app
                .state::<Arc<tcode_app::terminal::Terminals>>()
                .inner()
                .clone();
            let window = app.get_window("main").expect("the main window is open");
            let reporter = window.clone();
            let state_events = app.state::<Arc<Ctx>>().emit.clone();
            window.on_window_event(move |event| match event {
                tauri::WindowEvent::CloseRequested { .. } => {
                    browser.close_all();
                    terminals.close_all();
                }
                // A snap gesture, a double-click on the bar, the OS restoring
                // the window: all of them arrive as a resize and none of them
                // went through a button here. See `bridge::WINDOW_STATE`.
                tauri::WindowEvent::Resized(_) => {
                    if let Ok(maximized) = reporter.is_maximized() {
                        state_events
                            .emit(WINDOW_STATE, serde_json::json!({ "maximized": maximized }));
                    }
                }
                _ => {}
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            // One door. Everything the webview can ask for is a name in
            // `dispatch::Registry`; this shell contributes the browser verbs to
            // that table and otherwise only carries method and arguments across.
            rpc
        ])
        .run(tauri::generate_context!())
        .context("the desktop app exited with an error")
}
