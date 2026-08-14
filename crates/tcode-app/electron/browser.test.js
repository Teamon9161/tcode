const assert = require("node:assert/strict");
const test = require("node:test");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

const {
  browserVerbs,
  BROWSER_THUMBNAIL,
  BROWSER_DOWNLOAD,
} = require("./browser");

function fakeImage(width = 800, height = 600) {
  return {
    isEmpty: () => false,
    getSize: () => ({ width, height }),
    resize: ({ width: nextWidth }) => fakeImage(nextWidth, Math.round(height * nextWidth / width)),
    toPNG: () => Buffer.from(`image-${width}x${height}`),
  };
}

class FakeWebContentsView {
  static instances = [];

  constructor(options) {
    this.options = options;
    this.operations = [];
    this.bounds = [];
    this.webContents = {
      on() {},
      setWindowOpenHandler() {},
      loadURL: async (url) => { this.url = url; },
      capturePage: async () => fakeImage(),
      executeJavaScript: async () => null,
      close() {},
      getURL: () => this.url || "about:blank",
      getTitle: () => "",
      debugger: {
        isAttached: () => false,
        attach() {},
        sendCommand: async (method) => {
          throw new Error(`unexpected CDP command: ${method}`);
        },
      },
    };
    FakeWebContentsView.instances.push(this);
  }

  setVisible(visible) {
    this.operations.push(["visible", visible]);
  }

  setBounds(bounds) {
    this.bounds.push(bounds);
    this.lastBounds = bounds;
    this.operations.push(["bounds", bounds]);
  }

  getBounds() {
    return this.lastBounds;
  }
}

function harness(renderTimeout = 10, thumbnailDelay = 120) {
  FakeWebContentsView.instances = [];
  const events = [];
  // Captures the `will-download` handler so a test can fire it with a fake
  // DownloadItem, the way the partition would when a page starts a download.
  const partition = {
    setPermissionRequestHandler() {},
    on(event, handler) {
      if (event === "will-download") this.willDownload = handler;
    },
  };
  const downloadsDir = fs.mkdtempSync(path.join(os.tmpdir(), "tcode-dl-"));
  const shellCalls = [];
  const shell = {
    openPath: async (target) => {
      shellCalls.push(["open", target]);
      return "";
    },
    showItemInFolder(target) {
      shellCalls.push(["reveal", target]);
    },
  };
  const appView = {
    name: "app",
    webContents: {
      capturePage: async () => fakeImage(),
    },
  };
  const children = [appView];
  const contentView = {
    addChildView(view, index) {
      const existing = children.indexOf(view);
      if (existing >= 0) children.splice(existing, 1);
      if (index === undefined) children.push(view);
      else children.splice(index, 0, view);
    },
    removeChildView(view) {
      const existing = children.indexOf(view);
      if (existing >= 0) children.splice(existing, 1);
    },
  };
  const verbs = browserVerbs(
    {
      window: { contentView },
      appView,
      emit(name, payload) { events.push({ name, payload }); },
      resolveUrl: async (url) => url,
      downloadsDir,
    },
    {
      WebContentsView: FakeWebContentsView,
      session: { fromPartition: () => partition },
      shell,
      renderTimeout,
      thumbnailDelay,
    },
  );
  const first = verbs.browser_open({
    rect: { x: 0, y: 0, width: 800, height: 600 },
    select: true,
  });
  return {
    verbs,
    first,
    view: FakeWebContentsView.instances[0],
    appView,
    children,
    events,
    partition,
    downloadsDir,
    shellCalls,
  };
}

/** A minimal stand-in for Electron's DownloadItem: it remembers the save path
 *  the handler assigns, and lets a test drive `updated`/`done` by hand. */
function fakeDownloadItem({ filename = "report.pdf", url = "https://x/report.pdf", total = 100 } = {}) {
  const listeners = { updated: [], done: [] };
  let received = 0;
  return {
    savePath: null,
    getFilename: () => filename,
    getURL: () => url,
    getTotalBytes: () => total,
    getReceivedBytes: () => received,
    on(event, handler) {
      listeners[event].push(handler);
    },
    once(event, handler) {
      listeners[event].push(handler);
    },
    // Test drivers.
    progress(bytes) {
      received = bytes;
      listeners.updated.forEach((handler) => handler({}, "progressing"));
    },
    finish(state = "completed", bytes = total) {
      received = bytes;
      listeners.done.forEach((handler) => handler({}, state));
    },
  };
}

