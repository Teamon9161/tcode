//! The window's browser: a native child webview, positioned over one pane.
//!
//! ## Why native and not an `<iframe>`
//!
//! An iframe cannot browse the web. `X-Frame-Options` and `frame-ancestors`
//! are set by GitHub, Google, every login page and most documentation hosts, so
//! a frame-based browser is a blank rectangle for a large share of the web —
//! and blank, with the refusal only in a console nobody opened. A native
//! webview has no such notion: it is a browser, not a nested document.
//!
//! It also keeps the app's own CSP out of it. `serve.rs` explains the other
//! half of this: origins, not parsers, are what decide what a page can do.
//!
//! ## The one boundary that matters here
//!
//! **This webview is granted no capabilities, ever.** Tauri only injects its
//! IPC into webviews the capability set names, and this one is named by
//! nothing, so `window.__TAURI__` does not exist inside it. That is what makes
//! pointing it at an arbitrary URL a reasonable thing to do at all, and it is
//! why `eval_with_callback` is the only channel back: it runs at the runtime
//! layer (WebView2's `ExecuteScript`, WKWebView's `evaluateJavaScript`), not
//! through Tauri's IPC, so the page needs no privilege for us to read from it.
//! Nothing here may grow a `dangerousRemoteDomainIpcAccess` or an entry in
//! `capabilities/`.
//!
//! ## Rule 2, and why this file is its exception
//!
//! AGENTS.md rule 2 says logic goes on `Emit`, not on `AppHandle`, so a turn
//! can be driven by tests with no window. A browser is not a turn: a child
//! webview *is* a window's child, and there is no version of this that runs
//! headless. So the split here is different — everything decidable without a
//! window ([`to_url`], the bounds arithmetic) is a pure function with tests,
//! and only the webview calls themselves need the real thing.

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, LogicalPosition, LogicalSize, Manager, Webview, WebviewUrl, Wry};

/// The child webview's label. One per window: the browser is the window's, not
/// a conversation's, so there is never a second one to name.
const LABEL: &str = "tcode-browser";

/// Emitted when the page navigates, by any means — the address bar, a link, a
/// redirect, `history.back()`. The frontend's address bar is a view of this,
/// never the source of truth: the webview owns where it is.
pub const BROWSER_NAVIGATED: &str = "tcode://browser-navigated";

/// Where the browser starts. Deliberately not a search engine or a vendor
/// page: the app has no business making a request nobody asked for on startup.
const HOME: &str = "about:blank";

/// The pane rectangle, in CSS pixels as the webview measured it.
///
/// Logical rather than physical because that is what the DOM reports and what
/// Tauri's `LogicalPosition` expects; converting between them here would mean
/// applying the scale factor twice on any display where it is not 1.
#[derive(Deserialize, Serialize, Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Serialize, Clone, Debug)]
pub struct Navigated {
    pub url: String,
    pub title: String,
}

/// The window's one browser.
#[derive(Default)]
pub struct Browser {
    webview: Mutex<Option<Webview<Wry>>>,
}

impl Browser {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Shows the browser at `rect`, creating it on first use.
    ///
    /// Idempotent: the pane calls this whenever it mounts, and a second call
    /// only moves the existing webview. Recreating it would discard the page,
    /// which is the one thing the user was looking at.
    pub fn open(&self, app: &AppHandle, rect: Rect) -> Result<(), String> {
        let mut held = self.webview.lock().expect("browser lock");
        if let Some(webview) = held.as_ref() {
            place(webview, rect)?;
            webview.show().map_err(|error| error.to_string())?;
            return Ok(());
        }

        let window = app
            .get_window("main")
            .ok_or("the main window is not open")?;

        let builder = tauri::webview::WebviewBuilder::new(
            LABEL,
            WebviewUrl::External(HOME.parse().map_err(|_| "bad home url")?),
        )
        // One persistent user-data folder, so cookies, logins and (later)
        // bookmarks survive closing the browser and restarting the app — the
        // folder lives on disk between sessions, like Chrome's profile. The
        // freeze that motivated per-instance folders is avoided differently:
        // this webview is created once per app session and never recreated, so
        // a user-data folder is never handed to a second browser process while
        // the first is still closing (see the close menu in `WebPane.tsx`).
        // Through `home_dir()` rather than `dirs::home_dir()` — every tcode-
        // owned path resolves that one way, so `TCODE_HOME` moves this too
        // (root CLAUDE.md).
        .data_directory(
            tcode_core::home_dir()
                .ok_or("cannot resolve the tcode home directory")?
                .join("browser"),
        )
        .on_navigation({
            let app = app.clone();
            move |url| {
                let url = url.to_string();
                emit(
                    &app,
                    Navigated {
                        url,
                        title: String::new(),
                    },
                );
                // Every navigation is allowed. This is a browser; refusing
                // navigations here would be inventing a policy the user did
                // not ask for, and the isolation that matters is the empty
                // capability set, not a URL allowlist.
                true
            }
        })
        .on_document_title_changed({
            let app = app.clone();
            move |webview, title| {
                let url = webview.url().map(|url| url.to_string()).unwrap_or_default();
                emit(&app, Navigated { url, title });
            }
        });

        let webview = window
            .add_child(
                builder,
                LogicalPosition::new(rect.x, rect.y),
                LogicalSize::new(rect.width.max(1.0), rect.height.max(1.0)),
            )
            .map_err(|error| error.to_string())?;

        *held = Some(webview);
        Ok(())
    }

