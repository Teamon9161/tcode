/**
 * Phase 0 of the Tauri→Electron migration (now in `../AGENTS.md` rule 9h),
 * plus a second round for `../AGENT-BROWSER.md` (see `agentBrowserProbes`).
 * It answers its questions, writes `results.json`, and is kept only as long as
 * something is still reading those answers. The Linux/Wayland half is still
 * pending on every question here — the development machine is Windows, so
 * `results.json` carries a `platform` and none of it is a cross-platform claim.
 *
 * The migration questions, and why each one can change the plan:
 *
 *   1. Does a `WebContentsView` composite above the renderer's DOM?
 *      → decides whether `seat.ts::yieldToPopover` (125 lines) and the
 *        `browserYield` dance survive the migration, or are deleted.
 *   2. Does `setBounds` reach the *page*, not just the widget?
 *      → the whole reason for `place.rs`. On Linux today the answer is no
 *        without a `GtkFixed` and a follow-up `size_allocate`.
 *   3. Is CDP available on a `WebContentsView`'s `webContents`?
 *      → the reason for migrating at all.
 *   4. Does `session.fromPartition("persist:…")` share and persist cookies?
 *      → the shape browser profiles would be built on.
 *
 * Nothing here is measured by looking at it. Question 1 is answered by
 * capturing the screen and reading a pixel; question 2 by asking the page for
 * its own `innerWidth`, never the host for the widget's size — `place.rs`
 * records a failure where those two disagreed and only the page was right.
 */

const {
  app,
  BaseWindow,
  WebContentsView,
  session,
  screen,
  desktopCapturer,
} = require("electron");
const fs = require("node:fs");
const http = require("node:http");
const path = require("node:path");

/** Colours the two documents paint, as the capture will see them. */
const DOM_RED = [220, 30, 30];
const PAGE_BLUE = [20, 60, 220];

/** Window and view geometry. The view sits wholly inside the DOM probe. */
const WINDOW = { x: 60, y: 60, width: 900, height: 600 };
const VIEW = { x: 300, y: 150, width: 400, height: 300 };
/** Window-relative point to sample: the centre of the overlap. */
const SAMPLE = { x: 500, y: 300 };

const PARTITION = "persist:tcode-spike";

/**
 * The switches headed Puppeteer/Playwright launch Chromium with, appended only
 * when asked for (`npm run spike:unthrottled`).
 *
 * They have to be set before `app.whenReady`, which is why this is module-level
 * rather than a parameter: a process-wide compositing decision cannot be
 * toggled halfway through a run and still be believed. See `escapeRoutes`.
 *
 * **A CLI flag and not an environment variable**, which is a rule the app
 * learnt the hard way (`../AGENTS.md`): `FOO=1 electron .` is POSIX shell
 * syntax for one command's environment and a *parse error* in both cmd and
 * PowerShell, so an env-gated script is one that cannot be run on the machine
 * this is being developed on.
 */
const SWITCHES = process.argv.includes("--unthrottled");
if (SWITCHES) {
  for (const flag of [
    "disable-backgrounding-occluded-windows",
    "disable-renderer-backgrounding",
    "disable-background-timer-throttling",
  ]) {
    app.commandLine.appendSwitch(flag);
  }
}

const results = { platform: process.platform, versions: {}, probes: {} };

const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

/**
 * A promise that cannot hang the run.
 *
 * `loadURL` settles on `did-finish-load` or `did-fail-load`, and a machine with
 * no route to the host produces neither for as long as the connect attempt
 * takes — which is how the first run of the round-two probes hung with the
 * window still on screen. A spike that never writes `results.json` is worse
 * than one that writes "timed out": the second is an answer.
 */
function withTimeout(promise, ms, what) {
  return Promise.race([
    promise,
    new Promise((_resolve, reject) =>
      setTimeout(() => reject(new Error(`${what} did not finish within ${ms}ms`)), ms),
    ),
  ]);
}

/** Where the run got to, for when it does not get to the end. */
const step = (name) => console.error(`spike: ${name}`);

/** Cookies need a real origin; `file:` and `data:` have none. */
function serve() {
  return new Promise((resolve) => {
    const server = http.createServer((request, response) => {
      const file = request.url === "/" ? "page.html" : request.url.slice(1);
      fs.readFile(path.join(__dirname, path.basename(file)), (error, body) => {
        if (error) {
          response.writeHead(404).end("no");
          return;
        }
        response.writeHead(200, { "content-type": "text/html" }).end(body);
      });
    });
    server.listen(0, "127.0.0.1", () => resolve(server));
  });
}

