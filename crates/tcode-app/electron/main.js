// The Electron shell: a window, a pipe, and nothing else.
//
// The rule this file exists under (`../AGENTS.md`): **no business logic here.**
// If something in this file starts deciding which session to open, validating
// a path, or shaping a menu, that code belongs in Rust and this process should
// be forwarding a command instead. The only state it is allowed to own is
// state that genuinely lives in Electron — the window, and the
// tab-to-`WebContentsView` map the browser pane needs.
//
// Everything the app can ask for arrives as one method name and one argument
// object. A handful are about this window and are answered here; the rest go
// down the pipe to `tcode-sidecar`, which is the Rust backend.

const { spawn } = require("node:child_process");
const path = require("node:path");
const readline = require("node:readline");
const { pathToFileURL } = require("node:url");

const { app, BaseWindow, WebContentsView, dialog, ipcMain, net, protocol } =
  require("electron");

const { browserVerbs } = require("./browser");
const { resolveSidecarPath, sidecarMissingMessage } = require("./paths");
const { startAutomaticUpdates } = require("./updater");

const ROOT = path.resolve(__dirname, "..");
const DIST = path.join(ROOT, "ui", "dist");

// The app's identity outside the window itself. Without `app.setName` the
// process is named after the npm package (`tcode-shell`), which is not the
// product name; and on Windows the taskbar groups and draws by the
// Application User Model ID, which otherwise defaults to the executable and
// leaves a dev run grouped under "Electron". The `.ico` is the Windows form
// of the mark; the `.png` is the fallback every other platform draws from
// the window `icon` option.
app.setName("tcode");
if (process.platform === "win32") app.setAppUserModelId("com.tcode.app");

const APP_ICON = path.join(
  ROOT,
  "icons",
  process.platform === "win32" ? "icon.ico" : "icon.png",
);

/** Mirrors `bridge::WINDOW_STATE`, which mirrors `ui/src/types.ts`. */
const WINDOW_STATE = "tcode://window-state";

// The app's one Content-Security-Policy. Rule 11: `script-src` falls back to
// `default-src 'self'` and must never gain `unsafe-inline` or `unsafe-eval` —
// that is the layer stopping KaTeX's and the markdown renderer's output from
// executing. `frame-src` names the loopback origin `serve.rs` binds, which is
// the *other* frame in rule 11b and deliberately not this one.
const CSP =
  "default-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; " +
  "frame-src 'self' http://127.0.0.1:*";

// Enough to serve `ui/dist` correctly. A module script with the wrong type does
// not load at all, so guessing is not an option — and `net.fetch` on a `file:`
// URL is not a contract this file wants to depend on for that.
const MIME = {
  ".html": "text/html",
  ".js": "text/javascript",
  ".mjs": "text/javascript",
  ".css": "text/css",
  ".json": "application/json",
  ".svg": "image/svg+xml",
  ".png": "image/png",
  ".jpg": "image/jpeg",
  ".webp": "image/webp",
  ".ico": "image/x-icon",
  ".woff": "font/woff",
  ".woff2": "font/woff2",
  ".ttf": "font/ttf",
  ".wasm": "application/wasm",
  ".map": "application/json",
};

// A real origin, not `file:`. The app is `app://tcode` and a shown artifact is
// `http://127.0.0.1:<port>`; they are different origins, and that — not an
// attribute and not a parser — is what keeps a generated report away from the
// app's IPC (rule 11b, and the long comment in `ui/src/Framed.tsx`). Under
// `file:` there is no origin to be on the other side of.
protocol.registerSchemesAsPrivileged([
  {
    scheme: "app",
    privileges: { standard: true, secure: true, supportFetchAPI: true },
  },
]);

// ---------------------------------------------------------------- the sidecar

/** Resolved once, so a missing build is one clear message and not fifty. */
function sidecarPath() {
  return resolveSidecarPath({
    isPackaged: app.isPackaged,
    resourcesPath: process.resourcesPath,
    root: ROOT,
  });
}

function missingSidecarMessage() {
  return sidecarMissingMessage({
    isPackaged: app.isPackaged,
    resourcesPath: process.resourcesPath,
    root: ROOT,
  });
}

