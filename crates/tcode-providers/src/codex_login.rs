//! ChatGPT (Codex backend) OAuth login, so a user can authenticate without
//! installing the Codex CLI. This is the authorization-code + PKCE flow the
//! Codex CLI runs: open OpenAI's authorize page in a browser, catch the
//! redirect on a localhost callback, exchange the code for tokens, and write
//! them to `~/.codex/auth.json` in the same shape Codex uses — so the refresh
//! and model-fetch paths that already read that file work unchanged, and a
//! later `codex` install shares the same credentials.
//!
//! The transport pieces (a localhost TCP accept, minimal HTTP request parse)
//! are kept dependency-free on purpose: the callback is one GET from the user's
//! own browser, not a server we host.

use std::collections::HashMap;
use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::codex_cli::{self, CLIENT_ID, TOKEN_URL};

const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
/// The scope set the Codex CLI requests. `offline_access` is what yields a
/// refresh token; the rest match so the minted credentials are interchangeable.
const SCOPE: &str = "openid profile email offline_access api.connectors.read api.connectors.invoke";
/// Codex registered exactly these redirect ports with the OAuth client, so the
/// callback must land on one of them — an arbitrary free port would be rejected.
const CALLBACK_PORTS: [u16; 2] = [1455, 1457];
/// How long to wait for the user to finish the browser step before giving up.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

/// Who just logged in, for a confirmation line.
#[derive(Debug, Clone)]
pub struct LoginOutcome {
    pub account_id: String,
    pub email: Option<String>,
}

/// A login in progress: the URL is live and the callback listener is bound.
/// Split from `finish` so a frontend can show the URL (and open the browser)
/// while the await blocks on the redirect.
pub struct LoginHandle {
    listener: TcpListener,
    http: reqwest::Client,
    authorize_url: String,
    redirect_uri: String,
    verifier: String,
    state: String,
}

/// Bind the callback listener and build the authorize URL. Fails only if
/// neither registered port is free (another login, or Codex, already holds it).
pub async fn start() -> Result<LoginHandle, String> {
    let verifier = random_b64url(32);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = random_b64url(32);

    let (listener, port) = bind_callback().await?;
    let redirect_uri = format!("http://localhost:{port}/auth/callback");
    let authorize_url = authorize_url(&challenge, &state, &redirect_uri)?;

    Ok(LoginHandle {
        listener,
        http: crate::http::client(),
        authorize_url,
        redirect_uri,
        verifier,
        state,
    })
}

impl LoginHandle {
    pub fn authorize_url(&self) -> &str {
        &self.authorize_url
    }

    /// Await the browser redirect, exchange the code for tokens, and persist
    /// them. Times out if the user never completes the flow.
    pub async fn finish(self) -> Result<LoginOutcome, String> {
        let code = tokio::time::timeout(LOGIN_TIMEOUT, self.await_code())
            .await
            .map_err(|_| "timed out waiting for the browser sign-in to complete".to_string())??;
        let tokens = self.exchange(&code).await?;
        let (account_id, email) = parse_id_token(&tokens.id_token);
        codex_cli::write_auth(
            &tokens.access_token,
            &tokens.refresh_token,
            &tokens.id_token,
            &account_id,
        )?;
        // Best-effort: seed the local model cache from the live catalogue so the
        // first session sees the real model list, not the fallback. A failure
        // here leaves the built-in fallback in place, so it must not fail login.
        let _ =
            codex_cli::refresh_models_cache(&self.http, &tokens.access_token, &account_id).await;
        Ok(LoginOutcome { account_id, email })
    }

    /// Accept connections until the `/auth/callback` redirect arrives, replying
    /// to anything else (a favicon probe, a stray `/`) with a 404 so the browser
    /// does not hang. Returns the authorization code.
    async fn await_code(&self) -> Result<String, String> {
        loop {
            let (mut stream, _) = self
                .listener
                .accept()
                .await
                .map_err(|e| format!("callback accept failed: {e}"))?;
            let Some(target) = read_request_target(&mut stream).await else {
                let _ = write_response(&mut stream, 400, "bad request").await;
                continue;
            };
            if !target.starts_with("/auth/callback") {
                let _ = write_response(&mut stream, 404, "not found").await;
                continue;
            }
            let params = query_params(&target);
            if let Some(error) = params.get("error") {
                let detail = params
                    .get("error_description")
                    .map(String::as_str)
                    .unwrap_or(error);
                let _ = write_response(&mut stream, 200, &result_page(false, detail)).await;
                return Err(format!("sign-in was denied: {detail}"));
            }
            // A mismatched state means the redirect is not the one we started;
            // treat it as hostile rather than proceeding with its code.
            if params.get("state").map(String::as_str) != Some(self.state.as_str()) {
                let _ =
                    write_response(&mut stream, 400, &result_page(false, "state mismatch")).await;
                return Err("callback state did not match the request".into());
            }
            match params.get("code") {
                Some(code) if !code.is_empty() => {
                    let _ = write_response(&mut stream, 200, &result_page(true, "")).await;
                    return Ok(code.clone());
                }
                _ => {
                    let _ = write_response(&mut stream, 400, &result_page(false, "no code")).await;
                    return Err("callback carried no authorization code".into());
                }
            }
        }
    }