/** Evaluate in a page and get the value back, without any channel to it. */
async function evaluate(view, expression) {
  return view.webContents.executeJavaScript(expression, true);
}

/**
 * One pixel of the composited screen, in window-relative logical coordinates.
 *
 * The *screen* rather than the window: a window capture on Windows can go
 * through `PrintWindow`, which is exactly the path that can miss separate child
 * HWNDs — and native child views are the thing being measured, so a capture
 * that might omit them would answer the question wrongly and confidently.
 */
async function samplePixel(point) {
  const display = screen.getPrimaryDisplay();
  const scale = display.scaleFactor;
  const sources = await desktopCapturer.getSources({
    types: ["screen"],
    thumbnailSize: {
      width: Math.round(display.size.width * scale),
      height: Math.round(display.size.height * scale),
    },
  });
  const shot = sources[0]?.thumbnail;
  if (!shot || shot.isEmpty()) return { error: "screen capture came back empty" };

  const size = shot.getSize();
  const bitmap = shot.getBitmap();
  const stride = bitmap.length / size.height;
  // Physical pixel of the sample point, allowing for the display's origin.
  const px = Math.round((WINDOW.x + point.x - display.bounds.x) * scale);
  const py = Math.round((WINDOW.y + point.y - display.bounds.y) * scale);
  if (px < 0 || py < 0 || px >= size.width || py >= size.height) {
    return { error: `sample point ${px},${py} is outside the capture`, size };
  }
  const at = py * stride + px * 4;
  // `getBitmap` is BGRA.
  return { rgb: [bitmap[at + 2], bitmap[at + 1], bitmap[at]], capture: size, scale };
}

const near = (rgb, target) =>
  Array.isArray(rgb) && rgb.every((v, i) => Math.abs(v - target[i]) <= 24);

function whoIsOnTop(sample) {
  if (sample.error) return `unknown (${sample.error})`;
  if (near(sample.rgb, PAGE_BLUE)) return "view";
  if (near(sample.rgb, DOM_RED)) return "dom";
  return `neither (${sample.rgb})`;
}

/**
 * Round two: the three questions `../AGENT-BROWSER.md` needs answered before
 * an agent can drive one of these tabs.
 *
 *   1. Does CDP still work on a view the pane has hidden (`setVisible(false)`)?
 *      → decides whether an agent's commands can leave the screen alone. If
 *        the answer is no, the agent's tab has to be the one on screen while it
 *        works, which interrupts whatever the user was reading.
 *   2. How big is a real page's accessibility tree?
 *      → the snapshot is meant to be the *default* way the model looks at a
 *        page. A tree that serializes to 300KB is not a default, it is a
 *        budget problem, and the design would have to lead with something else.
 *   3. Does a hidden tab still report navigation?
 *      → `wait(idle)` is built on those events; Chromium throttles background
 *        work and this is where that would show.
 *
 * Question 1 is measured, not reasoned about: the click is read back through
 * `document.title`, exactly as the visible case is, and the screenshot is
 * counted in bytes. "The API returned without throwing" is not an answer —
 * `captureScreenshot` on a view with no compositor could plausibly hand back
 * a valid, empty PNG.
 */
/** Roles worth a `ref` even with no accessible name — the things a model
 *  clicks or types into. Deliberately short; this is a measurement, and the
 *  real list belongs with the tool. */
const INTERACTIVE = new Set([
  "button",
  "link",
  "textbox",
  "searchbox",
  "combobox",
  "checkbox",
  "radio",
  "menuitem",
  "tab",
  "switch",
  "slider",
  "option",
]);

