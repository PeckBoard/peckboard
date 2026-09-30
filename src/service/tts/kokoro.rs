//! Kokoro-82M inference: misaki G2P → Kokoro token ids → ONNX Runtime →
//! 24 kHz mono f32 samples. Pure helpers (token mapping, voices NPZ
//! parsing, WAV encoding) are separated from [`Kokoro`] so they are unit
//! testable without the model.

use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::sync::{LazyLock, Mutex};

use anyhow::{Context, anyhow, bail};
use misaki_rs::{G2P, Language};

pub const SAMPLE_RATE: u32 = 24_000;
/// Rows per voice pack: the style vector is chosen by token count, so a
/// single inference can carry at most `VOICE_ROWS - 1` tokens.
const VOICE_ROWS: usize = 510;
const STYLE_DIM: usize = 256;
const MAX_TOKENS: usize = VOICE_ROWS - 1;

/// Kokoro's phoneme vocab, copied verbatim from hexgrad/Kokoro-82M
/// `config.json`.
static VOCAB: LazyLock<HashMap<char, i64>> = LazyLock::new(|| {
    let raw: HashMap<String, i64> =
        serde_json::from_str(include_str!("kokoro_vocab.json")).expect("embedded vocab");
    raw.into_iter()
        .filter_map(|(k, v)| k.chars().next().map(|c| (c, v)))
        .collect()
});

/// misaki-rs joins diphthongs/affricates with a zero-width joiner
/// (`e‍ɪ`); Kokoro was trained on misaki's single-symbol spellings.
const JOINED: &[(&str, &str)] = &[
    ("e\u{200d}ɪ", "A"),
    ("a\u{200d}ɪ", "I"),
    ("o\u{200d}ʊ", "O"),
    ("ə\u{200d}ʊ", "Q"),
    ("a\u{200d}ʊ", "W"),
    ("ɔ\u{200d}ɪ", "Y"),
    ("d\u{200d}ʒ", "ʤ"),
    ("t\u{200d}ʃ", "ʧ"),
];

/// Normalise a misaki phoneme string into Kokoro's alphabet: fold joined
/// pairs, then drop any char the vocab doesn't know (ZWJ, `❓`, stray
/// punctuation).
pub fn normalize_phonemes(ph: &str) -> String {
    let mut s = ph.to_string();
    for (from, to) in JOINED {
        s = s.replace(from, to);
    }
    s.chars().filter(|c| VOCAB.contains_key(c)).collect()
}

/// Phoneme string → token ids (unknown chars dropped).
pub fn tokenize(ph: &str) -> Vec<i64> {
    ph.chars().filter_map(|c| VOCAB.get(&c).copied()).collect()
}

/// Split a normalised phoneme string into chunks of at most `MAX_TOKENS`
/// chars, breaking on spaces where possible.
fn chunk_phonemes(ph: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for word in ph.split(' ') {
        let wlen = word.chars().count();
        if !cur.is_empty() && cur.chars().count() + 1 + wlen > MAX_TOKENS {
            out.push(std::mem::take(&mut cur));
        }
        if wlen > MAX_TOKENS {
            // Pathological unbroken run: hard-split it.
            let chars: Vec<char> = word.chars().collect();
            for piece in chars.chunks(MAX_TOKENS) {
                out.push(piece.iter().collect());
            }
            continue;
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(word);
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

/// Parse `voices-v1.0.bin` — an (uncompressed or deflated) NPZ whose
/// members are `<voice>.npy` float32 arrays of shape `[510, 1, 256]`.
pub fn parse_voices_npz(bytes: &[u8]) -> anyhow::Result<HashMap<String, Vec<f32>>> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).context("voices: not a zip")?;
    let mut out = HashMap::new();
    for i in 0..zip.len() {
        let mut f = zip.by_index(i)?;
        let Some(name) = f.name().strip_suffix(".npy").map(str::to_string) else {
            continue;
        };
        let mut buf = Vec::with_capacity(f.size() as usize);
        f.read_to_end(&mut buf)?;
        let data = parse_npy_f32(&buf).with_context(|| format!("voice {name}"))?;
        if data.len() != VOICE_ROWS * STYLE_DIM {
            bail!(
                "voice {name}: expected {} floats, got {}",
                VOICE_ROWS * STYLE_DIM,
                data.len()
            );
        }
        out.insert(name, data);
    }
    Ok(out)
}

/// Minimal `.npy` reader: little-endian f32, C order.
fn parse_npy_f32(buf: &[u8]) -> anyhow::Result<Vec<f32>> {
    if buf.len() < 10 || &buf[..6] != b"\x93NUMPY" {
        bail!("not an npy file");
    }
    let (hlen, start) = match buf[6] {
        1 => (u16::from_le_bytes([buf[8], buf[9]]) as usize, 10),
        2 | 3 => {
            if buf.len() < 12 {
                bail!("truncated npy header");
            }
            (
                u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]) as usize,
                12,
            )
        }
        v => bail!("unsupported npy version {v}"),
    };
    let header = std::str::from_utf8(
        buf.get(start..start + hlen)
            .ok_or_else(|| anyhow!("truncated"))?,
    )?;
    if !header.contains("'<f4'") {
        bail!("expected '<f4' dtype, header: {header}");
    }
    if header.contains("'fortran_order': True") {
        bail!("fortran order unsupported");
    }
    let body = &buf[start + hlen..];
    Ok(body
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect())
}

