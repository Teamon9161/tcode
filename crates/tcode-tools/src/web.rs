use std::sync::OnceLock;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Url;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use tcode_core::{PermissionRequest, Tool, ToolCtx, ToolOutput};

const FETCH_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_BODY_BYTES: u64 = 5 * 1024 * 1024;
const MAX_REDIRECTS: usize = 5;
// Some sites reject unknown agents; a browser-family UA keeps fetches honest
// about being automated while not being trivially blocked.
const USER_AGENT: &str = "Mozilla/5.0 (compatible; tcode/0.1)";

/// Shared HTTP client. Redirects are handled manually so a cross-host
/// redirect can go back to the model (permission is granted per host).
fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(FETCH_TIMEOUT)
            .user_agent(USER_AGENT)
            .build()
            .expect("http client")
    })
}

fn parse_url(raw: &str) -> Result<Url, String> {
    let url = Url::parse(raw).map_err(|e| {
        format!(
            "invalid url '{raw}': {e}. Provide a full http(s) URL, e.g. https://example.com/page"
        )
    })?;
    match url.scheme() {
        "http" | "https" => Ok(url),
        s => Err(format!(
            "unsupported scheme '{s}': only http and https work"
        )),
    }
}

enum Fetched {
    Response(Url, reqwest::Response),
    /// Redirect left the approved host; the model must re-request it so the
    /// per-host permission stays honest.
    CrossHostRedirect {
        from: Url,
        to: Url,
    },
}

async fn get_following(
    start: Url,
    follow_cross_host: bool,
    cancel: &CancellationToken,
) -> Result<Fetched, String> {
    let mut url = start;
    for _ in 0..=MAX_REDIRECTS {
        let fut = client().get(url.clone()).send();
        let resp = tokio::select! {
            r = fut => r.map_err(|e| describe_http_error(&url, &e))?,
            _ = cancel.cancelled() => return Err("fetch cancelled by user".into()),
        };
        if !resp.status().is_redirection() {
            return Ok(Fetched::Response(url, resp));
        }
        let Some(loc) = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
        else {
            return Err(format!(
                "{url} answered {} without a Location header",
                resp.status()
            ));
        };
        let next = url
            .join(loc)
            .map_err(|e| format!("{url} redirects to unparseable location '{loc}': {e}"))?;
        if !follow_cross_host && next.host_str() != url.host_str() {
            return Ok(Fetched::CrossHostRedirect {
                from: url,
                to: next,
            });
        }
        url = next;
    }
    Err(format!(
        "too many redirects (>{MAX_REDIRECTS}) starting from {url}"
    ))
}

fn describe_http_error(url: &Url, e: &reqwest::Error) -> String {
    if e.is_timeout() {
        format!(
            "fetching {url} timed out after {}s",
            FETCH_TIMEOUT.as_secs()
        )
    } else if e.is_connect() {
        format!("could not connect to {url}: {e}. Check the host name; the site may be down or unreachable from this machine.")
    } else {
        format!("fetching {url} failed: {e}")
    }
}

fn html_to_markdown(html: &str) -> String {
    static CONVERTER: OnceLock<htmd::HtmlToMarkdown> = OnceLock::new();
    let converter = CONVERTER.get_or_init(|| {
        htmd::HtmlToMarkdown::builder()
            .skip_tags(vec!["script", "style", "head", "nav", "footer", "iframe"])
            .build()
    });
    converter.convert(html).unwrap_or_else(|_| html.to_string())
}

/// Convert a response body to model-readable text based on content type.
fn render_body(content_type: &str, body: &str) -> Result<String, String> {
    let ct = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if ct.contains("html") {
        return Ok(html_to_markdown(body));
    }
    if ct.contains("json") {
        return Ok(serde_json::from_str::<Value>(body)
            .and_then(|v| serde_json::to_string_pretty(&v))
            .unwrap_or_else(|_| body.to_string()));
    }
    if ct.is_empty() || ct.starts_with("text/") || ct.contains("xml") || ct.contains("javascript") {
        return Ok(body.to_string());
    }
    Err(format!(
        "unsupported content type '{ct}' (binary?). web_fetch handles html, text and json."
    ))
}

