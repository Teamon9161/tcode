//! Self-rendered TUI: the in-memory transcript (`transcript.rs`) is the
//! single source of truth; the alternate screen is only a viewport into
//! it. Scrolling, selection and copy are owned by tcode, which is what
//! makes rewind truncation, collapsible tool output and un-baked declined
//! diffs possible at all. Core never depends on this crate.

mod app;
mod approval;
mod diff;
mod editor;
mod live_panel;
mod markdown;
mod mathfmt;
mod mode_picker;
mod model_picker;
mod reference_style;
mod render;
mod resume;
mod theme;
mod transcript;
mod view;
mod view_picker;
pub mod wizard;

use std::io::{stdout, Write};
use std::sync::Arc;

use crossterm::cursor::SetCursorStyle;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use tcode_core::{Agent, Session};

pub use app::App;
pub use model_picker::{AgentMenu, ModelMenu, ModelOption, PinFn, SwitchFn};
pub use tcode_core::commands::OpeningContextFn;

pub enum Exit {
    Quit,
    /// The provider wizard runs outside the inline TUI. Return the live
    /// session so startup can reconfigure the model and immediately reopen
    /// it. Boxed: `Session` is large and `Quit` carries nothing.
    ConfigureProvider(Box<Session>),
}

/// Run the interactive TUI to completion. Owns terminal setup/teardown;
/// the terminal is restored even if the app errors or panics.
pub async fn run(
    agent: Arc<Agent>,
    session: Session,
    menu: ModelMenu,
    agents: AgentMenu,
    opening_context: OpeningContextFn,
    show_reasoning: bool,
    skills: Vec<tcode_tools::Skill>,
) -> anyhow::Result<Exit> {
    enable_raw_mode()?;
    execute!(
        stdout(),
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture,
        SetCursorStyle::SteadyBar,
    )?;

    // Restore the terminal on panic, then let the default hook print.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        default_hook(info);
    }));

    let result = match App::new(
        agent,
        session,
        menu,
        agents,
        opening_context,
        show_reasoning,
        skills,
    ) {
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
    let mut output = stdout();
    restore_terminal_output(&mut output);
    let _ = disable_raw_mode();
}

fn restore_terminal_output(output: &mut impl Write) {
    // Some terminal emulators keep mouse-reporting state across an alternate
    // screen switch. Return to the shell's screen before disabling it, or
    // pointer movement can be delivered to the shell as CSI mouse reports.
    let _ = execute!(output, LeaveAlternateScreen);
    let _ = execute!(output, DisableMouseCapture);
    let _ = execute!(output, DisableBracketedPaste);
    let _ = execute!(output, crossterm::cursor::Show);
}

#[cfg(test)]
mod tests {
    use super::restore_terminal_output;

    #[cfg(not(windows))]
    #[test]
    fn terminal_teardown_disables_mouse_after_leaving_alternate_screen() {
        let mut output = Vec::new();
        restore_terminal_output(&mut output);

        let output = String::from_utf8(output).unwrap();
        let alternate_end = output.find("\x1b[?1049l").unwrap();
        let mouse_end = output.find("\x1b[?1006l").unwrap();
        let paste_end = output.find("\x1b[?2004l").unwrap();
        assert!(alternate_end < mouse_end);
        assert!(mouse_end < paste_end);
    }

    #[cfg(windows)]
    #[test]
    fn terminal_teardown_uses_winapi_for_mouse_cleanup() {
        let mut output = Vec::new();
        restore_terminal_output(&mut output);

        // Crossterm uses WinAPI for mouse cleanup, so `?1006l` is deliberately
        // absent. The remaining ANSI commands still prove the teardown order:
        // leave the alternate screen, then disable paste, then reveal cursor.
        let output = String::from_utf8(output).unwrap();
        let alternate_end = output.find("\x1b[?1049l").unwrap();
        let paste_end = output.find("\x1b[?2004l").unwrap();
        let cursor_show = output.find("\x1b[?25h").unwrap();
        assert!(alternate_end < paste_end);
        assert!(paste_end < cursor_show);
        assert!(!output.contains("\x1b[?1006l"));
    }
}
