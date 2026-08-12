const assert = require("node:assert/strict");
const { EventEmitter } = require("node:events");
const test = require("node:test");

const { startAutomaticUpdates, supportsAutomaticUpdates } = require("./updater");

function fakeUpdater() {
  const autoUpdater = new EventEmitter();
  autoUpdater.checks = 0;
  autoUpdater.installs = 0;
  autoUpdater.checkForUpdates = () => {
    autoUpdater.checks += 1;
    return null;
  };
  autoUpdater.quitAndInstall = () => {
    autoUpdater.installs += 1;
  };
  return autoUpdater;
}

function collector() {
  const events = [];
  return {
    emit(name, payload) { events.push({ name, payload }); },
    events,
  };
}

test("does not load the updater for development or macOS", () => {
  for (const [isPackaged, platform] of [
    [false, "win32"],
    [true, "darwin"],
  ]) {
    assert.equal(
      startAutomaticUpdates({
        isPackaged,
        platform,
        emit() {},
        loadUpdater() {
          throw new Error("the updater must not load");
        },
      }),
      null,
    );
  }
});

test("auto-downloads and emits progress events", () => {
  const autoUpdater = fakeUpdater();
  const { emit, events } = collector();
  const handle = startAutomaticUpdates({
    isPackaged: true,
    platform: "win32",
    emit,
    logger: { error() {} },
    loadUpdater() { return { autoUpdater }; },
  });

  assert.notEqual(handle, null);
  assert.equal(autoUpdater.autoDownload, true);
  assert.equal(autoUpdater.autoInstallOnAppQuit, true);
  assert.equal(autoUpdater.checks, 1);

  autoUpdater.emit("update-available", { version: "0.1.23" });
  assert.deepEqual(events.at(-1).payload, {
    state: "downloading",
    version: "0.1.23",
    percent: 0,
  });

  autoUpdater.emit("download-progress", { percent: 42.7 });
  assert.deepEqual(events.at(-1).payload, {
    state: "downloading",
    version: "0.1.23",
    percent: 43,
  });

  autoUpdater.emit("update-downloaded", { version: "0.1.23" });
  assert.deepEqual(events.at(-1).payload, {
    state: "ready",
    version: "0.1.23",
  });
});

test("quitAndInstall triggers the updater", () => {
  const autoUpdater = fakeUpdater();
  const handle = startAutomaticUpdates({
    isPackaged: true,
    platform: "linux",
    emit() {},
    logger: { error() {} },
    loadUpdater() { return { autoUpdater }; },
  });

  handle.quitAndInstall();
  assert.equal(autoUpdater.installs, 1);
});

test("error clears the update state", () => {
  const autoUpdater = fakeUpdater();
  const { emit, events } = collector();
  startAutomaticUpdates({
    isPackaged: true,
    platform: "win32",
    emit,
    logger: { error() {} },
    loadUpdater() { return { autoUpdater }; },
  });

  autoUpdater.emit("update-available", { version: "0.1.24" });
  assert.equal(events.at(-1).payload.state, "downloading");

  autoUpdater.emit("error", new Error("network down"));
  assert.equal(events.at(-1).payload, null);
});
