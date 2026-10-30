//! External speech server for the voice assistant — VoiceStudio, or any
//! server speaking OpenAI's audio API (`/v1/audio/speech`,
//! `/v1/audio/voices`, `/v1/audio/transcriptions`).
//!
//! Peckboard only ever talks to it server-side: the browser asks
//! `/api/voice/tts` for a `studio:<id>` voice and this module forwards the
//! request with the API key, so the key never reaches a client and there
//! is no CORS / mixed-content problem.
//!
//! Settings live in `plugin_settings` under [`SETTINGS_ID`] (a core id —
//! the dot keeps it out of the plugin-id namespace). The API key is
//! write-only: [`SpeechServerView`] carries only whether one is set.

use std::sync::LazyLock;
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::Mutex;

use crate::db::Db;

/// `plugin_settings.plugin_id` for the speech-server settings.
pub const SETTINGS_ID: &str = "core.speech";
/// Voice-URI prefix the client uses for speech-server voices.
pub const VOICE_PREFIX: &str = "studio:";
/// OpenAI's TTS model alias — VoiceStudio maps it to its active engine.
pub const DEFAULT_MODEL: &str = "tts-1";
/// OpenAI's `pcm` format: raw 24 kHz signed 16-bit little-endian mono.
pub const PCM_RATE: u32 = 24_000;

const KEY_URL: &str = "base_url";
const KEY_API_KEY: &str = "api_key";
const KEY_MODEL: &str = "model";
const VOICES_TTL: Duration = Duration::from_secs(60);
const MAX_AUDIO_BYTES: usize = 32 * 1024 * 1024;

/// A configured speech server.
#[derive(Clone, Debug, PartialEq)]
pub struct SpeechServer {
    /// Service root without a trailing `/` or `/v1`, e.g. `http://gpu:3900`.
    pub root: String,
    pub api_key: Option<String>,
    pub model: String,
}

/// Settings as the Settings page sees them — never the key itself.
#[derive(Serialize, Debug, PartialEq)]
pub struct SpeechServerView {
    pub base_url: String,
    pub model: String,
    pub has_api_key: bool,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Voice {
    pub id: String,
    pub name: String,
}

/// Normalise a user-entered URL to the service root: http(s) only, no
/// trailing `/` or `/v1` (OpenAI clients are configured with `…/v1`, the
/// `/health` probe lives at the root).
pub fn normalize_url(raw: &str) -> anyhow::Result<String> {
    let mut url = raw.trim().trim_end_matches('/').to_string();
    if url.to_ascii_lowercase().ends_with("/v1") {
        url.truncate(url.len() - 3);
        url = url.trim_end_matches('/').to_string();
    }
    let parsed = reqwest::Url::parse(&url).map_err(|e| anyhow::anyhow!("invalid URL: {e}"))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        anyhow::bail!("the speech server URL must be http:// or https:// with a host");
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        anyhow::bail!("the speech server URL must not have a query or fragment");
    }
    Ok(url)
}

