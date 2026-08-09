const fs = require("node:fs");
const path = require("node:path");

function stageSidecar(source, destinationDirectory) {
  const sourcePath = path.resolve(source);
  const sourceStat = fs.statSync(sourcePath);
  if (!sourceStat.isFile()) throw new Error(`sidecar is not a file: ${sourcePath}`);

  const destination = path.join(path.resolve(destinationDirectory), path.basename(sourcePath));
  fs.mkdirSync(path.dirname(destination), { recursive: true });
  fs.copyFileSync(sourcePath, destination);
  fs.chmodSync(destination, sourceStat.mode & 0o777);
  return destination;
}

if (require.main === module) {
  const [source, destinationDirectory = path.join(process.cwd(), "release", "sidecar")] =
    process.argv.slice(2);
  if (!source) {
    console.error("usage: node scripts/stage-sidecar.js <built-sidecar> [staging-directory]");
    process.exitCode = 1;
  } else {
    try {
      console.log(stageSidecar(source, destinationDirectory));
    } catch (error) {
      console.error(`could not stage sidecar: ${error.message}`);
      process.exitCode = 1;
    }
  }
}

module.exports = { stageSidecar };
