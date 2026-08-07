//! Where a child webview actually goes, per platform.
//!
//! ## The problem this file exists for
//!
//! On Windows and macOS a child webview is a positioned native surface and
//! `set_bounds` is the whole story. **On GTK it is not, and the failure is
//! silent.** Tauri builds a child webview there by packing it into the window's
//! own vertical `GtkBox` (`tauri-runtime-wry`: `webview_builder.build_gtk(vbox)`
//! for `WebviewKind::WindowChild`), and wry, seeing a `GtkBox`, calls
//! `pack_start(webview, expand: true, fill: true, 0)` and records
//! `is_in_fixed_parent = false` — which is the flag its own `set_bounds` checks
//! before doing anything. So every rect is accepted and discarded, and a
//! vertical box does what a vertical box does: the app's own webview takes the
//! top half of the window, the browser takes the bottom half, both full width.
//! Measured, not deduced — asked for 1040×315 at (236, 110), the webview
//! reported 1280×430 in an 860px-tall window.
//!
//! wry *does* honour bounds for one container, a `GtkFixed`, and Tauri simply
//! never creates one. So this file makes one.
//!
//! ## What it does to the window
//!
//! Once, at startup, before anything is on screen:
//!
//! ```text
//! window                    window
//!  └ vbox          →         └ vbox
//!     └ app                     └ stage: Overlay
//!                                  ├ app          (fills)
//!                                  └ layer: Fixed (positions browser tabs)
//! ```
//!
//! A `GtkFixed` has no window of its own, so it does not take input where it is
//! empty — the app underneath keeps every click except over a tab.
//!
//! ## Three rules that are easy to break
//!
//! **Widgets are never stored.** GTK objects are `!Send`, and this module is
//! driven from behind a `Mutex<State>` shared with the async command that
//! creates webviews. So nothing is kept between calls: the layer is found by
//! widget name, and each tab's widget by *its own* name, which is set to the
//! webview's Tauri label when it is adopted. The lookup is a walk over a
//! handful of children and it removes a whole class of lifetime problem.
//!
//! **Every call happens on the main thread**, through `run_on_main_thread` —
//! GTK has no other legal thread, and `Browser::open` runs on the async
//! runtime's pool. Posting from the main thread is fine (it queues), so there
//! is one rule rather than two paths, and the queue is what keeps
//! adopt-then-place in order.
//!
//! **A tab is sized by `size_allocate`, never by `set_size_request` alone.** A
//! request is a minimum, and a minimum can only make a widget bigger — see
//! [`gtk_impl::place`], which is where the whole of that story lives.

use tauri::{AppHandle, Webview, Wry};

use super::Rect;

/// Prepare the window to hold positioned child webviews.
///
/// Called once from `main.rs`'s setup, deliberately before the first browser
/// tab exists: the app's own webview is re-parented here, and doing that to a
/// window somebody is already reading is a flicker at best.
pub fn install(app: &AppHandle) -> Result<(), String> {
    let _ = app;
    #[cfg(gtk)]
    {
        use tauri::Manager as _;
        let window = app
            .get_window("main")
            .ok_or("the main window is not open")?;
        let vbox = window.default_vbox().map_err(|error| error.to_string())?;
        gtk_impl::install(&vbox)?;
    }
    Ok(())
}

/// Take a freshly created child webview out of wherever the platform put it.
///
/// A no-op everywhere but GTK, where "wherever the platform put it" is the
/// window's vertical box, stacked under the app.
pub fn adopt(webview: &Webview<Wry>) {
    let _ = webview;
    #[cfg(gtk)]
    {
        let label = webview.label().to_string();
        on_main(webview, move |vbox| gtk_impl::adopt(vbox, &label));
    }
}

