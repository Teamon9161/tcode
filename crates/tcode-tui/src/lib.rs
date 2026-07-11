//! Inline TUI: content is baked into the terminal's native scrollback
//! via `insert_before`; only un-finalized state (streaming text, status
//! line, input box, dialogs) lives in the small bottom viewport. Core
//! never depends on this crate.

mod app;
mod approval;
mod diff;
mod editor;
mod markdown;
mod model_picker;
mod resume;
mod rewind;
mod theme;
pub mod wizard;

use std::io::stdout;
use std::sync::Arc;

use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use tcode_core::{Agent, Session};

pub use app::App;
pub use model_picker::{ModelMenu, ModelOption, SwitchFn};

pub enum Exit {
    Quit,
    /// The provider wizard runs outside the inline TUI. Return the live
    /// session so startup can reconfigure the model and immediately reopen
    /// it. Boxed: `Session` is large and `Quit` carries nothing.
    ConfigureProvider(Box<Session>),
}

/// Run the interactive TUI to completion. Owns terminal setup/teardown;
/// the terminal is restored even if the app errors or panics.
pub async fn run(agent: Arc<Agent>, session: Session, menu: ModelMenu) -> anyhow::Result<Exit> {
    enable_raw_mode()?;
    execute!(stdout(), EnableBracketedPaste)?;

    // Restore the terminal on panic, then let the default hook print.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        default_hook(info);
    }));

    let result = match App::new(agent, session, menu) {
        Ok(mut app) => match app.run().await {
            Ok(()) if app.provider_setup_requested() => app
                .take_session()
                .map(|session| Exit::ConfigureProvider(Box::new(session)))
                .ok_or_else(|| {
                    anyhow::anyhow!("provider setup requested without an active session")
                }),
            Ok(()) => Ok(Exit::Quit),
            Err(error) => Err(error),
        },
        Err(e) => Err(e),
    };

    let _ = std::panic::take_hook(); // drop our hook
    restore_terminal();
    result
}

fn restore_terminal() {
    let _ = execute!(stdout(), DisableBracketedPaste);
    let _ = disable_raw_mode();
    let _ = execute!(stdout(), crossterm::cursor::Show);
}
