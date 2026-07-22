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

    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    // Downloaded beside its destination rather than into memory: tens of
    // megabytes over a link that drops them is exactly the case where every
    // byte already received has to survive the drop.
    let partial = partial_path(dest)?;

    let url = format!("{base}/{asset}");
    let mut attempt = 0;
    loop {
        let have = tokio::fs::metadata(&partial)
            .await
            .map(|meta| meta.len())
            .unwrap_or(0);
        match fetch_into(&client, &url, &partial, have, progress).await {
            Ok(()) => break,
            // Retried on the assumption the link is flaky rather than the
            // asset missing: a resumed attempt starts where the last one
            // stopped, so retrying costs only what was actually lost.
            Err(error) if attempt + 1 < DOWNLOAD_ATTEMPTS && !is_fatal(&error) => {
                attempt += 1;
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
            Err(error) => {
                return Err(error).with_context(|| format!("cannot download {asset}"));
            }
        }
    }

    let bytes = tokio::fs::read(&partial)
        .await
        .with_context(|| format!("cannot read {}", partial.display()))?;
    if hex::encode(Sha256::digest(&bytes)) != expected {
        // The partial file is what a resumed attempt trusts, so a bad one has
        // to go: keeping it would make every future attempt fail the same way.
        let _ = tokio::fs::remove_file(&partial).await;
        bail!("checksum mismatch for {asset}; it was not installed");
    }
    drop(bytes);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&partial, std::fs::Permissions::from_mode(0o755))
            .await
            .with_context(|| format!("cannot make {} executable", partial.display()))?;
    }
    // Renamed only once it is complete and verified, so the destination path
    // never names a half-written executable.
    tokio::fs::rename(&partial, dest)
        .await
        .with_context(|| format!("cannot write {}", dest.display()))?;
    Ok(())
}

/// How many times a dropped download is resumed before giving up.
const DOWNLOAD_ATTEMPTS: usize = 6;

/// The scratch file a download accumulates into.
///
/// Built by hand rather than with `with_extension`, which would eat the `.16`
/// of a name like `tcode-voiced-0.1.16`.
fn partial_path(dest: &Path) -> Result<PathBuf> {
    let name = dest
        .file_name()
        .and_then(|name| name.to_str())
        .with_context(|| format!("{} is not a usable file name", dest.display()))?;
    Ok(dest.with_file_name(format!("{name}.part")))
}

/// Append one attempt's worth of `url` to `partial`, resuming after `have`
/// bytes, and report percentage of the whole as it goes.
async fn fetch_into(
    client: &reqwest::Client,
    url: &str,
    partial: &Path,
    have: u64,
    progress: &mut dyn FnMut(u8),
) -> Result<()> {
    use futures::StreamExt;
    use tokio::io::AsyncWriteExt;

    let mut request = client.get(url);
    if have > 0 {
        request = request.header(reqwest::header::RANGE, format!("bytes={have}-"));
    }
    let response = request.send().await?.error_for_status()?;

    // A range request the server honours comes back as 206 and continues the
    // file; anything else is a whole body, which means starting over.
    let resuming = response.status() == reqwest::StatusCode::PARTIAL_CONTENT;
    let start = if resuming { have } else { 0 };
    let total = response.content_length().unwrap_or(0) + start;

    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(resuming)
        .truncate(!resuming)
        .open(partial)
        .await
        .with_context(|| format!("cannot write {}", partial.display()))?;

    let mut written = start;
    let mut last = u8::MAX;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("download interrupted")?;
        file.write_all(&chunk)
            .await
            .with_context(|| format!("cannot write {}", partial.display()))?;
        written += chunk.len() as u64;
        // Streamed for the progress report: this is tens of megabytes, and a
        // silent wait is indistinguishable from a hang.
        if total > 0 {
            let pct = ((written * 100) / total).min(100) as u8;
            if pct != last {
                last = pct;
                progress(pct);
            }
        }
    }
    file.flush()
        .await
        .with_context(|| format!("cannot write {}", partial.display()))?;
    Ok(())
}

/// Whether retrying is pointless. A refusal from GitHub is about the asset, not
/// the link, and repeating it only makes the user wait to hear the same thing.
fn is_fatal(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<reqwest::Error>()
        .is_some_and(|error| error.status().is_some())
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
    fn a_resumed_download_keeps_the_whole_versioned_name() {
        assert_eq!(
            partial_path(Path::new("/home/u/.tcode/voice/tcode-voiced-0.1.16")).unwrap(),
            PathBuf::from("/home/u/.tcode/voice/tcode-voiced-0.1.16.part")
        );
    }

    #[test]
    fn staging_file_stays_beside_the_executable() {
        assert_eq!(
            staging_path(Path::new("/opt/bin/tcode")).unwrap(),
            PathBuf::from("/opt/bin/tcode.new")
        );
    }
}