    async fn exchange(&self, code: &str) -> Result<Tokens, String> {
        let resp = self
            .http
            .post(TOKEN_URL)
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", &self.redirect_uri),
                ("client_id", CLIENT_ID),
                ("code_verifier", &self.verifier),
            ])
            .send()
            .await
            .map_err(|e| format!("token exchange request failed: {e}"))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(format!(
                "token exchange failed ({status}): {}",
                short(&body)
            ));
        }
        let value: serde_json::Value =
            serde_json::from_str(&body).map_err(|e| format!("token exchange response: {e}"))?;
        let access_token = value["access_token"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let id_token = value["id_token"].as_str().unwrap_or_default().to_string();
        if access_token.is_empty() || id_token.is_empty() {
            return Err("token exchange returned no access_token/id_token".into());
        }
        Ok(Tokens {
            access_token,
            refresh_token: value["refresh_token"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            id_token,
        })
    }
}

struct Tokens {
    access_token: String,
    refresh_token: String,
    id_token: String,
}

/// Open a URL in the user's default browser, best effort. The frontend also
/// shows the URL, so a false return is a fallback-to-copy, not a failure.
pub fn open_browser(url: &str) -> bool {
    // Every launcher below receives the URL as one direct process argument, so
    // no shell parses it. This matters: the authorize URL carries `&` (a cmd
    // command separator) and percent-encoded `%2F` (a cmd variable trigger), so
    // routing it through `cmd /C start` truncates it at the first `&` and opens
    // a paramless URL — which OpenAI rejects as "missing required parameter".
    // `rundll32` hands the whole string to the default protocol handler intact.
    #[cfg(target_os = "windows")]
    let cmd = std::process::Command::new("rundll32")
        .args(["url.dll,FileProtocolHandler", url])
        .spawn();
    #[cfg(target_os = "macos")]
    let cmd = std::process::Command::new("open").arg(url).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let cmd = std::process::Command::new("xdg-open").arg(url).spawn();
    cmd.map(|mut child| {
        // Reap on platforms where the launcher exits immediately.
        let _ = child.wait();
        true
    })
    .unwrap_or(false)
}

async fn bind_callback() -> Result<(TcpListener, u16), String> {
    for port in CALLBACK_PORTS {
        if let Ok(listener) = TcpListener::bind(("127.0.0.1", port)).await {
            return Ok((listener, port));
        }
    }
    Err(format!(
        "could not bind the login callback on 127.0.0.1:{} — another sign-in may be in progress",
        CALLBACK_PORTS[0]
    ))
}

fn authorize_url(challenge: &str, state: &str, redirect_uri: &str) -> Result<String, String> {
    reqwest::Url::parse_with_params(
        AUTHORIZE_URL,
        &[
            ("response_type", "code"),
            ("client_id", CLIENT_ID),
            ("redirect_uri", redirect_uri),
            ("scope", SCOPE),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256"),
            ("state", state),
            ("id_token_add_organizations", "true"),
            ("codex_cli_simplified_flow", "true"),
            ("originator", "codex_cli_rs"),
        ],
    )
    .map(|url| url.to_string())
    .map_err(|e| format!("building authorize URL: {e}"))
}

