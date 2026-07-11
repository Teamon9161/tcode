use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::StreamExt;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::sinks::UTF8;
use grep_searcher::SearcherBuilder;
use reqwest::Url;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use tcode_core::{PermissionRequest, Tool, ToolCtx, ToolOutput};

const FETCH_TIMEOUT: Duration = Duration::from_secs(30);
const SEARCH_TIMEOUT: Duration = Duration::from_secs(25);
const MAX_BODY_BYTES: usize = 5 * 1024 * 1024;
const MAX_REDIRECTS: usize = 5;
/// Fetched pages are cached briefly so a follow-up (e.g. a second `pattern`
/// on the same URL) does not re-hit the network.
const CACHE_TTL: Duration = Duration::from_secs(15 * 60);
const CACHE_MAX_ENTRIES: usize = 32;
/// find_in_page output caps.
const FIND_MAX_BYTES: usize = 8 * 1024;
const FIND_MAX_LINE_CHARS: usize = 300;
/// Bound the LLM-optimized text Exa returns per search.
const EXA_CONTEXT_CHARS: u64 = 8000;

const EXA_MCP: &str = "https://mcp.exa.ai/mcp";
const PARALLEL_MCP: &str = "https://search.parallel.ai/mcp";
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

/// Read a response body streaming, aborting the moment it would exceed `cap`.
/// Content-Length is only a hint (absent for chunked responses), so the cap is
/// enforced on the actual bytes read, not the header.
async fn read_capped(
    resp: reqwest::Response,
    cap: usize,
    cancel: &CancellationToken,
) -> Result<Vec<u8>, String> {
    if let Some(len) = resp.content_length() {
        if len > cap as u64 {
            return Err(format!(
                "response is {len} bytes — over the {cap} byte limit; not read"
            ));
        }
    }
    let mut stream = std::pin::pin!(resp.bytes_stream());
    let mut buf: Vec<u8> = Vec::new();
    loop {
        tokio::select! {
            chunk = stream.next() => match chunk {
                Some(Ok(bytes)) => {
                    if buf.len() + bytes.len() > cap {
                        return Err(format!(
                            "response exceeded the {cap} byte limit mid-stream; aborted"
                        ));
                    }
                    buf.extend_from_slice(&bytes);
                }
                Some(Err(e)) => return Err(format!("reading response body failed: {e}")),
                None => break,
            },
            _ = cancel.cancelled() => return Err("fetch cancelled by user".into()),
        }
    }
    Ok(buf)
}

enum Fetched {
    Response(Url, reqwest::Response),
    /// Redirect left the approved site; the model must re-request it so the
    /// per-host permission stays honest.
    CrossHostRedirect {
        from: Url,
        to: Url,
    },
}

