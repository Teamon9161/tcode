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
TCODE_VERSION=0.2.7 TCODE_INSTALL_DIR=/usr/local/bin \
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

The sidecar is released independently at `voice-v<version>` only when its code or model support changes. Ordinary `v<version>` tcode releases reuse the installed sidecar.

## Desktop app

The Electron desktop app is distributed separately from the terminal CLI at [Teamon9161/tcode-desktop-releases](https://github.com/Teamon9161/tcode-desktop-releases/releases/latest). Its version and `app-v<version>` source tag are independent of the root CLI's `v<version>` releases.

- **Windows x64:** download the per-user NSIS installer. On later launches, the app checks for a newer desktop release and asks before downloading it and again before restarting to install it.
- **Linux x64:** download and run the AppImage. When it is launched as an AppImage, it follows the same consented update flow.
- **macOS universal:** download the DMG or ZIP and install manually. macOS does not use the app updater in this first release.

These initial desktop builds are intentionally **unsigned** for a small group of trusted users. Windows may show an unknown-publisher or SmartScreen warning, and macOS Gatekeeper may require a user to manually allow opening the downloaded app. Do not treat the GitHub Release checksum metadata as a replacement for platform code-signing identity.

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

## Configuration

By default tcode uses `~/.tcode/config.toml`; `--config <PATH>` (or `-C`) selects a different personal configuration for a run, including setup and all remembered UI choices. Runtime selections live in that file's `[tcode_state]` table, which tcode updates without rewriting other TOML or comments. `-p` remains the one-shot prompt flag.

```sh
tcode --config ~/work/tcode.toml --profile anthropic --model claude-sonnet-5
```

Project `.tcode/config.toml` remains an overlay and cannot provide runtime state. On first use of the default config, a legacy `~/.tcode/state.toml` is migrated into `[tcode_state]`; a custom config never imports it.

## Releasing

GitHub Actions validates pushes and pull requests. A release is only published when a `v*` tag is pushed, and the tag must match the root `Cargo.toml` `[workspace.package]` version. The release workflow builds these checksum-protected binaries in parallel:

- `tcode-x86_64-linux` and `tcode-aarch64-linux` (statically linked musl)
- `tcode-x86_64-macos` and `tcode-aarch64-macos`
- `tcode-x86_64-windows.exe` and `tcode-aarch64-windows.exe`

For example, after changing the manifest version to `0.2.0`, publishing is triggered externally with `git tag v0.2.0` followed by `git push origin v0.2.0`.

### Desktop release checklist

Desktop releases are deliberately separate so they never become the root CLI's `latest` feed.

1. Bump the same desktop version in `crates/tcode-app/Cargo.toml`, `crates/tcode-app/package.json`, and `crates/tcode-app/ui/package.json`; regenerate the affected lockfiles.
2. Ensure the source repository secret `TCODE_DESKTOP_RELEASE_TOKEN` is a fine-grained PAT limited to `contents:write` on `Teamon9161/tcode-desktop-releases`. Never put it in an app package or local config.
3. Push `app-v<version>`. The Desktop Release workflow builds the Windows x64 NSIS installer, Linux x64 AppImage, and unsigned macOS universal DMG/ZIP, then creates `v<version>` in the external release repository.
4. Confirm the external release contains `latest.yml`, `latest-linux.yml`, both Windows/Linux package assets and blockmaps, plus the macOS DMG/ZIP. There must be no `latest-mac.yml` in this unsigned manual-install phase.
5. Do not overwrite a desktop release. Publish a higher corrected desktop version instead. Before relying on the updater for a release, test an installed Windows and Linux version N upgrading to N+1; validate macOS manual installation separately.

When distribution expands, add Windows signing and Apple Developer ID signing/notarization before enabling macOS auto-update. This does not require changing the tag or external-repository topology.