pub struct WebFetchTool;

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn description(&self) -> &str {
        "Fetch a URL over http(s) and return its content; HTML is converted \
         to markdown. Large pages are truncated with a read_output paging \
         handle. A redirect to a different host is not followed automatically \
         — you get the target URL and can fetch it explicitly."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "Full http(s) URL" }
            },
            "required": ["url"]
        })
    }

    fn permission(&self, input: &Value) -> PermissionRequest {
        let url = input["url"].as_str().unwrap_or("?");
        let host = Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_else(|| "?".into());
        PermissionRequest::Ask {
            descriptor: format!("web_fetch({host})"),
            summary: format!("fetch {url}"),
            is_edit: false,
        }
    }

    async fn run(&self, input: Value, _ctx: &ToolCtx, cancel: &CancellationToken) -> ToolOutput {
        let Some(raw) = input["url"].as_str() else {
            return ToolOutput::err("missing required parameter: url");
        };
        let url = match parse_url(raw) {
            Ok(u) => u,
            Err(e) => return ToolOutput::err(e),
        };
        let (final_url, resp) = match get_following(url, false, cancel).await {
            Ok(Fetched::Response(u, r)) => (u, r),
            Ok(Fetched::CrossHostRedirect { from, to }) => {
                return ToolOutput::ok(format!(
                    "{from} redirects to a different host: {to}\nNot followed \
                     automatically. Call web_fetch with that exact URL to fetch it."
                ));
            }
            Err(e) => return ToolOutput::err(e),
        };
        let status = resp.status();
        if let Some(len) = resp.content_length() {
            if len > MAX_BODY_BYTES {
                return ToolOutput::err(format!(
                    "{final_url} is {len} bytes — over the {MAX_BODY_BYTES} byte limit; not fetched"
                ));
            }
        }
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let body = match resp.text().await {
            Ok(b) => b,
            Err(e) => return ToolOutput::err(format!("reading body of {final_url} failed: {e}")),
        };
        if !status.is_success() {
            let mut msg = format!("{final_url} answered HTTP {status}");
            let text = render_body(&content_type, &body).unwrap_or_default();
            let trimmed: String = text.chars().take(500).collect();
            if !trimmed.trim().is_empty() {
                msg.push_str(&format!("\n{trimmed}"));
            }
            return ToolOutput::err(msg);
        }
        match render_body(&content_type, &body) {
            Ok(text) => ToolOutput::ok(format!("URL: {final_url}\n\n{}", text.trim())),
            Err(e) => ToolOutput::err(format!("{final_url}: {e}")),
        }
    }
}

pub struct WebSearchTool;

#[derive(Debug, PartialEq)]
struct SearchResult {
    title: String,
    url: String,
    snippet: String,
}

/// DuckDuckGo redirect links look like
/// `//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2F&rut=…`;
/// the real target is the decoded `uddg` parameter.
fn decode_ddg_href(href: &str) -> Option<String> {
    let absolute = if let Some(rest) = href.strip_prefix("//") {
        format!("https://{rest}")
    } else {
        href.to_string()
    };
    let url = Url::parse(&absolute).ok()?;
    if url.path().starts_with("/l/") {
        return url
            .query_pairs()
            .find(|(k, _)| k == "uddg")
            .map(|(_, v)| v.into_owned());
    }
    Some(absolute)
}

fn squash_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn parse_ddg_results(html: &str, limit: usize) -> Vec<SearchResult> {
    let doc = scraper::Html::parse_document(html);
    let result_sel = scraper::Selector::parse("div.result").expect("selector");
    let title_sel = scraper::Selector::parse("a.result__a").expect("selector");
    let snippet_sel = scraper::Selector::parse(".result__snippet").expect("selector");
    let mut out = Vec::new();
    for r in doc.select(&result_sel) {
        if r.value().classes().any(|c| c == "result--ad") {
            continue;
        }
        let Some(a) = r.select(&title_sel).next() else {
            continue;
        };
        let Some(url) = a.value().attr("href").and_then(decode_ddg_href) else {
            continue;
        };
        let title = squash_whitespace(&a.text().collect::<String>());
        let snippet = r
            .select(&snippet_sel)
            .next()
            .map(|s| squash_whitespace(&s.text().collect::<String>()))
            .unwrap_or_default();
        out.push(SearchResult {
            title,
            url,
            snippet,
        });
        if out.len() >= limit {
            break;
        }
    }
    out
}

