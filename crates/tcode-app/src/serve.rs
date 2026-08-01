//! A loopback origin for the files a conversation puts on screen.
//!
//! ## Why this exists at all
//!
//! `show` was built for artifacts the *model* writes: a mermaid diagram, an
//! echarts option, a fragment of HTML. Those are rendered behind the execution
//! boundary in `ui/src/sandbox/` — an opaque-origin frame that may `innerHTML`
//! model output because the worst it can reach is itself. That is the right
//! answer for model-authored markup and the wrong one for the thing people
//! actually generate, which is a **report written by a script**:
//!
//!  - `innerHTML` does not execute `<script>`. Per the HTML spec, scripts
//!    inserted that way never run, so a self-contained plotly document (inline
//!    plotly.js plus an inline `Plotly.newPlot`) is a page of dead text.
//!  - The frame loads `sandbox.html` from the app's own origin and therefore
//!    inherits the app's CSP (`default-src 'self'`), so the CDN variant does
//!    not load either, and neither would an inline script if one could run.
//!  - An opaque origin has no useful base URL and cannot fetch, so `./fig1.png`,
//!    `../data.csv` and a report split across files are all unreachable.
//!
//! Every one of those is a property of *where the bytes are loaded from*, not of
//! how carefully they are parsed. So the fix is an origin: serve the file over
//! loopback and point a frame at it. Scripts run, relative references resolve,
//! `fetch` works, and the frame is cross-origin to the app — which is a stronger
//! separation than the sandbox attribute, not a weaker one, because it holds
//! without depending on any attribute being spelled correctly.
//!
//! ## What bounds it
//!
//! Two things, and neither is new:
//!
//!  - **A token per mount.** A TCP port is reachable by every process on the
//!    machine, so the port alone would make this a file server for anything that
//!    can guess a path. Each root is mounted under an unguessable prefix minted
//!    at runtime, and a request whose prefix is unknown is a 404 that never
//!    touches the disk.
//!  - **`tcode_tools::viewable_within`**, the boundary `show` and `shown_file`
//!    already share (rule 13). Roots come from it and every request is joined
//!    inside one. There is deliberately no third definition of "inside the
//!    workspace" here; the app has been bitten by having two.
//!
//! Loopback binding is not a boundary and is not treated as one — it only keeps
//! the origin off the network.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use http_body_util::{combinators::UnsyncBoxBody, BodyExt, Empty};
use hyper::body::Incoming;
use hyper::header::{HeaderValue, CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, HOST};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode, Uri};
use hyper_util::rt::TokioIo;
use percent_encoding::{percent_decode_str, utf8_percent_encode, AsciiSet, CONTROLS};
use tokio::net::TcpListener;
use tower::ServiceExt;
use tower_http::services::ServeDir;

/// Everything but the segment separator has to survive into the URL, and a
/// filename may legally contain any of these. `/` is excluded from the set
/// precisely because it is added between segments, never inside one.
const SEGMENT: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}')
    .add(b'\\');

/// Unsync because `ServeDir`'s body is: it streams a file through a boxed
/// future that is `Send` but not `Sync`, and hyper only ever needs the former.
type Body = UnsyncBoxBody<Bytes, std::io::Error>;

