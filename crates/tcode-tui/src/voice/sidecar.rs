//! The sidecar client: spawn `tcode-voiced`, write commands to its stdin,
//! turn its stdout into `VoiceEvent`s.
//!
//! Line-delimited JSON in both directions, like `tcode-tools::mcp` — but not
//! JSON-RPC, because there is nothing to correlate: one hold is one
//! start/stop, and everything the sidecar says is unsolicited.

use std::path::PathBuf;
use std::process::Stdio;

use serde::Deserialize;
use tcode_core::config::VoiceConfig;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use super::{VoiceBackend, VoiceCmd, VoiceEvent};

/// A hand-built sidecar, for whoever is working on it.
const EXECUTABLE: &str = if cfg!(windows) {
    "tcode-voiced.exe"
} else {
    "tcode-voiced"
};

/// A downloaded sidecar, named after the tcode it belongs to.
///
/// The version is in the filename so that "installed but out of date" is not a
/// state that can exist: a tcode looks for its own version and finds it or does
/// not. There is nothing to compare, and no way to run a binary that disagrees
/// with the flags being passed to it — which is the failure this replaces.
fn versioned_executable() -> String {
    let version = env!("CARGO_PKG_VERSION");
    let suffix = if cfg!(windows) { ".exe" } else { "" };
    format!("tcode-voiced-{version}{suffix}")
}

/// The release asset for this machine, or `None` where no sidecar can be built.
///
/// Fewer platforms than tcode itself: upstream publishes no musl and no
/// Windows-on-ARM libraries. Saying so up front is the difference between "not
/// supported here" and a download that fails for reasons nobody can act on.
pub(crate) fn release_asset() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("tcode-voiced-x86_64-linux"),
        ("linux", "aarch64") => Some("tcode-voiced-aarch64-linux"),
        ("macos", "x86_64") => Some("tcode-voiced-x86_64-macos"),
        ("macos", "aarch64") => Some("tcode-voiced-aarch64-macos"),
        ("windows", "x86_64") => Some("tcode-voiced-x86_64-windows.exe"),
        _ => None,
    }
}

/// Where a downloaded sidecar belongs.
pub(crate) fn install_path() -> Result<PathBuf, String> {
    let root = tcode_core::config::Config::global_path()
        .map_err(|_| "no home directory to install into".to_string())?;
    Ok(root.join("voice").join(versioned_executable()))
}

/// How a stale sidecar announces itself. Flags are only ever added, so a
/// rejected flag means the binary predates the tcode driving it — a situation
/// with one fix, which is worth saying outright rather than leaving as a
/// puzzle.
const STALE: &str = "unknown argument";

#[derive(Deserialize)]
#[serde(tag = "event", rename_all = "lowercase")]
enum WireEvent {
    Ready,
    Downloading { pct: u8 },
    Level { rms: f32 },
    Transcript { text: String },
    Error { message: String },
    Models { models: Vec<VoiceModel> },
}

/// One entry of the sidecar's model catalogue. `note` is prose from the
/// sidecar, shown in the picker as-is: this side deliberately knows nothing
/// about what models exist.
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct VoiceModel {
    pub(crate) name: String,
    pub(crate) note: String,
}

impl From<WireEvent> for VoiceEvent {
    fn from(wire: WireEvent) -> Self {
        match wire {
            WireEvent::Ready => VoiceEvent::Ready,
            WireEvent::Downloading { pct } => VoiceEvent::Downloading(pct),
            WireEvent::Level { rms } => VoiceEvent::Level(rms),
            WireEvent::Transcript { text } => VoiceEvent::Transcript(text),
            WireEvent::Error { message } => VoiceEvent::Failed(explain(message)),
            // Only ever produced by `--list-models`, which is read directly
            // rather than through the event stream.
            WireEvent::Models { .. } => VoiceEvent::Ready,
        }
    }
}

/// Turn a sidecar failure into something with a next step. Both cases here are
/// ones whose message names a cause the user cannot act on; everything else the
/// sidecar says already explains itself.
fn explain(message: String) -> String {
    if message.contains(STALE) {
        return format!(
            "{message} — this tcode-voiced is older than tcode. Delete it from \
             ~/.tcode/voice/ to fetch a matching one, or rebuild it with `cargo build \
             -p tcode-voiced --release --manifest-path crates/tcode-voiced/Cargo.toml`"
        );
    }
    // The dynamic linker's own wording, which names a file nobody has heard of
    // rather than a package they can install. Only Linux links ALSA.
    if cfg!(target_os = "linux") && message.contains("libasound") {
        return format!(
            "{message} — the voice backend records through ALSA. Install it with \
             `apt install libasound2` (Debian/Ubuntu), `dnf install alsa-lib` (Fedora), \
             or your distribution's equivalent."
        );
    }
    message
}