/// `example.com` and `www.example.com` are the same site — a redirect between
/// them is safe to follow automatically; anything else is a genuine host change.
fn same_site(a: &Url, b: &Url) -> bool {
    fn bare(host: &str) -> &str {
        host.strip_prefix("www.").unwrap_or(host)
    }
    match (a.host_str(), b.host_str()) {
        (Some(x), Some(y)) => bare(x) == bare(y) && a.scheme() == b.scheme(),
        _ => false,
    }
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
        if !follow_cross_host && !same_site(&url, &next) {
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

// ------------------------------------------------------------- page cache

struct Cached {
    at: Instant,
    final_url: String,
    text: String,
}

fn page_cache() -> &'static Mutex<HashMap<String, Cached>> {
    static C: OnceLock<Mutex<HashMap<String, Cached>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cache_get(url: &str) -> Option<(String, String)> {
    let mut c = page_cache().lock().expect("cache lock");
    match c.get(url) {
        Some(e) if e.at.elapsed() < CACHE_TTL => Some((e.final_url.clone(), e.text.clone())),
        Some(_) => {
            c.remove(url);
            None
        }
        None => None,
    }
}

fn cache_put(url: &str, final_url: String, text: String) {
    let mut c = page_cache().lock().expect("cache lock");
    if c.len() >= CACHE_MAX_ENTRIES && !c.contains_key(url) {
        if let Some(oldest) = c
            .iter()
            .min_by_key(|(_, e)| e.at)
            .map(|(k, _)| k.clone())
        {
            c.remove(&oldest);
        }
    }
    c.insert(
        url.to_string(),
        Cached {
            at: Instant::now(),
            final_url,
            text,
        },
    );
}

// ------------------------------------------------------------ find_in_page

/// Return only the lines of `text` matching `pattern` (regex), each capped,
/// with the whole result byte-bounded — the token-cheap alternative to
/// dumping a whole page.
fn find_in_page(text: &str, pattern: &str) -> Result<String, String> {
    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(true)
        .build(pattern)
        .map_err(|e| format!("invalid pattern regex: {e}"))?;
    let mut searcher = SearcherBuilder::new().line_number(true).build();
    let mut out = String::new();
    let mut hits = 0usize;
    let _ = searcher.search_slice(
        &matcher,
        text.as_bytes(),
        UTF8(|lnum, line| {
            let trimmed = line.trim_end();
            let clipped: String = if trimmed.chars().count() > FIND_MAX_LINE_CHARS {
                trimmed.chars().take(FIND_MAX_LINE_CHARS).collect::<String>() + "…"
            } else {
                trimmed.to_string()
            };
            out.push_str(&format!("{lnum}: {clipped}\n"));
            hits += 1;
            Ok(out.len() < FIND_MAX_BYTES)
        }),
    );
    if hits == 0 {
        return Err(format!(
            "pattern /{pattern}/ not found in page (fetch without pattern to see the full content)"
        ));
    }
    Ok(out)
}

fn fetch_output(final_url: &str, text: &str, pattern: Option<&str>) -> ToolOutput {
    match pattern {
        Some(p) => match find_in_page(text, p) {
            Ok(hits) => ToolOutput::ok(format!("URL: {final_url}  (lines matching /{p}/)\n\n{hits}")),
            Err(e) => ToolOutput::err(format!("{final_url}: {e}")),
        },
        None => ToolOutput::ok(format!("URL: {final_url}\n\n{}", text.trim())),
    }
}

// ---------------------------------------------------------------- web_fetch

pub struct WebFetchTool;

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn description(&self) -> &str {
        "Fetch a URL over http(s) and return its content; HTML is converted \
         to markdown. Pass `pattern` (a regex) to get back only the matching \
         lines instead of the whole page — do this when you are looking for \
         something specific, it is far cheaper. Large pages are truncated with \
         a read_output paging handle. A redirect to a different site is not \
         followed automatically — you get the target URL and can fetch it \
         explicitly. Responses are cached for 15 minutes."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "Full http(s) URL" },
                "pattern": { "type": "string", "description": "Optional regex; return only matching lines" }
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
        let pattern = input["pattern"].as_str().filter(|p| !p.is_empty());
        let url = match parse_url(raw) {
            Ok(u) => u,
            Err(e) => return ToolOutput::err(e),
        };

        if let Some((final_url, text)) = cache_get(raw) {
            return fetch_output(&final_url, &text, pattern);
        }

        let (final_url, resp) = match get_following(url, false, cancel).await {
            Ok(Fetched::Response(u, r)) => (u, r),
            Ok(Fetched::CrossHostRedirect { from, to }) => {
                return ToolOutput::ok(format!(
                    "{from} redirects to a different site: {to}\nNot followed \
                     automatically. Call web_fetch with that exact URL to fetch it."
                ));
            }
            Err(e) => return ToolOutput::err(e),
        };
        let status = resp.status();
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let bytes = match read_capped(resp, MAX_BODY_BYTES, cancel).await {
            Ok(b) => b,
            Err(e) => return ToolOutput::err(format!("{final_url}: {e}")),
        };
        let body = String::from_utf8_lossy(&bytes);
        if !status.is_success() {
            let mut msg = format!("{final_url} answered HTTP {status}");
            let text = render_body(&content_type, &body).unwrap_or_default();
            let trimmed: String = text.chars().take(500).collect();
            if !trimmed.trim().is_empty() {
                msg.push_str(&format!("\n{trimmed}"));
            }
            return ToolOutput::err(msg);
        }
        let text = match render_body(&content_type, &body) {
            Ok(t) => t,
            Err(e) => return ToolOutput::err(format!("{final_url}: {e}")),
        };
        cache_put(raw, final_url.to_string(), text.clone());
        fetch_output(final_url.as_str(), &text, pattern)
    }
}

// --------------------------------------------------------------- web_search

#[derive(Debug, PartialEq)]
struct SearchResult {
    title: String,
    url: String,
    snippet: String,
}

/// Backend chosen at call time. Default is keyless Exa (returns LLM-optimized
/// page text in one call); DDG is the always-available fallback.
enum Backend {
    Exa(Option<String>),
    Parallel(Option<String>),
    Ddg,
}

