mod anthropic;
mod chatgpt;
mod openai;
mod retry;

pub use anthropic::AnthropicProvider;
pub use chatgpt::ChatGptProvider;
pub use openai::OpenAiProvider;

use std::sync::Arc;

use tcode_core::config::{ModelDef, Profile, ProviderKind, Selection, WatchdogConfig};
use tcode_core::{ActiveModel, Provider};

/// Assemble a provider for one model of a profile. `profile_name` is
/// only used for error messages.
pub fn build(
    profile_name: &str,
    profile: &Profile,
    model: &ModelDef,
    watchdog: &WatchdogConfig,
) -> Result<Arc<dyn Provider>, tcode_core::config::ConfigError> {
    Ok(match profile.provider {
        ProviderKind::Anthropic => Arc::new(AnthropicProvider::new(
            profile.api_key(profile_name)?,
            model.name.clone(),
            profile.base_url.clone(),
            watchdog.clone(),
        )),
        ProviderKind::Openai => Arc::new(OpenAiProvider::new(
            profile.api_key(profile_name)?,
            model.name.clone(),
            profile.base_url.clone(),
            watchdog.clone(),
        )),
        ProviderKind::Chatgpt => {
            Arc::new(ChatGptProvider::new(model.name.clone(), watchdog.clone()))
        }
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
