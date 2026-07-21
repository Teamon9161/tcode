//! Self-update support for binaries installed from GitHub Releases.
//!
//! Release archives deliberately contain one raw executable per target. That
//! keeps this path small: download the matching asset, verify its published
//! SHA-256 digest, then atomically replace the installed executable.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

const REPOSITORY: &str = "Teamon9161/tcode";
const RELEASES_URL: &str = "https://github.com/Teamon9161/tcode/releases/latest/download";

pub async fn run() -> Result<()> {
    let asset = release_asset().context("this platform is not supported by release updates")?;
    let executable =
        std::env::current_exe().context("cannot locate the running tcode executable")?;
    let client = reqwest::Client::builder()
        .user_agent(concat!("tcode/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("cannot create update client")?;

    let latest = latest_version(&client).await?;
    let current = env!("CARGO_PKG_VERSION");
    if latest == current {
        println!("tcode {current} is already up to date");
        return Ok(());
    }

    println!("Updating tcode {current} -> {latest}...");
    let checksum_url = format!("{RELEASES_URL}/checksums.txt");
    let checksums = download_text(&client, &checksum_url).await?;
    let expected = checksum_for(&checksums, asset)
        .with_context(|| format!("release checksums do not contain {asset}"))?;

    let bytes = download_bytes(&client, &format!("{RELEASES_URL}/{asset}")).await?;
    let actual = hex::encode(Sha256::digest(&bytes));
    if actual != expected {
        bail!("checksum mismatch for {asset}; update was not installed");
    }

    let staged = staging_path(&executable)?;
    tokio::fs::write(&staged, bytes)
        .await
        .with_context(|| format!("cannot write update to {}", staged.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("cannot mark {} as executable", staged.display()))?;
        std::fs::rename(&staged, &executable).with_context(|| {
            format!(
                "cannot replace {}; reinstall with install.sh instead",
                executable.display()
            )
        })?;
        println!("Updated tcode to {latest}. Restart tcode to use the new version.");
    }

    #[cfg(windows)]
    {
        spawn_windows_replacer(&staged, &executable)?;
        println!(
            "Downloaded tcode {latest}; it will replace this executable after tcode exits. Restart tcode to use the new version."
        );
    }

    Ok(())
}

/// Fetch one asset from *this* version's release and write it to `dest`,
/// verified against the release's published checksums.
///
/// Pinned to the running version rather than `latest`, which is what the rest
/// of this module uses: tcode and the voice sidecar speak a private protocol
/// across a pipe, so they have to come from the same build. A sidecar from a
/// newer release would be exactly the mismatch the versioned filename exists
/// to rule out.
pub async fn install_release_asset(
    asset: &str,
    dest: &Path,
    progress: &mut dyn FnMut(u8),
) -> Result<()> {
    use futures::StreamExt;

    let base = format!(
        "https://github.com/{REPOSITORY}/releases/download/v{}",
        env!("CARGO_PKG_VERSION")
    );
    let client = reqwest::Client::builder()
        .user_agent(concat!("tcode/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("cannot create download client")?;

    let checksums = download_text(&client, &format!("{base}/checksums.txt")).await?;
    let expected = checksum_for(&checksums, asset).with_context(|| {
        format!("this release publishes no {asset}, so there is nothing to install")
    })?;

    let response = client
        .get(format!("{base}/{asset}"))
        .send()
        .await
        .with_context(|| format!("cannot download {asset}"))?
        .error_for_status()
        .with_context(|| format!("cannot download {asset}"))?;
    // Streamed for the progress report: this is tens of megabytes, and a
    // silent wait is indistinguishable from a hang.
    let total = response.content_length().unwrap_or(0);
    let mut bytes = Vec::with_capacity(total as usize);
    let mut stream = response.bytes_stream();
    let mut last = u8::MAX;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("download interrupted")?;
        bytes.extend_from_slice(&chunk);
        if total > 0 {
            let pct = ((bytes.len() as u64 * 100) / total).min(100) as u8;
            if pct != last {
                last = pct;
                progress(pct);
            }
        }
    }

    if hex::encode(Sha256::digest(&bytes)) != expected {
        bail!("checksum mismatch for {asset}; it was not installed");
    }

    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    tokio::fs::write(dest, bytes)
        .await
        .with_context(|| format!("cannot write {}", dest.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o755))
            .await
            .with_context(|| format!("cannot make {} executable", dest.display()))?;
    }
    Ok(())
}

fn release_asset() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Ok("tcode-x86_64-linux"),
        ("linux", "aarch64") => Ok("tcode-aarch64-linux"),
        ("macos", "x86_64") => Ok("tcode-x86_64-macos"),
        ("macos", "aarch64") => Ok("tcode-aarch64-macos"),
        ("windows", "x86_64") => Ok("tcode-x86_64-windows.exe"),
        ("windows", "aarch64") => Ok("tcode-aarch64-windows.exe"),
        (os, arch) => bail!("no release asset for {arch}-{os}"),
    }
}