/**
 * Round three: can a screenshot avoid needing the tab on screen?
 *
 * Round two established that `Page.captureScreenshot` against a
 * `setVisible(false)` view answers roughly every other call and hangs in
 * between, while the same call on a visible view is reliable. "So the tab has
 * to be on screen" was an *inference* from that, not a measurement, and it is a
 * big enough limitation to be worth disproving properly — Playwright screenshots
 * pages nobody is looking at all day.
 *
 * Why it might manage that, and why each reason may or may not reach us:
 *
 *   - Headless has no hidden state at all. Every page has its own compositor
 *     producing frames with no window system to occlude it. Not available to
 *     us: the entire point of this feature is that the tab is a real one in the
 *     user's window.
 *   - Headed Puppeteer/Playwright pass `--disable-backgrounding-occluded-windows`
 *     and `--disable-renderer-backgrounding`. Those govern Chromium's "this is
 *     not visible, stop spending on it" logic, and Electron takes the same
 *     switches. **Probe B.**
 *   - Their pages are top-level targets. Ours is a child view, and
 *     `setVisible(false)` removes it from the layer tree rather than merely
 *     covering it — which is stronger than occlusion and may be past what any
 *     switch reaches. If so, the way out is to never hide it: keep it visible
 *     and put it where nobody looks. **Probe A.**
 *
 * Three shots each, because round two is the reason: one shot cannot tell a
 * working configuration from a coin flip.
 */
async function escapeRoutes(view, probe, cdp) {
  const shoot = async (times = 3) => {
    const out = [];
    for (let i = 0; i < times; i += 1) {
      try {
        const shot = await cdp("Page.captureScreenshot", { format: "png" });
        out.push(shot.data ? Buffer.from(shot.data, "base64").length : 0);
      } catch (error) {
        out.push(String(error).includes("did not finish") ? "timeout" : String(error));
      }
    }
    return out;
  };

  // ── A: visible, but parked outside the window's content area ──────────────
  //
  // The view stays in the layer tree, so the compositor has a reason to keep
  // drawing it; the user sees nothing because it is not over the window. If
  // this works it is the answer, and it costs nothing — the pane already tells
  // the shell where to put each view.
  step("escape route A: visible but offscreen");
  view.setVisible(true);
  view.setBounds({ x: WINDOW.width + 200, y: 0, width: 400, height: 300 });
  await wait(600);
  probe.offscreenVisible = { bytes: await shoot() };

  // Same idea, less exotic: still inside the window, but underneath the app
  // view rather than on top of it. This is occlusion in the ordinary sense —
  // the case those Chromium switches are actually named after.
  step("escape route A2: visible but covered");
  view.setBounds({ x: 0, y: 0, width: 400, height: 300 });
  view.setVisible(true);
  await wait(600);
  probe.coveredVisible = { bytes: await shoot() };

  // ── C: the throttling switch Electron exposes per WebContents ─────────────
  //
  // Cheapest of the three to adopt if it works, and the one most likely not to:
  // it is documented as governing timers and animation rate, not compositing.
  // Measured rather than argued about, since that costs four lines.
  step("escape route C: setBackgroundThrottling(false) while hidden");
  view.webContents.setBackgroundThrottling(false);
  view.setBounds(VIEW);
  view.setVisible(false);
  await wait(600);
  probe.hiddenNoThrottle = { bytes: await shoot() };

  // The control, and it is not optional.
  //
  // C runs after A2 has had the view visible and painting, so "three good
  // shots" is also what a warmed-up view would produce with the switch doing
  // nothing at all. Putting the throttle back and repeating the same three
  // shots from the same place is the only thing that separates "the call fixed
  // it" from "the order fixed it" — and round two is the standing reminder of
  // what happens when a promising result is believed without one.
  step("escape route C: control — throttling back on, still hidden");
  view.webContents.setBackgroundThrottling(true);
  view.setVisible(true);
  await wait(400);
  view.setVisible(false);
  await wait(600);
  probe.hiddenThrottleRestored = { bytes: await shoot() };
  view.setVisible(true);

  // ── D: Electron's own `capturePage`, which is not CDP at all ──────────────
  //
  // Everything above asks Chromium's debugger for a frame. `capturePage` goes
  // through Electron's `WebContents` API instead, and there is no reason from
  // the outside to assume the two take the same path to the compositor — if it
  // does not, the whole "the pane has to be open" limitation goes away, and
  // that is the limitation worth the most.
  //
  // It answers a `NativeImage`, so "did it work" is `isEmpty()` and the size,
  // not a byte count of PNG: a 0×0 image is exactly what a view with nothing to
  // draw would hand back, and it would arrive without throwing.
  const capture = async (times = 3) => {
    const out = [];
    for (let i = 0; i < times; i += 1) {
      try {
        const image = await withTimeout(
          view.webContents.capturePage(),
          8000,
          "capturePage",
        );
        const size = image.getSize();
        out.push(
          image.isEmpty() ? "empty" : `${size.width}x${size.height}:${image.toPNG().length}`,
        );
      } catch (error) {
        out.push(String(error).includes("did not finish") ? "timeout" : String(error));
      }
    }
    return out;
  };

  step("escape route D: capturePage while hidden");
  view.setBounds(VIEW);
  view.setVisible(false);
  await wait(600);
  probe.hiddenCapturePage = { shots: await capture() };

  step("escape route D: capturePage while visible (control)");
  view.setVisible(true);
  await wait(600);
  probe.visibleCapturePage = { shots: await capture() };

  // ── B is a launch-time decision, so it is recorded rather than toggled ────
  //
  // Command-line switches have to be appended before `app.whenReady`, which is
  // above this function and above `main`. So the run carries whichever set it
  // was started with and says so, and the comparison is between two runs:
  //
  //     npm run spike                  → switches: false
  //     npm run spike:unthrottled      → switches: true
  //
  // Two runs rather than one clever one, because a switch that changes
  // process-wide compositing behaviour cannot be turned off again halfway
  // through and be trusted.
  probe.chromiumSwitches = SWITCHES;
}

