function supportsAutomaticUpdates({ isPackaged, platform = process.platform }) {
  return isPackaged && (platform === "win32" || platform === "linux");
}

function errorText(error) {
  return String(error?.stack ?? error?.message ?? error);
}

function report(logger, error) {
  logger.error(`tcode: automatic update failed: ${errorText(error)}`);
}

const UPDATE_STATE = "tcode://update-state";

/**
 * Configure the platform updater after the first window is usable.
 *
 * Downloads start automatically — the user should not have to approve a
 * background fetch. A small indicator in the title bar shows progress and,
 * once the download lands, becomes a "restart to update" button whose click
 * calls `update_restart` (a shell verb registered by the caller).
 *
 * Returns a handle with `quitAndInstall()` when the updater is active, or
 * `null` when it is not (dev mode, macOS).
 */
function startAutomaticUpdates({
  isPackaged,
  platform = process.platform,
  emit,
  logger = console,
  loadUpdater = () => require("electron-updater"),
}) {
  if (!supportsAutomaticUpdates({ isPackaged, platform })) return null;

  const { autoUpdater } = loadUpdater();
  autoUpdater.autoDownload = true;
  autoUpdater.autoInstallOnAppQuit = true;

  let version = null;

  autoUpdater.on("error", (error) => {
    report(logger, error);
    emit(UPDATE_STATE, null);
  });

  autoUpdater.on("update-available", (info) => {
    version = info.version;
    emit(UPDATE_STATE, { state: "downloading", version, percent: 0 });
  });

  autoUpdater.on("download-progress", (progress) => {
    emit(UPDATE_STATE, {
      state: "downloading",
      version,
      percent: Math.round(progress.percent),
    });
  });

  autoUpdater.on("update-downloaded", (info) => {
    emit(UPDATE_STATE, {
      state: "ready",
      version: info.version ?? version,
    });
  });

  try {
    Promise.resolve(autoUpdater.checkForUpdates()).catch((error) => report(logger, error));
  } catch (error) {
    report(logger, error);
  }

  return {
    quitAndInstall() {
      autoUpdater.quitAndInstall();
    },
  };
}

module.exports = { startAutomaticUpdates, supportsAutomaticUpdates };