fn str_setting(map: &std::collections::HashMap<String, serde_json::Value>, k: &str) -> String {
    map.get(k)
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// The configured server, or `None` when no URL is set.
pub async fn load(db: &Db) -> anyhow::Result<Option<SpeechServer>> {
    let map = db.list_plugin_settings(SETTINGS_ID).await?;
    let root = str_setting(&map, KEY_URL);
    if root.is_empty() {
        return Ok(None);
    }
    let key = str_setting(&map, KEY_API_KEY);
    let model = str_setting(&map, KEY_MODEL);
    Ok(Some(SpeechServer {
        root,
        api_key: (!key.is_empty()).then_some(key),
        model: if model.is_empty() {
            DEFAULT_MODEL.to_string()
        } else {
            model
        },
    }))
}

pub async fn view(db: &Db) -> anyhow::Result<SpeechServerView> {
    let map = db.list_plugin_settings(SETTINGS_ID).await?;
    let model = str_setting(&map, KEY_MODEL);
    Ok(SpeechServerView {
        base_url: str_setting(&map, KEY_URL),
        model: if model.is_empty() {
            DEFAULT_MODEL.to_string()
        } else {
            model
        },
        has_api_key: !str_setting(&map, KEY_API_KEY).is_empty(),
    })
}

/// Save settings. `None` leaves a field unchanged; an empty string clears
/// it (an empty URL turns the speech server off). Returns the new view.
pub async fn save(
    db: &Db,
    base_url: Option<&str>,
    api_key: Option<&str>,
    model: Option<&str>,
) -> anyhow::Result<SpeechServerView> {
    let mut updates = Vec::new();
    let as_value = |s: String| {
        if s.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::Value::String(s)
        }
    };
    if let Some(u) = base_url {
        let u = u.trim();
        let v = if u.is_empty() {
            String::new()
        } else {
            normalize_url(u)?
        };
        updates.push((KEY_URL.to_string(), as_value(v)));
    }
    if let Some(k) = api_key {
        let k = k.trim();
        if k.chars().any(|c| c.is_control() || c.is_whitespace()) || k.len() > 4096 {
            anyhow::bail!("the API key must be a single token without spaces");
        }
        updates.push((KEY_API_KEY.to_string(), as_value(k.to_string())));
    }
    if let Some(m) = model {
        let m = m.trim();
        if m.len() > 200 || m.chars().any(|c| c.is_control()) {
            anyhow::bail!("invalid model name");
        }
        updates.push((KEY_MODEL.to_string(), as_value(m.to_string())));
    }
    db.set_plugin_settings_batch(SETTINGS_ID, updates).await?;
    invalidate_voices().await;
    view(db).await
}

fn client(timeout: Duration) -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}

impl SpeechServer {
    fn authed(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.api_key {
            Some(k) => req.bearer_auth(k),
            None => req,
        }
    }

    /// An error message from an OpenAI-shaped (`{error:{message}}`) or
    /// VoiceStudio (`{detail}`) error body, else the status.
    async fn upstream_error(resp: reqwest::Response) -> anyhow::Error {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
        let msg = v["error"]["message"]
            .as_str()
            .or_else(|| v["detail"].as_str())
            .or_else(|| v["error"].as_str())
            .map(str::to_string)
            .unwrap_or_else(|| format!("HTTP {status}"));
        anyhow::anyhow!("speech server: {msg}")
    }

    /// `GET /v1/audio/voices`. Accepts `{voices:[…]}`, `{data:[…]}` or a
    /// bare array; each voice's id is `voice_id` or `id`.
    pub async fn fetch_voices(&self) -> anyhow::Result<Vec<Voice>> {
        let resp = self
            .authed(client(Duration::from_secs(10))?.get(format!("{}/v1/audio/voices", self.root)))
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(Self::upstream_error(resp).await);
        }
        let v: serde_json::Value = resp.json().await?;
        let list = v
            .get("voices")
            .or_else(|| v.get("data"))
            .unwrap_or(&v)
            .as_array()
            .cloned()
            .unwrap_or_default();
        Ok(list
            .iter()
            .filter_map(|x| {
                let id = x["voice_id"].as_str().or_else(|| x["id"].as_str())?.trim();
                if id.is_empty() {
                    return None;
                }
                let name = x["name"].as_str().map(str::trim).filter(|n| !n.is_empty());
                Some(Voice {
                    id: id.to_string(),
                    name: name.unwrap_or(id).to_string(),
                })
            })
            .collect())
    }

    /// `POST /v1/audio/speech` as raw 24 kHz PCM, wrapped in a WAV header
    /// so the client plays it exactly like a Kokoro clip.
    pub async fn speak(&self, text: &str, voice: &str, speed: f32) -> anyhow::Result<Vec<u8>> {
        let resp = self
            .authed(
                client(Duration::from_secs(60))?
                    .post(format!("{}/v1/audio/speech", self.root))
                    .json(&serde_json::json!({
                        "model": self.model,
                        "input": text,
                        "voice": voice,
                        "response_format": "pcm",
                        "speed": speed,
                    })),
            )
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(Self::upstream_error(resp).await);
        }
        let pcm = resp.bytes().await?;
        if pcm.len() > MAX_AUDIO_BYTES {
            anyhow::bail!("speech server returned too much audio");
        }
        Ok(pcm16_to_wav(&pcm, PCM_RATE))
    }

    /// Reachability + auth check for Settings → Voice → Test connection:
    /// `/health` when the server has it (VoiceStudio does), then the voice
    /// list (which needs a valid key).
    pub async fn test(&self) -> serde_json::Value {
        let mut out = serde_json::json!({ "ok": false });
        if let Ok(c) = client(Duration::from_secs(10))
            && let Ok(r) = self
                .authed(c.get(format!("{}/health", self.root)))
                .send()
                .await
            && r.status().is_success()
            && let Ok(h) = r.json::<serde_json::Value>().await
        {
            out["version"] = h["version"].clone();
            out["device"] = h["device"].clone();
        }
        match self.fetch_voices().await {
            Ok(v) => {
                out["ok"] = true.into();
                out["voices"] = v.len().into();
            }
            Err(e) => out["error"] = e.to_string().into(),
        }
        out
    }
}