/// Put a webview over its pane.
pub fn place(webview: &Webview<Wry>, rect: Rect) -> Result<(), String> {
    let _ = (webview, rect);

    #[cfg(gtk)]
    {
        let label = webview.label().to_string();
        on_main(webview, move |vbox| gtk_impl::place(vbox, &label, rect));
    }

    // Everywhere else the platform positions the surface itself, and this is
    // **one** `set_bounds` rather than a `set_position` followed by a
    // `set_size`: Tauri implements each of those by reading the webview's
    // current bounds and writing back one field, and that read cannot answer
    // where a child webview is — so the size call would undo the position.
    // `SetBounds` takes both fields as given.
    #[cfg(not(gtk))]
    {
        use tauri::{LogicalPosition, LogicalSize};
        webview
            .set_bounds(tauri::Rect {
                position: LogicalPosition::new(rect.x, rect.y).into(),
                // A zero dimension is a legal CSS rect (a pane mid-collapse) and
                // an illegal webview size on some platforms.
                size: LogicalSize::new(rect.width.max(1.0), rect.height.max(1.0)).into(),
            })
            .map_err(|error| error.to_string())?;
    }

    Ok(())
}

/// Run something against the window's box, on the one thread GTK allows.
///
/// Failures are printed rather than returned: this is posted work, so by the
/// time it fails its caller is long gone. A browser in the wrong place is
/// visible anyway; a browser that silently stopped following its pane is what
/// the printing is for.
#[cfg(gtk)]
fn on_main<F>(webview: &Webview<Wry>, what: F)
where
    F: FnOnce(&gtk::Box) -> Result<(), String> + Send + 'static,
{
    use tauri::Manager as _;

    let app = webview.app_handle().clone();
    let posted = app.clone().run_on_main_thread(move || {
        let Some(window) = app.get_window("main") else {
            return;
        };
        let outcome = window
            .default_vbox()
            .map_err(|error| error.to_string())
            .and_then(|vbox| what(&vbox));
        if let Err(error) = outcome {
            eprintln!("tcode-app: could not place the browser: {error}");
        }
    });
    if let Err(error) = posted {
        eprintln!("tcode-app: could not reach the main thread: {error}");
    }
}

#[cfg(gtk)]
mod gtk_impl {
    use gtk::prelude::*;

    use super::Rect;

    /// What the inserted widgets are called. Names rather than stored handles,
    /// for the reason in the module header: GTK objects are `!Send`.
    const STAGE: &str = "tcode-browser-stage";
    const LAYER: &str = "tcode-browser-layer";

    pub fn install(vbox: &gtk::Box) -> Result<(), String> {
        if named(vbox, STAGE).is_some() {
            return Ok(());
        }
        // At this point the box holds exactly one thing: the app's own webview.
        let app_webview = vbox
            .children()
            .into_iter()
            .next()
            .ok_or("the window has no webview to build the browser layer around")?;

        let stage = gtk::Overlay::new();
        stage.set_widget_name(STAGE);
        let layer = gtk::Fixed::new();
        layer.set_widget_name(LAYER);
        // The layer spans the window so a tab can be placed anywhere in it.
        layer.set_halign(gtk::Align::Fill);
        layer.set_valign(gtk::Align::Fill);

        vbox.remove(&app_webview);
        stage.add(&app_webview);
        stage.add_overlay(&layer);
        // **Without this the app is unclickable**, and nothing else about it
        // looks wrong. A `GtkFixed` has no window of its own, which is what
        // makes it sound safe to stretch one over the whole app — but
        // `GtkOverlay` gives *every* overlay child its own `GdkWindow` so it
        // can composite it on top, and that window is what swallows the
        // pointer. Pass-through is GDK's name for `pointer-events: none`, and
        // its documented behaviour is exactly the shape needed here: the layer
        // itself stops receiving anything, while a subwindow inside it — every
        // browser tab is one — still does.
        stage.set_overlay_pass_through(&layer, true);
        vbox.pack_start(&stage, true, true, 0);

        // `show`, never `show_all`: the tabs decide their own visibility (only
        // the current one is on screen, and only while the pane is), and
        // `show_all` would overrule every one of those decisions.
        stage.show();
        layer.show();
        app_webview.show();
        Ok(())
    }