pub(crate) struct SidecarBackend {
    /// Unbounded so that a keystroke never awaits a pipe. Commands are three
    /// bytes and arrive at human speed; there is nothing to back-pressure.
    tx: mpsc::UnboundedSender<VoiceCmd>,
}

impl SidecarBackend {
    pub(crate) fn spawn(
        cfg: &VoiceConfig,
        events: mpsc::Sender<VoiceEvent>,
    ) -> Result<Self, String> {
        let exe = resolve(cfg)?;
        let mut command = tokio::process::Command::new(&exe);
        command.arg("--language").arg(&cfg.language);
        if !cfg.model.is_empty() {
            command.arg("--model").arg(&cfg.model);
        }
        if !cfg.hotwords.is_empty() {
            command.arg("--hotwords").arg(cfg.hotwords.join(","));
        }
        if !cfg.device.is_empty() {
            command.arg("--device").arg(&cfg.device);
        }
        if !cfg.model_dir.is_empty() {
            command.arg("--model-dir").arg(&cfg.model_dir);
        }
        if !cfg.download_base.is_empty() {
            command.arg("--download-base").arg(&cfg.download_base);
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = command
            .spawn()
            .map_err(|e| format!("could not start {}: {e}", exe.display()))?;
        let mut stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");

        // Anything the sidecar prints to stderr is kept so that, if it dies,
        // the app can say why instead of reporting a bare exit code.
        let last_error = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let stderr_slot = last_error.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if !line.trim().is_empty() {
                    *stderr_slot.lock().expect("stderr slot") = line;
                }
            }
        });

        let reader_events = events.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(wire) = serde_json::from_str::<WireEvent>(&line) else {
                    continue; // not ours to interpret; the sidecar may log
                };
                if reader_events.send(wire.into()).await.is_err() {
                    break; // the app is gone
                }
            }
        });

        let (tx, mut rx) = mpsc::unbounded_channel();
        // Owns the child, so the process lives exactly as long as this task:
        // dropping the backend drops the sender, which ends the loop, which
        // closes stdin — how the sidecar is asked to exit.
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    cmd = rx.recv() => {
                        let Some(cmd) = cmd else { break };
                        let line = match cmd {
                            VoiceCmd::Start => "{\"cmd\":\"start\"}\n",
                            VoiceCmd::Stop => "{\"cmd\":\"stop\"}\n",
                            VoiceCmd::Cancel => "{\"cmd\":\"cancel\"}\n",
                        };
                        if stdin.write_all(line.as_bytes()).await.is_err() {
                            break;
                        }
                        let _ = stdin.flush().await;
                    }
                    status = child.wait() => {
                        let detail = last_error.lock().expect("stderr slot").clone();
                        let detail = if detail.is_empty() {
                            match status {
                                Ok(status) => format!("exited with {status}"),
                                Err(e) => format!("could not be waited on: {e}"),
                            }
                        } else {
                            detail
                        };
                        // Through `explain` as well: a sidecar that dies before
                        // it can speak JSON — a missing shared library, a
                        // rejected flag — is exactly the case whose raw
                        // message names something the user cannot act on.
                        let _ = events.send(VoiceEvent::Failed(explain(format!(
                            "voice sidecar stopped: {detail}"
                        )))).await;
                        return;
                    }
                }
            }
        });

        Ok(Self { tx })
    }
}

impl VoiceBackend for SidecarBackend {
    fn send(&mut self, cmd: VoiceCmd) -> Result<(), String> {
        self.tx
            .send(cmd)
            .map_err(|_| "voice sidecar is no longer running".to_string())
    }
}

/// Ask the sidecar what models it was built with. Synchronous and short: it
/// prints the list and exits before opening a microphone or touching the disk.
///
/// This is the only source of model names on this side. Keeping a copy here to
/// render a menu from would mean the menu could offer something the installed
/// binary cannot load, and the user would find out after a 500MB download.
pub(crate) fn list_models(cfg: &VoiceConfig) -> Result<Vec<VoiceModel>, String> {
    let exe = resolve(cfg)?;
    let output = std::process::Command::new(&exe)
        .arg("--list-models")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run {}: {e}", exe.display()))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if let Ok(WireEvent::Models { models }) = serde_json::from_str::<WireEvent>(line.trim()) {
            return Ok(models);
        }
    }
    // An older sidecar rejects the flag; say so, since the fix is the same one.
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(explain(format!(
        "{} could not list its models: {}",
        exe.display(),
        if stderr.trim().is_empty() {
            format!("{STALE} --list-models")
        } else {
            stderr.trim().to_string()
        }
    )))
}

