//! The Tauri shell. Opens the current directory as one session and hands the
//! webview the commands in [`tcode_app::commands`].

// Release builds must not also spawn a console window on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use anyhow::Context;

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

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(startup.supervisor)
        .invoke_handler(tauri::generate_handler![
            tcode_app::commands::sessions,
            tcode_app::commands::send_message,
            tcode_app::commands::respond_approval,
            tcode_app::commands::interrupt,
            tcode_app::commands::queued,
            tcode_app::commands::withdraw_queued,
            tcode_app::commands::interrupt_and_send,
            tcode_app::commands::rewind_targets,
            tcode_app::commands::rewind_preview,
            tcode_app::commands::rewind,
            tcode_app::commands::launchpad,
            tcode_app::commands::project_sessions,
            tcode_app::commands::open_folder,
            tcode_app::commands::close_session,
            tcode_app::commands::workspace_list,
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
