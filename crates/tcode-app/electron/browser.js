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

/**
 * A tab exists. Mirrors `bridge::BROWSER_TAB_OPENED` and `ui/src/types.ts`.
 *
 * New with the agent browser, and it **inverts who gets to create a tab**. The
 * strip used to be the only birthplace — `browser_open`'s id was whatever the
 * frontend recorded — which is why a page's own `window.open` had to be denied
 * and loaded in the same tab instead (there was no way to tell the strip about
 * a tab this process invented). Now the backend opens tabs too, so tab creation
 * is event-sourced and the strip learns about a tab whoever made it.
 */
const BROWSER_TAB_OPENED = "tcode://browser-tab-opened";

/**
 * How long any one CDP command may take.
 *
 * The spike hung twice on `Page.captureScreenshot` against a hidden view — the
 * command simply never came back — and each time the symptom was a window with
 * nothing happening in it. The backend's own `CALL_TIMEOUT` would eventually
 * free the caller, but "the shell did not answer" is the wrong sentence for
 * "that one CDP command is stuck", so the bound is here as well, where the
 * error can name the command.
 */
const CDP_TIMEOUT = 15_000;

function withTimeout(promise, ms, what) {
  return Promise.race([
    promise,
    new Promise((_resolve, reject) =>
      setTimeout(() => reject(new Error(`${what} did not answer within ${ms}ms`)), ms),
    ),
  ]);
}

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
  // A page in a tab can ask for the camera, the microphone, geolocation or
  // notifications. Chromium's answer would be a prompt, and there is nobody
  // here to answer a prompt about a page that came from the open web — so the
  // answer is always no, set once for the partition every tab shares.
  session
    .fromPartition(PARTITION)
    .setPermissionRequestHandler((_webContents, _permission, callback) =>
      callback(false),
    );

  /** @type {{ id: string, view: import("electron").WebContentsView }[]} */
  const tabs = [];
  /** The tab on screen. Empty before the first one exists. */
  let current = "";
  /**
   * The pane's rectangle, as last reported.
   *
   * Remembered even with no tab to put it on: a rect arriving while a view is
   * being built is the one that view will want, and the pane only reports a
   * rect when it *changes*.
   *
   * **It starts at a usable size rather than at zero**, which used to be
   * harmless and is not any more. The pane was the only thing that ever opened
   * a tab, so a rect had always arrived before a view existed; a model can now
   * open one in a window whose browser pane has never been mounted, and
   * `place()` would floor that to 1×1. A 1×1 viewport is not a small page, it
   * is a page that has stopped laying out — every media query narrow, most
   * content collapsed — so the snapshot would describe something nobody could
   * ever see. These numbers are only a default: the first real rect replaces
   * them, and until then they are what an ordinary window would have offered.
   */
  let rect = { x: 0, y: 0, width: 1280, height: 800 };
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

  /**
   * One CDP command against a tab, attaching the debugger the first time.
   *
   * Attached lazily and left attached: a tab nobody is inspecting should not
   * pay for a debugger, and re-attaching per command would make every call two
   * round trips. `isAttached()` is asked each time rather than remembered
   * because the attachment can end without this process deciding to — Electron
   * documents DevTools taking it over, and although the spike could not
   * reproduce that on this version (the debugger stayed attached with DevTools
   * open, twice), a remembered flag would be wrong exactly when the
   * documentation turns out to be right. Asking is one synchronous call.
   */
  const cdp = (id, method, params) => {
    const contents = find(id).webContents;
    if (!contents.debugger.isAttached()) contents.debugger.attach("1.3");
    return withTimeout(
      contents.debugger.sendCommand(method, params),
      CDP_TIMEOUT,
      method,
    );
  };

  /**
   * Where an element is, in viewport CSS pixels — which is what
   * `Input.dispatchMouseEvent` takes.
   *
   * Deliberately `getBoundingClientRect` inside the page rather than
   * `DOM.getBoxModel`. Both answer, but only one of them is *specified*: the
   * DOM method is defined against the viewport, while the quads CDP returns
   * follow a convention of Chromium's that nothing here can check and that a
   * mistake in would show up as clicks landing a few hundred pixels off — a
   * symptom that reads as a broken page rather than as a wrong coordinate
   * space.
   *
   * `scroll` is a parameter and not a second helper because it is exactly the
   * difference between the two callers: clicking wants the element brought into
   * view first, and scrolling *at* an element must not move it before the wheel
   * arrives.
   */
  const CENTER = `function (scroll) {
    if (scroll) this.scrollIntoView({ block: "center", inline: "center" });
    const box = this.getBoundingClientRect();
    return {
      x: box.left + box.width / 2,
      y: box.top + box.height / 2,
      width: box.width,
      height: box.height,
    };
  }`;

  /** Run one function with a `ref` as its `this`, and let the object go again.
   *  A stale ref fails here, in Chromium's words, which is the whole reason the
   *  `ref` is Chromium's own node id (see `../AGENT-BROWSER.md`). */
  const onNode = async (id, ref, body, args = []) => {
    await cdp(id, "DOM.enable");
    const resolved = await cdp(id, "DOM.resolveNode", { backendNodeId: ref });
    const objectId = resolved.object && resolved.object.objectId;
    if (!objectId) throw new Error(`ref_${ref} is not on this page any more`);
    try {
      const answer = await cdp(id, "Runtime.callFunctionOn", {
        objectId,
        functionDeclaration: body,
        arguments: args.map((value) => ({ value })),
        returnByValue: true,
      });
      if (answer.exceptionDetails) {
        throw new Error(answer.exceptionDetails.text || "the page refused that");
      }
      return answer.result.value;
    } finally {
      await cdp(id, "Runtime.releaseObject", { objectId }).catch(() => {});
    }
  };

  /**
   * Refuse to act unless the tab is still on the host the caller was told about.
   *
   * The backend decides *what to ask the user*, and it can only ask about a
   * page it has seen — the last one it navigated to or snapshotted. Between
   * that and the click the page can move on its own: a redirect, a meta
   * refresh, a script. Without this the approval would say `github.com` and the
   * click would land wherever the tab had drifted to, which is the one failure
   * mode the whole per-host descriptor exists to prevent.
   *
   * It is a comparison and not a judgement — the shell holds no policy here; it
   * enforces a value the backend computed. Doing it here rather than in a
   * separate round trip is what makes it airtight: there is no window between
   * the check and the act for the page to move in.
   *
   * `hostname` and not `host`, to match Rust's `Url::host_str` — the port is
   * not part of this vocabulary, exactly as it is not for `web_fetch`.
   */
  const onHost = (id, expected) => {
    const at = find(id).webContents.getURL();
    let host = "";
    try {
      host = new URL(at).hostname;
    } catch {
      // A tab at `about:blank` has no host, which is not this host either.
    }
    if (host !== expected) {
      throw new Error(
        `that tab is on ${host || at || "no page"}, not ${expected} — it moved since ` +
          `you looked at it, so snapshot it again before acting`,
      );
    }
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
    /**
     * Open a tab. Answers its id, which is this window's only identity for it.
     *
     * Two callers now, and they want different things. The pane opens a tab
     * because somebody clicked `+`, so it passes its rectangle and
     * `select: true`: the new tab is the one on screen. The backend opens a tab
     * because a model asked for one, and it passes neither — **an agent must
     * never change which tab the user is looking at** (`../AGENT-BROWSER.md`),
     * and it has no rectangle to offer because the pane owns that.
     *
     * `select` is read strictly rather than defaulted to true. A caller that
     * forgot it gets a background tab, which is the harmless direction: the
     * page is there and one click brings it forward. The permissive default
     * fails the other way — a tab stealing the screen from whoever was reading.
     */
    browser_open(args) {
      // A rect only arrives from the pane. Absent means "wherever the pane last
      // said", which is the right answer for a tab nobody is going to look at
      // yet, and the right answer for the pane's own next tab too.
      if (args.rect) rect = args.rect;
      const id = crypto.randomUUID();
      const view = create(id);
      tabs.push({ id, view });
      if (args.select === true) current = id;
      place(view);
      restack();
      // Announced, not returned-only: the caller gets the id back, but the
      // strip has to hear about a tab it did not open. Both paths add by id and
      // ignore a duplicate, so the order these arrive in does not matter.
      //
      // `agent` rides along because it is the one fact only this moment knows.
      // Which conversation owns the tab is learnt later, off the tool calls that
      // name it; *that it is not the user's* has to be true from the first
      // frame, or a page starts loading in a tab the strip is drawing as though
      // somebody had opened it themselves.
      emit(BROWSER_TAB_OPENED, { id, url: HOME, agent: args.agent === true });
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

    /**
     * What is on a page, as the accessibility tree sees it.
     *
     * **The raw nodes, unfiltered.** A real page hands back two thousand of
     * them and 700KB of JSON, and almost none of it is anything a model can
     * act on — but deciding *which* almost-none is a judgement that will be
     * tuned over and over, needs tests, and belongs with the tool
     * (`../src/browser.rs`). Filtering here would put that judgement in the
     * shell, which is the rule this file opens with, and would leave it
     * untested besides. The cost is one big frame down a local pipe per
     * snapshot; if that ever shows up in a measurement, it is a measurable
     * problem with an obvious fix, which is a better position than a policy
     * split across two languages.
     *
     * `Accessibility.enable` every time is deliberate and cheap: the domain is
     * disabled again by a navigation, and the alternative is remembering per
     * tab whether a page has navigated since — state that is wrong the first
     * time somebody clicks a link.
     */
    async browser_snapshot(args) {
      const contents = find(args.id).webContents;
      await cdp(args.id, "Accessibility.enable");
      const tree = await cdp(args.id, "Accessibility.getFullAXTree");
      return {
        url: contents.getURL(),
        title: contents.getTitle(),
        nodes: tree.nodes ?? [],
      };
    },

    /**
     * Click an element, with a real mouse event at its centre.
     *
     * Not `element.click()`. A synthetic DOM click skips everything between the
     * pointer and the handler — hover states, focus, `pointerdown` menus, the
     * overlay that was covering the button — and a page built on any of those
     * would appear to work while doing nothing. The point of driving the
     * window's own browser is that what happens is what would have happened.
     */
    async browser_click(args) {
      onHost(args.id, args.host);
      const at = await onNode(args.id, args.ref, CENTER, [true]);
      if (!at.width || !at.height) {
        throw new Error(`ref_${args.ref} has no size on screen — it is hidden or collapsed`);
      }
      const where = { x: at.x, y: at.y, button: "left", clickCount: 1 };
      await cdp(args.id, "Input.dispatchMouseEvent", { type: "mouseMoved", ...where });
      await cdp(args.id, "Input.dispatchMouseEvent", { type: "mousePressed", ...where });
      await cdp(args.id, "Input.dispatchMouseEvent", { type: "mouseReleased", ...where });
      return null;
    },

    /**
     * Put text in a field, replacing whatever was there.
     *
     * Replacing and not appending: "type Rust into the search box" means make it
     * say Rust, and a field that already held a previous query would otherwise
     * silently produce `oldRust`. The selection is made with the element's own
     * `select()` so the page sees an ordinary edit.
     *
     * `Input.insertText` rather than a key event per character — it is one round
     * trip instead of N, it does not mangle anything outside ASCII, and it is
     * what Chromium itself uses for a paste. `submit` is a real Enter, because
     * that is a different event from typing and plenty of forms only listen for
     * the one.
     */
    async browser_type(args) {
      onHost(args.id, args.host);
      await onNode(
        args.id,
        args.ref,
        `function () {
          this.focus();
          if (typeof this.select === "function") this.select();
        }`,
      );
      await cdp(args.id, "Input.insertText", { text: args.text });
      if (args.submit === true) {
        const key = {
          key: "Enter",
          code: "Enter",
          windowsVirtualKeyCode: 13,
          nativeVirtualKeyCode: 13,
        };
        await cdp(args.id, "Input.dispatchKeyEvent", { type: "keyDown", text: "\r", ...key });
        await cdp(args.id, "Input.dispatchKeyEvent", { type: "keyUp", ...key });
      }
      return null;
    },

    /** Turn the wheel. At an element when one is named, so a list with its own
     *  scrollbar can be reached; at the top-left corner otherwise, which is the
     *  page itself. */
    async browser_scroll(args) {
      const step = args.amount || 600;
      const by = {
        down: { deltaX: 0, deltaY: step },
        up: { deltaX: 0, deltaY: -step },
        right: { deltaX: step, deltaY: 0 },
        left: { deltaX: -step, deltaY: 0 },
      }[args.direction];
      if (!by) throw new Error(`'${args.direction}' is not a direction`);
      let at = { x: 8, y: 8 };
      if (args.ref) at = await onNode(args.id, args.ref, CENTER, [false]);
      await cdp(args.id, "Input.dispatchMouseEvent", {
        type: "mouseWheel",
        x: at.x,
        y: at.y,
        ...by,
      });
      return null;
    },

    /**
     * Wait for the page to settle, or for a phrase to appear on it.
     *
     * Polled, and that is the honest shape rather than a shortcut: "loaded" is
     * not a moment a single-page application has, so the question a model
     * actually has — *is the thing I am waiting for there yet* — is a question
     * about the page's current state, asked repeatedly. `idle` is Electron's
     * own `isLoading`, twice in a row, because a navigation that has started but
     * not registered yet reads as idle for one tick.
     *
     * The needle is interpolated through `JSON.stringify`, which is what makes
     * it a string literal in the page's eyes rather than an expression: a model
     * repeating a phrase off a hostile page must not be able to make that page
     * run it.
     */
    async browser_wait(args) {
      const contents = find(args.id).webContents;
      const limit = Math.min(Math.max(args.timeoutMs || 10000, 500), 30000);
      const until = Date.now() + limit;
      let quiet = 0;
      for (;;) {
        if (args.text) {
          const seen = await cdp(args.id, "Runtime.evaluate", {
            expression: `!!document.body && document.body.innerText.includes(${JSON.stringify(
              args.text,
            )})`,
            returnByValue: true,
          });
          if (seen.result && seen.result.value === true) return { settled: true };
        } else {
          quiet = contents.isLoading() ? 0 : quiet + 1;
          if (quiet >= 2) return { settled: true };
        }
        if (Date.now() >= until) return { settled: false };
        await new Promise((resolve) => setTimeout(resolve, 250));
      }
    },

    /**
     * A picture of the tab, whether or not anyone can see it.
     *
     * **`capturePage` and not `Page.captureScreenshot`**, and that choice is the
     * whole reason this works on a hidden tab. The CDP command wants a
     * compositor frame and a hidden `WebContentsView` is not producing any, so
     * it answers roughly every other call and hangs in between — measured three
     * times over, three shots each (`../AGENT-BROWSER.md`). Electron's own
     * capture takes a different path and came back 9 times out of 9 on a view
     * that was `setVisible(false)`, at the right size, with the right bytes.
     *
     * So the design that was written for this — briefly make the tab visible
     * underneath the current one, shoot, hide it again — is not needed and is
     * not here. Nothing on screen changes because nothing on screen is touched.
     *
     * Narrowed if the pane is wide, because the cost of an image is its
     * dimensions and a 2560-pixel screenshot buys a model nothing a 1400-pixel
     * one does not.
     */
    async browser_screenshot(args) {
      const contents = find(args.id).webContents;
      let image = await withTimeout(contents.capturePage(), CDP_TIMEOUT, "capturePage");
      if (image.isEmpty()) {
        throw new Error(
          "that tab had nothing to draw — it is probably still loading, or blank",
        );
      }
      const size = image.getSize();
      if (size.width > 1400) {
        image = image.resize({ width: 1400 });
      }
      return {
        url: contents.getURL(),
        data: image.toPNG().toString("base64"),
        width: image.getSize().width,
        height: image.getSize().height,
      };
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
      // Detached before the contents go, because `close()` on a webContents
      // with an attached debugger is the kind of ordering that works until it
      // does not. Wrapped because a tab that was never inspected has no
      // debugger to detach and that is the common case, not an error.
      try {
        if (tab.view.webContents.debugger.isAttached()) {
          tab.view.webContents.debugger.detach();
        }
      } catch {
        // Already gone. Nothing to undo and nobody to tell.
      }
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

module.exports = { browserVerbs, PARTITION, BROWSER_NAVIGATED, BROWSER_TAB_OPENED };
