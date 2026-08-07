// The window's browser: one `WebContentsView` per tab, over one pane.
//
// This is the file `src/browser/` becomes. The wire contract is unchanged to
// the byte — same nine verbs, same arguments, same `tcode://browser-navigated`
// payload — because the frontend's half of it (`ui/src/webHost.ts`, `web.ts`,
// `WebPane.tsx`) is not part of this migration and should not be able to tell.
//
// ## What is the same, and will not stop being the same
//
// A `WebContentsView` composites **above** the renderer, exactly as a Tauri
// child webview did: it is a native view in the window's view tree, not a DOM
// node, so no `z-index` reaches it. That is measured, not assumed — the Phase 0
// spike put a `z-index: 2147483647` element under one and read the screen back
// (`../spike/results.json`). So `browser_visible` is still how a popover gets
// the window for a moment, and `seat.ts` still calls it.
//
// ## What changed, and why the change is the point
//
//  - `setBounds()` is honoured on every platform, so there is no `place.rs`
//    here: no `GtkOverlay`, no `GtkFixed`, no re-allocation on idle. Three
//    hundred lines of the old module were that one problem.
//  - **The last tab is destroyed like any other.** Under wry the profile lived
//    in a `WebContext` keyed by data directory and died with the last webview
//    referencing it, so closing the final tab had to blank it instead. An
//    Electron session belongs to its partition, not to a view, so the rule has
//    nothing left to protect. `browser_close` still answers a boolean, because
//    the *other* shell still needs to say `false` — this one always says true.
//
// ## The boundary
//
// **These views get no preload.** That is the whole of what keeps pointing one
// at an arbitrary URL reasonable, and it is the same sentence the Tauri version
// wrote as "granted no capabilities, ever" — except there it was a file that
// could be filled in wrong, and here it is an option that has to be added.
// `../src/browser/mod.rs` has the tests that pinned the old spelling; the new
// one is pinned in `main.js`'s own tests-by-inspection: nothing below may pass
// `preload`, `nodeIntegration` or `contextIsolation: false`.

const { WebContentsView, session } = require("electron");

/** Mirrors `browser::BROWSER_NAVIGATED`, which mirrors `ui/src/types.ts`. */
const BROWSER_NAVIGATED = "tcode://browser-navigated";

/** Where a tab starts. Deliberately not a search engine or a vendor page: the
 *  app has no business making a request nobody asked for. */
const HOME = "about:blank";

/**
 * One partition for every tab, persisted.
 *
 * The successor to the shared `data_directory` — cookies and logins belong to
 * the browser rather than to a tab, and survive closing tabs and restarting the
 * app. The spike confirmed all three halves of that on this Electron
 * (`sharedWithSiblingView`, `isolatedFromOtherPartition`, and cookies still
 * there on the next run).
 *
 * **Not the app's own session.** A page loaded here must not share storage,
 * cookies or service workers with the document that talks to the backend.
 */
const PARTITION = "persist:tcode-browser";

/**
 * The browser verbs, for the shell's command table.
 *
 * `emit` sends an event to the app renderer; `resolveUrl` asks the backend what
 * a typed string means — that judgement is `crate::address::to_url`, has five
 * tests, and is deliberately not reimplemented here.
 */
