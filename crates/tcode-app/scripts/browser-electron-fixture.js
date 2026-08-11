const assert = require("node:assert/strict");
const http = require("node:http");
const { app, BaseWindow, WebContentsView } = require("electron");

const { browserVerbs, BROWSER_THUMBNAIL } = require("../electron/browser");

const PAGE = `<!doctype html>
<meta charset="utf-8">
<title>Browser compositor fixture</title>
<style>
  body { margin: 0; font-family: sans-serif; background: white; }
  main { padding: 32px; }
  canvas { display: block; width: 320px; height: 180px; }
  #open { display: inline-block; background-color: rgb(12, 34, 56); border: 3px solid rgb(78, 90, 123); }
  [role="dialog"] { position: fixed; inset: 80px; background: white; border: 2px solid black; padding: 24px; }
</style>
<main>
  <h1>Healthy loopback page</h1>
  <canvas id="chart" width="320" height="180" aria-label="Canvas chart"></canvas>
  <button id="open">Open portal</button>
</main>
<script>
  const chart = document.querySelector("#chart");
  const draw = (color) => {
    const context = chart.getContext("2d");
    context.fillStyle = color;
    context.fillRect(0, 0, chart.width, chart.height);
    context.fillStyle = "white";
    context.font = "24px sans-serif";
    context.fillText("rendered canvas", 58, 98);
  };
  requestAnimationFrame(() => draw("#2563eb"));
  window.narrowViewportSeen = matchMedia("(max-width: 700px)").matches;
  addEventListener("resize", () => {
    window.narrowViewportSeen ||= matchMedia("(max-width: 700px)").matches;
  });
  document.querySelector("#open").addEventListener("click", () => {
    draw("#15803d");
    const portal = document.createElement("div");
    portal.setAttribute("role", "dialog");
    portal.setAttribute("aria-label", "Browser portal");
    portal.textContent = "Portal content is visible";
    document.body.append(portal);
  });
</script>`;

const listen = (server) => new Promise((resolve, reject) => {
  server.once("error", reject);
  server.listen(0, "127.0.0.1", resolve);
});

const closeServer = (server) => new Promise((resolve) => server.close(resolve));

function namedNode(nodes, role, name) {
  return nodes.find((node) => node.role?.value === role && node.name?.value === name);
}