/// Ordered backends to try, each falling through to the next on failure. This
/// is harness state, not a model choice — the model just calls `web_search`.
/// Both Exa and Parallel serve their hosted endpoints anonymously; a key is
/// optional and only lifts the (undocumented) anonymous rate limits. DDG is
/// always the final, key-free safety net.
fn search_chain() -> Vec<Backend> {
    let exa_key = std::env::var("EXA_API_KEY").ok();
    let par_key = std::env::var("PARALLEL_API_KEY").ok();
    match std::env::var("TCODE_WEBSEARCH_BACKEND").ok().as_deref() {
        Some("ddg") => vec![Backend::Ddg],
        Some("exa") => vec![Backend::Exa(exa_key), Backend::Ddg],
        Some("parallel") => vec![Backend::Parallel(par_key), Backend::Ddg],
        // A configured key means the user picked that vendor — honor it, don't
        // silently spray queries at the other one; DDG still backs it up.
        _ if exa_key.is_some() => vec![Backend::Exa(exa_key), Backend::Ddg],
        _ if par_key.is_some() => vec![Backend::Parallel(par_key), Backend::Ddg],
        // Keyless default: try both hosted providers, then DDG.
        _ => vec![Backend::Exa(None), Backend::Parallel(None), Backend::Ddg],
    }
}

/// Single-shot MCP `tools/call` over HTTP (no initialize handshake, matching
/// how Exa/Parallel's hosted endpoints accept it). The reply is SSE or plain
/// JSON; either way the payload is `result.content[].text`.
async fn mcp_search(
    url: &str,
    tool: &str,
    args: Value,
    extra_headers: &[(&str, String)],
    cancel: &CancellationToken,
) -> Result<String, String> {
    let payload = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": tool, "arguments": args }
    });
    let mut rb = client()
        .post(url)
        .header(reqwest::header::ACCEPT, "application/json, text/event-stream")
        .json(&payload);
    for (k, v) in extra_headers {
        rb = rb.header(*k, v);
    }
    let resp = tokio::select! {
        r = tokio::time::timeout(SEARCH_TIMEOUT, rb.send()) => match r {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => return Err(format!("{tool} request failed: {e}")),
            Err(_) => return Err(format!("{tool} request timed out after {}s", SEARCH_TIMEOUT.as_secs())),
        },
        _ = cancel.cancelled() => return Err("search cancelled by user".into()),
    };
    if !resp.status().is_success() {
        return Err(format!("{tool} backend answered HTTP {}", resp.status()));
    }
    let bytes = read_capped(resp, MAX_BODY_BYTES, cancel).await?;
    parse_mcp_response(&String::from_utf8_lossy(&bytes))
}

