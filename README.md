# tcode

`tcode` is a terminal agent harness for coding tasks, with Anthropic, OpenAI-compatible, and ChatGPT/Codex providers.

## Install

Release binaries are available for Linux, macOS, and Windows on both x86_64 and ARM64. The installers download the latest release and verify its SHA-256 checksum before installing it.

### Linux / macOS

```sh
curl -fsSL https://raw.githubusercontent.com/Teamon9161/tcode/main/install.sh | sh
```

The default destination is `~/.local/bin`. To choose a version or install directory:

```sh
TCODE_VERSION=0.1.0 TCODE_INSTALL_DIR=/usr/local/bin \
  sh -c "$(curl -fsSL https://raw.githubusercontent.com/Teamon9161/tcode/main/install.sh)"
```

### Windows PowerShell

```powershell
irm https://raw.githubusercontent.com/Teamon9161/tcode/main/install.ps1 | iex
```

The default destination is `%LOCALAPPDATA%\Programs\tcode\bin`, which the installer adds to your user `PATH`.

### Upgrade

Once installed from a release, run:

```sh
tcode update
```

The command selects the current platform's release binary, verifies `checksums.txt`, and replaces the executable. On Windows the replacement completes immediately after `tcode` exits.

## Build from source

```sh
cargo build --release
cargo run
```

Run the full local verification suite with:

```sh
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets
```

## Releasing

GitHub Actions validates pushes and pull requests. A release is only published when a `v*` tag is pushed, and the tag must match the root `Cargo.toml` version. The release workflow builds these checksum-protected binaries in parallel:

- `tcode-x86_64-linux` and `tcode-aarch64-linux` (statically linked musl)
- `tcode-x86_64-macos` and `tcode-aarch64-macos`
- `tcode-x86_64-windows.exe` and `tcode-aarch64-windows.exe`

For example, after changing the manifest version to `0.2.0`, publishing is triggered externally with `git tag v0.2.0` followed by `git push origin v0.2.0`.