    /// Move the child webview Tauri just packed into the box onto the layer.
    ///
    /// The new one is identified as the box's child that is not the stage —
    /// true because every earlier tab was moved out by this same function, so
    /// the box is only ever `[stage]` or `[stage, the new one]`.
    pub fn adopt(vbox: &gtk::Box, label: &str) -> Result<(), String> {
        let layer = layer(vbox)?;
        let fresh = vbox
            .children()
            .into_iter()
            .find(|child| child.widget_name() != STAGE)
            .ok_or("the new browser webview is not in the window's box")?;
        // Its name is how `place` finds it again.
        fresh.set_widget_name(label);
        vbox.remove(&fresh);
        layer.put(&fresh, 0, 0);
        Ok(())
    }

    pub fn place(vbox: &gtk::Box, label: &str, rect: Rect) -> Result<(), String> {
        let layer = layer(vbox)?;
        let tab = layer
            .children()
            .into_iter()
            .find(|child| child.widget_name() == label)
            .ok_or("that browser tab is not on the layer")?;

        // A zero dimension is a legal CSS rect (a pane mid-collapse) and a
        // widget that never comes back: GTK reads a zero size request as "ask
        // the widget what it wants", and a webview wants nothing.
        let (x, y) = (rect.x as i32, rect.y as i32);
        let (width, height) = (rect.width.max(1.0) as i32, rect.height.max(1.0) as i32);

        // The request and the position are what the *next* allocation cycle
        // will read — a window resize, a theme change, anything that makes the
        // layer lay its children out again.
        tab.set_size_request(width, height);
        layer.move_(&tab, x, y);

        // **And this is what makes it happen, and the only thing that can make
        // a tab get smaller.** `set_size_request` sets a *minimum*, so lowering
        // it permits a smaller widget without producing one: the allocation a
        // `GtkFixed` hands out is the child's preferred size, and a
        // WebKitWebView that has already been given 1040px goes on preferring
        // 1040px. Measured with a page that prints its own `innerWidth`: with
        // the request alone the page sat at 895 through a shrink to 518 and a
        // grow to 771 without moving once. wry splits it the same way — it
        // requests a size when it `put`s a webview in a fixed parent, and every
        // `set_bounds` after that is this call.
        tab.size_allocate(&gtk::Allocation::new(x, y, width, height));

        // **And once more when the main loop next turns, because an allocation
        // made around a `show()` reaches the widget and not the page.** Hiding
        // is not a private matter between us and GTK: `Webview::show` goes
        // through wry's event loop while this runs on `run_on_main_thread`, so
        // "show it, then place it" is two queues with no order between them,
        // and an allocation that lands on a widget the toolkit still considers
        // hidden updates its `allocated_size` and never reaches the web
        // process. What that looks like is the browser's *frame* resizing while
        // the page inside keeps its old width, the rest of the pane left as
        // unpainted grey — and it is the common case, not a corner: dragging a
        // divider hides the browser for the whole drag (`browserYield.ts`) and
        // shows it on release. Proved by nudging the window 1px afterwards,
        // which is nothing but a second `place` with the webview already up:
        // the page snapped from a stale 678 to the correct 962.
        //
        // Idle rather than a timeout: it is the first moment the toolkit has
        // finished whatever it was doing, which is all this is waiting for.
        // Re-applying an allocation the widget already has is free, idles run
        // in the order they were added so the freshest rect is still the last
        // word, and the whole cost lands only where something moved.
        let tab = tab.clone();
        gtk::glib::idle_add_local_once(move || {
            tab.size_allocate(&gtk::Allocation::new(x, y, width, height));
        });
        Ok(())
    }

    fn layer(vbox: &gtk::Box) -> Result<gtk::Fixed, String> {
        let stage = named(vbox, STAGE)
            .ok_or("the browser layer was never installed — see `install`")?
            .downcast::<gtk::Overlay>()
            .map_err(|_| "the browser stage is not an overlay any more")?;
        stage
            .children()
            .into_iter()
            .find(|child| child.widget_name() == LAYER)
            .ok_or("the browser layer is gone from its stage")?
            .downcast::<gtk::Fixed>()
            .map_err(|_| "the browser layer is not a fixed any more".to_string())
    }

    fn named(vbox: &gtk::Box, name: &str) -> Option<gtk::Widget> {
        vbox.children()
            .into_iter()
            .find(|child| child.widget_name() == name)
    }
}