async function agentBrowserProbes(view, origin) {
  const probe = { hidden: {}, axTree: {}, backgroundNavigation: {} };
  const contents = view.webContents;
  const dbg = contents.debugger;
  // Every await in here is guarded. A hidden view is exactly the configuration
  // where "the call never comes back" is a plausible outcome, and an
  // unguarded await turns that outcome into a window sitting on the screen
  // with no `results.json` — which reads as "the spike is broken" rather than
  // as the answer it actually is.
  const cdp = (method, args) =>
    withTimeout(dbg.sendCommand(method, args), 8000, method);
  const load = (url, ms = 15_000) =>
    withTimeout(contents.loadURL(url), ms, `loading ${url}`);

  try {
    if (!dbg.isAttached()) dbg.attach("1.3");

    // ── Question 1: CDP against a hidden view ───────────────────────────────
    step("hidden view: loading the fixture");
    await load(`${origin}/page.html`);
    await wait(400);
    view.setVisible(false);
    await wait(400);
    step("hidden view: hidden, probing");

    probe.hidden.attachedWhileHidden = dbg.isAttached();

    const evaluated = await cdp("Runtime.evaluate", {
      expression: "document.title",
      returnByValue: true,
    });
    probe.hidden.runtimeEvaluate = evaluated.result?.value;

    // The same click the visible probe does, read back the same way. The page
    // sets its own title on click and nothing else can, so a match means the
    // event really landed in a view nobody can see.
    for (const type of ["mousePressed", "mouseReleased"]) {
      await cdp("Input.dispatchMouseEvent", {
        type,
        x: 70,
        y: 55,
        button: "left",
        clickCount: 1,
      });
    }
    await wait(300);
    const afterClick = await cdp("Runtime.evaluate", {
      expression: "document.title",
      returnByValue: true,
    });
    probe.hidden.inputReachedThePage = afterClick.result?.value === "clicked";

    // Four shots, not one.
    //
    // The first two runs of this probe disagreed with each other: one had the
    // plain capture time out and `captureBeyondViewport` succeed, the next had
    // it exactly the other way round. Two runs, two opposite answers, and
    // either one taken alone would have been written into the design as a
    // fact. What both runs share is that the *first* capture returned and the
    // *second* hung — which is a completely different finding, and the only way
    // to tell them apart is to ask more than twice.
    //
    // A hidden view has no compositor producing frames, so the plausible
    // mechanism is that the first call is answered from the last frame the view
    // produced while visible, and every call after it waits for a frame that is
    // never coming.
    probe.hidden.screenshots = [];
    for (const args of [
      { format: "png" },
      { format: "png" },
      { format: "png", captureBeyondViewport: true },
      { format: "png", captureBeyondViewport: true },
    ]) {
      const label = args.captureBeyondViewport ? "beyondViewport" : "plain";
      try {
        const shot = await cdp("Page.captureScreenshot", args);
        probe.hidden.screenshots.push({
          how: label,
          bytes: shot.data ? Buffer.from(shot.data, "base64").length : 0,
        });
      } catch (error) {
        probe.hidden.screenshots.push({ how: label, error: String(error) });
      }
    }

    // And the control: the same call on the same view, visible. If this one is
    // reliable, "screenshot needs the tab on screen" is the rule; if it is not,
    // the whole action is in question.
    view.setVisible(true);
    await wait(400);
    probe.hidden.screenshotsWhileVisible = [];
    for (let i = 0; i < 3; i += 1) {
      try {
        const shot = await cdp("Page.captureScreenshot", { format: "png" });
        probe.hidden.screenshotsWhileVisible.push(
          shot.data ? Buffer.from(shot.data, "base64").length : 0,
        );
      } catch (error) {
        probe.hidden.screenshotsWhileVisible.push(String(error));
      }
    }
    view.setVisible(false);
    await wait(300);

    await escapeRoutes(view, probe, cdp);

    // ── Question 3: does a hidden tab still say where it went ───────────────
    step("hidden view: navigating");
    const navigated = new Promise((resolve) => {
      const timer = setTimeout(() => resolve(false), 8000);
      contents.once("did-navigate", () => (clearTimeout(timer), resolve(true)));
    });
    const started = Date.now();
    await load(`${origin}/app.html`, 10_000);
    probe.backgroundNavigation.didNavigateFired = await navigated;
    probe.backgroundNavigation.millis = Date.now() - started;

    view.setVisible(true);
    await wait(300);

    // ── Question 2: a real page's accessibility tree ────────────────────────
    //
    // A real one on purpose. The local fixture has nine nodes, which answers
    // "does the command work" and nothing about whether a snapshot fits in a
    // conversation. Network-dependent, so a failure here is recorded and not
    // fatal — the rest of this file must still produce a result offline.
    const REAL = "https://github.com/rust-lang/rust/pull/1";
    try {
      step(`ax tree: loading ${REAL}`);
      await load(REAL, 25_000);
      await wait(2500);
      await cdp("Accessibility.enable");
      const tree = await withTimeout(
        dbg.sendCommand("Accessibility.getFullAXTree"),
        20_000,
        "getFullAXTree on a real page",
      );
      const nodes = tree.nodes ?? [];
      const line = (node, index) => {
        const role = node.role?.value ?? "";
        const name = node.name?.value ?? "";
        return name ? `ref_${index} ${role} "${name}"` : `ref_${index} ${role}`;
      };
      // Three numbers, because they lead to three different designs. The raw
      // one says what CDP hands over; `trimmed` says what the naive "print the
      // tree" snapshot would cost; `filtered` says what is left once the nodes
      // a model cannot use are gone. If only the third one fits in a
      // conversation, the snapshot is a filter, not a serializer.
      const useful = nodes.filter((node) => {
        if (node.ignored) return false;
        const role = node.role?.value ?? "";
        // Structural containers carry no information a model acts on: their
        // children already say everything, and they are the bulk of a real
        // page's tree.
        if (["generic", "none", "presentation", "InlineTextBox"].includes(role)) return false;
        return Boolean(node.name?.value) || INTERACTIVE.has(role);
      });
      probe.axTree = {
        url: REAL,
        nodes: nodes.length,
        rawJsonBytes: Buffer.byteLength(JSON.stringify(tree)),
        trimmedBytes: Buffer.byteLength(nodes.map(line).join("\n")),
        ignoredNodes: nodes.filter((node) => node.ignored).length,
        usefulNodes: useful.length,
        filteredBytes: Buffer.byteLength(useful.map(line).join("\n")),
        interactiveNodes: nodes.filter(
          (node) => !node.ignored && INTERACTIVE.has(node.role?.value ?? ""),
        ).length,
      };
    } catch (error) {
      probe.axTree.error = String(error);
    }
  } catch (error) {
    probe.error = String(error);
  } finally {
    view.setVisible(true);
    if (dbg.isAttached()) dbg.detach();
  }

  results.probes.agentBrowser = probe;
}