/// Extract the text payload from an MCP `tools/call` reply, tolerating both a
/// bare JSON body and SSE `data:` framing.
fn parse_mcp_response(body: &str) -> Result<String, String> {
    let trimmed = body.trim();
    let mut candidates: Vec<&str> = Vec::new();
    if trimmed.starts_with('{') {
        candidates.push(trimmed);
    }
    for line in body.lines() {
        if let Some(rest) = line.trim_start().strip_prefix("data:") {
            candidates.push(rest.trim());
        }
    }
    for c in candidates {
        let Ok(v) = serde_json::from_str::<Value>(c) else {
            continue;
        };
        if let Some(arr) = v["result"]["content"].as_array() {
            let text = arr
                .iter()
                .filter_map(|it| it["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n");
            if !text.trim().is_empty() {
                return Ok(text);
            }
        }
        if let Some(err) = v["error"]["message"].as_str() {
            return Err(format!("search backend error: {err}"));
        }
    }
    Err("no results found in search response".into())
}

async fn exa_search(
    key: Option<&str>,
    query: &str,
    limit: usize,
    cancel: &CancellationToken,
) -> Result<String, String> {
    let url = match key {
        Some(k) => Url::parse_with_params(EXA_MCP, [("exaApiKey", k)])
            .map(|u| u.to_string())
            .unwrap_or_else(|_| EXA_MCP.to_string()),
        None => EXA_MCP.to_string(),
    };
    let args = json!({
        "query": query,
        "type": "auto",
        "numResults": limit,
        "livecrawl": "fallback",
        "contextMaxCharacters": EXA_CONTEXT_CHARS,
    });
    let text = mcp_search(&url, "web_search_exa", args, &[], cancel).await?;
    Ok(format!("Web search (Exa) for \"{query}\":\n\n{}", text.trim()))
}

async fn parallel_search(
    key: Option<&str>,
    query: &str,
    cancel: &CancellationToken,
) -> Result<String, String> {
    let args = json!({ "objective": query, "search_queries": [query] });
    // Parallel's endpoint serves anonymously; the key, when present, just
    // raises rate limits.
    let mut headers = vec![("User-Agent", "tcode/0.1".to_string())];
    if let Some(k) = key {
        headers.push(("Authorization", format!("Bearer {k}")));
    }
    let text = mcp_search(PARALLEL_MCP, "web_search", args, &headers, cancel).await?;
    Ok(format!(
        "Web search (Parallel) for \"{query}\":\n\n{}",
        text.trim()
    ))
}

// ------------------------------------------------------------------- DDG

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

async fn ddg_search(
    query: &str,
    limit: usize,
    cancel: &CancellationToken,
) -> Result<String, String> {
    let url = Url::parse_with_params("https://html.duckduckgo.com/html/", [("q", query)])
        .map_err(|e| format!("could not build search url: {e}"))?;
    let Fetched::Response(_, resp) = get_following(url, true, cancel).await? else {
        unreachable!("cross-host follows enabled");
    };
    let status = resp.status();
    let bytes = read_capped(resp, MAX_BODY_BYTES, cancel).await?;
    let body = String::from_utf8_lossy(&bytes);
    if !status.is_success() {
        return Err(format!(
            "search backend answered HTTP {status}; it may be rate-limiting. \
             Retry later or use web_fetch on a known site."
        ));
    }
    let results = parse_ddg_results(&body, limit);
    if results.is_empty() {
        if body.contains("anomaly") || body.contains("challenge") {
            return Err("search backend blocked the request (bot check); retry later \
                 or use web_fetch on a known site."
                .into());
        }
        return Ok(format!(
            "No results for '{query}'. Try fewer or different keywords."
        ));
    }
    Ok(format_results(&results))
}

pub struct WebSearchTool;

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }

    // Structured/bounded result text — never blob-gate.
    fn gates_output(&self) -> bool {
        false
    }

    fn description(&self) -> &str {
        "Search the web. Returns page text (Exa) or titles/URLs/snippets \
         (DuckDuckGo fallback); with Exa you often do not need a follow-up \
         web_fetch. Use for current information beyond your knowledge cutoff."
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

        // Walk the backend chain; each failure falls through to the next so
        // search never goes dark just because a hosted endpoint is throttled.
        let mut last_err: Option<String> = None;
        for backend in search_chain() {
            if cancel.is_cancelled() {
                return ToolOutput::err("search cancelled by user");
            }
            let attempt = match backend {
                Backend::Exa(key) => exa_search(key.as_deref(), query, limit, cancel).await,
                Backend::Parallel(key) => parallel_search(key.as_deref(), query, cancel).await,
                Backend::Ddg => ddg_search(query, limit, cancel).await,
            };
            match attempt {
                Ok(text) => return ToolOutput::ok(text),
                Err(e) => last_err = Some(e),
            }
        }
        ToolOutput::err(last_err.unwrap_or_else(|| "web search failed".into()))
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
    fn same_site_allows_www_toggle() {
        let a = Url::parse("https://example.com/a").unwrap();
        let b = Url::parse("https://www.example.com/b").unwrap();
        let c = Url::parse("https://other.com/").unwrap();
        let http = Url::parse("http://example.com/").unwrap();
        assert!(same_site(&a, &b));
        assert!(same_site(&b, &a));
        assert!(!same_site(&a, &c));
        assert!(!same_site(&a, &http)); // scheme change is not "same site"
    }

    #[test]
    fn find_in_page_returns_only_matches() {
        let text = "alpha line\nbeta here\ngamma\nbeta again\n";
        let out = find_in_page(text, "beta").unwrap();
        assert!(out.contains("beta here"));
        assert!(out.contains("beta again"));
        assert!(!out.contains("alpha"));
        assert!(find_in_page(text, "delta").is_err());
    }

    #[test]
    fn mcp_response_parses_sse_and_plain() {
        let sse = "event: message\ndata: {\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"hello world\"}]},\"jsonrpc\":\"2.0\",\"id\":1}\n\n";
        assert_eq!(parse_mcp_response(sse).unwrap(), "hello world");
        let plain = "{\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"plain\"}]}}";
        assert_eq!(parse_mcp_response(plain).unwrap(), "plain");
        let err = "data: {\"error\":{\"message\":\"nope\"}}";
        assert!(parse_mcp_response(err).is_err());
    }

    #[tokio::test]
    #[ignore = "hits the real Exa keyless endpoint"]
    async fn exa_live_smoke() {
        let cancel = CancellationToken::new();
        let text = exa_search(None, "rust tokio async runtime", 3, &cancel)
            .await
            .expect("keyless exa search should return results");
        assert!(
            text.to_lowercase().contains("tokio"),
            "expected tokio in results, got: {text}"
        );
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
