//! First-run setup: pick providers, paste keys, choose a default model.
//! Runs before the inline TUI exists, so it renders directly with
//! crossterm in raw mode (↑↓ move, space toggles, enter confirms).

use std::io::{stdout, Write};

use crossterm::cursor::MoveTo;
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, Clear, ClearType};
use crossterm::{execute, queue};

use tcode_core::codex;
use tcode_core::config::{presets, Config, ModelState, Profile};

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

struct Candidate {
    id: &'static str,
    title: &'static str,
    status: String,
    detected: bool,
    key_env: Option<&'static str>,
}

fn candidates(profiles: &std::collections::BTreeMap<String, Profile>) -> Vec<Candidate> {
    let env_or_inline = |var: &str, id: &str| {
        if std::env::var(var).is_ok() {
            (format!("{GREEN}${var}{RESET} ✓"), true)
        } else if profiles.get(id).is_some_and(|p| p.api_key.is_some()) {
            (format!("{GREEN}inline key{RESET} ✓"), true)
        } else {
            (format!("{DIM}not configured{RESET}"), false)
        }
    };
    let codex_ok = codex::auth_available();
    let (a_status, a_found) = env_or_inline("ANTHROPIC_API_KEY", "anthropic");
    let (o_status, o_found) = env_or_inline("OPENAI_API_KEY", "openai");
    let (d_status, d_found) = env_or_inline("DEEPSEEK_API_KEY", "deepseek");
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
    run_with(Config::default(), None)
}

/// Reconfigure a profile whose credentials are missing. Existing profiles and
/// global settings are retained; the user can choose a different provider.
pub fn reconfigure(
    config: Config,
    missing_profile: &str,
) -> anyhow::Result<Option<(Config, ModelState)>> {
    run_with(config, Some(missing_profile))
}

