//! Microphone capture.
//!
//! The audio callback does three things and nothing else: mix to mono, append
//! to a bounded buffer, publish a level. No inference, no I/O, no waiting on
//! the main thread — a callback that misses its deadline is a click in the
//! recording, and it is the one place here with a hard deadline.
//!
//! It does take a mutex, which a strict real-time design would not. The
//! contention is two locks per take (`start` and `take`, both on the main
//! thread and both O(1) apart from the move), so the callback effectively
//! never waits. A lock-free ring buffer would cost a dependency and a second
//! copy to buy back nothing measurable at these rates.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SampleFormat;

/// A hold that somehow never ends stops costing memory here rather than
/// growing until the machine notices. 10 minutes at 48kHz mono f32 ≈ 115MB.
const MAX_SAMPLES: usize = 48_000 * 60 * 10;

#[derive(Default)]
struct Shared {
    samples: Mutex<Vec<f32>>,
    recording: AtomicBool,
    /// Peak of the last callback, as `f32::to_bits`. An atomic because the
    /// meter thread reads it while the callback writes it.
    level: AtomicU32,
}

pub struct Capture {
    /// Held only to keep the stream alive; dropping it closes the device.
    /// `cpal::Stream` is neither `Send` nor `Sync`, which is why `Capture`
    /// stays on the thread that opened it and the meter thread gets a
    /// `Meter` instead.
    _stream: cpal::Stream,
    shared: Arc<Shared>,
    sample_rate: u32,
}

/// The part of a capture that can cross threads: what the level meter reads.
/// Deliberately read-only — starting and stopping a take belongs to whoever
/// owns the stream.
pub struct Meter(Arc<Shared>);

impl Meter {
    pub fn is_recording(&self) -> bool {
        self.0.recording.load(Ordering::Relaxed)
    }

    pub fn level(&self) -> f32 {
        f32::from_bits(self.0.level.load(Ordering::Relaxed))
    }
}

impl Capture {
    /// Open `device` (or the system default) and leave the stream running with
    /// capture gated off. Opening once at startup rather than per take is what
    /// makes the first syllable after the key goes down land in the recording.
    pub fn open(device_name: &str) -> Result<Self, String> {
        let host = cpal::default_host();
        let device = if device_name.is_empty() {
            host.default_input_device()
                .ok_or_else(|| "no default input device".to_string())?
        } else {
            host.input_devices()
                .map_err(|e| format!("cannot list input devices: {e}"))?
                .find(|d| d.name().map(|n| n == device_name).unwrap_or(false))
                .ok_or_else(|| format!("no input device named '{device_name}'"))?
        };
        let config = device
            .default_input_config()
            .map_err(|e| format!("no usable input config: {e}"))?;
        let sample_rate = config.sample_rate().0;
        let channels = config.channels() as usize;
        let format = config.sample_format();
        let stream_config: cpal::StreamConfig = config.into();

        let shared = Arc::new(Shared::default());
        let on_error = |e| eprintln!("audio stream error: {e}");
        let stream = match format {
            SampleFormat::F32 => {
                let sink = shared.clone();
                device.build_input_stream(
                    &stream_config,
                    move |data: &[f32], _| sink.push(data, channels, |s| s),
                    on_error,
                    None,
                )
            }
            SampleFormat::I16 => {
                let sink = shared.clone();
                device.build_input_stream(
                    &stream_config,
                    move |data: &[i16], _| {
                        sink.push(data, channels, |s| s as f32 / i16::MAX as f32)
                    },
                    on_error,
                    None,
                )
            }
            SampleFormat::U16 => {
                let sink = shared.clone();
                device.build_input_stream(
                    &stream_config,
                    move |data: &[u16], _| {
                        sink.push(data, channels, |s| {
                            (s as f32 - u16::MAX as f32 / 2.0) / (u16::MAX as f32 / 2.0)
                        })
                    },
                    on_error,
                    None,
                )
            }
            other => return Err(format!("unsupported sample format {other:?}")),
        }
        .map_err(|e| format!("cannot open the microphone: {e}"))?;
        stream
            .play()
            .map_err(|e| format!("cannot start the microphone: {e}"))?;

        Ok(Self {
            _stream: stream,
            shared,
            sample_rate,
        })
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn start(&self) {
        self.shared.samples.lock().expect("samples").clear();
        self.shared.level.store(0, Ordering::Relaxed);
        self.shared.recording.store(true, Ordering::Relaxed);
    }

    /// Stop capturing and take the audio. The buffer is left empty, so a take
    /// can never be transcribed twice.
    pub fn take(&self) -> Vec<f32> {
        self.shared.recording.store(false, Ordering::Relaxed);
        std::mem::take(&mut *self.shared.samples.lock().expect("samples"))
    }

    pub fn meter(&self) -> Meter {
        Meter(self.shared.clone())
    }
}

impl Shared {
    fn push<T: Copy>(&self, data: &[T], channels: usize, to_f32: impl Fn(T) -> f32) {
        if !self.recording.load(Ordering::Relaxed) {
            return;
        }
        let mut buffer = self.samples.lock().expect("samples");
        let mut peak = 0.0f32;
        for frame in data.chunks(channels.max(1)) {
            let mono = frame.iter().map(|&s| to_f32(s)).sum::<f32>() / frame.len() as f32;
            peak = peak.max(mono.abs());
            if buffer.len() < MAX_SAMPLES {
                buffer.push(mono);
            }
        }
        self.level.store(peak.to_bits(), Ordering::Relaxed);
    }
}
