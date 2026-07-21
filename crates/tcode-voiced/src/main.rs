//! tcode's push-to-talk speech recogniser, as a separate process.
//!
//! It exists as its own binary because its dependencies (a downloaded ONNX
//! runtime, ALSA on Linux) exist on fewer platforms than tcode does, and
//! because an audio driver that falls over should not take an editor session
//! with it. tcode drives it over stdio — see `protocol.rs`.
//!
//! Everything here is synchronous. cpal owns a callback thread, sherpa blocks,
//! and the loop below blocks on stdin; an async runtime would add a scheduler
//! to a program with exactly one thing to wait for at a time.

mod asr;
mod bpe;
mod capture;
mod model;
mod protocol;

use std::io::BufRead;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use protocol::{Command, Event, Events};

/// How often the level meter is published while recording. Fast enough to look
/// live, slow enough that it is not the busiest thing on the pipe.
const METER_INTERVAL: Duration = Duration::from_millis(120);

struct Args {
    /// Print the model catalogue and exit, instead of opening a microphone.
    /// tcode's model picker is built from this: the binary knows which presets
    /// it was compiled with, and tcode must not keep a second list to guess
    /// from.
    list_models: bool,
    model: String,
    language: String,
    device: String,
    model_dir: Option<PathBuf>,
    download_base: String,
    /// Words to bias recognition towards, comma-separated on the command line
    /// because they are short and there are few of them. Only the transducer
    /// and Qwen3 presets can use them; the rest ignore them.
    hotwords: Vec<String>,
}

impl Args {
    /// Hand-rolled rather than clap: five flags, all passed by tcode itself,
    /// none of them worth a dependency and a help screen nobody will read.
    fn parse(argv: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut args = Args {
            list_models: false,
            // Empty rather than a name: `model::find` owns the default, so
            // there is one place that knows which preset is first.
            model: String::new(),
            language: "auto".into(),
            device: String::new(),
            model_dir: None,
            download_base: String::new(),
            hotwords: Vec::new(),
        };
        let mut argv = argv.skip(1);
        while let Some(flag) = argv.next() {
            if flag == "--list-models" {
                args.list_models = true;
                continue;
            }
            let Some(value) = argv.next() else {
                return Err(format!("{flag} needs a value"));
            };
            match flag.as_str() {
                "--model" => args.model = value,
                "--language" => args.language = value,
                "--device" => args.device = value,
                "--model-dir" => args.model_dir = Some(PathBuf::from(value)),
                "--download-base" => args.download_base = value,
                "--hotwords" => {
                    args.hotwords = value
                        .split(',')
                        .map(str::trim)
                        .filter(|word| !word.is_empty())
                        .map(str::to_string)
                        .collect()
                }
                // Named rather than shrugged off: silently ignoring a flag
                // would mean a setting the user chose quietly not applying.
                // tcode recognises this wording and turns it into a rebuild
                // instruction, because the usual cause is a stale binary.
                other => return Err(format!("unknown argument {other}")),
            }
        }
        Ok(args)
    }
}

fn main() {
    let events = Events;
    let args = match Args::parse(std::env::args()) {
        Ok(args) => args,
        Err(message) => {
            events.error(message);
            std::process::exit(2);
        }
    };
    if let Err(message) = run(args, &events) {
        events.error(message);
        std::process::exit(1);
    }
}

fn run(args: Args, events: &Events) -> Result<(), String> {
    // Answered before anything is opened or downloaded: the picker asks this
    // while the user is still deciding.
    if args.list_models {
        events.send(&Event::Models {
            models: model::PRESETS
                .iter()
                .map(|preset| protocol::ModelInfo {
                    name: preset.name.to_string(),
                    note: preset.note.to_string(),
                })
                .collect(),
        });
        return Ok(());
    }
    let dir = match args.model_dir {
        Some(dir) => dir,
        None => model::default_dir()?,
    };
    let preset = model::find(&args.model)?;
    let model = model::ensure(&dir, preset, &args.download_base, &mut |pct| {
        events.send(&Event::Downloading { pct })
    })?;
    let recognizer = asr::Recognizer::load(&model, &args.language, &args.hotwords, &dir)?;

    // The microphone opens before `ready`, so the first take does not pay for
    // device setup and cannot fail after tcode has told the user to speak.
    let capture = capture::Capture::open(&args.device)?;
    let sample_rate = capture.sample_rate();
    events.send(&Event::Ready);

    let stop_meter = Arc::new(AtomicBool::new(false));
    let meter = spawn_meter(capture.meter(), events.clone(), stop_meter.clone());

    // stdin closing is how tcode says it is done with us.
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let Some(command) = protocol::parse(&line) else {
            continue;
        };
        match command {
            Command::Start => capture.start(),
            Command::Cancel => {
                capture.take();
            }
            Command::Stop => {
                let samples = capture.take();
                if samples.is_empty() {
                    events.send(&Event::Transcript {
                        text: String::new(),
                    });
                    continue;
                }
                match recognizer.transcribe(&samples, sample_rate) {
                    Ok(text) => events.send(&Event::Transcript { text }),
                    // A failed take is not a failed session: report it and
                    // stay up, because the microphone still works.
                    Err(message) => events.error(message),
                }
            }
        }
    }

    stop_meter.store(true, Ordering::Relaxed);
    let _ = meter.join();
    Ok(())
}

/// Publishes the input level while a take is running. Its own thread because
/// the main one is blocked on stdin, and the audio callback must not do I/O.
fn spawn_meter(
    meter: capture::Meter,
    events: Events,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            std::thread::sleep(METER_INTERVAL);
            if meter.is_recording() {
                events.send(&Event::Level { rms: meter.level() });
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::Args;

    #[test]
    fn arguments_parse_in_any_order_and_default_sanely() {
        let args = Args::parse(
            [
                "tcode-voiced",
                "--language",
                "zh",
                "--model-dir",
                "/models",
                "--model",
                "qwen3",
                "--hotwords",
                "tokio, spawn_blocking ,,Cargo.toml",
            ]
            .iter()
            .map(|s| s.to_string()),
        )
        .expect("parses");
        assert_eq!(args.language, "zh");
        assert_eq!(args.model, "qwen3");
        // Trimmed, and empty entries dropped, so a trailing comma is harmless.
        assert_eq!(args.hotwords, ["tokio", "spawn_blocking", "Cargo.toml"]);
        assert_eq!(
            args.model_dir.as_deref(),
            Some(std::path::Path::new("/models"))
        );
        assert!(args.device.is_empty(), "the system default");

        let bare = Args::parse(["tcode-voiced"].iter().map(|s| s.to_string())).expect("parses");
        assert_eq!(bare.language, "auto");
        assert!(bare.model.is_empty(), "model::find picks the default");
        assert!(Args::parse(["tcode-voiced", "--language"].iter().map(|s| s.to_string())).is_err());
    }
}
