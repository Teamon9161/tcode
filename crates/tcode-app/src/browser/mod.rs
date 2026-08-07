//! The window's browser: native child webviews, one per tab, over one pane.
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
//! **These webviews are granted no capabilities, ever.** Tauri only injects its
//! IPC into webviews the capability set names, and none of these is named by
//! anything, so `window.__TAURI__` does not exist inside them. That is what
//! makes pointing one at an arbitrary URL a reasonable thing to do at all, and
//! it is why `eval_with_callback` is the only channel back: it runs at the
//! runtime layer (WebView2's `ExecuteScript`, WKWebView's `evaluateJavaScript`),
//! not through Tauri's IPC, so the page needs no privilege for us to read from
//! it. Nothing here may grow a `dangerousRemoteDomainIpcAccess` or an entry in
//! `capabilities/`.
//!
//! ## One tab, one webview — and never zero
//!
//! Tabs are separate webviews rather than one webview with a history per tab,
//! because a tab is a *live page*: a dev server mid-reload, a form half filled,
//! a video playing. Re-navigating one webview between tabs would keep only the
//! addresses and throw the pages away, which is the one thing a tab is for.
//!
//! They all share one `data_directory`, so cookies and logins are the browser's
//! rather than a tab's. That sharing is also the reason for a rule that looks
//! arbitrary from the frontend: **the last webview is never destroyed.**
//! `tauri-runtime-wry` keys its `WebContext` on the data directory and drops it
//! when the last webview referencing it goes (on Windows and macOS; Linux keeps
//! it deliberately). Dropping it tears down the WebView2 environment holding
//! that profile folder, and creating the next tab immediately after hands the
//! same folder to a second browser process while the first is still closing —
//! the freeze that once motivated per-instance profile folders. So closing the
//! last tab blanks its webview instead of closing it ([`Browser::close`]
//! reports which of the two happened), and the profile is opened exactly once
//! per app session.
//!
//! ## Rule 2, and why this file is its exception
//!
//! AGENTS.md rule 2 says logic goes on `Emit`, not on `AppHandle`, so a turn
//! can be driven by tests with no window. A browser is not a turn: a child
//! webview *is* a window's child, and there is no version of this that runs
//! headless. So the split here is different — everything decidable without a
//! window ([`to_url`], the bounds arithmetic) is a pure function with tests,
//! and only the webview calls themselves need the real thing.

mod place;

pub use place::install;

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, LogicalPosition, LogicalSize, Manager, Webview, WebviewUrl, Wry};

/// What every browser webview's label starts with. The tab's id follows, so a
/// label is unique per tab and recognisable as this app's — which is what the
/// capability test below matches on.
const LABEL_PREFIX: &str = "tcode-browser-";

/// Emitted when a page navigates, by any means — the address bar, a link, a
/// redirect, `history.back()`. The frontend's address bar is a view of this,
/// never the source of truth: the webview owns where it is. Carries the tab's
/// id, because the tab that moved is rarely the only one open.
pub const BROWSER_NAVIGATED: &str = "tcode://browser-navigated";

/// Where a tab starts. Deliberately not a search engine or a vendor page: the
/// app has no business making a request nobody asked for.
const HOME: &str = "about:blank";

/// The pane rectangle, in CSS pixels as the webview measured it.
///
/// Logical rather than physical because that is what the DOM reports and what
/// Tauri's `LogicalPosition` expects; converting between them here would mean
/// applying the scale factor twice on any display where it is not 1.
#[derive(Deserialize, Serialize, Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Serialize, Clone, Debug)]
pub struct Navigated {
    pub id: String,
    pub url: String,
    pub title: String,
}

/// One tab: the page, and the id the frontend's strip knows it by.
struct Tab {
    id: String,
    webview: Webview<Wry>,
}

/// Everything the browser has to remember between calls.
///
/// `rect` and `shown` are here because a tab that is *created* while the pane
/// is off screen, or selected while a popover has borrowed the window, has to
/// arrive in the state the pane is actually in — the frontend told us once and
/// must not have to tell us again on every verb.
#[derive(Default)]
struct State {
    tabs: Vec<Tab>,
    /// The tab on screen. Empty before the first one exists.
    current: String,
    /// The pane's rectangle, as last reported.
    rect: Rect,
    /// Whether the pane wants the browser on screen at all (see [`Browser::visible`]).
    shown: bool,
}

