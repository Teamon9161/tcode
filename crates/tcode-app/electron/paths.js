const fs = require("node:fs");
const path = require("node:path");

function sidecarExecutable(platform = process.platform) {
  return platform === "win32" ? "tcode-sidecar.exe" : "tcode-sidecar";
}

/**
 * Resolve the backend binary without letting a user environment variable
 * replace the binary that was packaged with the application.
 */
function resolveSidecarPath({
  isPackaged,
  resourcesPath,
  env = process.env,
  root,
  platform = process.platform,
  exists = fs.existsSync,
}) {
  const executable = sidecarExecutable(platform);

  if (isPackaged) {
    const candidate = path.join(resourcesPath, "sidecar", executable);
    return exists(candidate) ? candidate : null;
  }

  if (env.TCODE_SIDECAR) return env.TCODE_SIDECAR;
  for (const profile of ["debug", "release"]) {
    const candidate = path.join(root, "target", profile, executable);
    if (exists(candidate)) return candidate;
  }
  return null;
}

function sidecarMissingMessage({ isPackaged, resourcesPath, root, platform = process.platform }) {
  if (isPackaged) {
    return (
      "tcode-sidecar was not found in this application package.\n\n" +
      `Expected: ${path.join(resourcesPath, "sidecar", sidecarExecutable(platform))}`
    );
  }

  return (
    "tcode-sidecar was not found. Build it first:\n\n" +
    "    cargo build --bin tcode-sidecar\n\n" +
    `(looked under ${path.join(root, "target")}; set TCODE_SIDECAR to override)`
  );
}

module.exports = { resolveSidecarPath, sidecarExecutable, sidecarMissingMessage };
