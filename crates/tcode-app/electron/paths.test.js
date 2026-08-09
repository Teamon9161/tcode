const assert = require("node:assert/strict");
const path = require("node:path");
const test = require("node:test");

const { resolveSidecarPath, sidecarExecutable, sidecarMissingMessage } = require("./paths");

test("uses the bundled sidecar for packaged applications", () => {
  const actual = resolveSidecarPath({
    isPackaged: true,
    resourcesPath: "/opt/tcode/resources",
    root: "/workspace/tcode-app",
    env: { TCODE_SIDECAR: "/tmp/untrusted-sidecar" },
    platform: "linux",
    exists(candidate) {
      return candidate === path.join("/opt/tcode/resources", "sidecar", "tcode-sidecar");
    },
  });

  assert.equal(actual, path.join("/opt/tcode/resources", "sidecar", "tcode-sidecar"));
});

test("uses the Windows executable name in packaged resources", () => {
  assert.equal(sidecarExecutable("win32"), "tcode-sidecar.exe");
  const resourcesPath = "C:\\Program Files\\tcode\\resources";
  const expected = path.join(resourcesPath, "sidecar", "tcode-sidecar.exe");
  assert.equal(
    resolveSidecarPath({
      isPackaged: true,
      resourcesPath,
      root: "C:\\repo\\tcode-app",
      platform: "win32",
      exists(candidate) {
        return candidate === expected;
      },
    }),
    expected,
  );
});

test("allows development override before build output lookup", () => {
  const actual = resolveSidecarPath({
    isPackaged: false,
    resourcesPath: "/unused",
    root: "/workspace/tcode-app",
    env: { TCODE_SIDECAR: "/tmp/tcode-sidecar" },
    platform: "linux",
    exists() {
      throw new Error("development override must not probe the build directory");
    },
  });

  assert.equal(actual, "/tmp/tcode-sidecar");
});

test("prefers a debug sidecar over a release sidecar during development", () => {
  const root = "/workspace/tcode-app";
  const debug = path.join(root, "target", "debug", "tcode-sidecar");
  const actual = resolveSidecarPath({
    isPackaged: false,
    resourcesPath: "/unused",
    root,
    env: {},
    platform: "linux",
    exists(candidate) {
      return candidate === debug;
    },
  });

  assert.equal(actual, debug);
});

test("does not start a packaged application without its bundled sidecar", () => {
  assert.equal(
    resolveSidecarPath({
      isPackaged: true,
      resourcesPath: "/opt/tcode/resources",
      root: "/workspace/tcode-app",
      platform: "linux",
      exists() {
        return false;
      },
    }),
    null,
  );
});

test("reports the exact missing packaged sidecar location", () => {
  const message = sidecarMissingMessage({
    isPackaged: true,
    resourcesPath: "/opt/tcode/resources",
    root: "/workspace/tcode-app",
    platform: "linux",
  });

  assert.ok(
    message.includes(`Expected: ${path.join("/opt/tcode/resources", "sidecar", "tcode-sidecar")}`),
  );
});
