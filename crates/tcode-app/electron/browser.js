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

const {
  WebContentsView: NativeWebContentsView,
  session: nativeSession,
} = require("electron");

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
/** A transient low-resolution page preview for the app renderer. Never sent to
 *  the sidecar, ledger or model. Mirrors `ui/src/types.ts`. */
const BROWSER_THUMBNAIL = "tcode://browser-thumbnail";

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
/** A background tab only needs a few frames, not a second call-sized timeout. */
const RENDER_TIMEOUT = 3_000;

const FORCE_LAYOUT = `document.documentElement &&
  document.documentElement.getBoundingClientRect(); undefined`;

function withTimeout(promise, ms, what) {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      const error = new Error(`${what} did not answer within ${ms}ms`);
      error.name = "TimeoutError";
      reject(error);
    }, ms);
    promise.then(
      (value) => {
        clearTimeout(timer);
        resolve(value);
      },
      (error) => {
        clearTimeout(timer);
        reject(error);
      },
    );
  });
}

/** Let a native child-view stack change reach Electron before probing its frame.
 *  A DOM layout read alone cannot make that promise: the target and the app
 *  cover are sibling `WebContentsView`s, composed outside either renderer.
 *  One macrotask is too early on some Viz paths, so leave one short frame
 *  budget for the native compositor before asking it to capture. */