fn format_results(results: &[SearchResult]) -> String {
    let mut out = String::new();
    for (i, r) in results.iter().enumerate() {
        out.push_str(&format!("{}. {}\n   {}\n", i + 1, r.title, r.url));
        if !r.snippet.is_empty() {
            out.push_str(&format!("   {}\n", r.snippet));
        }
    }
    out.push_str("\nUse web_fetch to read a result.");
    out
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }

    fn description(&self) -> &str {
        "Search the web (DuckDuckGo). Returns titles, URLs and snippets; \
         follow up with web_fetch to read a result. Use for current \
         information beyond your knowledge cutoff."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string" },
                "limit": { "type": "integer", "description": "Max results (default 8, max 20)" }
            },
            "required": ["query"]
        })
    }

    fn permission(&self, input: &Value) -> PermissionRequest {
        let query = input["query"].as_str().unwrap_or("?");
        PermissionRequest::Ask {
            // No argument in the descriptor: one always-allow covers all searches.
            descriptor: "web_search".into(),
            summary: format!("search: {query}"),
            is_edit: false,
        }
    }

    async fn run(&self, input: Value, _ctx: &ToolCtx, cancel: &CancellationToken) -> ToolOutput {
        let Some(query) = input["query"].as_str().filter(|q| !q.trim().is_empty()) else {
            return ToolOutput::err("missing required parameter: query");
        };
        let limit = input["limit"].as_u64().unwrap_or(8).clamp(1, 20) as usize;
        let url = match Url::parse_with_params("https://html.duckduckgo.com/html/", [("q", query)])
        {
            Ok(u) => u,
            Err(e) => return ToolOutput::err(format!("could not build search url: {e}")),
        };
        let resp = match get_following(url, true, cancel).await {
            Ok(Fetched::Response(_, r)) => r,
            Ok(Fetched::CrossHostRedirect { .. }) => unreachable!("cross-host follows enabled"),
            Err(e) => return ToolOutput::err(e),
        };
        let status = resp.status();
        let body = match resp.text().await {
            Ok(b) => b,
            Err(e) => return ToolOutput::err(format!("reading search response failed: {e}")),
        };
        if !status.is_success() {
            return ToolOutput::err(format!(
                "search backend answered HTTP {status}; it may be rate-limiting. \
                 Retry later or use web_fetch on a known site."
            ));
        }
        let results = parse_ddg_results(&body, limit);
        if results.is_empty() {
            if body.contains("anomaly") || body.contains("challenge") {
                return ToolOutput::err(
                    "search backend blocked the request (bot check); retry later \
                     or use web_fetch on a known site.",
                );
            }
            return ToolOutput::ok(format!(
                "No results for '{query}'. Try fewer or different keywords."
            ));
        }
        ToolOutput::ok(format_results(&results))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_url_rejects_non_http() {
        assert!(parse_url("ftp://x").is_err());
        assert!(parse_url("not a url").is_err());
        assert!(parse_url("https://docs.rs/tokio").is_ok());
    }

    #[test]
    fn render_body_by_content_type() {
        let md = render_body("text/html; charset=utf-8", "<h1>Hi</h1><p>there</p>").unwrap();
        assert!(md.contains("# Hi"));
        let js = render_body("application/json", "{\"a\":1}").unwrap();
        assert!(js.contains("\"a\": 1"));
        let txt = render_body("text/plain", "raw").unwrap();
        assert_eq!(txt, "raw");
        assert!(render_body("image/png", "…").is_err());
    }

    #[test]
    fn html_conversion_skips_scripts() {
        let md = html_to_markdown("<script>var x=1;</script><p>Body</p>");
        assert!(md.contains("Body"));
        assert!(!md.contains("var x"));
    }

    #[test]
    fn ddg_href_decoding() {
        assert_eq!(
            decode_ddg_href("//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fpage&rut=abc")
                .unwrap(),
            "https://example.com/page"
        );
        assert_eq!(
            decode_ddg_href("https://direct.example.com/x").unwrap(),
            "https://direct.example.com/x"
        );
    }

    #[test]
    fn ddg_results_parse_and_skip_ads() {
        let html = r#"
        <div class="result result--ad">
          <a class="result__a" href="https://ad.example.com">Ad</a>
        </div>
        <div class="result results_links">
          <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fdocs.rs%2Ftokio">Tokio  Docs</a>
          <a class="result__snippet" href="/d.js">An   async runtime.</a>
        </div>"#;
        let results = parse_ddg_results(html, 10);
        assert_eq!(
            results,
            vec![SearchResult {
                title: "Tokio Docs".into(),
                url: "https://docs.rs/tokio".into(),
                snippet: "An async runtime.".into(),
            }]
        );
    }
}
