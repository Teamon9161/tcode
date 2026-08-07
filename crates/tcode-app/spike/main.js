/**
 * Phase 0 of `MIGRATION-ELECTRON.md`. Throwaway code: it answers four
 * questions, writes `results.json`, and gets deleted.
 *
 * The questions, and why each one can change the plan:
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
const results = { platform: process.platform, versions: {}, probes: {} };

const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

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
  // security section of `MIGRATION-ELECTRON.md` assumes.
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

  fs.writeFileSync(
    path.join(__dirname, "results.json"),
    JSON.stringify(results, null, 2),
  );
  server.close();
  app.quit();
}

app.whenReady().then(() =>
  main().catch((error) => {
    results.fatal = String(error?.stack ?? error);
    fs.writeFileSync(
      path.join(__dirname, "results.json"),
      JSON.stringify(results, null, 2),
    );
    app.exit(1);
  }),
);

app.on("window-all-closed", () => app.quit());
