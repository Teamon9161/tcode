//! Which model, and getting it onto the machine.
//!
//! Models are hundreds of megabytes, which is why none of them ship with tcode
//! and none are fetched until someone actually presses the key. The download
//! reports progress because a silent 200MB is indistinguishable from a hang.
//!
//! The list below is a table on purpose. Every model sherpa supports differs in
//! two ways only — which files the archive holds, and which of sherpa's model
//! slots they go in — so adding one is a row here plus an arm in `asr.rs`, and
//! nothing else in this program changes.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

const DEFAULT_BASE: &str = "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models";

/// The sherpa model family a preset belongs to, carrying the file names that
/// family needs. The names are spelled out rather than derived: upstream
/// archives disagree about whether a part is quantised and whether it carries
/// an epoch suffix, so any rule for guessing them would be wrong by the second
/// entry.
#[derive(Clone, Copy, Debug)]
pub enum Layout {
    SenseVoice {
        model: &'static str,
    },
    /// The only family here that can be biased towards particular words, and
    /// the reason `bpe` exists: hotwords are matched as subword pieces, so the
    /// tokenizer has to be readable.
    Transducer {
        encoder: &'static str,
        decoder: &'static str,
        joiner: &'static str,
        bpe: &'static str,
    },
    Qwen3 {
        conv_frontend: &'static str,
        encoder: &'static str,
        decoder: &'static str,
        tokenizer: &'static str,
    },
}

impl Layout {
    /// Everything that must be on disk before the model counts as installed.
    /// `tokens.txt` is separate because Qwen3 carries a tokenizer directory
    /// instead — see `asr.rs`.
    fn files(self) -> Vec<&'static str> {
        match self {
            Layout::SenseVoice { model } => vec![model, "tokens.txt"],
            Layout::Transducer {
                encoder,
                decoder,
                joiner,
                bpe,
            } => vec![encoder, decoder, joiner, bpe, "tokens.txt"],
            Layout::Qwen3 {
                conv_frontend,
                encoder,
                decoder,
                tokenizer,
            } => vec![conv_frontend, encoder, decoder, tokenizer],
        }
    }
}

#[derive(Debug)]
pub struct Preset {
    /// What goes in `[voice] model`.
    pub name: &'static str,
    /// The release asset. The directory it unpacks to is this minus the suffix,
    /// which is upstream's own convention and has held for every asset so far.
    pub archive: &'static str,
    pub layout: Layout,
    /// Shown when someone picks a name that does not exist, so the reply is a
    /// menu rather than a rejection.
    pub note: &'static str,
}

/// Ordered best-value first: `PRESETS[0]` is what a fresh install downloads.
pub const PRESETS: &[Preset] = &[
    Preset {
        name: "zh-en",
        archive: "sherpa-onnx-x-asr-zipformer-transducer-zh-en-punct-int8-2026-06-03.tar.bz2",
        layout: Layout::Transducer {
            encoder: "encoder-epoch-99-avg-1.int8.onnx",
            decoder: "decoder-epoch-99-avg-1.onnx",
            joiner: "joiner-epoch-99-avg-1.int8.onnx",
            bpe: "bpe.model",
        },
        note: "136MB, zh/en code-switching, punctuation, hotwords (default)",
    },
    Preset {
        name: "sense-voice",
        archive: "sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2024-07-17.tar.bz2",
        layout: Layout::SenseVoice {
            model: "model.int8.onnx",
        },
        note: "163MB, zh/en/ja/ko/yue, fastest, no hotwords, weak on mixed speech",
    },
    Preset {
        name: "qwen3",
        archive: "sherpa-onnx-qwen3-asr-0.6B-int8-2026-03-25.tar.bz2",
        layout: Layout::Qwen3 {
            conv_frontend: "conv_frontend.onnx",
            encoder: "encoder.int8.onnx",
            decoder: "decoder.int8.onnx",
            tokenizer: "tokenizer",
        },
        note: "879MB, most accurate, hotwords, decodes with an LLM so it is slowest",
    },
];

/// Look a preset up by name. The failure lists the alternatives: a name is
/// something you get wrong once, and the reply should be enough to fix it.
pub fn find(name: &str) -> Result<&'static Preset, String> {
    let name = if name.is_empty() {
        PRESETS[0].name
    } else {
        name
    };
    PRESETS
        .iter()
        .find(|preset| preset.name == name)
        .ok_or_else(|| {
            let menu = PRESETS
                .iter()
                .map(|preset| format!("  {} — {}", preset.name, preset.note))
                .collect::<Vec<_>>()
                .join("\n");
            format!("there is no voice model called '{name}'. Pick one of:\n{menu}")
        })
}

pub struct Model {
    root: PathBuf,
    pub layout: Layout,
}

impl Model {
    /// A file inside the model directory, as sherpa wants it: an owned string.
    pub fn path(&self, file: &str) -> String {
        self.root.join(file).display().to_string()
    }

    /// The subword vocabulary sherpa needs to split hotwords, derived from the
    /// model's own tokenizer on first use and cached beside it. Upstream ships
    /// `bpe.model` but not this; see `bpe.rs` for why deriving beats depending.
    pub fn bpe_vocab(&self, bpe: &str) -> Result<String, String> {
        let vocab = self.root.join("bpe.vocab");
        if vocab.is_file() {
            return Ok(vocab.display().to_string());
        }
        let source = self.root.join(bpe);
        let bytes = fs::read(&source)
            .map_err(|e| format!("cannot read {}: {e}", source.display()))?;
        let text = crate::bpe::export(&bytes)
            .map_err(|reason| format!("cannot read {}: {reason}", source.display()))?;
        fs::write(&vocab, text).map_err(|e| format!("cannot write {}: {e}", vocab.display()))?;
        Ok(vocab.display().to_string())
    }
}