async function main() {
  results.versions = {
    electron: process.versions.electron,
    chrome: process.versions.chrome,
    node: process.versions.node,
  };

  const store = session.fromPartition(PARTITION);

  // Question 4, first half: did anything survive the previous run? Read before
  // this run writes, or the answer is just this run's own cookie.
  const before = await store.cookies.get({});
  results.probes.sessionPersistedAcrossRuns = before.some(
    (cookie) => cookie.name === "spikePersist",
  );
  results.probes.cookiesAtStartup = before.map((c) => `${c.name}=${c.value}`);

  const server = await serve();
  const origin = `http://127.0.0.1:${server.address().port}`;

  const win = new BaseWindow({ ...WINDOW, frame: false, show: true });
  win.setAlwaysOnTop(true);

  // The app's own UI is a `WebContentsView` too — that is the target shape, and
  // it is what makes the z-order question meaningful rather than academic.
  const appView = new WebContentsView({
    webPreferences: { contextIsolation: true, nodeIntegration: false, sandbox: true },
  });
  appView.setBounds({ x: 0, y: 0, width: WINDOW.width, height: WINDOW.height });
  win.contentView.addChildView(appView);
  await appView.webContents.loadFile(path.join(__dirname, "app.html"));

  // A browser tab: no preload at all, its own partition. This is the shape the
  // security section of `AGENTS.md` rule 9h assumes.
  const browserView = new WebContentsView({
    webPreferences: {
      partition: PARTITION,
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: true,
    },
  });
  browserView.setBounds(VIEW);
  win.contentView.addChildView(browserView);

  let windowOpenAsked = null;
  browserView.webContents.setWindowOpenHandler(({ url }) => {
    windowOpenAsked = url;
    return { action: "deny" };
  });

  await browserView.webContents.loadURL(`${origin}/page.html`);
  await wait(600);

  // ── Question 1: who paints on top ──────────────────────────────────────────
  results.probes.zOrder = {};
  results.probes.zOrder.viewAdded = whoIsOnTop(await samplePixel(SAMPLE));

  // Can the DOM ever win? Two levers exist in this API: remove the child, or
  // hide it. Whichever works is what `yieldToPopover` becomes.
  const hasSetVisible = typeof browserView.setVisible === "function";
  results.probes.zOrder.setVisibleExists = hasSetVisible;
  if (hasSetVisible) {
    browserView.setVisible(false);
    await wait(300);
    results.probes.zOrder.afterSetVisibleFalse = whoIsOnTop(await samplePixel(SAMPLE));
    browserView.setVisible(true);
    await wait(300);
  }

  win.contentView.removeChildView(browserView);
  await wait(300);
  results.probes.zOrder.afterRemoveChildView = whoIsOnTop(await samplePixel(SAMPLE));
  win.contentView.addChildView(browserView);
  browserView.setBounds(VIEW);
  await wait(300);
  results.probes.zOrder.afterReAdd = whoIsOnTop(await samplePixel(SAMPLE));

  // Re-adding must not have cost the page its state — that is the whole reason
  // one tab is one view rather than one view navigated around.
  results.probes.zOrder.pageSurvivedReAdd =
    (await evaluate(browserView, "location.pathname")) === "/page.html";

  // ── Question 2: does a resize reach the page ───────────────────────────────
  const resized = { x: VIEW.x, y: VIEW.y, width: 520, height: 240 };
  browserView.setBounds(resized);
  await wait(500);
  const seen = await evaluate(
    browserView,
    "({ w: window.innerWidth, h: window.innerHeight })",
  );
  results.probes.geometry = {
    requested: { width: resized.width, height: resized.height },
    pageReports: seen,
    // The failure `place.rs` documents: the frame changes and the page does not.
    pageFollowedTheHost: seen.w === resized.width && seen.h === resized.height,
    hostReports: browserView.getBounds(),
  };
  browserView.setBounds(VIEW);
  await wait(300);

  // ── Question 3: CDP ────────────────────────────────────────────────────────
  const cdp = { available: false };
  try {
    const dbg = browserView.webContents.debugger;
    dbg.attach("1.3");
    cdp.available = true;

    const evaluated = await dbg.sendCommand("Runtime.evaluate", {
      expression: "document.title",
      returnByValue: true,
    });
    cdp.runtimeEvaluate = evaluated.result?.value;

    // A synthesized click, read back through the page's own title. Nothing but
    // the page can set that, so a match means the input actually landed.
    const at = { x: 70, y: 55 };
    for (const type of ["mousePressed", "mouseReleased"]) {
      await dbg.sendCommand("Input.dispatchMouseEvent", {
        type,
        x: at.x,
        y: at.y,
        button: "left",
        clickCount: 1,
      });
    }
    await wait(200);
    const afterClick = await dbg.sendCommand("Runtime.evaluate", {
      expression: "document.title",
      returnByValue: true,
    });
    cdp.inputReachedThePage = afterClick.result?.value === "clicked";

    const shot = await dbg.sendCommand("Page.captureScreenshot", { format: "png" });
    cdp.screenshotBytes = shot.data ? Buffer.from(shot.data, "base64").length : 0;

    await dbg.sendCommand("Accessibility.enable");
    const tree = await dbg.sendCommand("Accessibility.getFullAXTree");
    cdp.axNodes = tree.nodes?.length ?? 0;

    // What the app's own devtools would do to this view — Electron documents
    // that it detaches the debugger, and a BrowserManager has to know.
    //
    // Sampled twice with a generous wait, because a "still attached" read taken
    // before DevTools finished opening is indistinguishable from the docs being
    // out of date, and those two lead to opposite designs.
    browserView.webContents.openDevTools({ mode: "detach" });
    await wait(1500);
    cdp.attachedShortlyAfterDevTools = dbg.isAttached();
    await wait(3000);
    cdp.attachedWellAfterDevTools = dbg.isAttached();
    cdp.devToolsActuallyOpened = browserView.webContents.isDevToolsOpened();
    browserView.webContents.closeDevTools();
    if (dbg.isAttached()) dbg.detach();
  } catch (error) {
    cdp.error = String(error);
  }
  results.probes.cdp = cdp;

  // ── Question 4, second half: does the partition share cookies ──────────────
  const sessions = {};
  try {
    await evaluate(browserView, "document.cookie = 'spikeShared=1; path=/'");
    await evaluate(browserView, "document.cookie = 'spikePersist=1; path=/; max-age=86400'");

    const sibling = new WebContentsView({
      webPreferences: { partition: PARTITION, contextIsolation: true, sandbox: true },
    });
    sibling.setBounds({ x: 0, y: 0, width: 10, height: 10 });
    win.contentView.addChildView(sibling);
    await sibling.webContents.loadURL(`${origin}/page.html`);
    sessions.sharedWithSiblingView = (await evaluate(sibling, "document.cookie")).includes(
      "spikeShared=1",
    );
    win.contentView.removeChildView(sibling);
    sibling.webContents.close();

    // A different partition must not see them, or "profile" means nothing.
    const other = new WebContentsView({
      webPreferences: { partition: "persist:tcode-spike-other", contextIsolation: true, sandbox: true },
    });
    other.setBounds({ x: 0, y: 0, width: 10, height: 10 });
    win.contentView.addChildView(other);
    await other.webContents.loadURL(`${origin}/page.html`);
    sessions.isolatedFromOtherPartition = !(await evaluate(other, "document.cookie")).includes(
      "spikeShared",
    );
    win.contentView.removeChildView(other);
    other.webContents.close();

    sessions.visibleThroughSessionApi = (await store.cookies.get({}))
      .map((c) => c.name)
      .sort();
  } catch (error) {
    sessions.error = String(error);
  }
  results.probes.sessions = sessions;

  // ── The security assumptions the plan is written on ────────────────────────
  const isolation = {};
  try {
    isolation.exposedGlobals = await evaluate(
      browserView,
      "['process','require','tcode','__TAURI__'].filter((k) => k in window)",
    );
    await evaluate(browserView, "window.open('https://example.com/', '_blank')");
    await wait(300);
    isolation.windowOpenWentToHandler = windowOpenAsked === "https://example.com/";
  } catch (error) {
    isolation.error = String(error);
  }
  results.probes.isolation = isolation;

  await agentBrowserProbes(browserView, origin);

  report();
  server.close();
  app.quit();
}

/** Write whatever has been measured so far. Idempotent; the happy path calls
 *  it too, so there is one spelling of "this is the answer". */
function report() {
  fs.writeFileSync(
    path.join(__dirname, "results.json"),
    JSON.stringify(results, null, 2),
  );
}

app.whenReady().then(() => {
  // The run as a whole is bounded, not only its individual awaits. A spike
  // that leaves an always-on-top window on someone's screen and never writes a
  // file is the one failure mode worth spending eight lines on: it is
  // indistinguishable from a broken spike, and it has to be killed by hand.
  const watchdog = setTimeout(() => {
    results.fatal = "the run exceeded its overall budget; probes above are what finished";
    report();
    app.exit(1);
  }, 180_000);

  main()
    .catch((error) => {
      results.fatal = String(error?.stack ?? error);
      report();
      app.exit(1);
    })
    .finally(() => clearTimeout(watchdog));
});

app.on("window-all-closed", () => app.quit());
