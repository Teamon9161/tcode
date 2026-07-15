//! Read-only integration with a local Codex CLI installation: tcode
//! reuses its ChatGPT OAuth tokens and model list instead of running
//! its own login flow. Token refresh (needs HTTP) lives in
//! tcode-providers; this module only knows the file formats.

use std::path::PathBuf;

use crate::config::ModelDef;

pub fn codex_home() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("CODEX_HOME") {
        return Some(PathBuf::from(dir));
    }
    dirs::home_dir().map(|h| h.join(".codex"))
}

#[derive(Debug, Clone)]
pub struct CodexAuth {
    pub access_token: String,
    pub refresh_token: String,
    pub account_id: String,
}

/// Load ChatGPT tokens from `~/.codex/auth.json` (`auth_mode: chatgpt`).
pub fn load_auth() -> Option<CodexAuth> {
    let path = codex_home()?.join("auth.json");
    let text = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let tokens = v.get("tokens")?;
    Some(CodexAuth {
        access_token: tokens.get("access_token")?.as_str()?.to_string(),
        refresh_token: tokens
            .get("refresh_token")
            .and_then(|t| t.as_str())
            .unwrap_or_default()
            .to_string(),
        account_id: tokens
            .get("account_id")
            .and_then(|t| t.as_str())
            .unwrap_or_default()
            .to_string(),
    })
}

pub fn auth_available() -> bool {
    load_auth().is_some()
}

/// Persist refreshed tokens back into auth.json, preserving unknown
/// fields (Codex itself rewrites this file the same way).
pub fn save_tokens(access_token: &str, refresh_token: &str, id_token: Option<&str>) {
    let Some(path) = codex_home().map(|h| h.join("auth.json")) else {
        return;
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return;
    };
    let tokens = &mut v["tokens"];
    tokens["access_token"] = access_token.into();
    if !refresh_token.is_empty() {
        tokens["refresh_token"] = refresh_token.into();
    }
    if let Some(id) = id_token {
        tokens["id_token"] = id.into();
    }
    v["last_refresh"] = chrono_now().into();
    if let Ok(out) = serde_json::to_string_pretty(&v) {
        let _ = std::fs::write(path, out);
    }
}

/// RFC3339 UTC now without pulling in chrono.
fn chrono_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Days-to-date conversion (civil-from-days algorithm).
    let days = secs / 86400;
    let (h, m, s) = ((secs / 3600) % 24, (secs / 60) % 60, secs % 60);
    let z = days as i64 + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Models Codex has cached locally (`models_cache.json`), mapped to our
/// ModelDef. Falls back to a small embedded list when the cache is
/// missing so a ChatGPT profile always has something to offer.
pub fn cached_models() -> Vec<ModelDef> {
    read_models_cache().unwrap_or_else(default_models)
}

fn read_models_cache() -> Option<Vec<ModelDef>> {
    let path = codex_home()?.join("models_cache.json");
    let text = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let models = v.get("models")?.as_array()?;
    let defs: Vec<ModelDef> = models
        .iter()
        .filter(|m| m["visibility"].as_str() == Some("list"))
        .filter_map(|m| {
            let name = m["slug"].as_str()?;
            Some(ModelDef {
                name: name.to_string(),
                label: m["display_name"].as_str().map(String::from),
                context_window: m["context_window"].as_u64(),
                max_tokens: None,
                efforts: m["supported_reasoning_levels"]
                    .as_array()
                    .map(|ls| {
                        ls.iter()
                            .filter_map(|l| l["effort"].as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default(),
                default_effort: m["default_reasoning_level"].as_str().map(String::from),
                vision: None,
            })
        })
        .collect();
    (!defs.is_empty()).then_some(defs)
}

pub fn default_models() -> Vec<ModelDef> {
    ["gpt-5.5", "gpt-5.4", "gpt-5.4-mini"]
        .into_iter()
        .map(|name| ModelDef {
            name: name.into(),
            label: None,
            context_window: Some(272_000),
            max_tokens: None,
            efforts: ["low", "medium", "high", "xhigh"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            default_effort: Some("medium".into()),
            vision: None,
        })
        .collect()
}
