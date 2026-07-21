//! sherpa-onnx, pointed at whichever model `model.rs` resolved.
//!
//! Loaded once at startup: models are hundreds of megabytes on disk and several
//! seconds to initialise, which is time nobody should pay per sentence. Hotwords
//! are part of that construction, so changing them restarts the process — the
//! same deal as changing models.

use std::path::{Path, PathBuf};

use sherpa_onnx::{
    OfflineQwen3ASRModelConfig, OfflineRecognizer, OfflineRecognizerConfig,
    OfflineSenseVoiceModelConfig, OfflineTransducerModelConfig,
};

use crate::model::{Layout, Model};

/// How strongly a hotword outweighs what the model would otherwise have heard.
/// sherpa's own default; high enough to rescue a rare word, low enough not to
/// hallucinate one into silence.
const HOTWORD_SCORE: f32 = 2.0;

pub struct Recognizer {
    inner: OfflineRecognizer,
    /// Deleted on the way out. sherpa reads it during construction, but the
    /// path stays borrowed by the config for the recogniser's lifetime.
    hotwords_file: Option<PathBuf>,
}

impl Recognizer {
    /// `hotwords` are whole words or phrases. Empty means no biasing at all,
    /// which for the transducer also means the faster decoder — nobody should
    /// pay for beam search they are not using.
    pub fn load(model: &Model, language: &str, hotwords: &[String], scratch: &Path) -> Result<Self, String> {
        let mut config = OfflineRecognizerConfig::default();
        let mut hotwords_file = None;
        // Every family fills exactly one slot; sherpa picks the recogniser by
        // which one is populated. `tokens` is shared, except for Qwen3, which
        // carries a tokenizer directory of its own.
        match model.layout {
            Layout::SenseVoice { model: weights } => {
                config.model_config.sense_voice = OfflineSenseVoiceModelConfig {
                    model: Some(model.path(weights)),
                    // The one family with a language switch. The others are
                    // bilingual by construction and have nothing to set.
                    language: Some(language.to_string()),
                    // Inverse text normalisation: punctuation and digits, which
                    // is the difference between a transcript you can send and
                    // one you edit.
                    use_itn: true,
                };
                config.model_config.tokens = Some(model.path("tokens.txt"));
            }
            Layout::Transducer {
                encoder,
                decoder,
                joiner,
                bpe,
            } => {
                config.model_config.transducer = OfflineTransducerModelConfig {
                    encoder: Some(model.path(encoder)),
                    decoder: Some(model.path(decoder)),
                    joiner: Some(model.path(joiner)),
                };
                config.model_config.tokens = Some(model.path("tokens.txt"));
                if !hotwords.is_empty() {
                    // Contextual biasing works by re-scoring alternatives, so
                    // there have to be alternatives: greedy search has none.
                    config.decoding_method = Some("modified_beam_search".into());
                    // Chinese by character, English by subword — the mixed unit
                    // the model was trained on, and the only one under which a
                    // hotword list can hold both.
                    config.model_config.modeling_unit = Some("cjkchar+bpe".into());
                    config.model_config.bpe_vocab = Some(model.bpe_vocab(bpe)?);
                    let path = write_hotwords(scratch, hotwords)?;
                    config.hotwords_file = Some(path.display().to_string());
                    config.hotwords_score = HOTWORD_SCORE;
                    hotwords_file = Some(path);
                }
            }
            Layout::Qwen3 {
                conv_frontend,
                encoder,
                decoder,
                tokenizer,
            } => {
                config.model_config.qwen3_asr = OfflineQwen3ASRModelConfig {
                    conv_frontend: Some(model.path(conv_frontend)),
                    encoder: Some(model.path(encoder)),
                    decoder: Some(model.path(decoder)),
                    tokenizer: Some(model.path(tokenizer)),
                    // An LLM decoder takes its bias as prompt text, not as a
                    // tokenised list — comma-separated, as sherpa documents.
                    hotwords: (!hotwords.is_empty()).then(|| hotwords.join(",")),
                    ..Default::default()
                };
            }
        }
        // sherpa reports failure as `None` and prints its own diagnosis to
        // stderr, which tcode keeps and shows.
        OfflineRecognizer::create(&config)
            .map(|inner| Self {
                inner,
                hotwords_file,
            })
            .ok_or_else(|| "cannot load the speech model — see the lines above".to_string())
    }

    /// Transcribe one take. Blocking, and deliberately so: the caller has
    /// nothing else to do until this answers.
    pub fn transcribe(&self, samples: &[f32], sample_rate: u32) -> Result<String, String> {
        let stream = self.inner.create_stream();
        stream.accept_waveform(sample_rate as i32, samples);
        self.inner.decode(&stream);
        Ok(stream
            .get_result()
            .map(|result| result.text)
            .unwrap_or_default())
    }
}

impl Drop for Recognizer {
    fn drop(&mut self) {
        if let Some(path) = &self.hotwords_file {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// sherpa takes hotwords as a file, so one has to exist. It is named after this
/// process rather than shared, so two tcode windows with different word lists
/// cannot read each other's.
fn write_hotwords(scratch: &Path, hotwords: &[String]) -> Result<PathBuf, String> {
    let path = scratch.join(format!("hotwords-{}.txt", std::process::id()));
    // One phrase per line is the whole format; sherpa splits each into the
    // model's own pieces using the vocabulary above.
    let body = hotwords.join("\n") + "\n";
    std::fs::write(&path, body).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(path)
}
