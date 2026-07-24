//! The Tauri shell. Opens the current directory as one session and hands the
//! webview the commands in [`tcode_app::commands`].

// Release builds must not also spawn a console window on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use anyhow::Context;

fn main() -> anyhow::Result<()> {
    let cwd = std::env::current_dir()
        .context("cannot determine working directory")?
        .canonicalize()
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

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(startup.supervisor)
        .invoke_handler(tauri::generate_handler![
            tcode_app::commands::sessions,
            tcode_app::commands::send_message,
            tcode_app::commands::respond_approval,
            tcode_app::commands::interrupt,
            tcode_app::commands::launchpad,
            tcode_app::commands::project_sessions,
            tcode_app::commands::open_folder,
            tcode_app::commands::close_session,
        ])
        .run(tauri::generate_context!())
        .context("the desktop app exited with an error")
}
