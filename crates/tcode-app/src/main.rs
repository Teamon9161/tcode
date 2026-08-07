//! The Tauri shell. Opens the current directory as one session and hands the
//! webview the commands in [`tcode_app::commands`].

// Release builds must not also spawn a console window on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use anyhow::Context;
use std::sync::Arc;
use tauri::Manager;

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
            app.state::<Arc<tcode_app::state::Supervisor>>()
                .attach_emitter(Arc::new(app.handle().clone()));
            // The browser is a child webview of the main window. WebView2 hangs
            // when the parent HWND is destroyed while its controller is still
            // alive (tauri#13534 is the same teardown for owned windows), so
            // the child must be closed while the window is still whole, before
            // the window's own teardown drops it with its parent already gone.
            // The frontend's pane-close path is the same `browser_close`; this
            // is the version that runs on the app's own exit, which no frontend
            // code can intercept (the caption close is non-client area).
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
            app.get_window("main")
                .expect("the main window is open")
                .on_window_event(move |event| {
                    if let tauri::WindowEvent::CloseRequested { .. } = event {
                        let _ = browser.close();
                        terminals.close_all();
                    }
                });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            tcode_app::commands::sessions,
            tcode_app::commands::send_message,
            tcode_app::commands::slash_command,
            tcode_app::commands::slash_commands,
            tcode_app::commands::clipboard_image,
            tcode_app::commands::respond_approval,
            tcode_app::commands::interrupt,
            tcode_app::commands::queued,
            tcode_app::commands::withdraw_queued,
            tcode_app::commands::interrupt_and_send,
            tcode_app::commands::rewind_targets,
            tcode_app::commands::rewind_preview,
            tcode_app::commands::rewind,
            tcode_app::commands::project_list,
            tcode_app::commands::project_sessions,
            tcode_app::commands::open_folder,
            tcode_app::commands::close_session,
            tcode_app::commands::workspace_list,
            tcode_app::commands::workspace_complete,
            tcode_app::commands::workspace_present,
            tcode_app::commands::workspace_read_text,
            tcode_app::commands::workspace_read_binary,
            tcode_app::commands::workspace_write_text,
            tcode_app::commands::workspace_create,
            tcode_app::commands::workspace_rename,
            tcode_app::commands::workspace_delete,
            tcode_app::commands::workspace_openers,
            tcode_app::commands::workspace_open_external,
            tcode_app::commands::tool_views,
            tcode_app::commands::plan,
            tcode_app::commands::write_plan,
            tcode_app::commands::execute_plan_elsewhere,
            tcode_app::commands::shown_file,
            tcode_app::commands::serve_url,
            tcode_app::commands::browser_open,
            tcode_app::commands::browser_bounds,
            tcode_app::commands::browser_visible,
            tcode_app::commands::browser_navigate,
            tcode_app::commands::browser_step,
            tcode_app::commands::browser_reload,
            tcode_app::commands::browser_close,
            tcode_app::commands::terminal_open,
            tcode_app::commands::terminal_write,
            tcode_app::commands::terminal_resize,
            tcode_app::commands::terminal_close,
            tcode_app::commands::picker_state,
            tcode_app::commands::choose_model,
            tcode_app::commands::choose_preset,
            tcode_app::commands::pin_role,
            tcode_app::commands::save_preset,
            tcode_app::commands::choose_mode,
        ])
        .run(tauri::generate_context!())
        .context("the desktop app exited with an error")
}