/// 16-bit PCM mono WAV.
pub fn encode_wav(samples: &[f32], rate: u32) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut w = Vec::with_capacity(44 + data_len as usize);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + data_len).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes()); // PCM
    w.extend_from_slice(&1u16.to_le_bytes()); // mono
    w.extend_from_slice(&rate.to_le_bytes());
    w.extend_from_slice(&(rate * 2).to_le_bytes());
    w.extend_from_slice(&2u16.to_le_bytes());
    w.extend_from_slice(&16u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        let v = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
        w.extend_from_slice(&v.to_le_bytes());
    }
    w
}

/// Loaded model + voices. `Session::run` takes `&mut self`, so the session
/// sits behind a mutex; the caller's semaphore bounds concurrency anyway.
pub struct Kokoro {
    session: Mutex<ort::session::Session>,
    voices: HashMap<String, Vec<f32>>,
    input_names: [String; 3],
    speed_is_int: bool,
    g2p_us: Mutex<Option<G2P>>,
    g2p_gb: Mutex<Option<G2P>>,
}

impl Kokoro {
    /// Load ONNX Runtime from `ort_lib`, then the model and voices.
    pub fn load(ort_lib: &Path, model: &Path, voices: &Path) -> anyhow::Result<Self> {
        ort::init_from(ort_lib)
            .map_err(|e| anyhow!("load ONNX Runtime {}: {e}", ort_lib.display()))?
            .with_name("peckboard-tts")
            .commit();
        let threads = std::thread::available_parallelism()
            .map(|n| n.get() / 2)
            .unwrap_or(1)
            .clamp(1, 8);
        let session = ort::session::Session::builder()
            .map_err(|e| anyhow!("{e}"))?
            .with_intra_threads(threads)
            .map_err(|e| anyhow!("{e}"))?
            .commit_from_file(model)
            .map_err(|e| anyhow!("load model: {e}"))?;
        let names: Vec<String> = session
            .inputs()
            .iter()
            .map(|i| i.name().to_string())
            .collect();
        // Two published exports: (tokens, style, speed:f32) and
        // (input_ids, style, speed:i32).
        let (input_names, speed_is_int) = if names.iter().any(|n| n == "input_ids") {
            (["input_ids".into(), "style".into(), "speed".into()], true)
        } else {
            (["tokens".into(), "style".into(), "speed".into()], false)
        };
        let voices = parse_voices_npz(&std::fs::read(voices)?)?;
        let engine = Self {
            session: Mutex::new(session),
            voices,
            input_names,
            speed_is_int,
            g2p_us: Mutex::new(None),
            g2p_gb: Mutex::new(None),
        };
        // Warm up (US lexicon parse + first ORT run) so the user's first
        // sentence doesn't pay for it; a failure here surfaces as "not ready".
        engine.synthesize("Ready.", super::DEFAULT_VOICE, 1.0)?;
        Ok(engine)
    }

    fn phonemize(&self, text: &str, british: bool) -> anyhow::Result<String> {
        let slot = if british { &self.g2p_gb } else { &self.g2p_us };
        let mut g = slot.lock().unwrap();
        let g2p = g.get_or_insert_with(|| {
            G2P::new(if british {
                Language::EnglishGB
            } else {
                Language::EnglishUS
            })
        });
        let (ph, _) = g2p.g2p(text).map_err(|e| anyhow!("g2p: {e:?}"))?;
        Ok(normalize_phonemes(&ph))
    }

