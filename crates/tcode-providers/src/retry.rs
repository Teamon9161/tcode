use std::future::Future;
use std::time::Duration;

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

/// Cap the wait for a connection's response headers. A provider's connect
/// future resolves once headers arrive — before any streamed body — so this
/// bounds the "time to first byte" without touching model generation, which
/// is guarded separately by the idle watchdog. Elapsing yields a retryable
/// `ConnectTimeout` so the agent loop backs off and retries instead of
/// hanging until the OS finally tears the socket down.
pub async fn with_connect_timeout<F, T>(dur: Duration, fut: F) -> Result<T, ProviderError>
where
    F: Future<Output = Result<T, ProviderError>>,
{
    match tokio::time::timeout(dur, fut).await {
        Ok(res) => res,
        Err(_) => Err(ProviderError::ConnectTimeout(dur)),
    }
}

/// One connection attempt, with the response classified into a `ProviderError`
/// on failure. Retrying is the agent loop's job — it owns the single retry
/// loop so every attempt (connect and mid-stream) is visible to the user and
/// backs off uniformly. A non-2xx body is read and truncated for the message.
pub async fn connect_once<F, Fut>(
    timeout: Duration,
    attempt: F,
) -> Result<reqwest::Response, ProviderError>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<reqwest::Response, reqwest::Error>>,
{
    with_connect_timeout(timeout, async {
        match attempt().await {
            Ok(resp) if resp.status().is_success() => Ok(resp),
            Ok(resp) => {
                let status = resp.status().as_u16();
                let body = resp.text().await.unwrap_or_default();
                Err(ProviderError::Api {
                    status,
                    message: short(&body),
                })
            }
            Err(e) => Err(ProviderError::Network(e.to_string())),
        }
    })
    .await
}
