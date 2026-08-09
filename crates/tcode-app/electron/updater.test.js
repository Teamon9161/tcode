const assert = require("node:assert/strict");
const { EventEmitter } = require("node:events");
const test = require("node:test");

const { startAutomaticUpdates } = require("./updater");

function fakeUpdater() {
  const autoUpdater = new EventEmitter();
  autoUpdater.checks = 0;
  autoUpdater.downloads = 0;
  autoUpdater.installs = 0;
  autoUpdater.checkForUpdates = () => {
    autoUpdater.checks += 1;
    return null;
  };
  autoUpdater.downloadUpdate = async () => {
    autoUpdater.downloads += 1;
  };
  autoUpdater.quitAndInstall = () => {
    autoUpdater.installs += 1;
  };
  return autoUpdater;
}

function flush() {
  return new Promise((resolve) => setImmediate(resolve));
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
        loadUpdater() {
          throw new Error("the updater must not load");
        },
      }),
      false,
    );
  }
});

test("asks before downloading and before installing a Windows update", async () => {
  const autoUpdater = fakeUpdater();
  const dialogs = [];
  const started = startAutomaticUpdates({
    isPackaged: true,
    platform: "win32",
    window: { id: 1 },
    dialog: {
      async showMessageBox(window, options) {
        dialogs.push({ window, options });
        return { response: 0 };
      },
    },
    logger: { error() {} },
    loadUpdater() {
      return { autoUpdater };
    },
  });

  assert.equal(started, true);
  assert.equal(autoUpdater.autoDownload, false);
  assert.equal(autoUpdater.autoInstallOnAppQuit, false);
  assert.equal(autoUpdater.checks, 1);

  autoUpdater.emit("update-available", { version: "0.1.23" });
  await flush();
  assert.equal(autoUpdater.downloads, 1);
  assert.equal(dialogs[0].options.buttons[0], "Download");

  autoUpdater.emit("update-downloaded", { version: "0.1.23" });
  await flush();
  assert.equal(autoUpdater.installs, 1);
  assert.equal(dialogs[1].options.buttons[0], "Restart and install");
});

test("leaves an available update alone when the user chooses later", async () => {
  const autoUpdater = fakeUpdater();
  const started = startAutomaticUpdates({
    isPackaged: true,
    platform: "linux",
    window: { id: 1 },
    dialog: {
      async showMessageBox() {
        return { response: 1 };
      },
    },
    logger: { error() {} },
    loadUpdater() {
      return { autoUpdater };
    },
  });

  assert.equal(started, true);
  autoUpdater.emit("update-available", { version: "0.1.23" });
  await flush();
  assert.equal(autoUpdater.downloads, 0);
});