/// The policy served files run under.
///
/// **This is not the app's CSP and does not touch it.** The app's own policy
/// (`tauri.conf.json`) still governs the app's own document, and rule 11's
/// second line of defence — no `unsafe-inline`, no `unsafe-eval` there — is
/// untouched. This one applies to a different origin: the page loaded in the
/// frame, which is where the report lives.
///
/// What it buys is the one capability this whole change genuinely adds. Before
/// it, a shown `.html` could not run a script at all; now it can, and a script
/// in a mount can read that mount — which is the session's folder. So the leak
/// worth closing is not execution, it is **exfiltration**, and that is
/// `connect-src` and `form-action`:
///
///  - `connect-src 'self'` — a report may `fetch` its own data (the entire
///    reason `DATA-BINDING.md` existed as an unbuilt second stage) and may not
///    open an XHR, `fetch` or WebSocket to anywhere else.
///  - `form-action 'none'` — the other one-shot way to POST a body out.
///
/// Everything else is deliberately permissive, because the alternative is the
/// failure this file exists to end: `unsafe-inline` and `unsafe-eval` because a
/// self-contained plotly document is one enormous inline script, and `https:`
/// on the loading directives because the CDN variant is what half the tooling
/// emits by default.
///
/// It is honest about what it is: **defence in depth, not a boundary.**
/// `img-src https:` alone leaves a pixel-with-a-query-string channel open, and
/// closing that would take CDN images with it. The reason that trade is
/// acceptable is that anything able to write the HTML in the first place got
/// there by writing a file and running a script — both of which pass through
/// approval, and either of which is a more direct route out than this one.
const POLICY: &str = "default-src 'self' 'unsafe-inline' 'unsafe-eval' data: blob: https:; \
connect-src 'self' data: blob:; \
form-action 'none'";

/// The running origin. One per process: mounts are keyed by root directory, so
/// two sessions in the same folder share one, and a second server would only
/// mean a second port to explain.
pub struct Serve {
    addr: SocketAddr,
    mounts: Mutex<Mounts>,
}

#[derive(Default)]
struct Mounts {
    by_token: HashMap<String, PathBuf>,
    by_root: HashMap<PathBuf, String>,
}

impl Serve {
    /// Bind loopback on an ephemeral port and start serving.
    ///
    /// Binds before returning so a failure is a startup error with a reason,
    /// rather than a pane that stays empty once somebody shows a report.
    pub async fn start() -> std::io::Result<Arc<Self>> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let serve = Arc::new(Self {
            addr: listener.local_addr()?,
            mounts: Mutex::new(Mounts::default()),
        });

        let accepting = Arc::clone(&serve);
        tauri::async_runtime::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    // A failed accept is per-connection (fd limits, a client
                    // gone between SYN and accept); the listener is still good.
                    continue;
                };
                let serving = Arc::clone(&accepting);
                tauri::async_runtime::spawn(async move {
                    let io = TokioIo::new(stream);
                    let service = service_fn(move |request| respond(Arc::clone(&serving), request));
                    // Errors here are the client hanging up mid-response, which
                    // is normal for a frame that navigated away.
                    let _ = http1::Builder::new().serve_connection(io, service).await;
                });
            }
        });

        Ok(serve)
    }

    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// The URL a frame should load to display `file`, or the boundary's own
    /// refusal.
    ///
    /// `cwd` is the session's folder; it selects which root the file falls
    /// under. The path is not read here — existence and readability are the
    /// frame's request to find out, and answering them twice would only let the
    /// two answers disagree.
    pub fn url(&self, file: &Path, cwd: &Path) -> Result<String, String> {
        let (root, relative) = tcode_tools::viewable_within(file, cwd)
            .ok_or_else(|| tcode_tools::is_viewable_path(file, cwd).unwrap_err())?;

        let mut url = format!("http://127.0.0.1:{}/{}", self.addr.port(), self.mount(root));
        for part in relative.components() {
            url.push('/');
            url.extend(utf8_percent_encode(
                &part.as_os_str().to_string_lossy(),
                SEGMENT,
            ));
        }
        Ok(url)
    }

    /// The token for a root, minting one on first use. Idempotent per root, so
    /// a session that shows twenty reports has one mount and the frames share a
    /// browsing context.
    fn mount(&self, root: PathBuf) -> String {
        let mut mounts = self.mounts.lock().expect("serve mounts");
        if let Some(token) = mounts.by_root.get(&root) {
            return token.clone();
        }
        // 122 random bits, which is the point of it: the port is open to every
        // process on this machine and this is what they do not have.
        let token = uuid::Uuid::new_v4().simple().to_string();
        mounts.by_root.insert(root.clone(), token.clone());
        mounts.by_token.insert(token.clone(), root);
        token
    }

    fn root_of(&self, token: &str) -> Option<PathBuf> {
        self.mounts
            .lock()
            .expect("serve mounts")
            .by_token
            .get(token)
            .cloned()
    }
}