/**
 * The backend, as a child process.
 *
 * `deliver` is called with every event frame; the pending map correlates
 * replies by id. Requests are answered out of order on purpose — the sidecar
 * runs each command as its own task, so a folder listing that reads a hundred
 * logs does not hold up the pane next to it.
 *
 * `answer` is the other direction: the sidecar asking *this* process for
 * something only it can do — a native view, and eventually that view's debugger
 * (`../AGENT-BROWSER.md`). It is given a method name and an argument object and
 * returns the result; throwing is how it says no. Those frames carry `call`
 * rather than `id` because the two directions number their requests
 * independently and would otherwise collide.
 */
function startSidecar(deliver, answer) {
  const binary = sidecarPath();
  if (!binary) {
    return {
      failed: missingSidecarMessage(),
    };
  }

  // The working directory *is* the folder to open. Which folder that is stays
  // the shell's decision, and this is the whole of it — no path handling here,
  // because path handling is the backend's.
  const cwd = process.env.TCODE_CWD || process.cwd();
  const child = spawn(binary, [], { cwd, stdio: ["pipe", "pipe", "pipe"] });

  const pending = new Map();
  let nextId = 1;
  /** Set once the process is gone; every later call fails with this. */
  let gone = null;

  readline.createInterface({ input: child.stdout }).on("line", (line) => {
    if (!line.trim()) return;
    let frame;
    try {
      frame = JSON.parse(line);
    } catch (error) {
      // Not recoverable and not silent: stdout is reserved for frames
      // (`sidecar.rs`), so anything unparseable means something printed into
      // the middle of one.
      console.error("tcode: unreadable frame from the sidecar:", error, line);
      return;
    }
    if (frame.event !== undefined) return deliver(frame.event, frame.payload);
    if (frame.call !== undefined) return serveCall(frame);
    const waiting = pending.get(frame.id);
    if (!waiting) return;
    pending.delete(frame.id);
    waiting(frame.error !== undefined ? { error: frame.error } : { ok: frame.ok });
  });

  // The sidecar's diagnostics are this process's diagnostics. Dropping them
  // would make a backend that failed to boot look like a backend that is slow.
  readline.createInterface({ input: child.stderr }).on("line", (line) => {
    console.error(line);
  });

  /**
   * Answer one `call` frame.
   *
   * Always replies, including when `answer` throws or the process is already
   * gone: the sidecar's own timeout would eventually free the caller, but a
   * timeout says "the shell is not responding" for what is really "that verb
   * does not exist" — and rule 3's whole point is that a wrong answer costs
   * more than a slow one.
   */
  async function serveCall(frame) {
    let reply;
    try {
      reply = { call: frame.call, ok: (await answer(frame.method, frame.args)) ?? null };
    } catch (error) {
      reply = { call: frame.call, error: String(error?.message ?? error) };
    }
    if (gone) return;
    child.stdin.write(`${JSON.stringify(reply)}\n`);
  }

  const die = (why) => {
    if (gone) return;
    gone = why;
    // Rule 7 in the frontend says no promise rejects into nothing; that holds
    // only if this side actually rejects them. A pending `invoke` whose backend
    // died would otherwise hang forever, which reads on screen as a turn that
    // never finishes.
    for (const waiting of pending.values()) waiting({ error: why });
    pending.clear();
  };
  child.on("error", (error) => die(`the backend could not start: ${error.message}`));
  child.on("exit", (code, signal) =>
    die(
      `the backend stopped (${signal ? `signal ${signal}` : `exit code ${code}`}). ` +
        "Nothing in this window will respond until tcode is restarted.",
    ),
  );

  return {
    call(method, args) {
      if (gone) return Promise.resolve({ error: gone });
      const id = nextId++;
      return new Promise((resolve) => {
        pending.set(id, resolve);
        child.stdin.write(`${JSON.stringify({ id, method, args })}\n`);
      });
    },
    // Closing stdin is what ends the sidecar's read loop, and that loop's exit
    // is where it closes the terminals it owns (rule 9i: those are real child
    // processes and must not outlive the window).
    stop() {
      child.stdin.end();
    },
  };
}

// ------------------------------------------------------------------ the window

/** The verbs a window answers for itself. The backend has no window and must
 *  not grow the concept — see `main.rs::register_shell` for the same table on
 *  the other shell. */