function expectShownAt(view, rect) {
  assert.deepEqual(view.operations.slice(-2), [
    ["visible", true],
    ["bounds", rect],
  ]);
}

async function waitFor(check, timeout = 1_000) {
  const until = Date.now() + timeout;
  while (!check()) {
    if (Date.now() >= until) throw new Error("timed out waiting for test condition");
    await new Promise((resolve) => setTimeout(resolve, 1));
  }
}

test("a hidden browser defers divider bounds until it is shown", () => {
  const { verbs, view } = harness();
  verbs.browser_visible({ visible: false });
  view.bounds.length = 0;
  view.operations.length = 0;

  verbs.browser_bounds({ rect: { x: 100, y: 20, width: 600, height: 500 } });
  verbs.browser_bounds({ rect: { x: 140, y: 20, width: 560, height: 500 } });

  assert.equal(view.bounds.length, 0, "hidden pages must not re-layout on drag frames");

  verbs.browser_visible({ visible: true });
  assert.deepEqual(view.bounds, [{ x: 140, y: 20, width: 560, height: 500 }]);
  expectShownAt(view, { x: 140, y: 20, width: 560, height: 500 });
});

test("a visible browser still follows an ordinary bounds update", () => {
  const { verbs, view } = harness();
  verbs.browser_visible({ visible: true });
  view.bounds.length = 0;

  verbs.browser_bounds({ rect: { x: 80, y: 30, width: 720, height: 540 } });

  assert.deepEqual(view.bounds, [{ x: 80, y: 30, width: 720, height: 540 }]);
});

test("a screenshot uses a temporary viewport under the app and restores the pane", async () => {
  const { verbs, first, view, appView, children } = harness();
  const pane = { x: 40, y: 70, width: 900, height: 640 };
  verbs.browser_show({ rect: pane });
  let capturedAt;
  view.webContents.capturePage = async () => {
    capturedAt = view.lastBounds;
    return fakeImage(800, 600);
  };

  const answer = await verbs.browser_screenshot({
    id: first,
    viewport: { width: 800, height: 600 },
  });

  assert.deepEqual(capturedAt, { x: 40, y: 70, width: 800, height: 600 });
  assert.deepEqual(view.lastBounds, pane, "the responsive probe became the visible pane size");
  assert.equal(children.at(-1), view, "the app cover was not removed after capture");
  assert.equal(children.includes(appView), true);
  assert.deepEqual({ width: answer.width, height: answer.height }, { width: 800, height: 600 });
});

test("a failed responsive screenshot restores the visible pane", async () => {
  const { verbs, first, view, children } = harness();
  const pane = { x: 40, y: 70, width: 900, height: 640 };
  verbs.browser_show({ rect: pane });
  let captures = 0;
  view.webContents.capturePage = async () => {
    captures += 1;
    if (captures === 1) return fakeImage(800, 600);
    throw new Error("capture unavailable");
  };

  await assert.rejects(
    verbs.browser_screenshot({
      id: first,
      viewport: { width: 800, height: 600 },
    }),
    /capture unavailable/,
  );

  assert.deepEqual(view.lastBounds, pane);
  assert.equal(children.at(-1), view);
});

test("a background screenshot viewport restores that tab's pane bounds", async () => {
  const { verbs } = harness();
  const pane = { x: 30, y: 50, width: 900, height: 640 };
  verbs.browser_show({ rect: pane });
  const backgroundId = verbs.browser_open({ select: false });
  const background = FakeWebContentsView.instances[1];

  await verbs.browser_screenshot({
    id: backgroundId,
    viewport: { width: 640, height: 480 },
  });

  assert.deepEqual(background.lastBounds, pane);
  assert.deepEqual(background.operations.at(-2), ["bounds", pane]);
  assert.deepEqual(background.operations.at(-1), ["visible", false]);
});

test("an invalid screenshot viewport is rejected before capture", async () => {
  const { verbs, first, view } = harness();
  let captures = 0;
  view.webContents.capturePage = async () => {
    captures += 1;
    return fakeImage();
  };

  await assert.rejects(
    verbs.browser_screenshot({ id: first, viewport: { width: 200, height: 600 } }),
    /screenshot viewport/,
  );
  assert.equal(captures, 0);
});

