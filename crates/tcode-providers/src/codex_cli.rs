//! Local Codex CLI installation adapter. This is deliberately owned by the
//! Codex provider: file layout, OAuth tokens, and cached model metadata are not
//! tcode-core configuration semantics.

use std::path::PathBuf;

use tcode_core::config::{Config, ModelDef, ProviderKind};

/// OpenAI's OAuth token endpoint, shared by the login and refresh grants.
pub(crate) const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
/// The subscription model catalogue — the same endpoint the Codex CLI polls to
/// populate `models_cache.json`. Same auth as `/responses`.
const MODELS_URL: &str = "https://chatgpt.com/backend-api/codex/models";
/// Codex CLI's public OAuth client id. Both the login authorization_code grant
/// and the refresh_token grant are tied to it, and tcode reuses it so the
/// tokens it mints are the same ones a `codex login` would produce.
pub(crate) const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

pub(crate) fn codex_home() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("CODEX_HOME") {
        return Some(PathBuf::from(dir));
    }
    dirs::home_dir().map(|h| h.join(".codex"))
}

#[derive(Debug, Clone)]
pub(crate) struct CodexAuth {
    pub access_token: String,
    pub refresh_token: String,
    pub account_id: String,
}

pub(crate) fn load_auth() -> Option<CodexAuth> {
    let path = codex_home()?.join("auth.json");
    let text = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let tokens = value.get("tokens")?;
    Some(CodexAuth {
        access_token: tokens.get("access_token")?.as_str()?.to_string(),
        refresh_token: tokens
            .get("refresh_token")
            .and_then(|token| token.as_str())
            .unwrap_or_default()
            .to_string(),
        account_id: tokens
            .get("account_id")
            .and_then(|token| token.as_str())
            .unwrap_or_default()
            .to_string(),
    })
}

pub fn codex_auth_available() -> bool {
    load_auth().is_some()
}

/// Write a freshly-minted credential set to `~/.codex/auth.json`, creating the
/// file (and `~/.codex/`) if needed. Shape matches what the Codex CLI writes,
/// so a later `codex` install reads tcode's login and vice versa: a top-level
/// `OPENAI_API_KEY` (null for the subscription path), a `tokens` object, and a
/// `last_refresh` stamp. Any user fields in an existing file are preserved.
pub(crate) fn write_auth(
    access_token: &str,
    refresh_token: &str,
    id_token: &str,
    account_id: &str,
) -> Result<(), String> {
    let home = codex_home().ok_or("cannot locate home directory")?;
    std::fs::create_dir_all(&home).map_err(|e| format!("create {}: {e}", home.display()))?;
    let path = home.join("auth.json");
    // Preserve anything already in the file (e.g. an OPENAI_API_KEY the user set)
    // rather than clobbering the document.
    let mut value: serde_json::Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    if !value.is_object() {
        value = serde_json::json!({});
    }
    if value.get("OPENAI_API_KEY").is_none() {
        value["OPENAI_API_KEY"] = serde_json::Value::Null;
    }
    value["tokens"] = serde_json::json!({
        "id_token": id_token,
        "access_token": access_token,
        "refresh_token": refresh_token,
        "account_id": account_id,
    });
    value["last_refresh"] = rfc3339_now().into();
    let text = serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?;
    std::fs::write(&path, text).map_err(|e| format!("write {}: {e}", path.display()))
}

/// Persist refreshed tokens without discarding fields owned by the Codex CLI.
pub(crate) fn save_tokens(access_token: &str, refresh_token: &str, id_token: Option<&str>) {
    let Some(path) = codex_home().map(|home| home.join("auth.json")) else {
        return;
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return;
    };
    let tokens = &mut value["tokens"];
    tokens["access_token"] = access_token.into();
    if !refresh_token.is_empty() {
        tokens["refresh_token"] = refresh_token.into();
    }
    if let Some(id_token) = id_token {
        tokens["id_token"] = id_token.into();
    }
    value["last_refresh"] = rfc3339_now().into();
    if let Ok(output) = serde_json::to_string_pretty(&value) {
        let _ = std::fs::write(path, output);
    }
}

/// Fill empty Codex profile catalogues before core resolves selections. Explicit
/// user models always win over the CLI cache and fallback list.
pub fn hydrate_codex_models(config: &mut Config) {
    for profile in config.profiles.values_mut() {
        if profile.provider == Some(ProviderKind::Codex)
            && profile.models.is_empty()
            && profile.model.is_none()
        {
            profile.models = cached_models();
        }
    }
}

fn cached_models() -> Vec<ModelDef> {
    read_models_cache().unwrap_or_else(default_models)
}

