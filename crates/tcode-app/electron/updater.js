function supportsAutomaticUpdates({ isPackaged, platform = process.platform }) {
  return isPackaged && (platform === "win32" || platform === "linux");
}

function errorText(error) {
  return String(error?.stack ?? error?.message ?? error);
}

function report(logger, error) {
  logger.error(`tcode: automatic update failed: ${errorText(error)}`);
}

async function askToDownload({ autoUpdater, dialog, window, info }) {
  const answer = await dialog.showMessageBox(window, {
    type: "info",
    title: "Update available",
    message: `tcode ${info.version} is available.`,
    detail: "Download it now? tcode will ask before restarting to install it.",
    buttons: ["Download", "Later"],
    defaultId: 0,
    cancelId: 1,
    noLink: true,
  });
  if (answer.response === 0) await autoUpdater.downloadUpdate();
}

async function askToInstall({ autoUpdater, dialog, window, info }) {
  const answer = await dialog.showMessageBox(window, {
    type: "info",
    title: "Update ready",
    message: `tcode ${info.version} has finished downloading.`,
    detail: "Restart tcode now to install the update?",
    buttons: ["Restart and install", "Later"],
    defaultId: 0,
    cancelId: 1,
    noLink: true,
  });
  if (answer.response === 0) autoUpdater.quitAndInstall();
}

/**
 * Configure the platform updater after the first window is usable. This module
 * has no renderer API: update consent is native UI owned by the main process.
 */
function startAutomaticUpdates({
  isPackaged,
  platform = process.platform,
  window,
  dialog,
  logger = console,
  loadUpdater = () => require("electron-updater"),
}) {
  if (!supportsAutomaticUpdates({ isPackaged, platform })) return false;

  const { autoUpdater } = loadUpdater();
  autoUpdater.autoDownload = false;
  autoUpdater.autoInstallOnAppQuit = false;
  autoUpdater.on("error", (error) => report(logger, error));
  autoUpdater.on("update-available", (info) => {
    void askToDownload({ autoUpdater, dialog, window, info }).catch((error) => report(logger, error));
  });
  autoUpdater.on("update-downloaded", (info) => {
    void askToInstall({ autoUpdater, dialog, window, info }).catch((error) => report(logger, error));
  });

  try {
    Promise.resolve(autoUpdater.checkForUpdates()).catch((error) => report(logger, error));
  } catch (error) {
    report(logger, error);
  }
  return true;
}

module.exports = { startAutomaticUpdates, supportsAutomaticUpdates };
