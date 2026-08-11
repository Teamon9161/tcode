/*
 * Phase-0 probe for a permanently-topmost transparent app WebContentsView.
 *
 * Run with the production Electron dependency, not spike/package.json:
 *   gcc -O2 -Wall -Wextra -Werror spike/transparent-underlay-uinput.c \
 *     -o /tmp/tcode-transparent-underlay-click
 *   TCODE_UINPUT_CLICK=/tmp/tcode-transparent-underlay-click \
 *     ./node_modules/.bin/electron spike/transparent-underlay.js
 *
 * The optional helper injects one real compositor-level click. Visual probes,
 * browser capture, canvas/portal rendering, and fixed native bounds run without
 * it. Results are written beside this file.
 */

const {
  app,
  BaseWindow,
  WebContentsView,
  desktopCapturer,
  screen,
} = require("electron");
const { execFile } = require("node:child_process");
const fs = require("node:fs");
const http = require("node:http");
const path = require("node:path");
const { promisify } = require("node:util");

const execFileAsync = promisify(execFile);
const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const WINDOW = { width: 900, height: 620 };
const INITIAL = { x: 280, y: 140, width: 420, height: 300 };
const MOVED = { x: 420, y: 240, width: 360, height: 240 };
const COLORS = {
  browser: [20, 60, 220],
  canvas: [20, 180, 90],
  portal: [240, 180, 20],
  chrome: [225, 230, 238],
  popover: [220, 30, 30],
};
const resultPath = path.join(__dirname, "transparent-underlay-results.json");
const results = {
  platform: process.platform,
  versions: {},
  environment: {},
  isolation: {},
  probes: {},
};

function serveBrowserPage() {
  const page = `<!doctype html>
<meta charset="utf-8">
<title>transparent underlay browser</title>
<style>
  html, body { margin: 0; width: 100%; height: 100%; overflow: hidden; background: rgb(20, 60, 220); }
  #hit { position: fixed; left: 300px; top: 160px; width: 110px; height: 80px; border: 0; padding: 0; background: rgb(20, 60, 220); color: white; }
  #canvas { position: fixed; left: 470px; top: 220px; width: 160px; height: 100px; }
  #portal { position: fixed; left: 440px; top: 260px; width: 180px; height: 110px; background: rgb(240, 180, 20); color: black; display: none; }
</style>
<button id="hit">real click target</button>
<canvas id="canvas" width="160" height="100"></canvas>
<div id="portal" role="dialog" aria-label="Browser portal">browser portal</div>
<script>
  window.browserClicks = 0;
  const canvas = document.querySelector("#canvas");
  const draw = () => {
    const context = canvas.getContext("2d");
    context.fillStyle = "rgb(20, 180, 90)";
    context.fillRect(0, 0, canvas.width, canvas.height);
  };
  requestAnimationFrame(draw);
  document.querySelector("#hit").addEventListener("click", () => { window.browserClicks += 1; });
  window.showPortal = () => { document.querySelector("#portal").style.display = "block"; };
</script>`;

  return new Promise((resolve, reject) => {
    const server = http.createServer((_request, response) => {
      response.writeHead(200, {
        "content-type": "text/html; charset=utf-8",
        "cache-control": "no-store",
      });
      response.end(page);
    });
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => resolve(server));
  });
}

function appPage() {
  return `<!doctype html>
<meta charset="utf-8">
<title>transparent app overlay</title>
<style>
  html, body { margin: 0; width: 100%; height: 100%; overflow: hidden; background: transparent; pointer-events: none; }
  .chrome { position: fixed; background: rgb(225, 230, 238); pointer-events: auto; }
  #top { left: 0; top: 0; right: 0; }
  #left { left: 0; }
  #right { right: 0; }
  #bottom { left: 0; right: 0; bottom: 0; }
  #input-control { position: fixed; inset: 0; background: transparent; pointer-events: auto; }
  #popover { position: fixed; left: 430px; top: 220px; width: 200px; height: 140px; display: none; background: rgb(220, 30, 30); pointer-events: auto; }
</style>
<div id="top" class="chrome"></div>
<div id="left" class="chrome"></div>
<div id="right" class="chrome"></div>
<div id="bottom" class="chrome"></div>
<div id="input-control"></div>
<div id="popover"></div>
<script>
  window.appClicks = 0;
  document.querySelector("#input-control").addEventListener("click", () => { window.appClicks += 1; });
  window.enableInputControl = (enabled) => {
    document.querySelector("#input-control").style.pointerEvents = enabled ? "auto" : "none";
  };
  window.setOpening = ({ x, y, width, height }) => {
    Object.assign(document.querySelector("#top").style, { height: y + "px" });
    Object.assign(document.querySelector("#left").style, { top: y + "px", width: x + "px", height: height + "px" });
    Object.assign(document.querySelector("#right").style, { top: y + "px", left: (x + width) + "px", height: height + "px" });
    Object.assign(document.querySelector("#bottom").style, { top: (y + height) + "px" });
  };
  window.setPopover = (visible) => { document.querySelector("#popover").style.display = visible ? "block" : "none"; };
</script>`;
}

