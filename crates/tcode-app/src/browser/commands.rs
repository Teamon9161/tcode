//! The window's browser, as a small set of verbs over its native child webviews.
//!
//! **This is the one module the shell owns.** Everything in `crate::commands`
//! is written against `Supervisor` and knows nothing about windows; these take
//! an `AppHandle`, because a child webview is a window's child and has no
//! headless form (see `mod.rs`). That is why they are not in
//! [`crate::dispatch::Registry::builtin`] and arrive through [`register`]
//! instead — and why this whole file is what `MIGRATION-ELECTRON.md` replaces
//! with `WebContentsView` rather than ports.
//!
//! What *can* be decided without a window — turning what somebody typed into a
//! URL — is `browser::to_url`, a pure function with its own tests, and it stays
//! put across the migration.
//!
//! Every verb below the first takes an `id`: one tab is one webview, and the
//! tab a keystroke belongs to is rarely the only one open. The id comes from
//! the webview and is therefore data (rule 3) — `mod.rs` answers "that browser
//! tab is not open" rather than falling back to the current one, which would
//! mean reloading or closing a page nobody pointed at.

use std::sync::Arc;

use super::{Browser, Rect};
use crate::dispatch::{arg, handler, Registry};

/// Open a tab.
///
/// `async` on purpose. Tauri's `Window::add_child` documents
/// that on Windows it deadlocks when called from a synchronous command or event
/// handler (the IPC callback runs on the main thread, WebView2 controller
/// creation needs the browser process, and the browser process is waiting for
/// that same IPC call to return). An async command runs on the runtime's thread
/// pool, so `add_child` posts its creation work to the main thread instead of
/// running it inline, and nothing waits on itself. The other browser verbs
/// navigate or resize an existing webview and never create one, so they stay
/// synchronous.
pub async fn browser_open(
    app: &tauri::AppHandle,
    browser: &Arc<Browser>,
    rect: Rect,
) -> Result<String, String> {
    browser.open(app, rect)
}

/// The pane is back on screen with tabs already open — put the current one
/// where it belongs and show it again.
pub fn browser_show(browser: &Arc<Browser>, rect: Rect) -> Result<(), String> {
    browser.show(rect)
}

/// Bring one tab to the front.
pub fn browser_select(browser: &Arc<Browser>, id: String) -> Result<(), String> {
    browser.select(&id)
}

/// Follow the pane. Called for every layout change, including each frame of a
/// divider drag, so it stays a bare setter.
pub fn browser_bounds(browser: &Arc<Browser>, rect: Rect) -> Result<(), String> {
    browser.bounds(rect)
}

/// Give the window back to the HTML for a moment — see `Browser::visible`.
pub fn browser_visible(browser: &Arc<Browser>, visible: bool) -> Result<(), String> {
    browser.visible(visible)
}

/// What the user typed in the address bar. It is data (rule 3): `to_url`
/// refuses what it cannot read rather than guessing a search query out of it.
pub fn browser_navigate(browser: &Arc<Browser>, id: String, url: String) -> Result<(), String> {
    browser.navigate(&id, &url)
}

pub fn browser_step(browser: &Arc<Browser>, id: String, delta: i32) -> Result<(), String> {
    browser.step(&id, delta)
}

pub fn browser_reload(browser: &Arc<Browser>, id: String) -> Result<(), String> {
    browser.reload(&id)
}

/// Close one tab. Answers `false` when it was the last one and has been blanked
/// rather than destroyed — see `browser.rs` on why the profile's webview
/// outlives every tab.
pub fn browser_close(browser: &Arc<Browser>, id: String) -> Result<bool, String> {
    browser.close(&id)
}

/// Hand the shell's browser verbs to the registry.
///
/// The counterpart to [`crate::dispatch::Registry::builtin`]: those commands
/// take what they need out of `Ctx`, these take what they need out of the shell
/// that owns the views, so they arrive captured rather than looked up. `_ctx` is
/// ignored by every one of them, which is the point — a browser verb has no
/// business reaching a conversation.
pub fn register(registry: &mut Registry, app: tauri::AppHandle, browser: Arc<Browser>) {
    macro_rules! verb {
        ($name:literal, $f:ident ($($arg:ident),*)) => {{
            let browser = browser.clone();
            registry.add(
                $name,
                handler(move |_ctx, args| {
                    let browser = browser.clone();
                    Box::pin(async move {
                        let out = $f(&browser, $(arg(args, stringify!($arg))?,)*)?;
                        serde_json::to_value(out).map_err(|error| error.to_string())
                    })
                }),
            );
        }};
    }

    // The one verb that also needs the window itself, because it creates a
    // webview; see `browser_open` on why that has to happen off the main thread.
    {
        let owned = browser.clone();
        registry.add(
            "browser_open",
            handler(move |_ctx, args| {
                let (app, browser) = (app.clone(), owned.clone());
                Box::pin(async move {
                    let out = browser_open(&app, &browser, arg(args, "rect")?).await?;
                    serde_json::to_value(out).map_err(|error| error.to_string())
                })
            }),
        );
    }

    verb!("browser_show", browser_show(rect));
    verb!("browser_select", browser_select(id));
    verb!("browser_bounds", browser_bounds(rect));
    verb!("browser_visible", browser_visible(visible));
    verb!("browser_navigate", browser_navigate(id, url));
    verb!("browser_step", browser_step(id, delta));
    verb!("browser_reload", browser_reload(id));
    verb!("browser_close", browser_close(id));
}
