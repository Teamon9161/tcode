mod anthropic;
mod codex;
mod codex_cli;
mod codex_login;
mod http;
mod idle;
mod openai;
mod retry;

pub use anthropic::AnthropicProvider;
pub use codex::CodexProvider;
pub use codex_cli::{codex_auth_available, hydrate_codex_models};
pub use codex_login::{
    open_browser as open_login_browser, start as start_codex_login, LoginHandle, LoginOutcome,
};
pub use openai::OpenAiProvider;

use std::sync::Arc;

use tcode_core::config::{ModelDef, Profile, ProviderKind, Selection, WatchdogConfig};
use tcode_core::{ActiveModel, Provider};

/// Whether this profile can be used now. Provider-local credential sources
/// remain inside their adapter rather than leaking into core configuration.
pub fn profile_is_usable(profile_name: &str, profile: &Profile) -> bool {
    match profile.provider {
        Some(ProviderKind::Codex) => codex_auth_available(),
        Some(ProviderKind::Anthropic | ProviderKind::Openai) => {
            profile.api_key(profile_name).is_ok()
        }
        // Only a profile whose layers never named a provider; `Config::load`
        // rejects it, so it can never be the active one.
        None => false,
    }
}

/// Assemble a provider for one model of a profile. `profile_name` is
/// only used for error messages.
pub fn build(
    profile_name: &str,
    profile: &Profile,
    model: &ModelDef,
    watchdog: &WatchdogConfig,
) -> Result<Arc<dyn Provider>, tcode_core::config::ConfigError> {
    let kind = profile
        .provider
        .ok_or_else(|| tcode_core::config::ConfigError::NoProvider(profile_name.to_string()))?;
    Ok(match kind {
        ProviderKind::Anthropic => Arc::new(
            AnthropicProvider::new(
                profile.api_key(profile_name)?,
                model.name.clone(),
                profile.base_url.clone(),
                watchdog.clone(),
            )
            .with_vision(model.vision.or(profile.vision).unwrap_or(true)),
        ),
        ProviderKind::Openai => Arc::new(
            OpenAiProvider::new(
                profile.api_key(profile_name)?,
                model.name.clone(),
                profile.base_url.clone(),
                watchdog.clone(),
            )
            .with_vision(model.vision.or(profile.vision).unwrap_or(true)),
        ),
        ProviderKind::Codex => Arc::new(
            CodexProvider::new(model.name.clone(), watchdog.clone())
                .with_vision(model.vision.or(profile.vision).unwrap_or(true)),
        ),
    })
}

/// Resolve a full `Selection` into the swappable model handle contents.
pub fn build_active(
    profile: &Profile,
    selection: &Selection,
    watchdog: &WatchdogConfig,
) -> Result<ActiveModel, tcode_core::config::ConfigError> {
    let provider = build(&selection.profile, profile, &selection.model, watchdog)?;
    Ok(ActiveModel {
        provider,
        max_tokens: selection
            .model
            .max_tokens
            .or(profile.max_tokens)
            .unwrap_or(8192),
        context_window: selection
            .model
            .context_window
            .or(profile.context_window)
            .unwrap_or(200_000),
        effort: selection.effort.clone(),
    })
}
