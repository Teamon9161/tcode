//! Self-rendered TUI: the in-memory transcript (`transcript.rs`) is the
//! single source of truth; the alternate screen is only a viewport into
//! it. Scrolling, selection and copy are owned by tcode, which is what
//! makes rewind truncation, collapsible tool output and un-baked declined
//! diffs possible at all. Core never depends on this crate.

mod app;
mod approval;
mod composer;
mod diff;
mod editor;
mod folder_trust_picker;
mod live_panel;
mod markdown;
mod mathfmt;
mod mode_picker;
mod model_picker;
mod overlay;
mod provider_picker;
mod reference_style;
mod render;
mod resume;
mod setup;
mod surface;
mod theme;
mod transcript;
mod usage;
mod view;
mod view_picker;
mod voice;
mod voice_picker;
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
use tcode_core::config::{Config, ModelState};
use tcode_core::{Agent, Session};

pub use app::App;
pub use model_picker::{
    AgentMenu, AgentModelChoice, AgentRole, ModelMenu, ModelOption, PinFn, SwitchFn,
};
pub use tcode_core::commands::{EnvironmentFn, OpeningContextFn};

/// The two effects `/provider` needs. Both live in the binary crate, like
/// `SwitchFn` and `PinFn`, so the TUI depends neither on the concrete
/// providers nor on where config.toml lives.
pub struct ProviderSetup {
    /// The user's own global config, to seed the form. Never the merged
    /// runtime config: a project overlay must not be copied into
    /// `~/.tcode/config.toml` by saving.
    pub load: Box<dyn Fn() -> Result<Config, String> + Send + Sync>,
    /// Persist the result and rebuild everything derived from it: the active
    /// provider in the shared `ModelCell`, then both menus.
    #[allow(clippy::type_complexity)]
    pub apply:
        Box<dyn Fn(Config, ModelState) -> Result<(ModelMenu, AgentMenu), String> + Send + Sync>,
}

pub struct TuiConfig {
    pub menu: ModelMenu,
    pub agents: AgentMenu,
    pub provider_setup: ProviderSetup,
    pub opening_context: OpeningContextFn,
    pub environment: EnvironmentFn,
    pub show_reasoning: bool,
    pub skills: Vec<tcode_tools::Skill>,
    /// `[voice]`, with `enabled` already resolved against state.toml.
    pub voice: tcode_core::config::VoiceConfig,
}

/// Whether we asked the terminal to report key releases, so teardown only
/// pops what it pushed. A global because `restore_terminal` also runs from the
/// panic hook, where no `App` is reachable.
static KEY_RELEASES: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Ask for (or stop asking for) key-release events — what push-to-talk needs
/// to know the key was let go. Both platforms need asking, for unrelated
/// reasons, so both live here.
///
/// **Windows.** crossterm reads `INPUT_RECORD`s, and under a pseudoconsole
/// (Windows Terminal, VS Code) those records are *synthesised* by ConPTY from
/// the VT stream the terminal sends — a stream with no concept of key-up, so
/// every press gets a release manufactured in the same instant. `^[[?9001h`
/// (win32-input-mode) asks the terminal for full Win32 key events instead, at
/// which point ConPTY can reproduce real key-up records. Nothing changes for
/// us: crossterm never enables `ENABLE_VIRTUAL_TERMINAL_INPUT`, so the console
/// still hands us records, only now faithful ones. Terminals that do not
/// understand the request ignore it, and `Voice` detects the manufactured
/// release and falls back to a toggle.
///
/// **Elsewhere.** The kitty keyboard protocol, requesting *only*
/// `REPORT_EVENT_TYPES`: the disambiguation flags would change how every other
/// key is encoded, which is too much to trade for dictation.
pub(crate) fn set_key_release_reporting(on: bool) {
    use std::sync::atomic::Ordering;

    #[cfg(windows)]
    {
        if on == KEY_RELEASES.swap(on, Ordering::SeqCst) {
            return;
        }
        let request = if on { "\x1b[?9001h" } else { "\x1b[?9001l" };
        let mut out = stdout();
        let _ = out.write_all(request.as_bytes());
        let _ = out.flush();
    }
    #[cfg(not(windows))]
    {
        if !crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false) {
            return;
        }
        use crossterm::event::{
            KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
        };
        if on {
            if !KEY_RELEASES.swap(true, Ordering::SeqCst) {
                let _ = execute!(
                    stdout(),
                    PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::REPORT_EVENT_TYPES)
                );
            }
        } else if KEY_RELEASES.swap(false, Ordering::SeqCst) {
            let _ = execute!(stdout(), PopKeyboardEnhancementFlags);
        }
    }
}

/// Run the interactive TUI to completion. Owns terminal setup/teardown;
/// the terminal is restored even if the app errors or panics.
pub async fn run(agent: Arc<Agent>, session: Session, config: TuiConfig) -> anyhow::Result<()> {
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

    let result = match App::new(agent, session, config) {
        Ok(mut app) => app.run().await,
        Err(e) => Err(e),
    };

    let _ = std::panic::take_hook(); // drop our hook
    restore_terminal();
    result
}

fn restore_terminal() {
    // A keyboard mode we pushed must not outlive us: it is the terminal's
    // state, not ours, and a stale one breaks the shell we hand back to.
    set_key_release_reporting(false);
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