test("showing an existing browser places the current tab after visibility", () => {
  const { verbs, view } = harness();
  const rect = { x: 40, y: 70, width: 900, height: 640 };
  view.operations.length = 0;

  verbs.browser_show({ rect });

  expectShownAt(view, rect);
});

test("selecting a tab places it after it becomes visible", () => {
  const { verbs } = harness();
  verbs.browser_visible({ visible: true });
  const id = verbs.browser_open({ select: false });
  const selected = FakeWebContentsView.instances[1];
  selected.operations.length = 0;

  verbs.browser_select({ id });

  expectShownAt(selected, { x: 0, y: 0, width: 800, height: 600 });
});

test("opening a selected tab in a shown browser places it after visibility", () => {
  const { verbs } = harness();
  verbs.browser_visible({ visible: true });
  const rect = { x: 25, y: 45, width: 760, height: 520 };

  verbs.browser_open({ rect, select: true });

  expectShownAt(FakeWebContentsView.instances[1], rect);
});

test("closing a background tab re-places the visible current tab", () => {
  const { verbs, view } = harness();
  verbs.browser_visible({ visible: true });
  const background = verbs.browser_open({ select: false });
  view.operations.length = 0;

  verbs.browser_close({ id: background });

  expectShownAt(view, { x: 0, y: 0, width: 800, height: 600 });
});


test("computed style uses a fixed function and passes property names as values", async () => {
  const { verbs, first, view } = harness();
  const calls = [];
  view.webContents.debugger.sendCommand = async (method, params) => {
    calls.push([method, params]);
    if (method === "DOM.enable") return {};
    if (method === "DOM.resolveNode") return { object: { objectId: "node-7" } };
    if (method === "Runtime.callFunctionOn") {
      assert.equal(params.objectId, "node-7");
      assert.match(params.functionDeclaration, /getComputedStyle/);
      assert.ok(!params.functionDeclaration.includes("background-color"));
      assert.deepEqual(params.arguments, [{ value: ["display", "background-color"] }]);
      return { result: { value: { display: "block", "background-color": "rgb(1, 2, 3)" } } };
    }
    if (method === "Runtime.releaseObject") return {};
    throw new Error(`unexpected CDP command: ${method}`);
  };

  const answer = await verbs.browser_computed_style({
    id: first,
    ref: 7,
    url: "about:blank",
    properties: ["display", "background-color"],
  });

  assert.deepEqual(answer, {
    url: "about:blank",
    styles: { display: "block", "background-color": "rgb(1, 2, 3)" },
  });
  assert.ok(calls.some(([method]) => method === "Runtime.releaseObject"));
});

test("computed style rechecks its snapshotted URL inside rendered work", async () => {
  const { verbs, first, view } = harness();
  view.url = "https://evil.example/";
  let commands = 0;
  view.webContents.debugger.sendCommand = async () => { commands += 1; };

  await assert.rejects(
    verbs.browser_computed_style({
      id: first,
      ref: 7,
      url: "https://safe.example/",
      properties: ["display"],
    }),
    /not the https:\/\/safe\.example\/ page you snapshotted/,
  );
  assert.equal(commands, 0, "the stale URL reached DOM.resolveNode");
});

test("computed style rejects non-whitelisted properties before touching the page", async () => {
  const { verbs, first, view } = harness();
  let commands = 0;
  view.webContents.debugger.sendCommand = async () => { commands += 1; };

  await assert.rejects(
    verbs.browser_computed_style({ id: first, ref: 7, url: "about:blank", properties: ["content"] }),
    /not a supported computed-style property/,
  );
  assert.equal(commands, 0);
});


test("typing a static snapshot ref stops before text insertion", async () => {
  const { verbs, first, view } = harness();
  const methods = [];
  view.webContents.debugger.sendCommand = async (method, params) => {
    methods.push(method);
    if (method === "DOM.enable") return {};
    if (method === "DOM.resolveNode") return { object: { objectId: "node-8" } };
    if (method === "Runtime.callFunctionOn") {
      assert.match(params.functionDeclaration, /isContentEditable/);
      return { exceptionDetails: { text: "this element does not accept text" } };
    }
    if (method === "Runtime.releaseObject") return {};
    throw new Error(`unexpected CDP command: ${method}`);
  };

  await assert.rejects(
    verbs.browser_type({ id: first, ref: 8, host: "", text: "must not leak", submit: false }),
    /does not accept text/,
  );
  assert.ok(!methods.includes("Input.insertText"));
});