    /// Moves the webview to follow its pane.
    ///
    /// Called on every layout change, including each frame of a divider drag,
    /// so it must stay cheap and must not care about being called with the
    /// rect it already has.
    pub fn bounds(&self, rect: Rect) -> Result<(), String> {
        match self.webview.lock().expect("browser lock").as_ref() {
            Some(webview) => place(webview, rect),
            None => Ok(()),
        }
    }

    /// Hides or shows the webview without destroying it.
    ///
    /// A native webview composites *above* the HTML, outside any stacking
    /// context the document can reach, so every popover in the window would
    /// otherwise open behind it. `seat.ts` owns the one popover implementation
    /// and therefore the one call site (AGENTS.md rule 17). Dragging a divider
    /// uses it too: hiding beats watching the webview lag a pointer.
    pub fn visible(&self, visible: bool) -> Result<(), String> {
        let held = self.webview.lock().expect("browser lock");
        let Some(webview) = held.as_ref() else {
            return Ok(());
        };
        if visible {
            webview.show().map_err(|error| error.to_string())
        } else {
            webview.hide().map_err(|error| error.to_string())
        }
    }

    pub fn navigate(&self, input: &str) -> Result<(), String> {
        let url = to_url(input)?;
        let held = self.webview.lock().expect("browser lock");
        let webview = held.as_ref().ok_or("the browser is not open")?;
        webview
            .navigate(url.parse().map_err(|_| format!("bad url: {url}"))?)
            .map_err(|error| error.to_string())
    }

    /// Steps the page's own history.
    ///
    /// `eval` rather than a runtime call because Tauri exposes no back/forward:
    /// the page's history is the page's. A consequence worth knowing is that
    /// the buttons cannot be greyed out — whether there is anywhere to go back
    /// to lives across an origin this side cannot read. Going back with no
    /// history does nothing, which is the harmless direction.
    pub fn step(&self, delta: i32) -> Result<(), String> {
        let held = self.webview.lock().expect("browser lock");
        let webview = held.as_ref().ok_or("the browser is not open")?;
        webview
            .eval(format!("history.go({delta})"))
            .map_err(|error| error.to_string())
    }

    pub fn reload(&self) -> Result<(), String> {
        let held = self.webview.lock().expect("browser lock");
        let webview = held.as_ref().ok_or("the browser is not open")?;
        webview.reload().map_err(|error| error.to_string())
    }

    /// Destroys the webview, when its pane closes.
    pub fn close(&self) -> Result<(), String> {
        let Some(webview) = self.webview.lock().expect("browser lock").take() else {
            return Ok(());
        };
        webview.close().map_err(|error| error.to_string())
    }
}

/// Tells the app's own webview where the browser went.
///
/// Never swallowed silently, for the reason `bridge.rs` gives about the event
/// stream: an address bar that stops updating looks exactly like a browser that
/// stopped working.
fn emit(app: &AppHandle, what: Navigated) {
    use tauri::Emitter;
    if let Err(error) = app.emit(BROWSER_NAVIGATED, what) {
        eprintln!("tcode-app: could not emit '{BROWSER_NAVIGATED}': {error}");
    }
}

fn place(webview: &Webview<Wry>, rect: Rect) -> Result<(), String> {
    webview
        .set_position(LogicalPosition::new(rect.x, rect.y))
        .map_err(|error| error.to_string())?;
    // A zero dimension is a legal CSS rect (a pane mid-collapse) and an illegal
    // webview size on some platforms.
    webview
        .set_size(LogicalSize::new(rect.width.max(1.0), rect.height.max(1.0)))
        .map_err(|error| error.to_string())
}

