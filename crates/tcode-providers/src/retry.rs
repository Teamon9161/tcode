use std::future::Future;

use tcode_core::config::WatchdogConfig;
use tcode_core::ProviderError;

/// Truncate API error bodies so a failing endpoint cannot flood the UI.
pub fn short(body: &str) -> String {
    const MAX: usize = 600;
    if body.len() <= MAX {
        body.to_string()
    } else {
        let cut = body
            .char_indices()
            .take_while(|(i, _)| *i < MAX)
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        format!("{}…", &body[..cut])
    }
}

/// Establish a streaming connection with exponential backoff.
/// Retries connection errors, 429 and 5xx; anything else fails fast.
pub async fn connect_with_retry<F, Fut>(
    watchdog: &WatchdogConfig,
    mut attempt: F,
) -> Result<reqwest::Response, ProviderError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<reqwest::Response, reqwest::Error>>,
{
    let attempts = watchdog.max_retries.max(1);
    let mut delay = watchdog.initial_backoff();
    let mut last_err = ProviderError::Network("no attempts made".into());
    for i in 0..attempts {
        match attempt().await {
            Ok(resp) if resp.status().is_success() => return Ok(resp),
            Ok(resp) => {
                let status = resp.status().as_u16();
                let body = resp.text().await.unwrap_or_default();
                let err = ProviderError::Api {
                    status,
                    message: short(&body),
                };
                if !err.retryable() {
                    return Err(err);
                }
                last_err = err;
            }
            Err(e) => last_err = ProviderError::Network(e.to_string()),
        }
        if i + 1 < attempts {
            tokio::time::sleep(delay).await;
            delay *= 2;
        }
    }
    Err(last_err)
}