test("a queued click rechecks its host at execution time", async () => {
  const { verbs, first, view } = harness(100);
  view.url = "https://safe.example/";
  const methods = [];
  let releaseSnapshot;
  view.webContents.debugger.sendCommand = async (method) => {
    methods.push(method);
    if (method === "Accessibility.enable") {
      return new Promise((resolve) => { releaseSnapshot = resolve; });
    }
    if (method === "Accessibility.getFullAXTree") return { nodes: [{ nodeId: "1" }] };
    throw new Error(`unexpected CDP command: ${method}`);
  };

  const snapshot = verbs.browser_snapshot({ id: first });
  while (!releaseSnapshot) await new Promise((resolve) => setTimeout(resolve, 0));
  const click = verbs.browser_click({ id: first, ref: 7, host: "safe.example" });
  view.url = "https://evil.example/";
  releaseSnapshot({});

  await snapshot;
  await assert.rejects(click, /not safe\.example/);
  assert.ok(!methods.includes("Input.dispatchMouseEvent"));
});


test("a background navigation renders below the app and restores the selected tab", async () => {
  const { verbs, view: selected, appView, children } = harness();
  verbs.browser_visible({ visible: true });
  const backgroundId = verbs.browser_open({ select: false });
  const background = FakeWebContentsView.instances[1];
  let paints = 0;
  background.webContents.capturePage = async () => {
    paints += 1;
    return { isEmpty: () => false };
  };
  background.operations.length = 0;

  await verbs.browser_navigate({ id: backgroundId, url: "http://127.0.0.1:5174/" });

  assert.equal(background.webContents.getURL(), "http://127.0.0.1:5174/");
  assert.equal(paints, 1, "the loaded document needs a post-navigation frame");
  assert.deepEqual(children, [background, appView, selected]);
  assert.deepEqual(background.operations.filter(([kind]) => kind === "visible"), [
    ["visible", true],
    ["visible", false],
  ]);
  assert.deepEqual(selected.operations.at(-2), ["visible", true]);
});