const compositorTurn = () => new Promise((resolve) => setTimeout(resolve, 50));

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
function browserVerbs(
  { window, appView, emit, resolveUrl },
  {
    WebContentsView = NativeWebContentsView,
    session = nativeSession,
    renderTimeout = RENDER_TIMEOUT,
    thumbnailDelay = 120,
  } = {},
) {
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

  const STYLE_PROPERTIES = new Set([
    "display",
    "visibility",
    "position",
    "width",
    "height",
    "min-width",
    "min-height",
    "max-width",
    "max-height",
    "overflow",
    "overflow-x",
    "overflow-y",
    "color",
    "background-color",
    "border-color",
    "border-width",
    "border-style",
    "opacity",
    "z-index",
    "font-family",
    "font-size",
    "font-weight",
    "line-height",
    "padding",
    "margin",
    "gap",
    "flex-direction",
    "align-items",
    "justify-content",
    "transform",
  ]);
  const MAX_STYLE_PROPERTIES = 12;
  const SCREENSHOT_VIEWPORT = Object.freeze({
    minWidth: 320,
    maxWidth: 2560,
    minHeight: 240,
    maxHeight: 1440,
  });
  const screenshotViewport = (input) => {
    if (input == null) return null;
    const { width, height } = input;
    if (!Number.isInteger(width) || !Number.isInteger(height) ||
        width < SCREENSHOT_VIEWPORT.minWidth || width > SCREENSHOT_VIEWPORT.maxWidth ||
        height < SCREENSHOT_VIEWPORT.minHeight || height > SCREENSHOT_VIEWPORT.maxHeight) {
      throw new Error(
        `screenshot viewport must be ${SCREENSHOT_VIEWPORT.minWidth}-${SCREENSHOT_VIEWPORT.maxWidth}px ` +
          `wide and ${SCREENSHOT_VIEWPORT.minHeight}-${SCREENSHOT_VIEWPORT.maxHeight}px high`,
      );
    }
    return { width, height };
  };
  const COMPUTED_STYLE = `function (properties) {
    const view = this.ownerDocument && this.ownerDocument.defaultView;
    if (!view) throw new Error("this element has no document");
    const style = view.getComputedStyle(this);
    return Object.fromEntries(
      properties.map((property) => [property, style.getPropertyValue(property)]),
    );
  }`;

  /** Validate page-style inputs again at the process boundary.
   *  The Rust tool performs the same check for a precise model-facing error,
   *  but shell verbs are independently callable and must not turn arbitrary
   *  strings into a page execution surface. */
  const styleProperties = (input) => {
    if (!Array.isArray(input) || input.length < 1 || input.length > MAX_STYLE_PROPERTIES) {
      throw new Error(`computed style needs 1-${MAX_STYLE_PROPERTIES} properties`);
    }
    const properties = input.map((raw) => {
      if (typeof raw !== "string") throw new Error("computed style properties must be strings");
      const property = raw.trim().toLowerCase();
      if (!STYLE_PROPERTIES.has(property)) {
        throw new Error(`'${raw}' is not a supported computed-style property`);
      }
      return property;
    });
    return [...new Set(properties)];
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
  const place = (view, viewport = null) =>
    view.setBounds({
      x: Math.round(rect.x),
      y: Math.round(rect.y),
      width: Math.max(1, Math.round(viewport?.width ?? rect.width)),
      height: Math.max(1, Math.round(viewport?.height ?? rect.height)),
    });

  /** Only the current tab is on screen, and only when the pane wants one to be.
   *  Native views composite above the document *and above each other*, so "not
   *  the current tab" has to mean hidden — there is no z-order the document can
   *  impose on them. */
  const restack = () => {
    for (const tab of tabs) tab.view.setVisible(shown && tab.id === current);
  };

  /** Apply visibility first and geometry last. A current browser tab belongs
   *  above the app renderer; background render recovery temporarily puts a tab
   *  below it, so selecting that tab must also restore its native z-order. */
  const syncCurrent = () => {
    restack();
    if (shown && current) {
      const view = find(current);
      window.contentView.addChildView(view);
      place(view);
    }
  };

  const onUrl = (id, expected) => {
    const at = find(id).webContents.getURL();
    if (at !== expected) {
      throw new Error(
        `that tab is at ${at || "no page"}, not the ${expected} page you snapshotted — ` +
          "snapshot it again before querying its computed style",
      );
    }
  };

  /**
   * Give a background tab a compositor frame without showing it over the app.
   *
   * A WebContentsView hidden from birth has a live DOM and accessibility target,
   * but Electron 43 gives it no capture surface. `capturePage` options and
   * background throttling do not create that first frame. A visible child does,
   * so the target is placed at real bounds and made visible below the app
   * renderer. A selected browser sibling is briefly detached first because
   * Electron/Viz rejects capture while that sibling remains in the native tree;
   * detaching does not destroy or navigate it. The app still hides every target
   * pixel and the selection itself never changes.
   * Afterwards the tab returns to its ordinary hidden state, and neither
   * `current` nor `shown` changes.
   *
   * Calls are serialized because two simultaneous warm-ups would otherwise
   * reorder and hide each other's views. The queue survives a failed call.
   */
  let renderQueue = Promise.resolve();
  const rendered = (id, work, { before = true, after = false, viewport = null } = {}) => {
    const run = async () => {
      const view = find(id);
      const contents = view.webContents;
      const background = !shown || current !== id;
      const covered = background || viewport !== null;
      const state = () => {
        const bounds = typeof view.getBounds === "function" ? view.getBounds() : rect;
        return `url=${contents.getURL() || "no page"}, bounds=${bounds.width}x${bounds.height}` +
          `@${bounds.x},${bounds.y}, current=${current || "none"}, paneVisible=${shown}`;
      };
      const paint = async (when) => {
        try {
          await compositorTurn();
          await withTimeout(
            contents.executeJavaScript(FORCE_LAYOUT),
            renderTimeout,
            `browser ${when} layout`,
          );
          const until = Date.now() + renderTimeout;
          let last;
          do {
            try {
              const remaining = Math.max(1, until - Date.now());
              const probe = await withTimeout(
                contents.capturePage(),
                remaining,
                `browser ${when} frame`,
              );
              if (!probe.isEmpty()) return;
              last = new Error(`browser ${when} frame was empty`);
            } catch (error) {
              // Electron cannot cancel an in-flight capture. Retrying after it
              // timed out would queue more work behind the same stuck Viz path.
              if (error?.name === "TimeoutError") throw error;
              last = error;
            }
            if (Date.now() < until) {
              await new Promise((resolve) => setTimeout(resolve, 50));
            }
          } while (Date.now() < until);
          throw last;
        } catch (error) {
          throw new Error(`could not render that browser tab (${state()}): ${error.message}`);
        }
      };

      place(view, viewport);
      if (!covered) return work(view);

      if (!appView) throw new Error("the app renderer is unavailable for background capture");
      const cover = shown && current ? find(current) : null;
      if (cover) window.contentView.removeChildView(cover);
      // Reattaching the app view is deliberate. On an otherwise empty browser
      // tree, adding an already-attached sibling is not a reliable compositor
      // reorder on every Electron/Viz path; detach it so this is unambiguously
      // target below cover before the first frame is requested.
      window.contentView.removeChildView(appView);
      window.contentView.addChildView(view, 0);
      window.contentView.addChildView(appView);
      view.setVisible(true);

      try {
        if (before) await paint("initial");
        const answer = await work(view);
        if (after) await paint("result");
        return answer;
      } finally {
        if (viewport !== null) place(view);
        syncCurrent();
      }
    };

    const call = renderQueue.then(run, run);
    renderQueue = call.then(() => undefined, () => undefined);
    return call;
  };

  /**
   * Ask for a fresh transient page preview without making the Browser action
   * wait for it. Requests coalesce per tab; a revision that finishes after a
   * newer request is discarded rather than repainting the transcript with an
   * older page. Failures are intentionally silent here: the action already has
   * its own authoritative result, while a thumbnail is optional renderer state.
   */
  const thumbnailState = new Map();
  const requestThumbnail = (id) => {
    let state = thumbnailState.get(id);
    if (!state) {
      state = { requested: 0, timer: null, running: false };
      thumbnailState.set(id, state);
    }
    state.requested += 1;
    if (state.timer) clearTimeout(state.timer);
    state.timer = setTimeout(() => {
      state.timer = null;
      void publishThumbnail(id, state);
    }, thumbnailDelay);
  };

  const publishThumbnail = async (id, state) => {
    if (state.running) return;
    state.running = true;
    try {
      for (;;) {
        if (thumbnailState.get(id) !== state) break;
        const revision = state.requested;
        let preview;
        try {
          preview = await rendered(id, async (view) => {
            let image = await withTimeout(
              view.webContents.capturePage(),
              renderTimeout,
              "browser thumbnail",
            );
            if (image.isEmpty()) throw new Error("browser thumbnail was empty");
            if (image.getSize().width > 720) image = image.resize({ width: 720 });
            return {
              id,
              url: view.webContents.getURL(),
              data: image.toPNG().toString("base64"),
              width: image.getSize().width,
              height: image.getSize().height,
              revision,
            };
          });
        } catch {
          if (state.requested === revision) break;
          continue;
        }
        if (thumbnailState.get(id) !== state || state.requested !== revision) continue;
        emit(BROWSER_THUMBNAIL, preview);
        break;
      }
    } finally {
      state.running = false;
    }
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
    const report = (title) => {
      emit(BROWSER_NAVIGATED, {
        id,
        url: contents.getURL(),
        title: title ?? "",
      });
      requestThumbnail(id);
    };

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
    // Bounds must exist before the first navigation. A hidden-from-birth view
    // loaded at zero size can finish with a live DOM but no layout/capture
    // surface, and no later capture option repairs that lifecycle.
    place(view);
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
      if (shown && current === id) syncCurrent();
      else place(view);
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
      syncCurrent();
      return null;
    },

    browser_select(args) {
      find(args.id);
      current = args.id;
      syncCurrent();
      requestThumbnail(args.id);
      return null;
    },

    /** Follow the pane. During a divider drag the page is hidden, so only
     *  remember the newest rectangle; moving a hidden WebContentsView would
     *  still make Chromium re-layout the whole page in another process. The
     *  final rectangle is applied once when `browser_visible(true)` restores
     *  the pane. */
    browser_bounds(args) {
      rect = args.rect;
      if (shown && current) place(find(current));
      return null;
    },

    /** Give the window back to the document for a moment.
     *
     *  `setVisible(false)` rather than removing the view: the spike checked
     *  that both reveal the DOM and that neither destroys the page, and this is
     *  the one that does not have to remember where the view went.
     *
     *  Showing is ordered visibility first, placement last. Electron may
     *  allocate a native child at its remembered bounds when it becomes
     *  visible, so the pane's latest rectangle has to be the final operation. */
    browser_visible(args) {
      shown = args.visible;
      syncCurrent();
      return null;
    },

    async browser_navigate(args) {
      // The backend decides what the string means. `localhost:5173` is http and
      // `github.com` is https, and those rules have tests attached to them in
      // exactly one place (`crate::address`). Keep the target compositing under
      // the app until its new document has painted; loading a background view
      // while it stays hidden is the lifecycle that produced healthy-but-blank
      // loopback pages.
      const url = await resolveUrl(args.url);
      await rendered(
        args.id,
        (view) => view.webContents.loadURL(url),
        { before: false, after: true },
      );
      requestThumbnail(args.id);
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
      requestThumbnail(args.id);
      return null;
    },

    browser_reload(args) {
      find(args.id).webContents.reload();
      requestThumbnail(args.id);
      return null;
    },

    browser_zoom(args) {
      const contents = find(args.id).webContents;
      const current = contents.getZoomFactor();
      if (args.reset) {
        contents.setZoomFactor(1);
      } else {
        const next = Math.min(3, Math.max(0.25, current + (args.delta ?? 0)));
        contents.setZoomFactor(next);
      }
      return { zoom: contents.getZoomFactor() };
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
      return rendered(args.id, async (view) => {
        const contents = view.webContents;
        await cdp(args.id, "Accessibility.enable");
        const tree = await cdp(args.id, "Accessibility.getFullAXTree");
        const nodes = tree.nodes ?? [];
        const url = contents.getURL();
        if (nodes.length === 0 && url !== HOME) {
          const bounds = typeof view.getBounds === "function" ? view.getBounds() : rect;
          throw new Error(
            `the accessibility tree stayed empty after render recovery for ${url}; ` +
              `bounds=${bounds.width}x${bounds.height}@${bounds.x},${bounds.y}, ` +
              `current=${current || "none"}, paneVisible=${shown}`,
          );
        }
        return {
          url,
          title: contents.getTitle(),
          nodes,
        };
      });
    },

    async browser_computed_style(args) {
      const properties = styleProperties(args.properties);
      return rendered(args.id, async (view) => {
        onUrl(args.id, args.url);
        return {
          url: view.webContents.getURL(),
          styles: await onNode(args.id, args.ref, COMPUTED_STYLE, [properties]),
        };
      });
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
      await rendered(args.id, async () => {
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
      }, { after: true });
      requestThumbnail(args.id);
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
      await rendered(args.id, async () => {
        onHost(args.id, args.host);
        await onNode(
          args.id,
          args.ref,
          `function () {
            const typeable = this.localName === "input" ||
              this.localName === "textarea" || this.isContentEditable;
            if (!typeable) {
              throw new Error("this element does not accept text");
            }
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
      }, { after: true });
      requestThumbnail(args.id);
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
      await rendered(args.id, async () => {
        let at = { x: 8, y: 8 };
        if (args.ref) at = await onNode(args.id, args.ref, CENTER, [false]);
        await cdp(args.id, "Input.dispatchMouseEvent", {
          type: "mouseWheel",
          x: at.x,
          y: at.y,
          ...by,
        });
        return null;
      }, { after: true });
      requestThumbnail(args.id);
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
      return rendered(args.id, async (view) => {
        const contents = view.webContents;
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
      });
    },

    /**
     * A picture of the tab, whether or not anyone can see it.
     *
     * `capturePage`, not CDP `Page.captureScreenshot`: the latter still hangs
     * intermittently when a tab is hidden. Electron's capture is reliable once
     * the current document has produced a compositor frame. A view that was
     * hidden before its first load is the missing case the original 9/9 probe
     * did not measure, so `rendered` creates that frame under the app renderer
     * before this call.
     *
     * Captures wider than 1400 pixels are narrowed before encoding because the
     * image cost grows with its dimensions without buying the model useful
     * detail at that width. A requested viewport controls the pixels rendered
     * before that output cap is applied.
     */
    async browser_screenshot(args) {
      const viewport = screenshotViewport(args.viewport);
      return rendered(args.id, async (view) => {
        const contents = view.webContents;
        let image = await withTimeout(contents.capturePage(), CDP_TIMEOUT, "capturePage");
        if (image.isEmpty()) {
          const bounds = typeof view.getBounds === "function" ? view.getBounds() : rect;
          throw new Error(
            `capturePage returned an empty image after render recovery for ` +
              `${contents.getURL() || "no page"}; bounds=${bounds.width}x${bounds.height}` +
              `@${bounds.x},${bounds.y}, current=${current || "none"}, paneVisible=${shown}`,
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
      }, { viewport });
    },

    /** Close one tab, and answer whether the view is gone.
     *
     *  Always `true` here. The answer is kept because the Tauri shell still has
     *  to say `false` for its last tab, and the frontend draws whichever of the
     *  two happened rather than deciding for itself — see `webHost.ts::close`. */
    browser_close(args) {
      const at = tabs.findIndex((tab) => tab.id === args.id);
      if (at < 0) throw new Error("that browser tab is not open");
      const pendingThumbnail = thumbnailState.get(args.id);
      if (pendingThumbnail?.timer) clearTimeout(pendingThumbnail.timer);
      thumbnailState.delete(args.id);
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
      syncCurrent();
      return true;
    },
  };
}

module.exports = {
  browserVerbs,
  PARTITION,
  BROWSER_NAVIGATED,
  BROWSER_TAB_OPENED,
  BROWSER_THUMBNAIL,
};