async fn respond(serve: Arc<Serve>, request: Request<Incoming>) -> Result<Response<Body>, String> {
    Ok(route(serve, request).await.unwrap_or_else(|status| {
        let mut response = Response::new(empty());
        *response.status_mut() = status;
        response
    }))
}

async fn route(
    serve: Arc<Serve>,
    request: Request<Incoming>,
) -> Result<Response<Body>, StatusCode> {
    // A Host of anything but our own address is a name that resolved here —
    // the DNS-rebinding shape. The token already stops it; refusing the name
    // costs one comparison and stops it earlier.
    let expected = format!("127.0.0.1:{}", serve.port());
    if request.headers().get(HOST).map(HeaderValue::as_bytes) != Some(expected.as_bytes()) {
        return Err(StatusCode::NOT_FOUND);
    }

    let path = request.uri().path();
    let (token, rest) = path
        .trim_start_matches('/')
        .split_once('/')
        .unwrap_or((path.trim_start_matches('/'), ""));
    let root = serve.root_of(token).ok_or(StatusCode::NOT_FOUND)?;

    // `ServeDir` refuses traversal itself; this refuses it before a decoded
    // `..` is ever handed across, so neither side is the only thing standing
    // between a URL and the rest of the disk.
    if rest.split('/').any(is_traversal) {
        return Err(StatusCode::NOT_FOUND);
    }

    let mut parts = request.uri().clone().into_parts();
    parts.path_and_query = Some(
        format!("/{rest}")
            .parse()
            .map_err(|_| StatusCode::BAD_REQUEST)?,
    );
    let (head, body) = request.into_parts();
    let mut inner = Request::from_parts(head, body);
    *inner.uri_mut() = Uri::from_parts(parts).map_err(|_| StatusCode::BAD_REQUEST)?;

    // `ServeDir` is used rather than hand-rolled file serving because the
    // compatibility surface is the whole point here: byte ranges (video
    // scrubbing, PDF viewers), conditional requests, HEAD, directory index,
    // and MIME by extension are all things a report will exercise and all
    // things that are tedious to get right twice.
    let served = ServeDir::new(root)
        .oneshot(inner)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let (mut head, body) = served.into_parts();

    // A report is looked at again after the script that wrote it is re-run, and
    // the reload button in `Shown.tsx` is the whole affordance for that. A
    // cached response would make it a button that does nothing.
    head.headers
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    head.headers
        .insert(CONTENT_SECURITY_POLICY, HeaderValue::from_static(POLICY));
    if let Some(typed) = utf8(head.headers.get(CONTENT_TYPE)) {
        head.headers.insert(CONTENT_TYPE, typed);
    }

    Ok(Response::from_parts(
        head,
        body.map_err(std::io::Error::other).boxed_unsync(),
    ))
}

/// Textual types get an explicit `charset=utf-8`.
///
/// `mime_guess` returns bare `text/html`, which leaves the encoding to the
/// browser's sniffing and its locale default. Every file this app writes or
/// reads is UTF-8, and a report whose non-ASCII text arrives as mojibake is
/// exactly the "why does *this* one not display" failure this server exists to
/// stop having.
fn utf8(current: Option<&HeaderValue>) -> Option<HeaderValue> {
    let value = current?.to_str().ok()?;
    if !value.starts_with("text/") || value.contains("charset") {
        return None;
    }
    HeaderValue::from_str(&format!("{value}; charset=utf-8")).ok()
}

/// Whether a decoded URL segment could leave the mount. Checked after decoding,
/// because `%2e%2e` is the interesting spelling.
fn is_traversal(segment: &str) -> bool {
    let Ok(decoded) = percent_decode_str(segment).decode_utf8() else {
        return true;
    };
    decoded.contains('\\')
        || Path::new(decoded.as_ref())
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
}