/// What someone typed in the address bar, as a URL.
///
/// The whole of the guesswork, kept pure so it can be tested without a window.
/// Three rules, in order:
///
///  - An explicit scheme is honoured, including `file:` and `about:`.
///  - A loopback host gets **`http`**, not `https`. This is the case the
///    feature exists for — a dev server on `localhost:5173` is plain HTTP, and
///    defaulting it to `https` produces a TLS error page for the single most
///    common thing anyone will type in here.
///  - Anything else that looks like a host gets `https`.
///
/// A bare word is an error rather than a search: this app has no search
/// provider, and quietly sending what someone typed to one would be sending it
/// somewhere they did not name.
pub fn to_url(input: &str) -> Result<String, String> {
    let text = input.trim();
    if text.is_empty() {
        return Err("type an address".into());
    }
    if let Some((scheme, _)) = text.split_once("://") {
        if scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-')
        {
            return Ok(text.to_string());
        }
    }
    if text.starts_with("about:") || text.starts_with("file:") || text.starts_with("data:") {
        return Ok(text.to_string());
    }

    let host = text.split(['/', '?', '#']).next().unwrap_or(text);
    let name = host.split(':').next().unwrap_or(host);
    let loopback = name.eq_ignore_ascii_case("localhost")
        || name == "127.0.0.1"
        || name == "::1"
        || name == "[::1]";
    if loopback {
        return Ok(format!("http://{text}"));
    }
    // A dot or a port is what separates "a host" from "a word someone typed".
    if name.contains('.') || host.contains(':') {
        return Ok(format!("https://{text}"));
    }
    Err(format!(
        "'{text}' is not an address. Type a host (example.com), a loopback address (localhost:5173) or a full URL."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The capability file is scoped by webview label, never by window label.
    ///
    /// This is the single assumption the whole browser rests on, and it is not
    /// visible from anywhere near the code that depends on it. Tauri's own
    /// words (`tauri_utils::acl::capability::Capability::windows`):
    ///
    /// > If a window label matches any of the patterns in this list, the
    /// > capability will be enabled on **all the webviews of that window**,
    /// > regardless of the value of `webviews`.
    ///
    /// The browser is a child webview of `main`. So a single `"windows"` entry
    /// in that file — which is what it had, and what anyone would write by
    /// habit — grants `core:default` to whatever page is loaded in it, which
    /// is `window.__TAURI__`, which is an arbitrary command on this machine.
    /// Nothing fails visibly when that happens: the app works, the browser
    /// works, and every site is trusted.
    #[test]
    fn the_browser_webview_is_granted_nothing() {
        let file = concat!(env!("CARGO_MANIFEST_DIR"), "/capabilities/default.json");
        let text = std::fs::read_to_string(file).expect("capabilities/default.json");
        let capability: serde_json::Value = serde_json::from_str(&text).expect("valid json");

        assert!(
            capability.get("windows").is_none(),
            "capabilities/default.json must scope by `webviews`, not `windows`: a window entry \
             grants every webview in the window, including the browser pane"
        );

        let webviews = capability["webviews"]
            .as_array()
            .expect("`webviews` must name the app webview");
        assert!(
            !webviews.iter().any(|label| {
                let label = label.as_str().unwrap_or_default();
                label == LABEL || label.contains('*')
            }),
            "the browser webview ({LABEL}) must not be granted any capability, and a glob here \
             would grant it: {webviews:?}"
        );
    }

    #[test]
    fn app_owned_decorations_keep_system_color_out_of_the_caption() {
        let file = concat!(env!("CARGO_MANIFEST_DIR"), "/tauri.conf.json");
        let text = std::fs::read_to_string(file).expect("tauri.conf.json");
        let config: serde_json::Value = serde_json::from_str(&text).expect("valid tauri config");

        assert_eq!(
            config["app"]["windows"][0]["decorations"], false,
            "the app-owned title bar must not inherit the operating system's caption color"
        );
    }

    #[test]
    fn an_explicit_scheme_is_left_alone() {
        assert_eq!(
            to_url("https://github.com/x").unwrap(),
            "https://github.com/x"
        );
        assert_eq!(to_url("http://example.com").unwrap(), "http://example.com");
        assert_eq!(to_url("about:blank").unwrap(), "about:blank");
        assert_eq!(to_url("file:///tmp/x.html").unwrap(), "file:///tmp/x.html");
    }

    /// The case the browser pane was asked for. `https` here would put a TLS
    /// error page in front of the single most common thing typed into it.
    #[test]
    fn a_dev_server_is_plain_http() {
        assert_eq!(to_url("localhost:5173").unwrap(), "http://localhost:5173");
        assert_eq!(
            to_url("127.0.0.1:8080/app").unwrap(),
            "http://127.0.0.1:8080/app"
        );
        assert_eq!(to_url("LOCALHOST:3000").unwrap(), "http://LOCALHOST:3000");
        // …and a real host still is not.
        assert_eq!(to_url("github.com").unwrap(), "https://github.com");
    }

    #[test]
    fn a_host_gets_https_and_keeps_its_path() {
        assert_eq!(
            to_url("docs.rs/tauri/latest?q=1#frag").unwrap(),
            "https://docs.rs/tauri/latest?q=1#frag"
        );
    }

    /// Not a search box. Sending what someone typed to a search provider would
    /// be sending it somewhere they did not name.
    #[test]
    fn a_bare_word_is_refused_rather_than_searched() {
        let refusal = to_url("how do i center a div").unwrap_err();
        assert!(refusal.contains("is not an address"), "{refusal}");
        assert!(to_url("   ").is_err());
    }
}