/// Fetch the live subscription catalogue and write it to `models_cache.json`,
/// so the next startup reads real models instead of the fallback list. Same
/// file and shape the Codex CLI maintains. Best-effort: any failure returns an
/// error the caller ignores, leaving the fallback in place.
pub(crate) async fn refresh_models_cache(
    http: &reqwest::Client,
    access_token: &str,
    account_id: &str,
) -> Result<(), String> {
    let url = format!("{MODELS_URL}?client_version={}", env!("CARGO_PKG_VERSION"));
    let resp = http
        .get(url)
        .bearer_auth(access_token)
        .header("chatgpt-account-id", account_id)
        .header("OpenAI-Beta", "responses=experimental")
        .header("originator", "codex_cli_rs")
        .send()
        .await
        .map_err(|e| format!("model list request failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("model list request failed: {}", resp.status()));
    }
    let value: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("model list response: {e}"))?;
    // The endpoint may return a bare array or a `{ "models": [...] }` object;
    // the cache file is always the object form.
    let cache = if value.get("models").is_some() {
        value
    } else {
        serde_json::json!({ "models": value })
    };
    if models_from_cache(&cache).is_none() {
        return Err("model list contained no listable models".into());
    }
    let path = codex_home()
        .ok_or("cannot locate home directory")?
        .join("models_cache.json");
    let text = serde_json::to_string_pretty(&cache).map_err(|e| e.to_string())?;
    std::fs::write(&path, text).map_err(|e| format!("write {}: {e}", path.display()))
}

fn read_models_cache() -> Option<Vec<ModelDef>> {
    let path = codex_home()?.join("models_cache.json");
    let text = std::fs::read_to_string(path).ok()?;
    models_from_cache(&serde_json::from_str(&text).ok()?)
}

fn models_from_cache(value: &serde_json::Value) -> Option<Vec<ModelDef>> {
    let models = value.get("models")?.as_array()?;
    let defs: Vec<ModelDef> = models
        .iter()
        .filter(|model| model["visibility"].as_str() == Some("list"))
        .filter_map(|model| {
            let name = model["slug"].as_str()?;
            let context_window = model["context_window"].as_u64()?;
            // Codex's subscription catalogue may reserve a portion of the raw
            // model window. Use the same effective budget it advertises to its
            // own clients, not the public API model specification.
            let effective_percent = model["effective_context_window_percent"]
                .as_u64()
                .unwrap_or(100)
                .min(100);
            Some(ModelDef {
                name: name.to_string(),
                label: model["display_name"].as_str().map(String::from),
                context_window: Some(context_window * effective_percent / 100),
                max_tokens: None,
                efforts: model["supported_reasoning_levels"]
                    .as_array()
                    .map(|levels| {
                        levels
                            .iter()
                            .filter_map(|level| level["effort"].as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default(),
                default_effort: model["default_reasoning_level"].as_str().map(String::from),
                vision: None,
            })
        })
        .collect();
    (!defs.is_empty()).then_some(defs)
}

fn default_models() -> Vec<ModelDef> {
    ["gpt-5.5", "gpt-5.4", "gpt-5.4-mini"]
        .into_iter()
        .map(|name| ModelDef {
            name: name.into(),
            label: None,
            context_window: Some(272_000),
            max_tokens: None,
            efforts: ["low", "medium", "high", "xhigh"]
                .iter()
                .map(|effort| effort.to_string())
                .collect(),
            default_effort: Some("medium".into()),
            vision: None,
        })
        .collect()
}

fn rfc3339_now() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let days = seconds / 86_400;
    let (hours, minutes, seconds) = ((seconds / 3600) % 24, (seconds / 60) % 60, seconds % 60);
    let z = days as i64 + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let month_index = (5 * doy + 2) / 153;
    let day = doy - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = if month <= 2 { year + 1 } else { year };
    format!("{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{seconds:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_model_uses_codex_effective_context_window() {
        let models = models_from_cache(&serde_json::json!({
            "models": [{
                "slug": "gpt-5.6-sol",
                "display_name": "GPT-5.6 Sol",
                "visibility": "list",
                "context_window": 272_000,
                "effective_context_window_percent": 95
            }]
        }))
        .expect("one visible model");
        assert_eq!(models[0].context_window, Some(258_400));
    }

    #[test]
    fn hydration_only_fills_an_empty_codex_catalogue() {
        let mut config = Config::defaults();
        let codex = config.profiles.get_mut("codex").unwrap();
        codex.models = vec![ModelDef::bare("user-model")];
        hydrate_codex_models(&mut config);
        assert_eq!(config.profiles["codex"].models[0].name, "user-model");
    }
}