fn empty() -> Body {
    Empty::<Bytes>::new()
        .map_err(|never| match never {})
        .boxed_unsync()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `viewable_within` resolves tcode's scratch root through `home_dir()`, so
    /// the redirect has to be in place before any of this decides a boundary —
    /// otherwise the tests are answering against the developer's real home.
    async fn started() -> (Arc<Serve>, tempfile::TempDir) {
        tcode_core::home::testing::temp_home();
        let dir = tempfile::tempdir().unwrap();
        (Serve::start().await.unwrap(), dir)
    }

    async fn get(url: &str) -> (StatusCode, Vec<u8>, hyper::HeaderMap) {
        let response = reqwest::Client::new().get(url).send().await.unwrap();
        let status = StatusCode::from_u16(response.status().as_u16()).unwrap();
        let headers = response.headers().clone();
        (status, response.bytes().await.unwrap().to_vec(), headers)
    }

    #[tokio::test]
    async fn a_shown_file_is_served_with_its_relatives() {
        let (serve, dir) = started().await;
        std::fs::create_dir(dir.path().join("out")).unwrap();
        std::fs::write(dir.path().join("out/report.html"), "<h1>hi</h1>").unwrap();
        std::fs::write(dir.path().join("data.csv"), "a,b\n1,2\n").unwrap();

        let url = serve
            .url(&dir.path().join("out/report.html"), dir.path())
            .unwrap();
        let (status, body, headers) = get(&url).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, b"<h1>hi</h1>");
        assert_eq!(headers[CONTENT_TYPE], "text/html; charset=utf-8");
        assert_eq!(headers[CACHE_CONTROL], "no-store");

        // The reference a report would actually make, resolved by the browser
        // against the URL above rather than by anything in this process.
        let relative = url.replace("/out/report.html", "/data.csv");
        let (status, body, _) = get(&relative).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, b"a,b\n1,2\n");
    }

    #[tokio::test]
    async fn an_unknown_mount_is_not_a_file_server() {
        let (serve, dir) = started().await;
        std::fs::write(dir.path().join("secret.txt"), "no").unwrap();
        serve
            .url(&dir.path().join("secret.txt"), dir.path())
            .unwrap();

        let guessed = format!(
            "http://127.0.0.1:{}/{}/secret.txt",
            serve.port(),
            uuid::Uuid::new_v4().simple()
        );
        assert_eq!(get(&guessed).await.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn traversal_out_of_the_mount_is_refused() {
        let (serve, dir) = started().await;
        std::fs::write(dir.path().join("in.txt"), "in").unwrap();
        let url = serve.url(&dir.path().join("in.txt"), dir.path()).unwrap();
        let mount = url.rsplit_once('/').unwrap().0;

        for escape in ["../../../etc/passwd", "..%2f..%2fetc%2fpasswd", "%2e%2e/x"] {
            assert_eq!(
                get(&format!("{mount}/{escape}")).await.0,
                StatusCode::NOT_FOUND,
                "{escape} was not refused"
            );
        }
    }

    #[tokio::test]
    async fn a_file_outside_the_boundary_has_no_url() {
        let (serve, dir) = started().await;
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("elsewhere.html"), "<p>x</p>").unwrap();

        let refusal = serve
            .url(&outside.path().join("elsewhere.html"), dir.path())
            .unwrap_err();
        assert!(
            refusal.contains("outside this session's folder"),
            "{refusal}"
        );
    }

    #[tokio::test]
    async fn a_range_request_is_answered_with_the_range() {
        let (serve, dir) = started().await;
        std::fs::write(dir.path().join("clip.bin"), "0123456789").unwrap();
        let url = serve.url(&dir.path().join("clip.bin"), dir.path()).unwrap();

        let response = reqwest::Client::new()
            .get(&url)
            .header("range", "bytes=2-5")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 206);
        assert_eq!(response.bytes().await.unwrap().to_vec(), b"2345");
    }

    /// The shape a generated report actually has, request by request.
    ///
    /// Every one of these is a way a report "just does not display", and each
    /// has a specific cause worth naming rather than a general one:
    ///
    ///  - The script arriving as `text/plain` — a `<script type="module">` is
    ///    refused outright for a bad MIME type, and this is the single most
    ///    likely thing to be wrong in a hand-rolled file server.
    ///  - A `.wasm` served as anything but `application/wasm` fails
    ///    `instantiateStreaming`, which is how the fast path of every plotting
    ///    library that has one loads.
    ///  - Fonts and images resolving relative to the document rather than to
    ///    some mount root the URL does not mirror.
    #[tokio::test]
    async fn a_report_and_everything_it_pulls_in() {
        let (serve, dir) = started().await;
        let out = dir.path().join("out");
        std::fs::create_dir(&out).unwrap();
        std::fs::write(out.join("report.html"), "<script src='./app.js'></script>").unwrap();
        std::fs::write(out.join("app.js"), "export const draw = () => {};").unwrap();
        std::fs::write(out.join("engine.wasm"), b"\0asm").unwrap();
        std::fs::write(out.join("fig1.png"), b"\x89PNG\r\n\x1a\n").unwrap();
        std::fs::write(out.join("style.css"), "body{margin:0}").unwrap();
        std::fs::write(out.join("Inter.woff2"), b"wOF2").unwrap();

        let url = serve.url(&out.join("report.html"), dir.path()).unwrap();
        let base = url.rsplit_once('/').unwrap().0;

        for (file, expected) in [
            ("app.js", "text/javascript"),
            ("engine.wasm", "application/wasm"),
            ("fig1.png", "image/png"),
            ("style.css", "text/css"),
            ("Inter.woff2", "font/woff2"),
        ] {
            let (status, _, headers) = get(&format!("{base}/{file}")).await;
            assert_eq!(status, StatusCode::OK, "{file} was not served");
            let kind = headers[CONTENT_TYPE].to_str().unwrap();
            assert!(
                kind.starts_with(expected),
                "{file} was served as {kind}, not {expected}"
            );
        }
    }

    /// The one capability this change adds is script execution inside a mount,
    /// and a mount is the session's folder. So the thing to hold still is not
    /// that scripts run — they must — but that what they read cannot be sent
    /// anywhere. Pinned by value because both halves are easy to widen by
    /// accident while making some report work.
    #[tokio::test]
    async fn a_served_page_may_run_but_may_not_phone_home() {
        let (serve, dir) = started().await;
        std::fs::write(dir.path().join("report.html"), "<p>x</p>").unwrap();
        let url = serve
            .url(&dir.path().join("report.html"), dir.path())
            .unwrap();

        let (_, _, headers) = get(&url).await;
        let policy = headers["content-security-policy"].to_str().unwrap();
        // A report draws itself, and reads its own data.
        assert!(policy.contains("'unsafe-inline'"), "{policy}");
        assert!(policy.contains("connect-src 'self'"), "{policy}");
        // And has no way to open a socket or post a form to anywhere else.
        assert!(!policy.contains("connect-src *"), "{policy}");
        assert!(policy.contains("form-action 'none'"), "{policy}");
    }

    #[tokio::test]
    async fn one_root_keeps_one_mount() {
        let (serve, dir) = started().await;
        std::fs::write(dir.path().join("a.html"), "a").unwrap();
        std::fs::write(dir.path().join("b.html"), "b").unwrap();

        let first = serve.url(&dir.path().join("a.html"), dir.path()).unwrap();
        let second = serve.url(&dir.path().join("b.html"), dir.path()).unwrap();
        assert_eq!(
            first.rsplit_once('/').unwrap().0,
            second.rsplit_once('/').unwrap().0
        );
    }
}
