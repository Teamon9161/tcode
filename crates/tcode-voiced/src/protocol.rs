//! The wire between tcode and this process: single-line JSON, commands in on
//! stdin, events out on stdout.
//!
//! Not JSON-RPC. There is nothing to correlate — one hold is one start/stop,
//! and everything sent back is unsolicited. Keeping it this small is what
//! makes the sidecar replaceable by anything that can print JSON lines.
//!
//! stdout carries events and nothing else. Human-readable diagnostics go to
//! stderr, which tcode keeps and shows if this process dies.

use std::io::Write;

use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(tag = "cmd", rename_all = "lowercase")]
pub enum Command {
    /// Open the microphone and start accumulating audio.
    Start,
    /// Stop, transcribe what was captured, and answer with a `transcript`.
    Stop,
    /// Stop and throw the audio away. No answer follows.
    Cancel,
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(tag = "event", rename_all = "lowercase")]
pub enum Event {
    /// Model loaded and microphone open. Commands are honoured from here on.
    Ready,
    /// Model download progress, 0-100.
    Downloading {
        pct: u8,
    },
    /// Input level, 0.0-1.0, while recording.
    Level {
        rms: f32,
    },
    Transcript {
        text: String,
    },
    Error {
        message: String,
    },
    /// The answer to `--list-models`, printed instead of starting a session.
    /// It is the model picker's only source of names: this binary knows which
    /// presets it was built with, and tcode does not.
    Models {
        models: Vec<ModelInfo>,
    },
}

/// One row of the model picker. Ordered as `model::PRESETS` is, so the first
/// entry is the default.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct ModelInfo {
    pub name: String,
    pub note: String,
}

/// Writes events to stdout. Cloneable and internally locked, because the level
/// meter is emitted from its own thread while the main one is blocked reading
/// stdin.
#[derive(Clone, Default)]
pub struct Events;

impl Events {
    pub fn send(&self, event: &Event) {
        let Ok(line) = serde_json::to_string(event) else {
            return;
        };
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{line}");
        let _ = out.flush();
    }

    pub fn error(&self, message: impl Into<String>) {
        let message = message.into();
        eprintln!("{message}");
        self.send(&Event::Error { message });
    }
}

/// Parse one line of stdin. Unrecognised lines are `None` rather than fatal:
/// a newer tcode must be able to send a command this build has never heard of
/// without killing dictation outright.
pub fn parse(line: &str) -> Option<Command> {
    serde_json::from_str(line.trim()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_parse_and_junk_is_ignored() {
        assert_eq!(parse(r#"{"cmd":"start"}"#), Some(Command::Start));
        assert_eq!(parse(r#"  {"cmd":"stop"}  "#), Some(Command::Stop));
        assert_eq!(parse(r#"{"cmd":"cancel"}"#), Some(Command::Cancel));
        assert_eq!(parse(r#"{"cmd":"teleport"}"#), None);
        assert_eq!(parse("not json"), None);
    }

    /// The field names here are the contract with `tcode-tui`'s `WireEvent`.
    #[test]
    fn events_serialize_to_the_shape_tcode_parses() {
        let cases = [
            (Event::Ready, r#"{"event":"ready"}"#),
            (
                Event::Downloading { pct: 37 },
                r#"{"event":"downloading","pct":37}"#,
            ),
            (Event::Level { rms: 0.5 }, r#"{"event":"level","rms":0.5}"#),
            (
                Event::Transcript {
                    text: "改一下 editor".into(),
                },
                r#"{"event":"transcript","text":"改一下 editor"}"#,
            ),
            (
                Event::Error {
                    message: "no input device".into(),
                },
                r#"{"event":"error","message":"no input device"}"#,
            ),
            (
                Event::Models {
                    models: vec![ModelInfo {
                        name: "zh-en".into(),
                        note: "136MB".into(),
                    }],
                },
                r#"{"event":"models","models":[{"name":"zh-en","note":"136MB"}]}"#,
            ),
        ];
        for (event, expected) in cases {
            assert_eq!(serde_json::to_string(&event).expect("encode"), expected);
        }
    }
}
