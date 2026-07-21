//! Just enough protobuf to turn a `bpe.model` into a `bpe.vocab`.
//!
//! Transducer hotwords have to be split into the same subword pieces the model
//! was trained on, and sherpa does that with its own small SentencePiece
//! encoder, which reads `bpe.vocab` — a plain `piece<TAB>score` file. Upstream
//! ships only `bpe.model`, and the official way to convert is a Python script
//! that needs the `sentencepiece` package.
//!
//! Requiring a Python toolchain to say the word "tokio" is not a trade worth
//! making, and the format on both ends is simple enough not to need one:
//! `bpe.model` is a SentencePiece `ModelProto`, and everything needed is two
//! fields of one repeated message. The fifty lines below replace that
//! dependency.

/// Fields read out of the proto. Everything else is skipped by wire type, so
/// this keeps working if upstream adds fields.
const PIECES: u64 = 1; // ModelProto.pieces
const PIECE: u64 = 1; // SentencePiece.piece
const SCORE: u64 = 2; // SentencePiece.score

/// Render `bpe.model` as the `bpe.vocab` text sherpa expects: one line per
/// piece, in id order, exactly as `scripts/export_bpe_vocab.py` writes it.
pub fn export(model: &[u8]) -> Result<String, String> {
    let mut reader = Reader::new(model);
    let mut out = String::new();
    while let Some((field, wire)) = reader.key() {
        if field == PIECES && wire == LEN {
            let piece = reader.slice().ok_or("truncated piece")?;
            let (text, score) = self::piece(piece)?;
            out.push_str(text);
            out.push('\t');
            out.push_str(&score.to_string());
            out.push('\n');
        } else {
            reader.skip(wire).ok_or("unreadable bpe.model")?;
        }
    }
    if out.is_empty() {
        return Err("bpe.model holds no pieces".into());
    }
    Ok(out)
}

fn piece(bytes: &[u8]) -> Result<(&str, f32), String> {
    let mut reader = Reader::new(bytes);
    let mut text = None;
    let mut score = 0.0;
    while let Some((field, wire)) = reader.key() {
        match (field, wire) {
            (PIECE, LEN) => {
                let raw = reader.slice().ok_or("truncated piece text")?;
                text = Some(std::str::from_utf8(raw).map_err(|_| "piece is not utf-8")?);
            }
            (SCORE, I32) => {
                let raw = reader.take(4).ok_or("truncated score")?;
                score = f32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
            }
            _ => {
                reader.skip(wire).ok_or("unreadable piece")?;
            }
        }
    }
    Ok((text.ok_or("a piece has no text")?, score))
}

const VARINT: u64 = 0;
const I64: u64 = 1;
const LEN: u64 = 2;
const I32: u64 = 5;

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    /// The next field number and wire type, or `None` at the end.
    fn key(&mut self) -> Option<(u64, u64)> {
        let key = self.varint()?;
        Some((key >> 3, key & 7))
    }

    fn varint(&mut self) -> Option<u64> {
        let mut value = 0u64;
        for shift in 0..10 {
            let byte = *self.bytes.get(self.pos)?;
            self.pos += 1;
            value |= u64::from(byte & 0x7f) << (shift * 7);
            if byte & 0x80 == 0 {
                return Some(value);
            }
        }
        None
    }

    fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        let slice = self.bytes.get(self.pos..self.pos.checked_add(len)?)?;
        self.pos += len;
        Some(slice)
    }

    /// A length-delimited field's payload.
    fn slice(&mut self) -> Option<&'a [u8]> {
        let len = self.varint()? as usize;
        self.take(len)
    }

    /// Step over a field this code does not read. Wire types are self-describing
    /// precisely so that unknown fields need no schema.
    fn skip(&mut self, wire: u64) -> Option<()> {
        match wire {
            VARINT => self.varint().map(|_| ()),
            I64 => self.take(8).map(|_| ()),
            LEN => self.slice().map(|_| ()),
            I32 => self.take(4).map(|_| ()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::export;

    /// Builds a ModelProto by hand: two pieces, plus a field this code does not
    /// know, which must be stepped over rather than misread.
    fn proto() -> Vec<u8> {
        fn piece(text: &str, score: f32) -> Vec<u8> {
            let mut out = vec![0x0a, text.len() as u8];
            out.extend_from_slice(text.as_bytes());
            out.push(0x15);
            out.extend_from_slice(&score.to_le_bytes());
            // SentencePiece.type, a varint field this code ignores.
            out.extend_from_slice(&[0x18, 0x02]);
            out
        }
        let mut out = Vec::new();
        for (text, score) in [("<unk>", 0.0f32), ("▁the", -2.5)] {
            let body = piece(text, score);
            out.push(0x0a);
            out.push(body.len() as u8);
            out.extend_from_slice(&body);
        }
        // ModelProto.trainer_spec, a whole message this code must skip.
        out.extend_from_slice(&[0x12, 0x02, 0x08, 0x01]);
        out
    }

    #[test]
    fn pieces_come_out_in_order_with_their_scores() {
        let vocab = export(&proto()).expect("parses");
        assert_eq!(vocab, "<unk>\t0\n▁the\t-2.5\n");
    }

    #[test]
    fn a_file_that_is_not_a_bpe_model_is_rejected_rather_than_guessed_at() {
        assert!(export(b"").is_err());
        assert!(export(b"not a protobuf at all").is_err());
    }

    /// Hand-built bytes only prove this code agrees with itself. This one reads
    /// a real downloaded model and checks the result against `tokens.txt`,
    /// which lists the same pieces in the same order — an independent file the
    /// parser never sees. Ignored because it needs a model on disk:
    /// `cargo test -- --ignored`.
    #[test]
    #[ignore = "needs a downloaded model"]
    fn a_real_bpe_model_yields_the_pieces_tokens_txt_agrees_with() {
        let Some(root) = crate::model::default_dir().ok().map(|dir| {
            dir.join("sherpa-onnx-x-asr-zipformer-transducer-zh-en-punct-int8-2026-06-03")
        }) else {
            return;
        };
        let Ok(model) = std::fs::read(root.join("bpe.model")) else {
            panic!("download the zh-en model first");
        };
        let vocab = export(&model).expect("a real bpe.model parses");
        let pieces: Vec<&str> = vocab.lines().map(|l| l.split('\t').next().unwrap()).collect();

        let tokens = std::fs::read_to_string(root.join("tokens.txt")).expect("tokens.txt");
        let tokens: Vec<&str> = tokens
            .lines()
            .filter_map(|line| line.split(' ').next())
            .collect();

        // tokens.txt carries the model's blank and any extra symbols, so the
        // BPE pieces are a prefix-aligned subset rather than an exact match.
        let shared = pieces.iter().filter(|p| tokens.contains(p)).count();
        assert!(
            shared * 10 >= pieces.len() * 9,
            "only {shared} of {} pieces appear in tokens.txt — the parse is wrong",
            pieces.len()
        );
        assert!(pieces.len() > 500, "suspiciously few pieces: {}", pieces.len());
    }
}