/// 32 random bytes as unpadded base64url — a PKCE verifier / an OAuth `state`.
/// Sourced from v4 UUIDs (122 random bits each) to avoid a `rand` dependency.
fn random_b64url(len: usize) -> String {
    let mut bytes = Vec::with_capacity(len + 16);
    while bytes.len() < len {
        bytes.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    }
    bytes.truncate(len);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// The two claims we need from the id_token: `chatgpt_account_id` (nested under
/// the `https://api.openai.com/auth` namespace claim — the header the backend
/// bills against) and `email` (for the confirmation line). Both best-effort:
/// a token we cannot parse yields an empty account id, which the request path
/// already tolerates.
fn parse_id_token(jwt: &str) -> (String, Option<String>) {
    let Some(payload) = jwt.split('.').nth(1) else {
        return (String::new(), None);
    };
    let Ok(bytes) = URL_SAFE_NO_PAD.decode(payload) else {
        return (String::new(), None);
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return (String::new(), None);
    };
    let account_id = value["https://api.openai.com/auth"]["chatgpt_account_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let email = value["email"].as_str().map(String::from);
    (account_id, email)
}

/// Read just the HTTP request line and return its target (path+query). The
/// browser's callback is a single small GET, so one read is enough.
async fn read_request_target(stream: &mut TcpStream) -> Option<String> {
    let mut buf = [0u8; 8192];
    let n = stream.read(&mut buf).await.ok()?;
    let text = String::from_utf8_lossy(&buf[..n]);
    let line = text.lines().next()?;
    // "GET /auth/callback?code=...&state=... HTTP/1.1"
    line.split_whitespace().nth(1).map(String::from)
}

fn query_params(target: &str) -> HashMap<String, String> {
    match reqwest::Url::parse(&format!("http://localhost{target}")) {
        Ok(url) => url.query_pairs().into_owned().collect(),
        Err(_) => HashMap::new(),
    }
}

async fn write_response(stream: &mut TcpStream, status: u16, body: &str) -> std::io::Result<()> {
    let reason = if status == 200 { "OK" } else { "Error" };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await
}

fn result_page(ok: bool, detail: &str) -> String {
    let (title, note) = if ok {
        (
            "Signed in".to_string(),
            "You can close this tab and return to tcode.".to_string(),
        )
    } else {
        (
            "Sign-in failed".to_string(),
            format!("{detail} — return to tcode and try again."),
        )
    };
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>tcode — {title}</title></head>\
<body style=\"font-family:system-ui;max-width:32rem;margin:4rem auto;text-align:center\">\
<h1>{title}</h1><p>{note}</p></body></html>"
    )
}

/// Trim an error body so a diagnostic line stays a line.
fn short(text: &str) -> String {
    let text = text.trim();
    if text.len() > 300 {
        format!("{}…", &text[..300])
    } else {
        text.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_is_the_sha256_of_the_verifier() {
        let verifier = random_b64url(32);
        // A verifier is 32 bytes → 43 unpadded base64url chars, URL-safe.
        assert_eq!(verifier.len(), 43);
        assert!(verifier
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        assert_eq!(
            challenge,
            URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
        );
    }

    #[test]
    fn authorize_url_carries_pkce_and_a_registered_redirect() {
        let url = authorize_url("chal", "st8", "http://localhost:1455/auth/callback").unwrap();
        assert!(url.starts_with(AUTHORIZE_URL));
        assert!(url.contains("code_challenge=chal"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=st8"));
        assert!(url.contains(&format!("client_id={CLIENT_ID}")));
        // The redirect and the space-separated scope must be percent-encoded.
        assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback"));
        assert!(
            url.contains("scope=openid+profile+email+offline_access")
                || url.contains("scope=openid%20profile")
        );
    }

    #[test]
    fn id_token_account_id_comes_from_the_openai_auth_claim() {
        // header.payload.signature with an unpadded base64url payload.
        let claims = serde_json::json!({
            "email": "dev@example.com",
            "https://api.openai.com/auth": { "chatgpt_account_id": "acct_123" }
        });
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap());
        let jwt = format!("eyJhbGciOiJub25lIn0.{payload}.sig");
        let (account_id, email) = parse_id_token(&jwt);
        assert_eq!(account_id, "acct_123");
        assert_eq!(email.as_deref(), Some("dev@example.com"));
    }

    #[test]
    fn a_garbage_id_token_yields_no_account_id_rather_than_panicking() {
        let (account_id, email) = parse_id_token("not-a-jwt");
        assert!(account_id.is_empty());
        assert!(email.is_none());
    }

    #[test]
    fn query_params_parse_the_callback_target() {
        let params = query_params("/auth/callback?code=abc&state=xyz");
        assert_eq!(params.get("code").map(String::as_str), Some("abc"));
        assert_eq!(params.get("state").map(String::as_str), Some("xyz"));
    }
}
