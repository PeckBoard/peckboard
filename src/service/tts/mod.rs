//! Server-side Kokoro text-to-speech for the voice assistant.
//!
//! Nothing ships in the binary but code: the first `prepare` downloads the
//! ONNX Runtime shared library and the Kokoro model/voices into
//! `<data-dir>/tts/`, loads them once, and from then on `synthesize` turns a
//! sentence into a 24 kHz WAV. Inference is CPU-heavy, so it runs on the
//! blocking pool behind a small semaphore.

pub mod download;
pub mod kokoro;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};

use serde::Serialize;

use kokoro::Kokoro;

/// Longest text one request may synthesize (chars). The browser sends a
/// sentence at a time, so this only bounds abuse.
pub const MAX_TEXT_CHARS: usize = 500;
pub const DEFAULT_VOICE: &str = "af_heart";

/// English voices in `voices-v1.0.bin`: (id, display name).
pub const VOICES: &[(&str, &str)] = &[
    ("af_heart", "Heart"),
    ("af_bella", "Bella"),
    ("af_nicole", "Nicole"),
    ("af_aoede", "Aoede"),
    ("af_kore", "Kore"),
    ("af_sarah", "Sarah"),
    ("af_nova", "Nova"),
    ("af_sky", "Sky"),
    ("af_alloy", "Alloy"),
    ("af_jessica", "Jessica"),
    ("af_river", "River"),
    ("am_michael", "Michael"),
    ("am_fenrir", "Fenrir"),
    ("am_puck", "Puck"),
    ("am_echo", "Echo"),
    ("am_eric", "Eric"),
    ("am_liam", "Liam"),
    ("am_onyx", "Onyx"),
    ("am_adam", "Adam"),
    ("am_santa", "Santa"),
    ("bf_emma", "Emma"),
    ("bf_isabella", "Isabella"),
    ("bf_alice", "Alice"),
    ("bf_lily", "Lily"),
    ("bm_george", "George"),
    ("bm_fable", "Fable"),
    ("bm_lewis", "Lewis"),
    ("bm_daniel", "Daniel"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TtsState {
    NotStarted,
    /// Downloading files or loading the model.
    Downloading,
    Ready,
    Unavailable,
}

#[derive(Debug, Clone, Serialize)]
pub struct TtsStatus {
    pub state: TtsState,
    /// 0.0–1.0 over the bytes still to download.
    pub progress: f32,
    pub error: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum TtsError {
    #[error("tts not ready")]
    NotReady(TtsStatus),
    #[error("{0}")]
    BadRequest(String),
    #[error("{0:#}")]
    Failed(anyhow::Error),
}

pub struct TtsService {
    dir: PathBuf,
    state: Mutex<(TtsState, Option<String>)>,
    done: AtomicU64,
    total: AtomicU64,
    engine: OnceLock<Arc<Kokoro>>,
    permits: tokio::sync::Semaphore,
}

static SERVICES: LazyLock<Mutex<HashMap<PathBuf, Arc<TtsService>>>> =
    LazyLock::new(Default::default);

/// The process-wide service for `data_dir` (one per data dir, so tests with
/// separate tmp dirs don't share state).
pub fn service_for(data_dir: &Path) -> Arc<TtsService> {
    SERVICES
        .lock()
        .unwrap()
        .entry(data_dir.to_path_buf())
        .or_insert_with(|| Arc::new(TtsService::new(data_dir.join("tts"))))
        .clone()
}

impl TtsService {
    fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            state: Mutex::new((TtsState::NotStarted, None)),
            done: AtomicU64::new(0),
            total: AtomicU64::new(0),
            engine: OnceLock::new(),
            permits: tokio::sync::Semaphore::new(2),
        }
    }

    pub fn status(&self) -> TtsStatus {
        let (state, error) = self.state.lock().unwrap().clone();
        let total = self.total.load(Ordering::Relaxed);
        let progress = match state {
            TtsState::Ready => 1.0,
            _ if total == 0 => 0.0,
            _ => (self.done.load(Ordering::Relaxed) as f64 / total as f64).min(1.0) as f32,
        };
        TtsStatus {
            state,
            progress,
            error,
        }
    }

    /// Start downloading/loading unless already underway or ready. An
    /// `Unavailable` service retries.
    pub fn prepare(self: &Arc<Self>) {
        {
            let mut st = self.state.lock().unwrap();
            if matches!(st.0, TtsState::Downloading | TtsState::Ready) {
                return;
            }
            *st = (TtsState::Downloading, None);
        }
        let this = self.clone();
        tokio::spawn(async move {
            let result = this.run_prepare().await;
            let mut st = this.state.lock().unwrap();
            *st = match result {
                Ok(()) => (TtsState::Ready, None),
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "kokoro tts unavailable");
                    (TtsState::Unavailable, Some(format!("{e:#}")))
                }
            };
        });
    }

    async fn run_prepare(&self) -> anyhow::Result<()> {
        let ort = download::ort_asset().ok_or_else(|| {
            anyhow::anyhow!(
                "no ONNX Runtime build for {}/{}",
                std::env::consts::OS,
                std::env::consts::ARCH
            )
        })?;
        let ort_dir = self
            .dir
            .join(format!("onnxruntime-{}", download::ORT_VERSION));
        tokio::fs::create_dir_all(&ort_dir).await?;
        let lib = ort_dir.join(ort.lib_name);
        let model = self.dir.join("kokoro-v1.0.fp16.onnx");
        let voices = self.dir.join("voices-v1.0.bin");

        let need_lib = !lib.exists();
        let need_model = !model.exists();
        let need_voices = !voices.exists();
        let total = [
            (need_lib, ort.archive.size),
            (need_model, download::MODEL.size),
            (need_voices, download::VOICES.size),
        ]
        .iter()
        .filter(|(n, _)| *n)
        .map(|(_, s)| s)
        .sum();
        // Test servers (the e2e suite) must never pull ~200 MB.
        if total > 0 && std::env::var("PECKBOARD_TTS_DOWNLOAD").as_deref() == Ok("0") {
            anyhow::bail!("model downloads are disabled (PECKBOARD_TTS_DOWNLOAD=0)");
        }
        self.done.store(0, Ordering::Relaxed);
        self.total.store(total, Ordering::Relaxed);
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(30))
            .build()?;
        let tick = |n: u64| {
            self.done.fetch_add(n, Ordering::Relaxed);
        };
        if need_lib {
            let ext = if ort.archive.url.ends_with(".zip") {
                "zip"
            } else {
                "tgz"
            };
            let archive = ort_dir.join(format!("archive.{ext}"));
            download::fetch(&client, &ort.archive, &archive, &tick).await?;
            let (a, entry, dest) = (archive.clone(), ort.entry, lib.clone());
            tokio::task::spawn_blocking(move || download::extract_lib(&a, entry, &dest)).await??;
            let _ = tokio::fs::remove_file(&archive).await;
        }
        if need_model {
            download::fetch(&client, &download::MODEL, &model, &tick).await?;
        }
        if need_voices {
            download::fetch(&client, &download::VOICES, &voices, &tick).await?;
        }
        let started = std::time::Instant::now();
        let engine =
            tokio::task::spawn_blocking(move || Kokoro::load(&lib, &model, &voices)).await??;
        tracing::info!(
            ms = started.elapsed().as_millis() as u64,
            "kokoro tts loaded"
        );
        let _ = self.engine.set(Arc::new(engine));
        Ok(())
    }

    /// Synthesize `text` → WAV bytes. Reports `NotReady` until `prepare`
    /// has finished loading the model (the browser calls it explicitly).
    pub async fn synthesize(
        &self,
        text: &str,
        voice: Option<&str>,
        speed: Option<f32>,
    ) -> Result<Vec<u8>, TtsError> {
        let text = text.trim();
        if text.is_empty() {
            return Err(TtsError::BadRequest("text is empty".into()));
        }
        if text.chars().count() > MAX_TEXT_CHARS {
            return Err(TtsError::BadRequest(format!(
                "text is longer than {MAX_TEXT_CHARS} characters"
            )));
        }
        let voice = voice.unwrap_or(DEFAULT_VOICE);
        if !VOICES.iter().any(|(id, _)| *id == voice) {
            return Err(TtsError::BadRequest(format!("unknown voice '{voice}'")));
        }
        let speed = speed.unwrap_or(1.0).clamp(0.5, 2.0);
        let Some(engine) = self.engine.get().cloned() else {
            return Err(TtsError::NotReady(self.status()));
        };
        let _permit = self
            .permits
            .acquire()
            .await
            .map_err(|e| TtsError::Failed(e.into()))?;
        let (text, voice) = (text.to_string(), voice.to_string());
        let samples = tokio::task::spawn_blocking(move || engine.synthesize(&text, &voice, speed))
            .await
            .map_err(|e| TtsError::Failed(e.into()))?
            .map_err(TtsError::Failed)?;
        Ok(kokoro::encode_wav(&samples, kokoro::SAMPLE_RATE))
    }
}