impl State {
    fn find(&self, id: &str) -> Result<&Webview<Wry>, String> {
        self.tabs
            .iter()
            .find(|tab| tab.id == id)
            .map(|tab| &tab.webview)
            .ok_or_else(|| "that browser tab is not open".to_string())
    }
}

/// The window's browser.
#[derive(Default)]
pub struct Browser {
    state: Mutex<State>,
}

impl Browser {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Opens a new tab at `rect` and makes it the current one.
    ///
    /// Returns the tab's id, which is the frontend strip's identity for it —
    /// there is no second, frontend-side numbering to keep in step.
    ///
    /// **The lock is not held across `add_child`, and that is load-bearing
    /// twice over.** Creating a webview takes real time — a whole WebKit or
    /// WebView2 instance — and it does that work *on the main thread*, which is
    /// also the thread every synchronous command in this file runs on. Holding
    /// the lock here would mean a `browser_bounds` arriving mid-creation blocks
    /// the main thread on a mutex that is waiting for the main thread. It would
    /// also be the wrong answer even if it worked: the pane keeps measuring
    /// while the webview is being built, so the rect this was *called* with is
    /// already stale by the time there is anything to place. The freshest one
    /// wins, and it is read back out of the state after creation.
    pub fn open(&self, app: &AppHandle, rect: Rect) -> Result<String, String> {
        self.state.lock().expect("browser lock").rect = rect;

        let window = app
            .get_window("main")
            .ok_or("the main window is not open")?;

        let id = uuid::Uuid::new_v4().to_string();
        let builder = tauri::webview::WebviewBuilder::new(
            format!("{LABEL_PREFIX}{id}"),
            WebviewUrl::External(HOME.parse().map_err(|_| "bad home url")?),
        )
        // One persistent user-data folder for every tab, so cookies, logins and
        // (later) bookmarks are the browser's rather than one tab's, and
        // survive closing tabs and restarting the app — the folder lives on
        // disk between sessions, like Chrome's profile. It is also why the last
        // webview is never destroyed; see the module header.
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
            let id = id.clone();
            move |url| {
                let url = url.to_string();
                emit(
                    &app,
                    Navigated {
                        id: id.clone(),
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
            let id = id.clone();
            move |webview, title| {
                let url = webview.url().map(|url| url.to_string()).unwrap_or_default();
                emit(
                    &app,
                    Navigated {
                        id: id.clone(),
                        url,
                        title,
                    },
                );
            }
        });

        let webview = window
            .add_child(
                builder,
                LogicalPosition::new(rect.x, rect.y),
                LogicalSize::new(rect.width.max(1.0), rect.height.max(1.0)),
            )
            .map_err(|error| error.to_string())?;

        let mut state = self.state.lock().expect("browser lock");

        // Off the container the platform put it in and onto the one this app
        // can position. A no-op except on GTK; `place::adopt` is where the
        // whole of that story lives.
        place::adopt(&webview);

        // The tab that was on screen steps aside. Native webviews composite
        // above the HTML and above each other, so "not the current tab" has to
        // mean hidden — there is no z-order the document can impose on them.
        for tab in &state.tabs {
            let _ = tab.webview.hide();
        }

        if state.shown {
            let _ = webview.show();
        } else {
            // Created while the pane is off screen (another pane is expanded, a
            // popover has the window). `add_child` shows what it creates, so
            // this is the one place that can put it back.
            let _ = webview.hide();
        }
        // Placed again, from the rect as it is *now* and after the visibility
        // is settled — see `place` for why that order, and `bounds` for why the
        // creation rect is not the one to trust.
        let rect = state.rect;
        place::place(&webview, rect)?;
        trace("open", &webview, rect);

        state.tabs.push(Tab {
            id: id.clone(),
            webview,
        });
        state.current = id.clone();
        Ok(id)
    }

    /// Puts the current tab back on screen at `rect`, when the pane mounts with
    /// tabs already open.
    ///
    /// Idempotent, and it must be: the pane calls it on every mount, and hiding
    /// the pane never destroyed anything.
    pub fn show(&self, rect: Rect) -> Result<(), String> {
        let mut state = self.state.lock().expect("browser lock");
        state.rect = rect;
        state.shown = true;
        let Ok(webview) = state.find(&state.current.clone()) else {
            return Ok(());
        };
        webview.show().map_err(|error| error.to_string())?;
        place::place(webview, rect)?;
        trace("show", webview, rect);
        Ok(())
    }

    /// Brings one tab to the front.
    pub fn select(&self, id: &str) -> Result<(), String> {
        let mut state = self.state.lock().expect("browser lock");
        let rect = state.rect;
        let shown = state.shown;
        let chosen = state.find(id)?.clone();
        for tab in &state.tabs {
            if tab.id != id {
                let _ = tab.webview.hide();
            }
        }
        state.current = id.to_string();
        if shown {
            chosen.show().map_err(|error| error.to_string())?;
        }
        place::place(&chosen, rect)?;
        trace("select", &chosen, rect);
        Ok(())
    }

    /// Moves the current tab to follow its pane.
    ///
    /// Called on every layout change, including each frame of a divider drag,
    /// so it must stay cheap and must not care about being called with the
    /// rect it already has. Only the current tab is placed: the others are
    /// hidden, and they are placed when they are selected.
    pub fn bounds(&self, rect: Rect) -> Result<(), String> {
        let mut state = self.state.lock().expect("browser lock");
        state.rect = rect;
        // Remembered even when there is no tab to put it on — a rect that
        // arrives while a webview is being created is the one the webview will
        // want, and the pane will not send it twice.
        match state.find(&state.current.clone()) {
            Ok(webview) => {
                place::place(webview, rect)?;
                trace("bounds", webview, rect);
                Ok(())
            }
            Err(_) => Ok(()),
        }
    }

    /// Hides or shows the current tab without destroying anything.
    ///
    /// A native webview composites *above* the HTML, outside any stacking
    /// context the document can reach, so every popover in the window would
    /// otherwise open behind it. `seat.ts` owns the one popover implementation
    /// and therefore the one call site (AGENTS.md rule 17). Dragging a divider
    /// uses it too: hiding beats watching the webview lag a pointer.
    pub fn visible(&self, visible: bool) -> Result<(), String> {
        let mut state = self.state.lock().expect("browser lock");
        state.shown = visible;
        let rect = state.rect;
        let Ok(webview) = state.find(&state.current.clone()) else {
            return Ok(());
        };
        if !visible {
            return webview.hide().map_err(|error| error.to_string());
        }
        webview.show().map_err(|error| error.to_string())?;
        // Placed again on the way back, and this is the whole reason `rect`
        // lives in the state. A popover borrows the window for a moment and
        // gives it back; a pane stops being covered by an expanded one. Nothing
        // on the frontend reports a rect for either, because from the DOM's
        // side nothing *moved* — so a show that does not re-place is a browser
        // that comes back wherever the toolkit last left the widget.
        place::place(webview, rect)?;
        trace("visible", webview, rect);
        Ok(())
    }

    pub fn navigate(&self, id: &str, input: &str) -> Result<(), String> {
        let url = to_url(input)?;
        let state = self.state.lock().expect("browser lock");
        state
            .find(id)?
            .navigate(url.parse().map_err(|_| format!("bad url: {url}"))?)
            .map_err(|error| error.to_string())
    }

    /// Steps one tab's own history.
    ///
    /// `eval` rather than a runtime call because Tauri exposes no back/forward:
    /// the page's history is the page's. A consequence worth knowing is that
    /// the buttons cannot be greyed out — whether there is anywhere to go back
    /// to lives across an origin this side cannot read. Going back with no
    /// history does nothing, which is the harmless direction.
    pub fn step(&self, id: &str, delta: i32) -> Result<(), String> {
        let state = self.state.lock().expect("browser lock");
        state
            .find(id)?
            .eval(format!("history.go({delta})"))
            .map_err(|error| error.to_string())
    }

    pub fn reload(&self, id: &str) -> Result<(), String> {
        let state = self.state.lock().expect("browser lock");
        state.find(id)?.reload().map_err(|error| error.to_string())
    }

    /// Closes one tab, and answers whether the webview is gone.
    ///
    /// `false` means it was the last one and has been blanked and hidden
    /// instead — the module header explains why the profile's webview outlives
    /// every tab. The frontend needs the answer because its strip has to say
    /// the same thing this side does: a tab removed, or a tab back at its blank
    /// start.
    pub fn close(&self, id: &str) -> Result<bool, String> {
        let mut state = self.state.lock().expect("browser lock");
        let at = state
            .tabs
            .iter()
            .position(|tab| tab.id == id)
            .ok_or("that browser tab is not open")?;

        if state.tabs.len() == 1 {
            let webview = &state.tabs[at].webview;
            let _ = webview.hide();
            webview
                .navigate(HOME.parse().map_err(|_| "bad home url")?)
                .map_err(|error| error.to_string())?;
            state.shown = false;
            return Ok(false);
        }

        let tab = state.tabs.remove(at);
        tab.webview.close().map_err(|error| error.to_string())?;
        if state.current == id {
            // Whoever the frontend selects next will `select` it; until then
            // nothing is current, so nothing is shown.
            state.current.clear();
        }
        Ok(true)
    }

    /// Every tab, on the app's own exit.
    ///
    /// The window closing is not a frontend event any pane can intercept (the
    /// caption is non-client area), and WebView2 hangs when the parent window
    /// is destroyed with a child controller still alive — so this is the one
    /// place the floor of one webview does not apply.
    pub fn close_all(&self) {
        let mut state = self.state.lock().expect("browser lock");
        state.current.clear();
        for tab in state.tabs.drain(..) {
            let _ = tab.webview.close();
        }
    }

    /// How many tabs are open. For tests and for nothing else.
    pub fn count(&self) -> usize {
        self.state.lock().expect("browser lock").tabs.len()
    }
}

/// Tells the app's own webview where a tab went.
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

/// What the webview actually did with a rect, when `TCODE_BROWSER_DEBUG` is set.
///
/// This exists because the browser is the one surface in this app that nothing
/// else can observe. It is not in the DOM, the design preview cannot composite
/// it, and a screenshot of it in the wrong place says only that — so a webview
/// parked over half the window is a bug with no evidence attached to it. The
/// read-back is the point: it separates "we sent the wrong rect" from "we sent
/// the right rect and the platform did something else with it", which are
/// different bugs with no shared fix.
///
/// Off by default because `bounds` runs on every frame of a divider drag.
///
/// **The read-back is only half trustworthy, and knowing which half is the
/// point.** `size` is measured (`allocated_size`). `position` is not: on GTK
/// without X11 the runtime has no way to ask where a child webview is and
/// answers `(0, 0)` — the very gap that makes `set_size` destructive there, see
/// [`place`]. So a position of `0, 0` in this output means "not known", not
/// "at the origin", and a *size* that disagrees with what was asked is the
/// signal worth chasing.
fn trace(what: &str, webview: &Webview<Wry>, asked: Rect) {
    if std::env::var_os("TCODE_BROWSER_DEBUG").is_none() {
        return;
    }
    let at = webview.position();
    let size = webview.size();
    eprintln!(
        "tcode-app: browser {what}: asked {asked:?} (logical), reports {at:?} \
         sized {size:?} (physical; position is unknowable on gtk/wayland)"
    );
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
    /// The browser's tabs are child webviews of `main`. So a single `"windows"`
    /// entry in that file — which is what it had, and what anyone would write
    /// by habit — grants `core:default` to whatever page is loaded in them,
    /// which is `window.__TAURI__`, which is an arbitrary command on this
    /// machine. Nothing fails visibly when that happens: the app works, the
    /// browser works, and every site is trusted.
    #[test]
    fn the_browser_webviews_are_granted_nothing() {
        let file = concat!(env!("CARGO_MANIFEST_DIR"), "/capabilities/default.json");
        let text = std::fs::read_to_string(file).expect("capabilities/default.json");
        let capability: serde_json::Value = serde_json::from_str(&text).expect("valid json");

        assert!(
            capability.get("windows").is_none(),
            "capabilities/default.json must scope by `webviews`, not `windows`: a window entry \
             grants every webview in the window, including the browser's tabs"
        );

        let webviews = capability["webviews"]
            .as_array()
            .expect("`webviews` must name the app webview");
        assert!(
            !webviews.iter().any(|label| {
                let label = label.as_str().unwrap_or_default();
                label.starts_with(LABEL_PREFIX) || label.contains('*')
            }),
            "no browser webview ({LABEL_PREFIX}*) may be granted a capability, and a glob here \
             would grant every one of them: {webviews:?}"
        );
    }

    /// An app-owned title bar has to be granted the commands its controls call.
    ///
    /// `core:default` looks like it covers windows, and it does — the *reading*
    /// half. `is-maximized`, `title`, the monitor list and
    /// `internal-toggle-maximize` are in it; `minimize`, `toggle-maximize`,
    /// `close` and `start-dragging` are not. So the three buttons rejected into
    /// a `console.warn` and the bar could not be dragged, on a window that has
    /// no system caption to fall back to — the same failure mode rule 6 is
    /// about, one layer along: nothing throws, nothing is logged where anyone
    /// looks, the controls are simply inert.
    ///
    /// Pinned from the component rather than as a fixed list, so a control
    /// added later brings its grant with it or fails here.
    #[test]
    fn the_title_bar_is_granted_the_commands_it_calls() {
        /// Window methods that *act*, and the permission each one needs. The
        /// readers `WindowControls` also uses (`isMaximized`, `onResized`) are
        /// in `core:default` already and are deliberately absent.
        const ACTS: &[(&str, &str)] = &[
            (".minimize()", "core:window:allow-minimize"),
            (".maximize()", "core:window:allow-maximize"),
            (".unmaximize()", "core:window:allow-unmaximize"),
            (".unminimize()", "core:window:allow-unminimize"),
            (".toggleMaximize()", "core:window:allow-toggle-maximize"),
            (".close()", "core:window:allow-close"),
            (".hide()", "core:window:allow-hide"),
            (".show()", "core:window:allow-show"),
            (".setTitle(", "core:window:allow-set-title"),
            (".setFullscreen(", "core:window:allow-set-fullscreen"),
            (".startDragging()", "core:window:allow-start-dragging"),
        ];

        let component = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/ui/src/components/WindowControls.tsx"
        ))
        .expect("ui/src/components/WindowControls.tsx");
        let text = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/capabilities/default.json"
        ))
        .expect("capabilities/default.json");
        let capability: serde_json::Value = serde_json::from_str(&text).expect("valid json");
        let granted: Vec<&str> = capability["permissions"]
            .as_array()
            .expect("`permissions` must be a list")
            .iter()
            .filter_map(|entry| entry.as_str())
            .collect();

        for (call, permission) in ACTS {
            if !component.contains(call) {
                continue;
            }
            assert!(
                granted.contains(permission),
                "the title bar calls `{call}` but capabilities/default.json does not grant \
                 `{permission}` — the control will reject and do nothing"
            );
        }

        // Tauri's own drag-region script calls `start_dragging` on pointer-down
        // over `data-tauri-drag-region`, so the grant is needed even though no
        // line here calls it (AGENTS.md rule 9c puts that attribute on the bar).
        assert!(
            component.contains("data-tauri-drag-region"),
            "the drag region moved out of WindowControls.tsx — this test and the \
             `start-dragging` grant below it now pin nothing"
        );
        assert!(
            granted.contains(&"core:window:allow-start-dragging"),
            "the title bar carries `data-tauri-drag-region` but the window cannot be dragged \
             without `core:window:allow-start-dragging`"
        );
    }

    /// A command the webview calls but nobody registered rejects its promise
    /// and does nothing else — the failure mode rule 6 is about, invisible from
    /// either side on its own. The terminal has the same test; the browser
    /// grew a tab strip's worth of new verbs and needed one.
    #[test]
    fn every_browser_command_the_frontend_calls_is_registered() {
        let host =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/ui/src/webHost.ts"))
                .expect("ui/src/webHost.ts");
        let yielded = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/ui/src/browserYield.ts"
        ))
        .expect("ui/src/browserYield.ts");
        let main = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/main.rs"))
            .expect("src/main.rs");

        // Every `"browser_…"` string literal in the two files that talk to this
        // module. Matching the literal rather than the `invoke(` around it: the
        // command name is the whole of the contract, and a scanner that has to
        // find the call as well is a scanner that quietly stops matching when
        // somebody wraps one.
        let called: Vec<&str> = [host.as_str(), yielded.as_str()]
            .iter()
            .flat_map(|text| {
                text.match_indices("\"browser_").filter_map(|(at, _)| {
                    let rest = &text[at + 1..];
                    Some(&rest[..rest.find('"')?])
                })
            })
            .collect();
        assert!(
            !called.is_empty(),
            "no browser commands found in webHost.ts — this test now pins nothing"
        );

        for command in called {
            assert!(
                main.contains(&format!("commands::{command},")),
                "the frontend calls `{command}` but main.rs does not register it — the call will \
                 reject and the browser will look inert"
            );
        }
    }

    /// A webview is placed with one `set_bounds`, never with `set_position`
    /// plus `set_size`.
    ///
    /// The pair reads as the obvious way to write it and is what this module
    /// had. Tauri implements each half by reading the webview's current bounds
    /// and writing back one field, and on GTK that read cannot answer where a
    /// child webview is — it says `(0, 0)` — so the size call posts the origin
    /// as the position and undoes the placement. Pinned mechanically because
    /// "these two calls say the same thing more clearly" is a change somebody
    /// will reach for, and the symptom is one platform drawing a browser in the
    /// wrong place with nothing in any log.
    #[test]
    fn a_webview_is_placed_with_one_call() {
        let source =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/browser/place.rs"))
                .expect("src/browser/place.rs");

        // Spelled in halves so this test does not match itself through the file
        // it scans — they are neighbours in the same module.
        for call in [concat!(".set_", "position("), concat!(".set_", "size(")] {
            assert!(
                !source.contains(call),
                "`{call}` is back in browser/place.rs — placing a child webview in two calls loses \
                 the position on GTK; use one `set_bounds`"
            );
        }
        assert!(
            source.contains(".set_bounds("),
            "nothing places a webview any more — this test now pins nothing"
        );
    }

    /// A tab on the GTK layer is sized by allocation, not by request alone.
    ///
    /// `set_size_request` reads as "make it this size" and is a *minimum*, so a
    /// pane that grows is followed and a pane that shrinks is not: the browser
    /// keeps whatever width it was widest at and covers whatever opened beside
    /// it. Nothing errors, and the half that still works is the half anybody
    /// tries first — a divider dragged outward.
    ///
    /// Pinned mechanically because the pair looks redundant (they name the same
    /// two numbers twice) and deleting the allocation is the plausible tidy-up.
    #[test]
    fn a_gtk_tab_is_sized_by_allocation() {
        let source =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/browser/place.rs"))
                .expect("src/browser/place.rs");
        assert!(
            source.contains(".size_allocate("),
            "browser/place.rs sizes a tab with `set_size_request` alone — that is a minimum, so \
             the browser will grow with its pane and never shrink back"
        );
        // The second half, and the one that looks most like a redundant line to
        // delete: an allocation made around a `show()` updates the widget and
        // never reaches the page, so a divider drag — which hides the browser
        // for its whole length — leaves the frame resized and the page inside
        // it at its old width, the rest of the pane unpainted grey.
        assert!(
            source.contains("idle_add_local_once"),
            "browser/place.rs places a tab exactly once — an allocation that lands while the \
             toolkit still considers the webview hidden never reaches the web process, so a \
             divider drag ends with the page at its old width"
        );
    }

    /// `cfg(gtk)` and the `gtk` dependency cover the same targets.
    ///
    /// They are two lists in two files and they are not independent: the code
    /// behind `cfg(gtk)` calls into the `gtk` crate, so a target that gets the
    /// flag without the dependency does not compile, and one that gets the
    /// dependency without the flag silently keeps the broken placement this
    /// module exists to replace. Only the second is quiet, which is why this is
    /// a test rather than a comment.
    #[test]
    fn the_gtk_flag_and_the_gtk_dependency_agree() {
        let build = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/build.rs"))
            .expect("build.rs");
        let manifest = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
            .expect("Cargo.toml");

        for target in ["linux", "dragonfly", "freebsd", "netbsd", "openbsd"] {
            assert!(
                build.contains(&format!("\"{target}\"")),
                "build.rs does not set `cfg(gtk)` for {target}, but Cargo.toml gives it the gtk \
                 dependency — the browser pane would fall back to a placement that does nothing"
            );
            assert!(
                manifest.contains(&format!("target_os = \"{target}\"")),
                "Cargo.toml does not give {target} the gtk dependency, but build.rs sets \
                 `cfg(gtk)` for it — that target will not compile"
            );
        }
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