async fn latest_version(client: &reqwest::Client) -> Result<String> {
    #[derive(serde::Deserialize)]
    struct Release {
        tag_name: String,
    }

    let release: Release = client
        .get(format!(
            "https://api.github.com/repos/{REPOSITORY}/releases/latest"
        ))
        .send()
        .await
        .context("cannot contact GitHub releases API")?
        .error_for_status()
        .context("GitHub did not return a latest release")?
        .json()
        .await
        .context("cannot parse GitHub release metadata")?;
    Ok(release.tag_name.trim_start_matches('v').to_string())
}

async fn download_text(client: &reqwest::Client, url: &str) -> Result<String> {
    client
        .get(url)
        .send()
        .await
        .with_context(|| format!("cannot download {url}"))?
        .error_for_status()
        .with_context(|| format!("GitHub returned an error for {url}"))?
        .text()
        .await
        .with_context(|| format!("cannot read {url}"))
}

async fn download_bytes(client: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    client
        .get(url)
        .send()
        .await
        .with_context(|| format!("cannot download {url}"))?
        .error_for_status()
        .with_context(|| format!("GitHub returned an error for {url}"))?
        .bytes()
        .await
        .with_context(|| format!("cannot read {url}"))
        .map(|bytes| bytes.to_vec())
}

fn checksum_for(checksums: &str, asset: &str) -> Option<String> {
    checksums.lines().find_map(|line| {
        let (digest, name) = line.split_once(char::is_whitespace)?;
        (name.trim_start().trim_start_matches('*') == asset).then(|| digest.to_ascii_lowercase())
    })
}

fn staging_path(executable: &Path) -> Result<PathBuf> {
    let file_name = executable
        .file_name()
        .and_then(|name| name.to_str())
        .context("running executable has no UTF-8 file name")?;
    Ok(executable.with_file_name(format!("{file_name}.new")))
}

#[cfg(windows)]
fn spawn_windows_replacer(staged: &Path, executable: &Path) -> Result<()> {
    use std::process::Command;

    let script = std::env::temp_dir().join(format!("tcode-update-{}.cmd", std::process::id()));
    let script_body = format!(
        "@echo off\r\n:retry\r\nmove /Y \"{}\" \"{}\" >nul 2>nul\r\nif errorlevel 1 (\r\n  timeout /t 1 /nobreak >nul\r\n  goto retry\r\n)\r\ndel \"%~f0\"\r\n",
        staged.display(),
        executable.display()
    );
    std::fs::write(&script, script_body)
        .with_context(|| format!("cannot create {}", script.display()))?;
    Command::new("cmd")
        .args(["/C", &script.to_string_lossy()])
        .spawn()
        .context("cannot start the Windows update helper")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_named_checksum_only() {
        let checksums = "abc123  tcode-x86_64-linux\ndef456 *tcode-x86_64-windows.exe\n";
        assert_eq!(
            checksum_for(checksums, "tcode-x86_64-windows.exe"),
            Some("def456".to_string())
        );
        assert_eq!(checksum_for(checksums, "tcode-aarch64-linux"), None);
    }

    #[test]
    fn staging_file_stays_beside_the_executable() {
        assert_eq!(
            staging_path(Path::new("/opt/bin/tcode")).unwrap(),
            PathBuf::from("/opt/bin/tcode.new")
        );
    }
}
