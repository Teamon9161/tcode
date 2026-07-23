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
use std::path::PathBuf;
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
    AgentMenu, AgentModelChoice, AgentRole, ApplyPresetFn, ModelMenu, ModelOption, PinFn,
    PresetDraft, PresetMenu, PresetOption, RoleSection, SavePresetFn, SwitchFn,
};
pub use tcode_core::commands::{EnvironmentFn, OpeningContextFn};

/// Runtime-state access is injected by the binary so every frontend action
/// writes the config file selected at startup, never a hard-coded home path.
#[derive(Clone)]
pub struct StateStore {
    load: Arc<dyn Fn() -> Result<ModelState, String> + Send + Sync>,
    update:
        Arc<dyn Fn(Box<dyn FnOnce(&mut ModelState) + Send>) -> Result<(), String> + Send + Sync>,
}

impl StateStore {
    pub fn new(
        load: impl Fn() -> Result<ModelState, String> + Send + Sync + 'static,
        update: impl Fn(Box<dyn FnOnce(&mut ModelState) + Send>) -> Result<(), String>
            + Send
            + Sync
            + 'static,
    ) -> Self {
        Self {
            load: Arc::new(load),
            update: Arc::new(update),
        }
    }

    pub fn load(&self) -> Result<ModelState, String> {
        (self.load)()
    }

    pub fn update_checked(
        &self,
        edit: impl FnOnce(&mut ModelState) + Send + 'static,
    ) -> Result<(), String> {
        (self.update)(Box::new(edit))
    }

    pub fn update(&self, edit: impl FnOnce(&mut ModelState) + Send + 'static) {
        let _ = self.update_checked(edit);
    }
}

/// The two effects `/provider` needs. Both live in the binary crate, like
/// `SwitchFn` and `PinFn`, so the TUI depends neither on the concrete
/// providers nor on where config.toml lives.
pub struct ProviderSetup {
    /// The selected user config, to seed the form. Never the merged runtime
    /// config: a project overlay must not be copied into the selected file by
    /// saving.
    pub load: Box<dyn Fn() -> Result<Config, String> + Send + Sync>,
    /// Persist the result and rebuild everything derived from it: the active
    /// provider in the shared `ModelCell`, then both menus.
    #[allow(clippy::type_complexity)]
    pub apply: Box<dyn Fn(Config) -> Result<(ModelMenu, AgentMenu), String> + Send + Sync>,
    /// Rebuild the menus from the config already on disk, persisting nothing.
    /// Used after a `/login` changes provider availability without editing the
    /// config file.
    #[allow(clippy::type_complexity)]
    pub refresh: Box<dyn Fn() -> Result<(ModelMenu, AgentMenu), String> + Send + Sync>,
}

/// Progress of a `/login` run, delivered to the app loop from the injected
/// flow. The concrete OAuth work lives in the binary crate (like `SwitchFn`),
/// so the TUI stays free of the provider implementations.
pub enum LoginUpdate {
    /// The authorize URL is live; show it. `browser_opened` reports whether the
    /// default browser was launched, so the hint can tell the user to open it
    /// themselves when it was not.
    Started { url: String, browser_opened: bool },
    /// Terminal result: `Ok(summary)` (an email or account id) or `Err(reason)`.
    Finished(Result<String, String>),
}

/// Runs the whole ChatGPT/Codex OAuth flow, reporting progress on the channel.
/// `Arc` + boxed future so the app can spawn it on the runtime, the same shape
/// reason `VoiceInstall` is an `Arc`.
#[allow(clippy::type_complexity)]
#[derive(Clone)]
pub struct CodexLogin(
    pub  Arc<
        dyn Fn(
                tokio::sync::mpsc::Sender<LoginUpdate>,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
            + Send
            + Sync,
    >,
);

/// Fetches the voice sidecar for this platform and puts it at the given path.
/// Injected because the TUI must not know release URLs, checksums or how this
/// machine downloads things — the same reason `ProviderSetup` is a pair of
/// closures rather than a filesystem path.
///
/// `progress` is called with 0-100. Returns the reason on failure, since a
/// download that fails has to say what to do next.
/// `Arc` rather than `Box`: the download runs on a worker thread, so the
/// closure has to outlive the call that started it.
#[allow(clippy::type_complexity)]
#[derive(Clone)]
pub struct VoiceInstall(
    pub  Arc<
        dyn Fn(&'static str, PathBuf, Box<dyn FnMut(u8) + Send>) -> Result<(), String>
            + Send
            + Sync,
    >,
);

pub struct TuiConfig {
    pub menu: ModelMenu,
    pub agents: AgentMenu,
    pub presets: PresetMenu,
    pub provider_setup: ProviderSetup,
    /// Runs the ChatGPT/Codex `/login` OAuth flow off the UI thread.
    pub codex_login: CodexLogin,
    pub state_store: StateStore,
    pub opening_context: OpeningContextFn,
    pub environment: EnvironmentFn,
    pub show_reasoning: bool,
    pub skills: Vec<tcode_tools::Skill>,
    /// `[voice]`, with `enabled` already resolved against [tcode_state] in the selected config.
    pub voice: tcode_core::config::VoiceConfig,
    /// How to fetch the voice sidecar when it is not installed yet.
    pub voice_install: VoiceInstall,
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
        if on {
            if !KEY_RELEASES.swap(true, Ordering::SeqCst) {
                let _ = write_key_release_reporting(&mut stdout(), true);
            }
        } else if KEY_RELEASES.swap(false, Ordering::SeqCst) {
            let _ = write_key_release_reporting(&mut stdout(), false);
        }
    }
}

#[cfg(not(windows))]
fn write_key_release_reporting(output: &mut impl Write, on: bool) -> std::io::Result<()> {
    use crossterm::event::{
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    };

    if on {
        execute!(
            output,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::REPORT_EVENT_TYPES)
        )
    } else {
        execute!(output, PopKeyboardEnhancementFlags)
    }
}

#[cfg(unix)]
fn discard_terminal_input() {
    // Mouse reports and terminal replies can already be queued when output-side
    // modes are disabled. Drop them before restoring echo for the shell.
    unsafe {
        libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH);
    }
}

#[cfg(not(unix))]
fn discard_terminal_input() {
    // Windows console input is record-based and the output-mode teardown above
    // does not leave VT replies in the shell input stream.
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
    discard_terminal_input();
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
    use super::write_key_release_reporting;

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

    #[cfg(not(windows))]
    #[test]
    fn key_release_reporting_does_not_issue_a_terminal_capability_query() {
        let mut output = Vec::new();
        write_key_release_reporting(&mut output, true).unwrap();
        write_key_release_reporting(&mut output, false).unwrap();

        assert_eq!(String::from_utf8(output).unwrap(), "\x1b[>2u\x1b[<1u");
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