function windowVerbs(window) {
  return {
    window_minimize: () => (window.minimize(), null),
    window_close: () => (window.close(), null),
    window_is_maximized: () => window.isMaximized(),
    window_toggle_maximize: () => {
      if (window.isMaximized()) window.unmaximize();
      else window.maximize();
      return null;
    },
    async dialog_open_folder() {
      const chosen = await dialog.showOpenDialog(window, {
        properties: ["openDirectory"],
      });
      // Cancelling is an answer, not a failure.
      return chosen.canceled ? null : (chosen.filePaths[0] ?? null);
    },
  };
}

/** Serve `ui/dist` under `app://tcode`, with the app's CSP on every response. */
function serveBundle() {
  protocol.handle("app", async (request) => {
    const { pathname } = new URL(request.url);
    const relative = pathname === "/" ? "index.html" : decodeURIComponent(pathname.slice(1));
    const file = path.join(DIST, relative);
    // A request that escapes the bundle is refused rather than resolved. There
    // is no untrusted author of these URLs today, and that is exactly the kind
    // of assumption that stops being true quietly.
    if (!file.startsWith(DIST + path.sep) && file !== DIST) {
      return new Response("not found", { status: 404 });
    }

    const response = await net.fetch(pathToFileURL(file).toString());
    const headers = new Headers(response.headers);
    // The CSP is set at the source rather than through a `webRequest` filter:
    // this is not an HTTP request, and the header that governs the document is
    // worth being able to read in the same function that produces the document.
    headers.set("Content-Security-Policy", CSP);
    const mime = MIME[path.extname(file).toLowerCase()];
    if (mime) headers.set("Content-Type", mime);
    return new Response(response.body, { status: response.status, headers });
  });
}

// The classic no-flash startup is `show: false` plus `window.show()` once the
// page has painted. That pattern stalls here: with a hidden window the view's
// first compositor frame never completes in software-rendering environments
// (no GPU / VM / RDP), so neither `ready-to-show` nor `did-finish-load`
// fires and the window never appears at all. On platforms with `setOpacity`
// the window is instead created visible but transparent and faded in on load —
// the same no-flash effect, while the compositor runs from the first frame.
// Linux has no `setOpacity`, so it keeps the hidden-until-ready pattern.
const FADE_IN = process.platform === "win32" || process.platform === "darwin";

function createWindow() {
  // `BaseWindow` with an explicit child view rather than `BrowserWindow`: the
  // browser pane puts sibling `WebContentsView`s in this same container, and
  // having the app itself be one of them means there is one kind of thing in
  // the window instead of two. It also fixes the stacking for free — a view
  // added later composites above, and the browser is always added later.
  const window = new BaseWindow({
    // A `BaseWindow` does not take the document's `<title>` the way a
    // `BrowserWindow` does — its contents are a child view, not the window —
    // so this is the name in the taskbar and in alt-tab, and without it the
    // window is called after the npm package.
    title: "tcode",
    // The mark in the title bar and taskbar. Without it the window draws the
    // default Electron icon everywhere a window icon is asked for.
    icon: APP_ICON,
    width: 1280,
    height: 860,
    minWidth: 760,
    minHeight: 520,
    backgroundColor: "#ffffff",
    // The title bar is the app's own (rule 9c). `main.tsx` puts the matching
    // drag surface on it; without `frame: false` there would be two.
    frame: false,
    show: !FADE_IN,
  });
  // See `FADE_IN`: the window must be compositor-visible from the start, and
  // transparent until the first page paint hides the white background.
  if (FADE_IN) window.setOpacity(0);

  const view = new WebContentsView({
    webPreferences: {
      preload: path.join(__dirname, "preload.js"),
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: true,
    },
  });
  window.contentView.addChildView(view);

  // `BaseWindow` does not resize its children, and the app is the whole window.
  const fill = () => {
    const { width, height } = window.getContentBounds();
    view.setBounds({ x: 0, y: 0, width, height });
  };
  fill();
  window.on("resize", fill);

  // The app document is never navigated and never opens a window. Both are
  // defaults Tauri had and Electron does not: without them a stray `<a href>`
  // that escaped the frontend's own handling replaces the entire app with a
  // page of someone else's choosing, and `target="_blank"` opens a frameless
  // Electron window with this preload in it.
  view.webContents.on("will-navigate", (event) => event.preventDefault());
  view.webContents.setWindowOpenHandler(() => ({ action: "deny" }));

  // With no devtools open, a document that failed to load and a document that
  // rendered nothing look identical: a window the colour of the background.
  // These two are the only things that can say which — and there is no Tauri
  // here to have opened an inspector for you.
  view.webContents.on("did-fail-load", (_event, code, description, url) => {
    console.error(`tcode: the app failed to load ${url}: ${description} (${code})`);
  });
  view.webContents.on("console-message", (event) => {
    if (event.level === "error") console.error(`tcode: renderer: ${event.message}`);
  });

  // Everything this process sends the app renderer goes through one function,
  // so "an event" means the same thing whether the sidecar produced it or the
  // window did.
  const emit = (name, payload) => view.webContents.send("tcode:event", name, payload);

  // The window can be maximized or restored without a button in the title bar
  // being pressed — a snap gesture, a double-click on the bar, the OS.
  const report = () => emit(WINDOW_STATE, { maximized: window.isMaximized() });
  window.on("maximize", report);
  window.on("unmaximize", report);

  return { window, view, emit };
}