function dataUrl(html) {
  return `data:text/html;charset=utf-8,${encodeURIComponent(html)}`;
}

function near(rgb, expected, tolerance = 28) {
  return Array.isArray(rgb) && rgb.every((value, index) => Math.abs(value - expected[index]) <= tolerance);
}

function pixel(image, x, y) {
  const size = image.getSize();
  if (x < 0 || y < 0 || x >= size.width || y >= size.height) {
    return { error: `pixel ${x},${y} outside ${size.width}x${size.height}` };
  }
  const bitmap = image.toBitmap();
  const stride = bitmap.length / size.height;
  const at = Math.floor(y) * stride + Math.floor(x) * 4;
  return {
    rgb: [bitmap[at + 2], bitmap[at + 1], bitmap[at]],
    alpha: bitmap[at + 3],
  };
}

async function displayCapture(win) {
  const bounds = win.getContentBounds();
  const display = screen.getDisplayNearestPoint({ x: bounds.x, y: bounds.y });
  const sources = await desktopCapturer.getSources({
    types: ["screen"],
    thumbnailSize: {
      width: Math.round(display.size.width * display.scaleFactor),
      height: Math.round(display.size.height * display.scaleFactor),
    },
  });
  const source =
    sources.find((candidate) => String(candidate.display_id) === String(display.id)) ??
    (sources.length === 1 ? sources[0] : null);
  if (!source || source.thumbnail.isEmpty()) {
    return {
      error: `no capture source matched display ${display.id}`,
      display,
      sources: sources.map((candidate) => ({ id: candidate.id, displayId: candidate.display_id })),
    };
  }
  return {
    image: source.thumbnail,
    display,
    bounds,
    source: {
      id: source.id,
      name: source.name,
      displayId: source.display_id,
      size: source.thumbnail.getSize(),
      available: sources.map((candidate) => ({
        id: candidate.id,
        name: candidate.name,
        displayId: candidate.display_id,
        size: candidate.thumbnail.getSize(),
      })),
    },
  };
}

function sampleScreen(capture, point) {
  if (capture.error) return { error: capture.error };
  const scale = capture.display.scaleFactor;
  const x = Math.round((capture.bounds.x + point.x - capture.display.bounds.x) * scale);
  const y = Math.round((capture.bounds.y + point.y - capture.display.bounds.y) * scale);
  return { ...pixel(capture.image, x, y), physical: { x, y }, scale };
}

function classify(sample) {
  if (sample.error) return `unknown (${sample.error})`;
  for (const [name, color] of Object.entries(COLORS)) {
    if (near(sample.rgb, color)) return name;
  }
  return `other (${sample.rgb.join(",")})`;
}

async function evaluate(view, expression) {
  return view.webContents.executeJavaScript(expression, true);
}

async function realClick(win, appView, browserView) {
  const helper = process.env.TCODE_UINPUT_CLICK;
  if (!helper) return { skipped: "TCODE_UINPUT_CLICK was not set" };
  if (!fs.existsSync(helper)) return { skipped: `${helper} does not exist` };

  const target = {
    x: win.getContentBounds().x + INITIAL.x + 75,
    y: win.getContentBounds().y + INITIAL.y + 60,
  };
  const click = async () => {
    await execFileAsync(helper, [String(target.x), String(target.y)], { timeout: 5000 });
    await wait(300);
    return {
      appClicks: await evaluate(appView, "window.appClicks"),
      browserClicks: await evaluate(browserView, "window.browserClicks"),
    };
  };

  await evaluate(appView, "window.enableInputControl(true)");
  const appControl = await click();
  await evaluate(appView, "window.enableInputControl(false)");
  const transparentOpening = await click();
  return {
    backend: "/dev/uinput relative mouse",
    target,
    appControl,
    transparentOpening,
    controlReachedTopAppView: appControl.appClicks === 1 && appControl.browserClicks === 0,
    reachedBrowserThroughTransparentPixels:
      appControl.appClicks === 1 &&
      transparentOpening.appClicks === 1 &&
      transparentOpening.browserClicks === 1,
  };
}

