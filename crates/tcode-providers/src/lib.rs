mod anthropic;
mod openai;
mod retry;

pub use anthropic::AnthropicProvider;
pub use openai::OpenAiProvider;

use std::sync::Arc;

use tcode_core::config::{Profile, ProviderKind, WatchdogConfig};
use tcode_core::Provider;

/// Assemble a provider from a config profile. `profile_name` is only
/// used for error messages.
pub fn build(
    profile_name: &str,
    profile: &Profile,
    watchdog: &WatchdogConfig,
    model_override: Option<String>,
) -> Result<Arc<dyn Provider>, tcode_core::config::ConfigError> {
    let api_key = profile.api_key(profile_name)?;
    let model = model_override.unwrap_or_else(|| profile.model.clone());
    Ok(match profile.provider {
        ProviderKind::Anthropic => Arc::new(AnthropicProvider::new(
            api_key,
            model,
            profile.base_url.clone(),
            watchdog.clone(),
        )),
        ProviderKind::Openai => Arc::new(OpenAiProvider::new(
            api_key,
            model,
            profile.base_url.clone(),
            watchdog.clone(),
        )),
    })
}