fn run_with(
    mut config: Config,
    missing_profile: Option<&str>,
) -> anyhow::Result<Option<(Config, ModelState)>> {
    match missing_profile {
        Some(profile) => println!(
            "{BOLD}tcode setup{RESET} {DIM}— profile '{profile}' is not configured; choose a provider{RESET}"
        ),
        None => println!(
            "{BOLD}tcode setup{RESET} {DIM}— no config found, let's create ~/.tcode/config.toml{RESET}"
        ),
    }
    println!(
        "{DIM}keys are saved in ~/.tcode/config.toml; environment variables also work{RESET}\n"
    );

    let cands = candidates(&config.profiles);
    let Some(chosen) = select_providers(&cands, &config.profiles, missing_profile)? else {
        return Ok(None);
    };
    if chosen.is_empty() {
        println!("{DIM}nothing selected — setup cancelled{RESET}");
        return Ok(None);
    }

    for &(i, ref inline_key) in &chosen {
        let cand = &cands[i];
        if let Some(profile) = config.profiles.get_mut(cand.id) {
            if let Some(key) = inline_key {
                profile.api_key = Some(key.clone());
            }
        } else {
            let profile: Profile = match cand.id {
                "chatgpt" => presets::chatgpt(),
                "anthropic" => presets::anthropic(inline_key.clone()),
                "openai" => presets::openai(inline_key.clone()),
                "deepseek" => presets::deepseek(inline_key.clone()),
                _ => unreachable!(),
            };
            config.profiles.insert(cand.id.to_string(), profile);
        }
    }

    // Default model across everything just configured.
    let mut options: Vec<(String, String, Option<String>)> = Vec::new();
    let mut labels: Vec<String> = Vec::new();
    for &(i, _) in &chosen {
        let pname = cands[i].id;
        let profile = config
            .profiles
            .get(pname)
            .expect("selected provider is configured");
        for def in profile.model_defs() {
            labels.push(format!("{pname} · {}", def.display()));
            options.push((
                pname.to_string(),
                def.name.clone(),
                def.default_effort.clone(),
            ));
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
        config.default_profile = chosen.first().map(|&(i, _)| cands[i].id.to_string());
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

/// Candidate indices chosen in the wizard, each with an optional inline
/// API key (None = use the env var).
type ProviderSelection = Vec<(usize, Option<String>)>;

/// Select providers and optionally enter API keys inline via Tab.
/// None = user cancelled.
fn select_providers(
    cands: &[Candidate],
    profiles: &std::collections::BTreeMap<String, Profile>,
    missing_profile: Option<&str>,
) -> anyhow::Result<Option<ProviderSelection>> {
    #[derive(Clone)]
    struct Entry {
        selected: bool,
        key: Option<String>,
    }

    let initial_selected = |i: usize| -> bool {
        missing_profile
            .map(|profile| cands[i].id == profile || cands[i].detected)
            .unwrap_or(cands[i].detected)
    };

    let mut entries: Vec<Entry> = (0..cands.len())
        .map(|i| {
            let existing_key = profiles.get(cands[i].id).and_then(|p| p.api_key.clone());
            Entry {
                selected: initial_selected(i),
                key: existing_key,
            }
        })
        .collect();

    let mut cursor = 0usize;
    let _guard = RawGuard::new()?;
    let mut out = stdout();
    loop {
        queue!(out, MoveTo(0, 0), Clear(ClearType::All))?;
        write!(
            out,
            "{BOLD}providers to configure{RESET}  {DIM}(space = toggle, tab = set key){RESET}\r\n"
        )?;
        for (i, cand) in cands.iter().enumerate() {
            let ptr = if i == cursor { "▸" } else { " " };
            let mark = if entries[i].selected {
                format!("{CYAN}[x]{RESET}")
            } else {
                "[ ]".into()
            };
            // Show key status
            let key_status = if let Some(key) = &entries[i].key {
                format!("{GREEN}key set{RESET} {DIM}({} chars){RESET}", key.len())
            } else {
                let has_env = cand.key_env.is_some_and(|v| std::env::var(v).is_ok());
                if has_env {
                    format!("{GREEN}${}{RESET}", cand.key_env.unwrap())
                } else {
                    cand.status.clone()
                }
            };
            // If the candidate has an inline key from the config already,
            // the "key set" display above already matches. If there's no entry
            // key and no env var, use the candidate's pre-computed status from
            // candidates() which checks both env and config.
            // The status below is only reached when both entry.key and env are absent.

            write!(out, " {ptr} {mark} {}  {key_status}\r\n", cand.title)?;
        }
        write!(
            out,
            "{DIM}   ↑↓ move · space toggle · tab key · enter confirm · esc cancel{RESET}\r\n"
        )?;
        out.flush()?;
        let k = read_key()?;
        if is_cancel(&k) {
            return Ok(None);
        }
        match k.code {
            KeyCode::Up => cursor = cursor.saturating_sub(1),
            KeyCode::Down => cursor = (cursor + 1).min(cands.len() - 1),
            KeyCode::Char(' ') => entries[cursor].selected = !entries[cursor].selected,
            KeyCode::Tab => {
                // Inline key input for the current candidate
                let cand = &cands[cursor];
                let var = cand.key_env.unwrap_or("API_KEY");
                let key = read_inline_key(cand.id, var, &mut out)?;
                match key {
                    InlineKeyResult::Set(k) => {
                        entries[cursor].key = Some(k);
                        entries[cursor].selected = true;
                    }
                    InlineKeyResult::Skip => {
                        entries[cursor].key = None;
                        entries[cursor].selected = true;
                    }
                    InlineKeyResult::Cancelled => {}
                }
            }
            KeyCode::Enter => {
                let result: Vec<(usize, Option<String>)> = entries
                    .iter()
                    .enumerate()
                    .filter(|(_, e)| e.selected)
                    .map(|(i, e)| (i, e.key.clone()))
                    .collect();
                return Ok(Some(result));
            }
            _ => {}
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
enum InlineKeyResult {
    Set(String),
    Skip,
    Cancelled,
}

/// Inline key input that appears at the bottom of the selection screen.
/// Returns the key, "skip" (use env var), or "cancelled".
fn read_inline_key(
    label: &str,
    var: &str,
    out: &mut std::io::Stdout,
) -> anyhow::Result<InlineKeyResult> {
    let mut buf = String::new();
    let mut pasted = false;
    loop {
        let hint = if pasted {
            "  [pasted · Enter to confirm]"
        } else if buf.is_empty() {
            "  [type or paste (Ctrl+Shift+V) · Esc skip]"
        } else {
            "  [Enter confirm · Esc skip]"
        };
        queue!(out, MoveTo(0, 0), Clear(ClearType::All))?;
        write!(
            out,
            "{BOLD}{label} API key{RESET}  {DIM}(empty = ${var}){RESET}\r\n\r\n"
        )?;
        if buf.is_empty() {
            write!(out, "  {DIM}<type or paste key here>{RESET}")?;
        } else {
            write!(out, "  {}", "•".repeat(buf.chars().count()))?;
        }
        write!(out, "{hint}")?;
        out.flush()?;
        match crossterm::event::read()? {
            Event::Paste(text) => {
                let text = text.trim();
                if !text.is_empty() {
                    buf.push_str(text);
                    pasted = true;
                }
            }
            Event::Key(k) if k.kind != KeyEventKind::Release => {
                if is_cancel(&k) {
                    // Esc: skip (go back to selection without setting key)
                    return Ok(InlineKeyResult::Cancelled);
                }
                match k.code {
                    KeyCode::Enter => {
                        let trimmed = buf.trim().to_string();
                        return if trimmed.is_empty() {
                            Ok(InlineKeyResult::Skip)
                        } else {
                            Ok(InlineKeyResult::Set(trimmed))
                        };
                    }
                    KeyCode::Backspace => {
                        buf.pop();
                        pasted = false;
                    }
                    KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => {
                        buf.push(c);
                    }
                    KeyCode::Tab => {
                        // Tab during input = confirm (same as Enter)
                        let trimmed = buf.trim().to_string();
                        return if trimmed.is_empty() {
                            Ok(InlineKeyResult::Skip)
                        } else {
                            Ok(InlineKeyResult::Set(trimmed))
                        };
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

fn select_one(title: &str, items: &[String], start: usize) -> anyhow::Result<Option<usize>> {
    let mut cursor = start.min(items.len().saturating_sub(1));
    let _guard = RawGuard::new()?;
    let mut out = stdout();
    loop {
        queue!(out, MoveTo(0, 0), Clear(ClearType::All))?;
        write!(out, "{BOLD}{title}{RESET}\r\n")?;
        for (i, label) in items.iter().enumerate() {
            if i == cursor {
                write!(out, " {CYAN}▸ {label}{RESET}\r\n")?;
            } else {
                write!(out, "   {label}\r\n")?;
            }
        }
        write!(
            out,
            "{DIM}   ↑↓ move · enter confirm · esc cancel{RESET}\r\n"
        )?;
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

/// Non-interactive fallback config (pipes/CI): env-key profiles only.
pub fn default_config() -> Config {
    let mut config = Config::default();
    config
        .profiles
        .insert("anthropic".into(), presets::anthropic(None));
    config
        .profiles
        .insert("openai".into(), presets::openai(None));
    if codex::auth_available() {
        config.profiles.insert("chatgpt".into(), presets::chatgpt());
    }
    config.default_profile = Some("anthropic".into());
    config
}
