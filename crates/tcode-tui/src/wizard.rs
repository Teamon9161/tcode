//! First-run setup: pick providers, paste keys, choose a default model.
//! Runs before the inline TUI exists, so it renders directly with
//! crossterm in raw mode (↑↓ move, space toggles, enter confirms).

use std::io::{stdout, Write};

use crossterm::cursor::MoveUp;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, Clear, ClearType};
use crossterm::queue;

use tcode_core::codex;
use tcode_core::config::{presets, Config, ModelState, Profile};

const CYAN: &str = "\x1b[36m";
const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";

struct RawGuard;
impl RawGuard {
    fn new() -> std::io::Result<Self> {
        enable_raw_mode()?;
        Ok(Self)
    }
}
impl Drop for RawGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

struct Candidate {
    id: &'static str,
    title: &'static str,
    status: String,
    detected: bool,
    key_env: Option<&'static str>,
}

fn candidates() -> Vec<Candidate> {
    let env_status = |var: &str| {
        if std::env::var(var).is_ok() {
            (format!("${var} ✓"), true)
        } else {
            (format!("${var} not set — you can paste a key"), false)
        }
    };
    let codex_ok = codex::auth_available();
    let (a_status, a_found) = env_status("ANTHROPIC_API_KEY");
    let (o_status, o_found) = env_status("OPENAI_API_KEY");
    let (d_status, d_found) = env_status("DEEPSEEK_API_KEY");
    vec![
        Candidate {
            id: "chatgpt",
            title: "ChatGPT subscription (reuse Codex login)",
            status: if codex_ok {
                "~/.codex/auth.json ✓".into()
            } else {
                "not logged in — run `codex login` first".into()
            },
            detected: codex_ok,
            key_env: None,
        },
        Candidate {
            id: "anthropic",
            title: "Anthropic API",
            status: a_status,
            detected: a_found,
            key_env: Some("ANTHROPIC_API_KEY"),
        },
        Candidate {
            id: "openai",
            title: "OpenAI API",
            status: o_status,
            detected: o_found,
            key_env: Some("OPENAI_API_KEY"),
        },
        Candidate {
            id: "deepseek",
            title: "DeepSeek (Anthropic-compatible endpoint)",
            status: d_status,
            detected: d_found,
            key_env: Some("DEEPSEEK_API_KEY"),
        },
    ]
}

/// Returns None if the user cancelled. On success the caller writes the
/// config and state to disk.
pub fn run() -> anyhow::Result<Option<(Config, ModelState)>> {
    println!("{BOLD}tcode setup{RESET} {DIM}— no config found, let's create ~/.tcode/config.toml{RESET}");
    println!("{DIM}(you can rerun this any time by deleting that file, or just edit it){RESET}\n");

    let cands = candidates();
    let items: Vec<(String, bool)> = cands
        .iter()
        .map(|c| (format!("{}  {DIM}{}{RESET}", c.title, c.status), c.detected))
        .collect();
    let Some(chosen) = multi_select("providers to configure (space = toggle):", &items)? else {
        return Ok(None);
    };
    if chosen.is_empty() {
        println!("{DIM}nothing selected — setup cancelled{RESET}");
        return Ok(None);
    }

    let mut config = Config::default();
    for &i in &chosen {
        let cand = &cands[i];
        let inline_key = match cand.key_env {
            Some(var) if std::env::var(var).is_err() => {
                let key = read_secret(&format!(
                    "{} API key {DIM}(enter = rely on ${var} later){RESET}: ",
                    cand.title
                ))?;
                key.filter(|k| !k.trim().is_empty()).map(|k| k.trim().to_string())
            }
            _ => None,
        };
        let profile: Profile = match cand.id {
            "chatgpt" => presets::chatgpt(),
            "anthropic" => presets::anthropic(inline_key),
            "openai" => presets::openai(inline_key),
            "deepseek" => presets::deepseek(inline_key),
            _ => unreachable!(),
        };
        config.profiles.insert(cand.id.to_string(), profile);
    }

    // Default model across everything just configured.
    let mut options: Vec<(String, String, Option<String>)> = Vec::new();
    let mut labels: Vec<String> = Vec::new();
    for (pname, profile) in &config.profiles {
        for def in profile.model_defs() {
            labels.push(format!("{pname} · {}", def.display()));
            options.push((pname.clone(), def.name.clone(), def.default_effort.clone()));
        }
    }
    let default_idx = match options.len() {
        0 => {
            println!("{DIM}selected providers expose no models — edit config.toml manually{RESET}");
            None
        }
        1 => Some(0),
        _ => select_one("default model:", &labels, 0)?,
    };
    let Some(idx) = default_idx.or((!options.is_empty()).then_some(0)) else {
        // No models at all (e.g. only ChatGPT chosen without codex
        // installed); still write the config so the user can finish up.
        config.default_profile = chosen.first().map(|&i| cands[i].id.to_string());
        return Ok(Some((config, ModelState::default())));
    };
    let (profile, model, effort) = options[idx].clone();
    config.default_profile = Some(profile.clone());
    let state = ModelState {
        profile: Some(profile),
        model: Some(model),
        effort,
    };
    Ok(Some((config, state)))
}