// ------------------------------------------------------------------- assembly

app.whenReady().then(() => {
  serveBundle();

  const { window, view, emit } = createWindow();

  // Declared before the sidecar starts and filled in just below: the table
  // needs `sidecar.call` (for `resolveUrl`) and the sidecar needs the table
  // (to answer its `call` frames). One of the two has to be late-bound, and a
  // closure reading a `let` is the whole of it — the first frame cannot arrive
  // before this function returns.
  let verbs = {};
  const sidecar = startSidecar(emit, async (method, args) => {
    const own = verbs[method];
    // Deliberately the same table the renderer's `invoke` reaches, not a
    // second one. A shell-owned verb is a thing only this process can do; who
    // asked does not change what it is. Narrowing it needs a reason, and today
    // there is none — the renderer is if anything the *less* trusted caller of
    // the two, since it renders model output.
    if (!own) throw new Error(`unknown shell verb '${method}'`);
    return own(args);
  });

  if (sidecar.failed) {
    // Before the window is worth showing: an app whose backend never started
    // has nothing to draw, and a blank window with a console message is the
    // failure mode the frontend's own fault screen exists to prevent.
    dialog.showErrorBox("tcode could not start", sidecar.failed);
    app.quit();
    return;
  }

  // What this process answers for itself: the window it owns, and the native
  // views the browser pane is made of. Everything else is somebody else's.
  verbs = {
    ...windowVerbs(window),
    ...browserVerbs({
      window,
      appView: view,
      emit,
      // The browser asks the backend what a typed address means rather than
      // deciding here — one implementation of that guesswork, with its tests
      // (`crate::address`).
      async resolveUrl(input) {
        const reply = await sidecar.call("resolve_url", { input });
        if (reply.error !== undefined) throw new Error(reply.error);
        return reply.ok;
      },
    }),
  };

  // The one door, and the only place that decides who answers. A shell verb is
  // handled here; everything else is a method name and an argument object going
  // down a pipe to the sidecar's command registry.
  ipcMain.handle("tcode:invoke", async (_event, method, args) => {
    const own = verbs[method];
    if (!own) return sidecar.call(method, args);
    try {
      return { ok: await own(args) };
    } catch (error) {
      return { error: String(error) };
    }
  });

  view.webContents.loadURL("app://tcode/index.html");

  let shown = false;
  const showAndCheckForUpdates = () => {
    if (shown) return;
    shown = true;
    if (FADE_IN) window.setOpacity(1);
    window.show();
    startAutomaticUpdates({ isPackaged: app.isPackaged, window, dialog });
  };
  window.once("ready-to-show", showAndCheckForUpdates);
  // `BaseWindow` has no `ready-to-show` of its own — that belongs to the
  // contents — so the first paint of the view is the signal.
  view.webContents.once("did-finish-load", showAndCheckForUpdates);
  // A load that neither finishes nor fails must not leave a transparent (or
  // hidden, on Linux) window forever; the page is local, so 10s is generous.
  setTimeout(showAndCheckForUpdates, 10000);

  app.on("before-quit", () => sidecar.stop());
});

app.on("window-all-closed", () => app.quit());
