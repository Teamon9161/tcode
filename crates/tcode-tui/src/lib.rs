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
mod syntax;
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
    KeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use tcode_core::config::ModelState;
use tcode_core::{Agent, Session};

pub use app::App;
pub use model_picker::{
    AgentMenu, AgentModelChoice, AgentRole, ApplyPresetFn, ModelMenu, ModelOption, PinFn,
    PresetDraft, PresetMenu, PresetOption, RoleSection, SavePresetFn, SwitchFn,
};
pub use tcode_core::commands::{EnvironmentFn, OpeningContextFn};
pub use tcode_frontend::{CodexLogin, LoginUpdate, ProviderSetup};

/// One read-modify-write of the selected config's `[tcode_state]`. Boxed
/// because the edit crosses a `dyn Fn` boundary, and `FnOnce` because it is
/// applied exactly once — the writer owns the read and the write around it.
type StateEdit = Box<dyn FnOnce(&mut ModelState) + Send>;

/// Runtime-state access is injected by the binary so every frontend action
/// writes the config file selected at startup, never a hard-coded home path.
#[derive(Clone)]
pub struct StateStore {
    load: Arc<dyn Fn() -> Result<ModelState, String> + Send + Sync>,
    update: Arc<dyn Fn(StateEdit) -> Result<(), String> + Send + Sync>,
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

#[derive(Clone)]
pub struct FreshSession(pub Arc<dyn Fn() -> Result<Session, String> + Send + Sync>);

pub struct TuiConfig {
    pub menu: ModelMenu,
    pub agents: AgentMenu,
    pub presets: PresetMenu,
    pub provider_setup: ProviderSetup,
    /// Runs the ChatGPT/Codex `/login` OAuth flow off the UI thread.
    pub codex_login: CodexLogin,
    pub state_store: StateStore,
    /// Creates the isolated session used by Plan review's fresh-execution handoff.
    /// The composition root owns persistence, config selection and session policy.
    pub fresh_session: FreshSession,
    pub opening_context: OpeningContextFn,
    pub environment: EnvironmentFn,
    pub show_reasoning: bool,
    pub skills: Vec<tcode_tools::Skill>,
    /// `[voice]`, with `enabled` already resolved against [tcode_state] in the selected config.
    pub voice: tcode_core::config::VoiceConfig,
    /// How to fetch the voice sidecar when it is not installed yet.
    pub voice_install: VoiceInstall,
}

/// The kitty-protocol flags we last asked the terminal to apply. A global
/// because `restore_terminal` also runs from the panic hook, where no `App`
/// is reachable; it is also what lets teardown push a full reset even when
/// voice was on at exit — a pop would only unwind one stack level.
static KEY_ENHANCEMENT_FLAGS: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// Ask the terminal to apply `flags` from the kitty keyboard protocol, or to
/// disable it entirely (`empty`). Each change *pushes* the full new state —
/// the protocol's flag stack makes "replace with this" a push — so the
/// terminal's effective flags are always exactly the last set we asked for,
/// and teardown pushes `empty` no matter how many toggles happened.
///
/// This is unconditional because the prompt editor's newline is Enter with
/// Shift held, and a terminal cannot express that modifier in the legacy
/// encoding: shift+enter is byte-for-byte the same `\r` as plain enter unless
/// `DISAMBIGUATE_ESCAPE_CODES` is active. Voice then layers
/// `REPORT_EVENT_TYPES` on top when push-to-talk is on.
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
/// release and falls back to a toggle. The disambiguation flag is irrelevant
/// there — console records carry modifier state natively.
pub(crate) fn set_key_enhancements(flags: KeyboardEnhancementFlags) {
    use std::sync::atomic::Ordering;

    let value = flags.bits();
    let old = KEY_ENHANCEMENT_FLAGS.swap(value, Ordering::SeqCst);
    if old == value {
        return;
    }
    #[cfg(windows)]
    {
        const RELEASE_BIT: u8 = KeyboardEnhancementFlags::REPORT_EVENT_TYPES.bits();
        let releases_on = flags.contains(KeyboardEnhancementFlags::REPORT_EVENT_TYPES);
        let releases_off = (old & RELEASE_BIT) != 0;
        if releases_on != releases_off {
            let request = if releases_on {
                "\x1b[?9001h"
            } else {
                "\x1b[?9001l"
            };
            let mut out = stdout();
            let _ = out.write_all(request.as_bytes());
            let _ = out.flush();
        }
    }
    #[cfg(not(windows))]
    {
        let _ = write_key_enhancements(&mut stdout(), flags);
    }
}

#[cfg(not(windows))]
fn write_key_enhancements(
    output: &mut impl Write,
    flags: KeyboardEnhancementFlags,
) -> std::io::Result<()> {
    execute!(output, PushKeyboardEnhancementFlags(flags))
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
    // Ask the terminal to report Enter/Tab/Backspace/Escape with their
    // modifiers: shift+enter is the prompt editor's newline, and the legacy
    // encoding cannot express it. Terminals without kitty-protocol support
    // ignore the request and keep sending legacy codes, so this is a strict
    // improvement wherever it lands.
    set_key_enhancements(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES);

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
    // Push a full reset rather than popping — voice may be on at exit, in
    // which case one pop would only unwind to the always-on disambiguation.
    set_key_enhancements(KeyboardEnhancementFlags::empty());
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
    use super::write_key_enhancements;

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
    fn key_enhancements_write_only_the_protocol_push_never_a_capability_query() {
        use crossterm::event::KeyboardEnhancementFlags;

        let mut output = Vec::new();
        write_key_enhancements(
            &mut output,
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES,
        )
        .unwrap();
        write_key_enhancements(
            &mut output,
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_EVENT_TYPES,
        )
        .unwrap();
        write_key_enhancements(&mut output, KeyboardEnhancementFlags::empty()).unwrap();

        // Baseline disambiguation, voice layers release reporting on top, and
        // teardown resets to nothing — never a Device Attributes query whose
        // reply could leak into the shell.
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "\x1b[>1u\x1b[>3u\x1b[>0u"
        );
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
