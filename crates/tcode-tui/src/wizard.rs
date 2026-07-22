//! First-run setup, drawn straight to the terminal with crossterm.
//!
//! This renderer exists because `App::new` needs an `Arc<Agent>`, i.e. a
//! provider that was already built — precisely what setup is there to
//! produce. So the first run and a startup with unusable credentials cannot
//! be an overlay. Everything it decides lives in `setup.rs`; the in-session
//! `/provider` overlay drives the same state machine.

use std::io::{stdout, Write};

use crossterm::cursor::MoveTo;
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste, Event, KeyEventKind};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, Clear, ClearType};
use crossterm::{execute, queue};

use std::path::Path;

use tcode_core::config::Config;

use crate::setup::{Mark, Progress, Row, Setup, Tone, View};

const CYAN: &str = "\x1b[36m";
const GREEN: &str = "\x1b[32m";
const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";

struct RawGuard;
impl RawGuard {
    fn new() -> std::io::Result<Self> {
        enable_raw_mode()?;
        if let Err(error) = execute!(stdout(), EnableBracketedPaste) {
            let _ = disable_raw_mode();
            return Err(error);
        }
        Ok(Self)
    }
}
impl Drop for RawGuard {
    fn drop(&mut self) {
        let _ = execute!(stdout(), DisableBracketedPaste);
        let _ = disable_raw_mode();
    }
}

/// Returns None if the user cancelled. On success the caller writes the
/// selected config file to disk.
pub fn run(config_file: &Path) -> anyhow::Result<Option<Config>> {
    println!(
        "{BOLD}tcode setup{RESET} {DIM}— no config found, let's create {}{RESET}",
        config_file.display()
    );
    run_setup(Setup::new(Config::default(), None), config_file)
}

/// Reconfigure a profile whose credentials are missing. Existing profiles and
/// selected user settings are retained; the user can choose a different
/// provider.
pub fn reconfigure(
    config: Config,
    missing_profile: &str,
    config_file: &Path,
) -> anyhow::Result<Option<Config>> {
    println!(
        "{BOLD}tcode setup{RESET} {DIM}— profile '{missing_profile}' is not configured; choose a provider{RESET}"
    );
    run_setup(Setup::new(config, Some(missing_profile)), config_file)
}

fn run_setup(mut setup: Setup, config_file: &Path) -> anyhow::Result<Option<Config>> {
    println!(
        "{DIM}keys are saved in {}; environment variables also work{RESET}\n",
        config_file.display()
    );
    let _guard = RawGuard::new()?;
    let mut out = stdout();
    loop {
        draw(&mut out, &setup.view())?;
        let progress = match crossterm::event::read()? {
            Event::Key(key) if key.kind != KeyEventKind::Release => setup.on_key(key),
            Event::Paste(text) => setup.on_paste(text),
            _ => Progress::Stay,
        };
        if let Progress::Done(outcome) = progress {
            queue!(out, MoveTo(0, 0), Clear(ClearType::All))?;
            out.flush()?;
            if outcome.is_none() {
                println!("{DIM}setup cancelled{RESET}");
            }
            return Ok(outcome.map(|boxed| *boxed));
        }
    }
}

fn draw(out: &mut std::io::Stdout, view: &View) -> std::io::Result<()> {
    queue!(out, MoveTo(0, 0), Clear(ClearType::All))?;
    write!(out, "{BOLD}{}{RESET}\r\n", view.title)?;
    for row in &view.rows {
        write!(out, "{}\r\n", line(row))?;
    }
    write!(out, "{DIM}   {}{RESET}\r\n", view.hint)?;
    out.flush()
}

fn line(row: &Row) -> String {
    let pointer = if row.active { "▸" } else { " " };
    let mark = match row.mark {
        Mark::Checked => format!(" {CYAN}[x]{RESET}"),
        Mark::Unchecked => " [ ]".into(),
        Mark::None => String::new(),
    };
    let label = if row.active && row.mark == Mark::None {
        format!("{CYAN}{}{RESET}", row.label)
    } else {
        row.label.clone()
    };
    let status = match (row.status.is_empty(), row.tone) {
        (true, _) => String::new(),
        (false, Tone::Ok) => format!("  {GREEN}{}{RESET}", row.status),
        (false, Tone::Dim) => format!("  {DIM}{}{RESET}", row.status),
    };
    format!(" {pointer}{mark} {label}{status}")
}

/// Non-interactive fallback config (pipes/CI): all profiles from the
/// built-in catalogue, defaulting to "anthropic".
pub fn default_config() -> Config {
    let mut config = Config::defaults();
    config.default_profile = Some("anthropic".into());
    config
}