/// Wrap raw signed 16-bit LE mono PCM in a WAV header.
pub fn pcm16_to_wav(pcm: &[u8], rate: u32) -> Vec<u8> {
    let data = &pcm[..pcm.len() & !1];
    let len = data.len() as u32;
    let mut w = Vec::with_capacity(44 + data.len());
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + len).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes()); // PCM
    w.extend_from_slice(&1u16.to_le_bytes()); // mono
    w.extend_from_slice(&rate.to_le_bytes());
    w.extend_from_slice(&(rate * 2).to_le_bytes());
    w.extend_from_slice(&2u16.to_le_bytes());
    w.extend_from_slice(&16u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&len.to_le_bytes());
    w.extend_from_slice(data);
    w
}

struct VoiceCache {
    root: String,
    at: Instant,
    voices: Vec<Voice>,
}

static VOICES: LazyLock<Mutex<Option<VoiceCache>>> = LazyLock::new(|| Mutex::new(None));

async fn invalidate_voices() {
    *VOICES.lock().await = None;
}

/// The server's voices, cached for a minute. Empty when unreachable — the
/// voice picker then just shows Kokoro and the browser voices.
pub async fn voices_cached(server: &SpeechServer) -> Vec<Voice> {
    let mut cache = VOICES.lock().await;
    if let Some(c) = cache.as_ref()
        && c.root == server.root
        && c.at.elapsed() < VOICES_TTL
    {
        return c.voices.clone();
    }
    let voices = match server.fetch_voices().await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "speech server voices unavailable");
            Vec::new()
        }
    };
    *cache = Some(VoiceCache {
        root: server.root.clone(),
        at: Instant::now(),
        voices: voices.clone(),
    });
    voices
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_urls() {
        assert_eq!(
            normalize_url(" http://gpu:3900/v1/ ").unwrap(),
            "http://gpu:3900"
        );
        assert_eq!(
            normalize_url("https://x.ts.net").unwrap(),
            "https://x.ts.net"
        );
        assert!(normalize_url("ftp://x").is_err());
        assert!(normalize_url("http://x/?a=1").is_err());
        assert!(normalize_url("gpu:3900").is_err());
    }

    #[test]
    fn wraps_pcm_in_wav() {
        let wav = pcm16_to_wav(&[1, 0, 2, 0, 9], 24_000);
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(wav[40..44].try_into().unwrap()), 4); // odd byte dropped
        assert_eq!(wav.len(), 48);
    }

    #[tokio::test]
    async fn settings_round_trip_without_exposing_the_key() {
        let db = Db::in_memory().unwrap();
        assert!(load(&db).await.unwrap().is_none());
        let v = save(&db, Some("http://gpu:3900/v1"), Some("sekrit"), None)
            .await
            .unwrap();
        assert_eq!(
            v,
            SpeechServerView {
                base_url: "http://gpu:3900".into(),
                model: DEFAULT_MODEL.into(),
                has_api_key: true,
            }
        );
        let s = load(&db).await.unwrap().unwrap();
        assert_eq!(s.api_key.as_deref(), Some("sekrit"));
        // None keeps the key; "" clears it; an empty URL turns it off.
        save(&db, None, None, Some("omnivoice")).await.unwrap();
        assert_eq!(
            load(&db).await.unwrap().unwrap().api_key.as_deref(),
            Some("sekrit")
        );
        save(&db, None, Some(""), None).await.unwrap();
        assert!(!view(&db).await.unwrap().has_api_key);
        save(&db, Some(""), None, None).await.unwrap();
        assert!(load(&db).await.unwrap().is_none());
        assert!(save(&db, Some("nope"), None, None).await.is_err());
    }
}