/// Where the sidecar is: the configured path, else `~/.tcode/voice/`. A miss
/// returns instructions rather than "not found" — nothing about where this
/// binary comes from is guessable.
pub(crate) fn resolve(cfg: &VoiceConfig) -> Result<PathBuf, String> {
    if !cfg.command.is_empty() {
        let path = PathBuf::from(&cfg.command);
        return path.is_file().then_some(path).ok_or_else(|| {
            format!(
                "[voice] command points at {}, which does not exist",
                cfg.command
            )
        });
    }
    let Ok(root) = tcode_core::config::Config::global_path() else {
        return Err("no voice backend, and no home directory to look in".into());
    };
    // The one matching this tcode first, then a hand-built one — someone
    // working on the sidecar wants their own build, and `[voice] command`
    // above is the way to insist on it.
    let downloaded = root.join("voice").join(versioned_executable());
    if downloaded.is_file() {
        return Ok(downloaded);
    }
    let built = root.join("voice").join(EXECUTABLE);
    if built.is_file() {
        return Ok(built);
    }
    if release_asset().is_none() {
        return Err(format!(
            "there is no voice backend for {}-{}: the speech library it needs is not published \
             for this platform. Everything else in tcode works as usual.",
            std::env::consts::ARCH,
            std::env::consts::OS
        ));
    }
    Err(format!(
        "no voice backend yet — it downloads on first use, or build it with `cargo build \
         -p tcode-voiced --release --manifest-path crates/tcode-voiced/Cargo.toml` and copy \
         the binary to {}",
        built.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The downloaded name carries this tcode's version, and the asset it comes
    /// from agrees about the executable suffix. Both are strings assembled by
    /// hand in two places, which is exactly where a release goes quietly wrong.
    #[test]
    fn the_installed_name_is_pinned_to_this_version_and_matches_its_asset() {
        let name = versioned_executable();
        assert!(
            name.contains(env!("CARGO_PKG_VERSION")),
            "{name} does not say which tcode it belongs to"
        );
        assert_eq!(name.ends_with(".exe"), cfg!(windows));
        if let Some(asset) = release_asset() {
            assert_eq!(
                asset.ends_with(".exe"),
                name.ends_with(".exe"),
                "{asset} and {name} disagree about the suffix"
            );
        }
        // Unsupported platforms must say so rather than name a file to fetch.
        assert_eq!(
            release_asset().is_none(),
            matches!(
                (std::env::consts::OS, std::env::consts::ARCH),
                ("windows", "aarch64")
            ) || !matches!(std::env::consts::OS, "linux" | "macos" | "windows"),
        );
    }

    /// A linker error names a file; users need a package. Only Linux links it.
    #[test]
    fn a_missing_audio_library_is_turned_into_something_installable() {
        let raw = "voice sidecar stopped: error while loading shared libraries: \
                   libasound.so.2: cannot open shared object file";
        let explained = explain(raw.to_string());
        assert_eq!(explained.contains("libasound2"), cfg!(target_os = "linux"));
        // Unrelated failures are passed through untouched.
        assert_eq!(explain("no input device".into()), "no input device");
    }

    #[test]
    fn wire_events_parse_into_voice_events() {
        let cases = [
            (r#"{"event":"ready"}"#, VoiceEvent::Ready),
            (
                r#"{"event":"downloading","pct":37}"#,
                VoiceEvent::Downloading(37),
            ),
            (r#"{"event":"level","rms":0.5}"#, VoiceEvent::Level(0.5)),
            (
                r#"{"event":"transcript","text":"改一下 editor"}"#,
                VoiceEvent::Transcript("改一下 editor".into()),
            ),
            (
                r#"{"event":"error","message":"no input device"}"#,
                VoiceEvent::Failed("no input device".into()),
            ),
        ];
        for (line, expected) in cases {
            let wire: WireEvent = serde_json::from_str(line).expect("parse");
            assert_eq!(VoiceEvent::from(wire), expected);
        }
    }

    /// The point of the message is that it tells the user what to do next.
    #[test]
    fn a_missing_backend_explains_how_to_get_one() {
        let cfg = VoiceConfig {
            command: "definitely/not/here".into(),
            ..VoiceConfig::default()
        };
        let error = resolve(&cfg).expect_err("configured path does not exist");
        assert!(error.contains("definitely/not/here"), "{error}");
    }
}
