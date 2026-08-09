const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const test = require("node:test");

const { stageSidecar } = require("./stage-sidecar");

test("stages only the named sidecar and preserves executable permissions", () => {
  const temporary = fs.mkdtempSync(path.join(os.tmpdir(), "tcode-stage-sidecar-"));
  try {
    const source = path.join(temporary, "build", "tcode-sidecar");
    const staging = path.join(temporary, "release", "sidecar");
    fs.mkdirSync(path.dirname(source), { recursive: true });
    fs.writeFileSync(source, "sidecar bytes");
    fs.chmodSync(source, 0o755);

    const staged = stageSidecar(source, staging);

    assert.equal(staged, path.join(staging, "tcode-sidecar"));
    assert.equal(fs.readFileSync(staged, "utf8"), "sidecar bytes");
    assert.equal(fs.statSync(staged).mode & 0o777, 0o755);
  } finally {
    fs.rmSync(temporary, { recursive: true, force: true });
  }
});