function browserVerbs({ window, emit, resolveUrl }) {
  /** @type {{ id: string, view: import("electron").WebContentsView }[]} */
  const tabs = [];
  /** The tab on screen. Empty before the first one exists. */
  let current = "";
  /** The pane's rectangle, as last reported. Remembered even with no tab to
   *  put it on: a rect arriving while a view is being built is the one that
   *  view will want, and the pane only reports a rect when it *changes*. */
  let rect = { x: 0, y: 0, width: 0, height: 0 };
  /** Whether the pane wants the browser on screen at all. */
  let shown = false;

  const find = (id) => {
    const tab = tabs.find((tab) => tab.id === id);
    // The id came from the frontend and is therefore data (rule 3): an unknown
    // one is an error, never a fallback to the current tab, which would reload
    // or close a page nobody pointed at.
    if (!tab) throw new Error("that browser tab is not open");
    return tab.view;
  };

  // Rects arrive as CSS pixels measured by the app renderer, which fills the
  // window's content area at (0,0) — so they are already the DIP the view
  // wants. Rounded because `setBounds` takes integers, and floored at one
  // pixel because a zero-sized view is a page that stops laying out.
  const place = (view) =>
    view.setBounds({
      x: Math.round(rect.x),
      y: Math.round(rect.y),
      width: Math.max(1, Math.round(rect.width)),
      height: Math.max(1, Math.round(rect.height)),
    });

  /** Only the current tab is on screen, and only when the pane wants one to be.
   *  Native views composite above the document *and above each other*, so "not
   *  the current tab" has to mean hidden — there is no z-order the document can
   *  impose on them. */
  const restack = () => {
    for (const tab of tabs) tab.view.setVisible(shown && tab.id === current);
  };

  function create(id) {
    const view = new WebContentsView({
      webPreferences: {
        // No `preload`, and that omission is the security boundary. Anything
        // exposed here would be exposed to every page the browser visits.
        session: session.fromPartition(PARTITION),
        nodeIntegration: false,
        contextIsolation: true,
        sandbox: true,
      },
    });

    const contents = view.webContents;
    const report = (title) =>
      emit(BROWSER_NAVIGATED, {
        id,
        url: contents.getURL(),
        title: title ?? "",
      });

    // Three events for two facts. A navigation and the title it later resolves
    // to are separate events for the same page rather than a correction — the
    // frontend's `navigatedTab` is written for exactly that, and an in-page
    // navigation (a SPA route) has to count or the address bar freezes on the
    // first URL a single-page app ever had.
    contents.on("did-navigate", () => report());
    contents.on("did-navigate-in-page", () => report());
    contents.on("page-title-updated", (_event, title) => report(title));

    // A page-initiated `window.open` lands in this tab instead of opening an
    // Electron window with no chrome and no address bar. Handing it back to the
    // frontend's open-tab path would be better and is not migration work: that
    // path starts with the frontend calling `browser_open` and recording the id
    // it gets back, so a tab this process invented would exist on screen and
    // not in the strip. Same-tab is the honest interim — it never silently does
    // nothing, and a page could have navigated itself there anyway.
    contents.setWindowOpenHandler(({ url }) => {
      contents.loadURL(url).catch(() => {});
      return { action: "deny" };
    });

    view.setVisible(false);
    window.contentView.addChildView(view);
    contents.loadURL(HOME).catch(() => {});
    return view;
  }

  return {
    /** Open a tab and make it the current one. Answers its id, which is the
     *  strip's identity for it — there is no second, frontend-side numbering. */
    browser_open(args) {
      rect = args.rect;
      const id = crypto.randomUUID();
      const view = create(id);
      tabs.push({ id, view });
      current = id;
      place(view);
      restack();
      return id;
    },

    /** The pane is back with tabs already open. Idempotent, and it must be:
     *  the pane calls it on every mount and hiding never destroyed anything. */
    browser_show(args) {
      rect = args.rect;
      shown = true;
      for (const tab of tabs) place(tab.view);
      restack();
      return null;
    },

    browser_select(args) {
      find(args.id);
      current = args.id;
      place(find(args.id));
      restack();
      return null;
    },

    /** Follow the pane. Runs for every frame of a divider drag, so it stays a
     *  bare setter. Only the current tab is placed; the others are placed when
     *  they are selected. */
    browser_bounds(args) {
      rect = args.rect;
      if (current) place(find(current));
      return null;
    },

    /** Give the window back to the document for a moment.
     *
     *  `setVisible(false)` rather than removing the view: the spike checked
     *  that both reveal the DOM and that neither destroys the page, and this is
     *  the one that does not have to remember where the view went. */
    browser_visible(args) {
      shown = args.visible;
      restack();
      return null;
    },

    async browser_navigate(args) {
      const view = find(args.id);
      // The backend decides what the string means. `localhost:5173` is http and
      // `github.com` is https, and those rules have tests attached to them in
      // exactly one place (`crate::address`).
      const url = await resolveUrl(args.url);
      await view.webContents.loadURL(url);
      return null;
    },

    /** Step this tab's own history.
     *
     *  Unlike the Tauri version this is the real navigation history rather than
     *  `history.go()` evaluated inside the page — but **the buttons still must
     *  not be greyed out**. Electron could answer `canGoBack()` here; the
     *  frontend deliberately does not ask, because a strip that greys a button
     *  is making a claim it has to keep in step with a page that navigates on
     *  its own. Stepping past the end does nothing, which is the harmless
     *  direction. */
    browser_step(args) {
      find(args.id).webContents.navigationHistory.goToOffset(args.delta);
      return null;
    },

    browser_reload(args) {
      find(args.id).webContents.reload();
      return null;
    },

    /** Close one tab, and answer whether the view is gone.
     *
     *  Always `true` here. The answer is kept because the Tauri shell still has
     *  to say `false` for its last tab, and the frontend draws whichever of the
     *  two happened rather than deciding for itself — see `webHost.ts::close`. */
    browser_close(args) {
      const at = tabs.findIndex((tab) => tab.id === args.id);
      if (at < 0) throw new Error("that browser tab is not open");
      const [tab] = tabs.splice(at, 1);
      window.contentView.removeChildView(tab.view);
      tab.view.webContents.close();
      if (current === args.id) {
        // Whoever the frontend selects next will `browser_select` it; until
        // then nothing is current, so nothing is shown.
        current = "";
      }
      restack();
      return true;
    },
  };
}

module.exports = { browserVerbs, PARTITION, BROWSER_NAVIGATED };