app.whenReady().then(async () => {
  const server = http.createServer((_request, response) => {
    response.writeHead(200, { "content-type": "text/html; charset=utf-8", "cache-control": "no-store" });
    response.end(PAGE);
  });
  let window;
  let appView;
  const browserViews = [];
  const thumbnails = new Map();
  const thumbnailWaiters = new Set();
  const emit = (event, payload) => {
    if (event !== BROWSER_THUMBNAIL) return;
    thumbnails.set(payload.id, payload);
    for (const waiter of thumbnailWaiters) waiter(payload);
  };
  const waitForThumbnail = (id, afterRevision = 0) => {
    const current = thumbnails.get(id);
    if (current?.revision > afterRevision) return Promise.resolve(current);
    return new Promise((resolve, reject) => {
      const timeout = setTimeout(() => {
        thumbnailWaiters.delete(onThumbnail);
        reject(new Error(`timed out waiting for browser thumbnail ${id}`));
      }, 5000);
      const onThumbnail = (thumbnail) => {
        if (thumbnail.id !== id || thumbnail.revision <= afterRevision) return;
        clearTimeout(timeout);
        thumbnailWaiters.delete(onThumbnail);
        resolve(thumbnail);
      };
      thumbnailWaiters.add(onThumbnail);
    });
  };

  try {
    await listen(server);
    const address = server.address();
    const url = `http://127.0.0.1:${address.port}/`;

    window = new BaseWindow({ width: 1024, height: 768, show: true });
    appView = new WebContentsView();
    appView.setBounds({ x: 0, y: 0, width: 1000, height: 730 });
    window.contentView.addChildView(appView);
    await appView.webContents.loadURL(
      "data:text/html,<body style='margin:0;background:white'>app cover</body>",
    );

    const verbs = browserVerbs({
      window,
      appView,
      emit,
      resolveUrl: async (input) => input,
    });
    const rect = { x: 0, y: 0, width: 1000, height: 730 };

    const selectedId = verbs.browser_open({ rect, select: true });
    browserViews.push(window.contentView.children.at(-1));
    verbs.browser_show({ rect });
    await verbs.browser_navigate({ id: selectedId, url: "data:text/html,<body>selected tab</body>" });
    const selectedView = browserViews[0];

    const backgroundId = verbs.browser_open({ select: false, agent: true });
    browserViews.push(window.contentView.children.at(-1));
    const backgroundView = browserViews[1];
    assert.equal(backgroundView.getVisible(), false, "the agent tab must start hidden");

    await verbs.browser_navigate({ id: backgroundId, url });
    const firstTree = await verbs.browser_snapshot({ id: backgroundId });
    assert.equal(firstTree.url, url);
    assert.ok(firstTree.nodes.length > 0, "the healthy page needs a non-empty AX tree");
    const button = namedNode(firstTree.nodes, "button", "Open portal");
    assert.ok(button?.backendDOMNodeId, "the fixture button must be actionable");
    const style = await verbs.browser_computed_style({
      id: backgroundId,
      ref: button.backendDOMNodeId,
      url,
      properties: ["display", "background-color", "border-style"],
    });
    assert.equal(style.url, url);
    assert.deepEqual(style.styles, {
      display: "inline-block",
      "background-color": "rgb(12, 34, 56)",
      "border-style": "solid",
    });

    const responsive = await verbs.browser_screenshot({
      id: backgroundId,
      viewport: { width: 640, height: 480 },
    });
    assert.ok(responsive.data.length > 1000, "the responsive screenshot needs real pixels");
    assert.equal(
      await backgroundView.webContents.executeJavaScript("window.narrowViewportSeen"),
      true,
      "the page must lay out at the requested temporary viewport",
    );
    assert.deepEqual(backgroundView.getBounds(), rect, "the responsive probe must restore bounds");

    const before = await verbs.browser_screenshot({ id: backgroundId });
    assert.ok(before.data.length > 1000, "the canvas page needs a real screenshot");
    assert.ok(before.width > 0 && before.height > 0);
    const beforeThumbnail = await waitForThumbnail(backgroundId);

    await verbs.browser_click({
      id: backgroundId,
      host: "127.0.0.1",
      ref: button.backendDOMNodeId,
    });
    const secondTree = await verbs.browser_snapshot({ id: backgroundId });
    assert.ok(
      namedNode(secondTree.nodes, "dialog", "Browser portal"),
      "the click-created portal must commit while the tab is under the app",
    );
    const after = await verbs.browser_screenshot({ id: backgroundId });
    assert.notEqual(after.data, before.data, "the visual update must reach capturePage");
    const afterThumbnail = await waitForThumbnail(backgroundId, beforeThumbnail.revision);
    assert.notEqual(
      afterThumbnail.data,
      beforeThumbnail.data,
      "the click-created portal must reach the transient thumbnail",
    );

    assert.equal(backgroundView.getVisible(), false, "the agent tab must return to background");
    assert.equal(selectedView.getVisible(), true, "the user's selected tab must remain visible");
    assert.deepEqual(
      window.contentView.children,
      [backgroundView, appView, selectedView],
      "background rendering must restore the app and selected browser view above it",
    );

    verbs.browser_close({ id: backgroundId });
    verbs.browser_close({ id: selectedId });
    console.log("browser compositor regression passed");
  } catch (error) {
    console.error(error.stack || error);
    process.exitCode = 1;
  } finally {
    for (const view of browserViews) {
      if (!view.webContents.isDestroyed()) view.webContents.close();
    }
    if (appView && !appView.webContents.isDestroyed()) appView.webContents.close();
    if (window && !window.isDestroyed()) window.destroy();
    await closeServer(server);
    app.exit(process.exitCode || 0);
  }
});