/// Where models live: `--model-dir`, else `~/.tcode/voice/models`.
pub fn default_dir() -> Result<PathBuf, String> {
    dirs::home_dir()
        .map(|home| home.join(".tcode").join("voice").join("models"))
        .ok_or_else(|| "cannot locate the home directory".to_string())
}

/// Resolve the model, downloading and unpacking it if this is the first run
/// with this preset. `progress` is called with 0-100 while downloading.
///
/// Presets live in their own directories, so switching between them and back
/// does not re-download.
pub fn ensure(
    dir: &Path,
    preset: &Preset,
    base: &str,
    progress: &mut dyn FnMut(u8),
) -> Result<Model, String> {
    let root = dir.join(directory(preset.archive));
    let model = Model {
        root,
        layout: preset.layout,
    };
    let missing = |model: &Model| {
        preset
            .layout
            .files()
            .into_iter()
            .find(|file| !Path::new(&model.path(file)).exists())
    };
    if missing(&model).is_none() {
        return Ok(model);
    }
    let base = if base.is_empty() {
        DEFAULT_BASE
    } else {
        base.trim_end_matches('/')
    };
    fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    download_and_unpack(
        &format!("{base}/{}", preset.archive),
        dir,
        preset.archive,
        progress,
    )?;

    if let Some(file) = missing(&model) {
        return Err(format!(
            "the archive unpacked but {} is not where it was expected",
            model.path(file)
        ));
    }
    Ok(model)
}

/// Upstream names the directory after the archive it came in.
fn directory(archive: &str) -> &str {
    archive.strip_suffix(".tar.bz2").unwrap_or(archive)
}

fn download_and_unpack(
    url: &str,
    dir: &Path,
    archive: &str,
    progress: &mut dyn FnMut(u8),
) -> Result<(), String> {
    let response = ureq::get(url)
        .call()
        .map_err(|e| format!("cannot download {url}: {e}"))?;
    let total: u64 = response
        .header("Content-Length")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);

    // Streamed to disk rather than held in memory: this is hundreds of
    // megabytes, and the machine may be doing something else.
    let temp = dir.join(format!("{archive}.part"));
    let mut file =
        fs::File::create(&temp).map_err(|e| format!("cannot write {}: {e}", temp.display()))?;
    let mut reader = response.into_reader();
    let mut buffer = vec![0u8; 256 * 1024];
    let mut done: u64 = 0;
    let mut last_pct = u8::MAX;
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|e| format!("download interrupted: {e}"))?;
        if read == 0 {
            break;
        }
        std::io::Write::write_all(&mut file, &buffer[..read])
            .map_err(|e| format!("cannot write {}: {e}", temp.display()))?;
        done += read as u64;
        if total > 0 {
            let pct = ((done * 100) / total).min(100) as u8;
            // Only on change: one event per percent, not per 256KB.
            if pct != last_pct {
                last_pct = pct;
                progress(pct);
            }
        }
    }
    drop(file);

    let file = fs::File::open(&temp).map_err(|e| format!("cannot read {}: {e}", temp.display()))?;
    tar::Archive::new(bzip2::read::BzDecoder::new(file))
        .unpack(dir)
        .map_err(|e| format!("cannot unpack {}: {e}", temp.display()))?;
    let _ = fs::remove_file(&temp);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{directory, find, Layout, PRESETS};

    #[test]
    fn every_preset_unpacks_into_a_directory_named_after_its_archive() {
        for preset in PRESETS {
            assert!(
                preset.archive.ends_with(".tar.bz2"),
                "{} is not an archive name",
                preset.name
            );
            assert!(!directory(preset.archive).contains(".tar"));
        }
    }

    #[test]
    fn an_unknown_name_is_answered_with_the_menu() {
        let reason = find("whisper").expect_err("not a preset");
        for preset in PRESETS {
            assert!(
                reason.contains(preset.name),
                "{} is missing from the menu",
                preset.name
            );
        }
        // Empty means "whatever the default is", not "no model".
        assert_eq!(find("").expect("defaults").name, PRESETS[0].name);
    }

    /// Whichever presets claim hotwords in their note must actually be a family
    /// that can do them — `asr.rs` silently ignores hotwords for the rest, so a
    /// wrong note is a promise the recogniser will not keep.
    #[test]
    fn only_the_families_that_can_bias_advertise_hotwords() {
        for preset in PRESETS {
            let capable = matches!(
                preset.layout,
                Layout::Transducer { .. } | Layout::Qwen3 { .. }
            );
            assert_eq!(
                capable,
                preset.note.contains("hotwords") && !preset.note.contains("no hotwords"),
                "{} says one thing and its layout another",
                preset.name
            );
        }
    }

    #[test]
    fn only_the_families_that_use_tokens_txt_ask_for_it() {
        for preset in PRESETS {
            let wants_tokens = preset.layout.files().contains(&"tokens.txt");
            assert_eq!(
                wants_tokens,
                !matches!(preset.layout, Layout::Qwen3 { .. }),
                "{} disagrees with asr.rs about tokens.txt",
                preset.name
            );
        }
    }
}