    /// Synthesize `text` → f32 samples at [`SAMPLE_RATE`].
    pub fn synthesize(&self, text: &str, voice: &str, speed: f32) -> anyhow::Result<Vec<f32>> {
        let pack = self
            .voices
            .get(voice)
            .ok_or_else(|| anyhow!("unknown voice '{voice}'"))?;
        let ph = self.phonemize(text, voice.starts_with('b'))?;
        let mut audio = Vec::new();
        for chunk in chunk_phonemes(&ph) {
            let toks = tokenize(&chunk);
            if toks.is_empty() {
                continue;
            }
            let n = toks.len();
            let mut padded = Vec::with_capacity(n + 2);
            padded.push(0);
            padded.extend(toks);
            padded.push(0);
            let style = pack[n * STYLE_DIM..(n + 1) * STYLE_DIM].to_vec();
            let tokens = ort::value::Tensor::from_array(([1usize, n + 2], padded))?;
            let style = ort::value::Tensor::from_array(([1usize, STYLE_DIM], style))?;
            let mut session = self.session.lock().unwrap();
            let [a, b, c] = &self.input_names;
            let outputs = if self.speed_is_int {
                let sp = ort::value::Tensor::from_array(([1usize], vec![speed.round() as i32]))?;
                session.run(
                    ort::inputs![a.as_str() => tokens, b.as_str() => style, c.as_str() => sp],
                )?
            } else {
                let sp = ort::value::Tensor::from_array(([1usize], vec![speed]))?;
                session.run(
                    ort::inputs![a.as_str() => tokens, b.as_str() => style, c.as_str() => sp],
                )?
            };
            let (_, data) = outputs[0].try_extract_tensor::<f32>()?;
            audio.extend_from_slice(data);
        }
        Ok(audio)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_folds_joined_pairs_and_drops_unknown() {
        assert_eq!(normalize_phonemes("kˈe\u{200d}ɪ"), "kˈA");
        assert_eq!(normalize_phonemes("ha\u{200d}ʊs ❓"), "hWs ");
        assert_eq!(tokenize("hWs"), vec![50, 39, 61]);
        // Anything outside the vocab is dropped, never mapped to 0.
        assert_eq!(tokenize("a\u{200d}#"), vec![43]);
    }

    #[test]
    fn g2p_output_maps_to_tokens() {
        let g2p = G2P::new(Language::EnglishUS);
        let (ph, _) = g2p.g2p("Hello world.").unwrap();
        let norm = normalize_phonemes(&ph);
        assert!(!norm.contains('\u{200d}'));
        let toks = tokenize(&norm);
        assert_eq!(toks.len(), norm.chars().count());
        assert!(toks.len() > 5, "{ph:?} → {norm:?}");
    }

    #[test]
    fn chunking_respects_token_limit() {
        let long = vec!["abcdefghij"; 200].join(" ");
        let chunks = chunk_phonemes(&long);
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|c| c.chars().count() <= MAX_TOKENS));
    }

    #[test]
    fn wav_header_and_samples() {
        let wav = encode_wav(&[0.0, 1.0, -1.0], SAMPLE_RATE);
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(&wav[8..16], b"WAVEfmt ");
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 24_000);
        assert_eq!(u32::from_le_bytes(wav[40..44].try_into().unwrap()), 6);
        assert_eq!(wav.len(), 50);
        assert_eq!(i16::from_le_bytes([wav[46], wav[47]]), i16::MAX);
    }

    fn npy(data: &[f32]) -> Vec<u8> {
        let mut header = format!(
            "{{'descr': '<f4', 'fortran_order': False, 'shape': ({VOICE_ROWS}, 1, {STYLE_DIM}), }}"
        );
        while (10 + header.len() + 1) % 64 != 0 {
            header.push(' ');
        }
        header.push('\n');
        let mut b = b"\x93NUMPY\x01\x00".to_vec();
        b.extend_from_slice(&(header.len() as u16).to_le_bytes());
        b.extend_from_slice(header.as_bytes());
        for f in data {
            b.extend_from_slice(&f.to_le_bytes());
        }
        b
    }

    #[test]
    fn parses_voices_npz() {
        use std::io::Write;
        let data: Vec<f32> = (0..VOICE_ROWS * STYLE_DIM).map(|i| i as f32).collect();
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut zw = zip::ZipWriter::new(&mut buf);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            zw.start_file("af_heart.npy", opts).unwrap();
            zw.write_all(&npy(&data)).unwrap();
            zw.finish().unwrap();
        }
        let voices = parse_voices_npz(buf.get_ref()).unwrap();
        let v = &voices["af_heart"];
        assert_eq!(v.len(), VOICE_ROWS * STYLE_DIM);
        assert_eq!(v[STYLE_DIM + 3], (STYLE_DIM + 3) as f32);
        assert!(parse_voices_npz(b"nope").is_err());
    }
}