test("a failed background paint reports the concrete tab state and restores visibility", async () => {
  const { verbs } = harness();
  const backgroundId = verbs.browser_open({ select: false });
  const background = FakeWebContentsView.instances[1];
  background.webContents.capturePage = async () => {
    throw new Error("renderer unavailable");
  };
  background.operations.length = 0;

  await assert.rejects(
    verbs.browser_navigate({ id: backgroundId, url: "https://example.com/" }),
    (error) => {
      assert.match(error.message, /url=https:\/\/example\.com\//);
      assert.match(error.message, /bounds=800x600@0,0/);
      assert.match(error.message, /current=/);
      assert.match(error.message, /paneVisible=false/);
      assert.match(error.message, /renderer unavailable/);
      return true;
    },
  );

  assert.deepEqual(background.operations.filter(([kind]) => kind === "visible"), [
    ["visible", true],
    ["visible", false],
  ]);
});


test("a transient Viz failure is retried within the render budget", async () => {
  const { verbs } = harness(200);
  const backgroundId = verbs.browser_open({ select: false });
  const background = FakeWebContentsView.instances[1];
  let attempts = 0;
  background.webContents.capturePage = async () => {
    attempts += 1;
    if (attempts === 1) throw new Error("UnknownVizError");
    return { isEmpty: () => false };
  };

  await verbs.browser_navigate({ id: backgroundId, url: "https://example.com/" });

  assert.equal(attempts, 2);
});


test("a no-pane browser screenshot restores the cover above the agent tab", async () => {
  const { verbs, first, appView, children } = harness();
  verbs.browser_close({ id: first });
  const id = verbs.browser_open({ select: false });
  const target = FakeWebContentsView.instances[1];

  await verbs.browser_navigate({ id, url: "https://example.com/" });

  assert.deepEqual(children, [target, appView]);
  assert.deepEqual(target.operations.filter(([kind]) => kind === "visible").slice(-2), [
    ["visible", true],
    ["visible", false],
  ]);
});


test("a timed-out initial frame does not queue another capture", async () => {
  const { verbs } = harness(20);
  const backgroundId = verbs.browser_open({ select: false });
  const background = FakeWebContentsView.instances[1];
  let captures = 0;
  background.webContents.capturePage = () => {
    captures += 1;
    return new Promise(() => {});
  };

  background.operations.length = 0;
  await assert.rejects(
    verbs.browser_navigate({ id: backgroundId, url: "https://example.com/" }),
    /browser (?:initial|result) frame did not answer within/,
  );
  assert.equal(captures, 1, "a hung capture must not accumulate retry calls");
  assert.deepEqual(background.operations.filter(([kind]) => kind === "visible"), [
    ["visible", true],
    ["visible", false],
  ]);
});


test("navigation publishes a resized transient Browser thumbnail", async () => {
  const { verbs, first, events } = harness(100, 0);

  await verbs.browser_navigate({ id: first, url: "https://example.com/report" });
  await waitFor(() => events.some((event) => event.name === BROWSER_THUMBNAIL));

  const thumbnails = events.filter((event) => event.name === BROWSER_THUMBNAIL);
  assert.equal(thumbnails.length, 1);
  assert.deepEqual(thumbnails[0].payload, {
    id: first,
    url: "https://example.com/report",
    data: Buffer.from("image-720x540").toString("base64"),
    width: 720,
    height: 540,
    revision: 1,
  });
});

test("thumbnail requests coalesce per tab and retain the newest revision", async () => {
  const { verbs, first, view, events } = harness(100, 0);
  let captures = 0;
  view.webContents.capturePage = async () => {
    captures += 1;
    return fakeImage();
  };

  verbs.browser_select({ id: first });
  verbs.browser_select({ id: first });
  await waitFor(() => events.some((event) => event.name === BROWSER_THUMBNAIL));

  const thumbnails = events.filter((event) => event.name === BROWSER_THUMBNAIL);
  assert.equal(captures, 2, "one readiness probe and one thumbnail capture");
  assert.equal(thumbnails.length, 1);
  assert.equal(thumbnails[0].payload.revision, 2);
});

test("an in-flight thumbnail is discarded when its exact tab closes", async () => {
  const { verbs, first, view, events } = harness(100, 0);
  let finishCapture;
  view.webContents.capturePage = () => new Promise((resolve) => { finishCapture = resolve; });

  verbs.browser_select({ id: first });
  while (!finishCapture) await new Promise((resolve) => setTimeout(resolve, 0));
  verbs.browser_close({ id: first });
  finishCapture(fakeImage());
  await new Promise((resolve) => setTimeout(resolve, 10));

  assert.equal(events.filter((event) => event.name === BROWSER_THUMBNAIL).length, 0);
});

// ---- Downloads ----------------------------------------------------------

test("a download is saved under the downloads directory and reported to its tab", async () => {
  const { verbs, first, view, partition, downloadsDir, events } = harness();
  const item = fakeDownloadItem({ filename: "report.pdf", total: 2048 });

  partition.willDownload({}, item, view.webContents);
  assert.equal(path.dirname(item.savePath), downloadsDir);
  assert.equal(path.basename(item.savePath), "report.pdf");

  item.progress(1024);
  item.finish("completed", 2048);

  const report = await verbs.browser_download({ id: first });
  assert.equal(report.state, "completed");
  assert.equal(report.path, item.savePath);
  assert.equal(report.filename, "report.pdf");
  assert.equal(report.tabId, first);
  assert.equal(report.receivedBytes, 2048);
  assert.ok(events.some((event) => event.name === BROWSER_DOWNLOAD));
});

test("a server filename cannot escape the downloads directory", () => {
  const { view, partition, downloadsDir } = harness();
  const item = fakeDownloadItem({ filename: "../../etc/passwd" });

  partition.willDownload({}, item, view.webContents);
  assert.equal(path.dirname(item.savePath), downloadsDir);
  assert.equal(path.basename(item.savePath), "passwd");
});

test("a second download with a taken name does not overwrite the first", () => {
  const { view, partition } = harness();
  const first = fakeDownloadItem({ filename: "report.pdf" });
  partition.willDownload({}, first, view.webContents);
  // The real DownloadItem would have written the file; the fake does not, so
  // create it to prove uniquePath steps around what is already on disk.
  fs.writeFileSync(first.savePath, "one");

  const second = fakeDownloadItem({ filename: "report.pdf" });
  partition.willDownload({}, second, view.webContents);

  assert.notEqual(second.savePath, first.savePath);
  assert.equal(path.basename(second.savePath), "report (1).pdf");
});

test("browser_download reports none when the tab has saved nothing", async () => {
  const { verbs, first } = harness();
  const report = await verbs.browser_download({ id: first });
  assert.deepEqual(report, { none: true });
});

test("browser_download returns pending progress when a download is still running", async () => {
  const { verbs, first, view, partition } = harness();
  const item = fakeDownloadItem({ filename: "big.zip", total: 4096 });
  partition.willDownload({}, item, view.webContents);
  item.progress(1024);

  const report = await verbs.browser_download({ id: first, timeoutMs: 500 });
  assert.equal(report.pending, true);
  assert.equal(report.receivedBytes, 1024);
  assert.equal(report.state, "progressing");
});

test("open and reveal are confined to the downloads directory", async () => {
  const { verbs, view, partition, shellCalls } = harness();
  const item = fakeDownloadItem({ filename: "report.pdf" });
  partition.willDownload({}, item, view.webContents);

  verbs.browser_download_reveal({ path: item.savePath });
  assert.deepEqual(shellCalls.at(-1), ["reveal", path.resolve(item.savePath)]);

  await assert.rejects(
    () => verbs.browser_download_open({ path: "/etc/passwd" }),
    /not in the downloads directory/,
  );
});

test("removing a download drops the record and can leave the file on disk", () => {
  const { verbs, view, partition, events } = harness();
  const item = fakeDownloadItem({ filename: "keep.pdf" });
  partition.willDownload({}, item, view.webContents);
  item.finish("completed", 100);
  fs.writeFileSync(item.savePath, "bytes");
  const { id } = events.at(-1).payload.downloads.at(-1);

  verbs.browser_download_remove({ id, deleteFile: false });

  assert.equal(fs.existsSync(item.savePath), true, "record-only removal keeps the file");
  assert.deepEqual(events.at(-1).payload.downloads, [], "the record is gone");
});

test("removing a download with deleteFile also unlinks it", () => {
  const { verbs, view, partition, events } = harness();
  const item = fakeDownloadItem({ filename: "gone.pdf" });
  partition.willDownload({}, item, view.webContents);
  item.finish("completed", 100);
  fs.writeFileSync(item.savePath, "bytes");
  const { id } = events.at(-1).payload.downloads.at(-1);

  verbs.browser_download_remove({ id, deleteFile: true });

  assert.equal(fs.existsSync(item.savePath), false, "the file was deleted");
  assert.deepEqual(events.at(-1).payload.downloads, []);
});

test("removing a running download cancels the transfer", () => {
  const { verbs, view, partition, events } = harness();
  let cancelled = false;
  const item = fakeDownloadItem({ filename: "big.zip", total: 4096 });
  item.cancel = () => {
    cancelled = true;
  };
  partition.willDownload({}, item, view.webContents);
  item.progress(1024);
  const { id } = events.at(-1).payload.downloads.at(-1);

  verbs.browser_download_remove({ id, deleteFile: false });

  assert.equal(cancelled, true, "an in-flight transfer is stopped, not left record-less");
  assert.deepEqual(events.at(-1).payload.downloads, []);
});

test("clearing empties the shelf, optionally deleting the files", () => {
  const { verbs, view, partition, events } = harness();
  const a = fakeDownloadItem({ filename: "a.pdf" });
  const b = fakeDownloadItem({ filename: "b.pdf" });
  partition.willDownload({}, a, view.webContents);
  a.finish("completed", 100);
  fs.writeFileSync(a.savePath, "a");
  partition.willDownload({}, b, view.webContents);
  b.finish("completed", 100);
  fs.writeFileSync(b.savePath, "b");

  verbs.browser_download_clear({ deleteFile: true });

  assert.equal(fs.existsSync(a.savePath), false);
  assert.equal(fs.existsSync(b.savePath), false);
  assert.deepEqual(events.at(-1).payload.downloads, []);
});