function report() {
  fs.writeFileSync(resultPath, `${JSON.stringify(results, null, 2)}\n`);
}

async function main() {
  results.versions = {
    electron: process.versions.electron,
    chrome: process.versions.chrome,
    node: process.versions.node,
  };
  results.environment = {
    sessionType: process.env.XDG_SESSION_TYPE ?? null,
    desktop: process.env.XDG_CURRENT_DESKTOP ?? null,
    waylandDisplay: process.env.WAYLAND_DISPLAY ?? null,
    xDisplay: process.env.DISPLAY ?? null,
    commandLine: process.argv,
    displays: screen.getAllDisplays(),
    gpuFeatureStatus: app.getGPUFeatureStatus(),
  };
  try {
    const gpu = await app.getGPUInfo("complete");
    results.environment.gpu = {
      auxAttributes: gpu.auxAttributes,
      gpuDevice: gpu.gpuDevice,
    };
  } catch (error) {
    results.environment.gpuError = String(error);
  }

  const server = await serveBrowserPage();
  let win;
  let appView;
  let browserView;
  try {
    const display = screen.getPrimaryDisplay();
    win = new BaseWindow({
      x: display.workArea.x + 60,
      y: display.workArea.y + 60,
      ...WINDOW,
      frame: false,
      show: true,
      backgroundColor: "#ffffff",
    });
    win.setAlwaysOnTop(true);

    let nativeBoundsWrites = 0;
    browserView = new WebContentsView({
      webPreferences: {
        partition: "persist:tcode-transparent-underlay-spike",
        nodeIntegration: false,
        contextIsolation: true,
        sandbox: true,
      },
    });
    browserView.setBounds({ x: 0, y: 0, ...WINDOW });
    nativeBoundsWrites += 1;
    win.contentView.addChildView(browserView);

    appView = new WebContentsView({
      webPreferences: {
        transparent: true,
        nodeIntegration: false,
        contextIsolation: true,
        sandbox: true,
      },
    });
    appView.setBackgroundColor("#00000000");
    appView.setBounds({ x: 0, y: 0, ...WINDOW });
    win.contentView.addChildView(appView);

    const origin = `http://127.0.0.1:${server.address().port}`;
    await Promise.all([
      browserView.webContents.loadURL(origin),
      appView.webContents.loadURL(dataUrl(appPage())),
    ]);
    await evaluate(appView, `window.setOpening(${JSON.stringify(INITIAL)})`);
    await wait(900);

    results.isolation = {
      browserWebPreferences: {
        preload: null,
        nodeIntegration: false,
        contextIsolation: true,
        sandbox: true,
        partition: "persist:tcode-transparent-underlay-spike",
      },
      exposedGlobals: await evaluate(
        browserView,
        "['process', 'require', 'tcode', '__TAURI__'].filter((name) => name in window)",
      ),
      siblingOrder: win.contentView.children.map((view) =>
        view === browserView ? "browser" : view === appView ? "app" : "other",
      ),
    };

    let capture = await displayCapture(win);
    const visible = sampleScreen(capture, { x: 450, y: 180 });
    const canvas = sampleScreen(capture, { x: 520, y: 250 });
    const chrome = sampleScreen(capture, { x: 100, y: 100 });
    results.probes.transparentOpening = {
      captureSource: capture.source,
      browserSample: visible,
      browserClass: classify(visible),
      canvasSample: canvas,
      canvasClass: classify(canvas),
      chromeSample: chrome,
      chromeClass: classify(chrome),
      passed: classify(visible) === "browser" && classify(canvas) === "canvas" && classify(chrome) === "chrome",
    };

    await evaluate(appView, "window.setPopover(true)");
    await wait(300);
    capture = await displayCapture(win);
    const popover = sampleScreen(capture, { x: 520, y: 250 });
    results.probes.trustedOverlay = {
      sample: popover,
      class: classify(popover),
      passed: classify(popover) === "popover",
    };
    await evaluate(appView, "window.setPopover(false)");
    await wait(200);

    results.probes.realPointerInput = await realClick(win, appView, browserView);

    const beforeBounds = browserView.getBounds();
    const beforeViewport = await evaluate(browserView, "({ width: innerWidth, height: innerHeight })");
    await evaluate(appView, `window.setOpening(${JSON.stringify(MOVED)})`);
    await wait(350);
    const appCapture = await appView.webContents.capturePage();
    const appOldOpening = pixel(appCapture, 300, 180);
    const appNewOpening = pixel(appCapture, 760, 450);
    capture = await displayCapture(win);
    const oldOpening = sampleScreen(capture, { x: 300, y: 180 });
    const newOpening = sampleScreen(capture, { x: 760, y: 450 });
    appView.webContents.invalidate();
    await wait(500);
    const invalidatedCapture = await displayCapture(win);
    const invalidatedOldOpening = sampleScreen(invalidatedCapture, { x: 300, y: 180 });
    const invalidatedNewOpening = sampleScreen(invalidatedCapture, { x: 760, y: 450 });
    const afterBounds = browserView.getBounds();
    const afterViewport = await evaluate(browserView, "({ width: innerWidth, height: innerHeight })");
    results.probes.domOnlyOpeningMove = {
      captureSource: capture.source,
      nativeBoundsWrites,
      beforeBounds,
      afterBounds,
      beforeViewport,
      afterViewport,
      oldOpeningClass: classify(oldOpening),
      oldOpeningSample: oldOpening,
      newOpeningClass: classify(newOpening),
      newOpeningSample: newOpening,
      appCaptureOldOpening: appOldOpening,
      appCaptureNewOpening: appNewOpening,
      afterInvalidateOldOpeningClass: classify(invalidatedOldOpening),
      afterInvalidateOldOpeningSample: invalidatedOldOpening,
      afterInvalidateNewOpeningClass: classify(invalidatedNewOpening),
      afterInvalidateNewOpeningSample: invalidatedNewOpening,
      passed:
        nativeBoundsWrites === 1 &&
        JSON.stringify(beforeBounds) === JSON.stringify(afterBounds) &&
        JSON.stringify(beforeViewport) === JSON.stringify(afterViewport) &&
        classify(invalidatedOldOpening) === "chrome" &&
        classify(invalidatedNewOpening) === "browser",
    };

    await evaluate(browserView, "window.showPortal()");
    await wait(250);
    const shots = [];
    for (let index = 0; index < 3; index += 1) {
      const image = await browserView.webContents.capturePage();
      shots.push({
        empty: image.isEmpty(),
        size: image.getSize(),
        pngBytes: image.toPNG().length,
        canvasPixel: pixel(image, 520, 250),
        portalPixel: pixel(image, 500, 300),
      });
    }
    results.probes.browserRenderingAndCapture = {
      shots,
      portalPresentInAccessibilityTree: Boolean(
        await evaluate(browserView, "document.querySelector('[role=dialog]')?.getAttribute('aria-label')"),
      ),
      passed: shots.every(
        (shot) =>
          !shot.empty &&
          shot.size.width === WINDOW.width &&
          shot.size.height === WINDOW.height &&
          near(shot.canvasPixel.rgb, COLORS.canvas) &&
          near(shot.portalPixel.rgb, COLORS.portal),
      ),
    };

    results.probes.summary = {
      transparentOpening: results.probes.transparentOpening.passed,
      trustedOverlay: results.probes.trustedOverlay.passed,
      realPointerInput: results.probes.realPointerInput.reachedBrowserThroughTransparentPixels === true,
      domOnlyOpeningMove: results.probes.domOnlyOpeningMove.passed,
      browserRenderingAndCapture: results.probes.browserRenderingAndCapture.passed,
    };
  } finally {
    report();
    if (browserView && !browserView.webContents.isDestroyed()) browserView.webContents.close();
    if (appView && !appView.webContents.isDestroyed()) appView.webContents.close();
    if (win && !win.isDestroyed()) win.destroy();
    await new Promise((resolve) => server.close(resolve));
  }
}

app.whenReady().then(() => {
  const watchdog = setTimeout(() => {
    results.fatal = "probe exceeded 60 seconds";
    report();
    app.exit(1);
  }, 60_000);
  main()
    .catch((error) => {
      results.fatal = String(error?.stack ?? error);
      report();
      app.exitCode = 1;
    })
    .finally(() => {
      clearTimeout(watchdog);
      app.quit();
    });
});

app.on("window-all-closed", () => app.quit());