fn read_key() -> std::io::Result<KeyEvent> {
    loop {
        if let Event::Key(k) = crossterm::event::read()? {
            if k.kind != KeyEventKind::Release {
                return Ok(k);
            }
        }
    }
}

fn is_cancel(k: &KeyEvent) -> bool {
    k.code == KeyCode::Esc
        || (k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL))
}

/// Space toggles, enter confirms. None = cancelled.
fn multi_select(title: &str, items: &[(String, bool)]) -> anyhow::Result<Option<Vec<usize>>> {
    let mut on: Vec<bool> = items.iter().map(|(_, d)| *d).collect();
    let mut cursor = 0usize;
    let _guard = RawGuard::new()?;
    let mut out = stdout();
    let height = items.len() as u16 + 2;
    let mut first = true;
    loop {
        if !first {
            queue!(out, MoveUp(height))?;
        }
        first = false;
        write!(out, "\r{}", Clear(ClearType::FromCursorDown))?;
        write!(out, "{BOLD}{title}{RESET}\r\n")?;
        for (i, (label, _)) in items.iter().enumerate() {
            let ptr = if i == cursor { "▸" } else { " " };
            let mark = if on[i] { format!("{CYAN}[x]{RESET}") } else { "[ ]".into() };
            write!(out, " {ptr} {mark} {label}\r\n")?;
        }
        write!(out, "{DIM}   ↑↓ move · space toggle · enter confirm · esc cancel{RESET}\r\n")?;
        out.flush()?;
        let k = read_key()?;
        if is_cancel(&k) {
            return Ok(None);
        }
        match k.code {
            KeyCode::Up => cursor = cursor.saturating_sub(1),
            KeyCode::Down => cursor = (cursor + 1).min(items.len() - 1),
            KeyCode::Char(' ') => on[cursor] = !on[cursor],
            KeyCode::Enter => {
                return Ok(Some(
                    (0..items.len()).filter(|&i| on[i]).collect::<Vec<_>>(),
                ))
            }
            _ => {}
        }
    }
}

fn select_one(title: &str, items: &[String], start: usize) -> anyhow::Result<Option<usize>> {
    let mut cursor = start.min(items.len().saturating_sub(1));
    let _guard = RawGuard::new()?;
    let mut out = stdout();
    let height = items.len() as u16 + 2;
    let mut first = true;
    loop {
        if !first {
            queue!(out, MoveUp(height))?;
        }
        first = false;
        write!(out, "\r{}", Clear(ClearType::FromCursorDown))?;
        write!(out, "{BOLD}{title}{RESET}\r\n")?;
        for (i, label) in items.iter().enumerate() {
            if i == cursor {
                write!(out, " {CYAN}▸ {label}{RESET}\r\n")?;
            } else {
                write!(out, "   {label}\r\n")?;
            }
        }
        write!(out, "{DIM}   ↑↓ move · enter confirm · esc cancel{RESET}\r\n")?;
        out.flush()?;
        let k = read_key()?;
        if is_cancel(&k) {
            return Ok(None);
        }
        match k.code {
            KeyCode::Up => cursor = cursor.saturating_sub(1),
            KeyCode::Down => cursor = (cursor + 1).min(items.len() - 1),
            KeyCode::Enter => return Ok(Some(cursor)),
            _ => {}
        }
    }
}

/// Line input with masked echo (keys are secrets). None = cancelled.
fn read_secret(prompt: &str) -> anyhow::Result<Option<String>> {
    let _guard = RawGuard::new()?;
    let mut out = stdout();
    let mut buf = String::new();
    loop {
        write!(out, "\r{}{prompt}{}", Clear(ClearType::CurrentLine), "•".repeat(buf.chars().count()))?;
        out.flush()?;
        let k = read_key()?;
        if is_cancel(&k) {
            write!(out, "\r\n")?;
            return Ok(None);
        }
        match k.code {
            KeyCode::Enter => {
                write!(out, "\r\n")?;
                return Ok(Some(buf));
            }
            KeyCode::Backspace => {
                buf.pop();
            }
            KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => buf.push(c),
            _ => {}
        }
    }
}

/// Non-interactive fallback config (pipes/CI): env-key profiles only.
pub fn default_config() -> Config {
    let mut config = Config::default();
    config
        .profiles
        .insert("anthropic".into(), presets::anthropic(None));
    config.profiles.insert("openai".into(), presets::openai(None));
    if codex::auth_available() {
        config.profiles.insert("chatgpt".into(), presets::chatgpt());
    }
    config.default_profile = Some("anthropic".into());
    config
}
