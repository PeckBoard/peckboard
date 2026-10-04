//! Assistant conversation mirror: copies the voice (Assistant) session's
//! conversation to Slack and/or Discord webhooks, and emails a digest of
//! the turns that complete while nobody is watching the Assistant panel.
//!
//! - **Tap**: a `subscribe_all` listener that only reacts to the voice
//!   session's `user` and `agent-end` events (and to broadcast lag). It
//!   never trusts the broadcast copy of an event: each trigger re-reads the
//!   event log from the last processed `seq`, so lagged or size-capped
//!   broadcasts lose nothing. [`format::TurnAssembler`] turns the events
//!   into messages: user utterances at once, a reply once at `agent-end`;
//!   relay turns, thinking, and tool events are never mirrored.
//! - **Scrub**: every outgoing text goes through [`format::scrub`] (secret
//!   masker on a blocking thread, then pattern masks, then code blocks).
//! - **Delivery**: one bounded queue + worker per webhook; the tap only
//!   `try_send`s, so a slow or rate-limited webhook never blocks the voice
//!   path. Retries back off exponentially, honour `429` Retry-After, and
//!   give up after [`MAX_ATTEMPTS`]. The last result per channel is kept in
//!   memory for the settings API.
//! - **Email**: turns mirrored while the panel is not visible (no
//!   `panel_visible` heartbeat within [`WATCHED_WINDOW`]) collect in a
//!   bounded in-memory digest, sent [`DIGEST_DELAY`] after the last such
//!   turn and at most once per [`DIGEST_MIN_INTERVAL`].
//!
//! One mirror per `AppState`, found through [`AssistantMirror::of`] (keyed
//! by the state's broadcaster, so tests with their own state get their
//! own mirror). Boot creates it eagerly; nothing here starts an agent.

pub mod format;
pub mod settings;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::Duration;

use serde::Serialize;
use tokio::sync::{Notify, broadcast, mpsc};
use tokio::time::Instant;

use crate::db::Db;
use crate::restart_resume::{NO_RESUME_ENV, resume_disabled};
use crate::routes::settings::{SETTINGS_COLLECTION, SETTINGS_NS};
use crate::service::secret_mask::SecretMasker;
use crate::service::voice_relay::VOICE_EXPERT_KIND;
use crate::state::AppState;
use crate::ws::broadcaster::{Broadcaster, WsEvent};

use format::{
    DISCORD_LIMIT, Line, SLACK_LIMIT, TurnAssembler, discord_payload, slack_payload, split_message,
};
use settings::{
    FieldErrors, MirrorPatch, MirrorSettings, SETTINGS_KEY, SmtpSettings, SmtpTls, WebhookKind,
    validate_webhook_url,
};

/// A `panel_visible` heartbeat counts as "watched" for this long.
pub const WATCHED_WINDOW: Duration = Duration::from_secs(75);
/// A digest goes out this long after the last unwatched turn…
pub const DIGEST_DELAY: Duration = Duration::from_secs(120);
/// …and at most once per this interval.
pub const DIGEST_MIN_INTERVAL: Duration = Duration::from_secs(15 * 60);
/// Messages held for the next digest; the oldest are dropped past this.
pub const DIGEST_CAP: usize = 200;
/// Attempts per webhook post before giving up.
pub const MAX_ATTEMPTS: u32 = 5;
/// Pending messages per webhook queue; more are dropped (and reported).
const QUEUE_CAP: usize = 256;
const FIRST_BACKOFF: Duration = Duration::from_secs(1);
/// Cap on any single wait, including a server-sent Retry-After.
const MAX_WAIT: Duration = Duration::from_secs(60);
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
const SMTP_TIMEOUT: Duration = Duration::from_secs(20);
/// A failed digest is retried no sooner than this.
const DIGEST_FAILURE_RETRY: Duration = Duration::from_secs(5 * 60);
/// The digest loop re-checks at least this often.
const DIGEST_IDLE_RECHECK: Duration = Duration::from_secs(30);
/// The cached secret masker is rebuilt after this (env vars may change).
const MASKER_TTL: Duration = Duration::from_secs(5 * 60);
const EVENT_PAGE: i64 = 500;
/// Owner key for [`crate::service::secret_mask::set_extra_secrets`].
const SECRET_OWNER: &str = "assistant-mirror";
const ERROR_CHARS: usize = 200;

/// Deliver even with [`NO_RESUME_ENV`] set — for a scratch run that
/// deliberately points the mirror at a local receiver.
pub const ALLOW_UNDER_NO_RESUME_ENV: &str = "PECKBOARD_MIRROR_ALLOW_UNDER_NO_RESUME";

/// `PECKBOARD_NO_RESUME=1` marks a scratch or copied data dir: its stored
/// webhooks and SMTP settings may be the user's real ones, so the mirror
/// must not send anything unless [`ALLOW_UNDER_NO_RESUME_ENV`] is `1`.
fn delivery_suppressed() -> bool {
    let var = |k: &str| std::env::var(k).ok();
    resume_disabled(var(NO_RESUME_ENV).as_deref())
        && var(ALLOW_UNDER_NO_RESUME_ENV).as_deref() != Some("1")
}

fn suppressed_reason() -> String {
    format!("Delivery is off: this server runs with {NO_RESUME_ENV} set")
}

// ── status ──────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum StatusState {
    Ok,
    Error,
    #[default]
    Never,
}

/// Last delivery result of one channel (in memory only).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ChannelStatus {
    pub state: StatusState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Statuses {
    pub slack: ChannelStatus,
    pub discord: ChannelStatus,
    pub email: ChannelStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    Slack,
    Discord,
    Email,
}

impl Channel {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "slack" => Channel::Slack,
            "discord" => Channel::Discord,
            "email" => Channel::Email,
            _ => return None,
        })
    }
}

fn short_error(e: impl std::fmt::Display) -> String {
    let s = e.to_string();
    match s.char_indices().nth(ERROR_CHARS) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s,
    }
}

// ── presence + digest (pure, time-injected) ─────────────────────────────

fn watched_at(last_visible: Option<Instant>, now: Instant) -> bool {
    last_visible.is_some_and(|t| now.saturating_duration_since(t) <= WATCHED_WINDOW)
}

#[derive(Clone, Copy, Debug)]
struct DigestTimings {
    delay: Duration,
    min_interval: Duration,
}

impl Default for DigestTimings {
    fn default() -> Self {
        DigestTimings {
            delay: DIGEST_DELAY,
            min_interval: DIGEST_MIN_INTERVAL,
        }
    }
}

#[derive(Default)]
struct DigestBuf {
    items: VecDeque<String>,
    dropped: usize,
    last_turn: Option<Instant>,
    last_sent: Option<Instant>,
    failed_at: Option<Instant>,
}

impl DigestBuf {
    fn push(&mut self, text: String, now: Instant) {
        if self.items.len() >= DIGEST_CAP {
            self.items.pop_front();
            self.dropped += 1;
        }
        self.items.push_back(text);
        self.last_turn = Some(now);
    }

    /// When the pending digest may go out, or `None` with nothing pending.
    fn due(&self, t: DigestTimings) -> Option<Instant> {
        let last_turn = self.last_turn.filter(|_| !self.items.is_empty())?;
        let mut at = last_turn + t.delay;
        if let Some(sent) = self.last_sent {
            at = at.max(sent + t.min_interval);
        }
        if let Some(failed) = self.failed_at {
            at = at.max(failed + DIGEST_FAILURE_RETRY.max(t.min_interval));
        }
        Some(at)
    }

    fn take(&mut self) -> (Vec<String>, usize) {
        (
            self.items.drain(..).collect(),
            std::mem::take(&mut self.dropped),
        )
    }

    /// Put an unsent digest back in front of anything newer.
    fn restore(&mut self, items: Vec<String>, dropped: usize) {
        let newer: Vec<String> = self.items.drain(..).collect();
        self.dropped += dropped;
        for t in items.into_iter().chain(newer) {
            if self.items.len() >= DIGEST_CAP {
                self.items.pop_front();
                self.dropped += 1;
            }
            self.items.push_back(t);
        }
    }
}

fn digest_bodies(items: &[String], dropped: usize) -> (String, String, String) {
    let n = items.len();
    let subject = format!(
        "Peckboard Assistant: {n} new message{}",
        if n == 1 { "" } else { "s" }
    );
    let note = (dropped > 0)
        .then(|| format!("({dropped} older message(s) were dropped because the digest was full.)"));
    let mut text = String::new();
    let mut html =
        String::from("<div style=\"font-family:sans-serif\"><h3>Peckboard Assistant</h3>");
    if let Some(note) = &note {
        text.push_str(note);
        text.push_str("\n\n");
        html.push_str(&format!("<p><em>{}</em></p>", html_escape(note)));
    }
    text.push_str(&items.join("\n\n"));
    for item in items {
        html.push_str(&format!(
            "<p style=\"white-space:pre-wrap\">{}</p>",
            html_escape(item)
        ));
    }
    html.push_str("</div>");
    (subject, text, html)
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

// ── mail transport ──────────────────────────────────────────────────────

/// Sends one email. [`SmtpMailer`] in production; tests substitute a fake.
#[async_trait::async_trait]
pub trait Mailer: Send + Sync {
    async fn send(
        &self,
        smtp: &SmtpSettings,
        to: &str,
        subject: &str,
        text: &str,
        html: &str,
    ) -> Result<(), String>;
}

/// SMTP through lettre (rustls; STARTTLS, implicit TLS, or plaintext to
/// loopback only).
pub struct SmtpMailer;

#[async_trait::async_trait]
impl Mailer for SmtpMailer {
    async fn send(
        &self,
        smtp: &SmtpSettings,
        to: &str,
        subject: &str,
        text: &str,
        html: &str,
    ) -> Result<(), String> {
        use lettre::message::{Mailbox, MultiPart};
        use lettre::transport::smtp::authentication::Credentials;
        use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

        if smtp.host.is_empty() || smtp.from.is_empty() || to.is_empty() {
            return Err("SMTP host, from address, and recipient are required".into());
        }
        // Same process-wide provider the HTTPS listener installs.
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        type Transport = AsyncSmtpTransport<Tokio1Executor>;
        let builder = match smtp.tls {
            SmtpTls::Tls => Transport::relay(&smtp.host).map_err(short_error)?,
            SmtpTls::Starttls => Transport::starttls_relay(&smtp.host).map_err(short_error)?,
            SmtpTls::None => {
                if !settings::is_loopback_host(&smtp.host) {
                    return Err("unencrypted SMTP is only allowed to localhost".into());
                }
                Transport::builder_dangerous(&smtp.host)
            }
        };
        let mut builder = builder.port(smtp.port).timeout(Some(SMTP_TIMEOUT));
        if !smtp.username.is_empty() {
            builder = builder.credentials(Credentials::new(
                smtp.username.clone(),
                smtp.password.clone(),
            ));
        }
        let transport = builder.build();
        let from: Mailbox = smtp.from.parse().map_err(short_error)?;
        let to: Mailbox = to.parse().map_err(short_error)?;
        let message = Message::builder()
            .from(from)
            .to(to)
            .subject(subject)
            .multipart(MultiPart::alternative_plain_html(
                text.to_string(),
                html.to_string(),
            ))
            .map_err(short_error)?;
        transport.send(message).await.map_err(short_error)?;
        Ok(())
    }
}

// ── the mirror ──────────────────────────────────────────────────────────

/// One webhook message: every part of one mirrored line, posted in order.
struct Job {
    url: String,
    payloads: Vec<serde_json::Value>,
}

/// Why a settings update was refused.
pub enum UpdateError {
    Invalid(FieldErrors),
    Storage(anyhow::Error),
}

pub struct AssistantMirror {
    /// Delivery off (scratch run, see [`delivery_suppressed`]): nothing is
    /// posted or mailed; settings still load and save.
    suppressed: bool,
    db: Db,
    http: reqwest::Client,
    mailer: Arc<dyn Mailer>,
    settings: RwLock<MirrorSettings>,
    /// Serialises read-modify-write settings updates.
    update_lock: tokio::sync::Mutex<()>,
    statuses: Mutex<Statuses>,
    last_visible: Mutex<Option<Instant>>,
    digest: Mutex<DigestBuf>,
    digest_wake: Notify,
    timings: Mutex<DigestTimings>,
    masker: Mutex<Option<(std::time::Instant, Arc<SecretMasker>)>>,
    slack_tx: mpsc::Sender<Job>,
    discord_tx: mpsc::Sender<Job>,
}

type Registry = Vec<(Weak<Broadcaster>, Arc<AssistantMirror>)>;
static MIRRORS: tokio::sync::Mutex<Registry> = tokio::sync::Mutex::const_new(Vec::new());

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl AssistantMirror {
    /// The mirror for `state`, started on first use.
    pub async fn of(state: &AppState) -> Arc<AssistantMirror> {
        let mut reg = MIRRORS.lock().await;
        reg.retain(|(b, _)| b.strong_count() > 0);
        if let Some((_, m)) = reg
            .iter()
            .find(|(b, _)| std::ptr::eq(b.as_ptr(), Arc::as_ptr(&state.broadcaster)))
        {
            return m.clone();
        }
        let m = Self::start(state.db.clone(), &state.broadcaster, Arc::new(SmtpMailer)).await;
        reg.push((Arc::downgrade(&state.broadcaster), m.clone()));
        m
    }

    /// Load the stored settings, then start the tap, the webhook workers,
    /// and the digest loop.
    pub async fn start(
        db: Db,
        broadcaster: &Broadcaster,
        mailer: Arc<dyn Mailer>,
    ) -> Arc<AssistantMirror> {
        let rx = broadcaster.subscribe_all();
        let settings = load_settings(&db).await;
        let me = Self::new(db, settings, mailer);
        tokio::spawn(tap(Arc::downgrade(&me), rx));
        me
    }

    /// Build and start the webhook workers and the digest loop, without
    /// the tap (unit tests feed lines directly).
    fn new(db: Db, settings: MirrorSettings, mailer: Arc<dyn Mailer>) -> Arc<AssistantMirror> {
        let suppressed = delivery_suppressed();
        if suppressed {
            tracing::info!(
                "{NO_RESUME_ENV} set: Assistant mirror delivery is off (set {ALLOW_UNDER_NO_RESUME_ENV}=1 to override)"
            );
        }
        Self::build(db, settings, mailer, suppressed)
    }

    fn build(
        db: Db,
        settings: MirrorSettings,
        mailer: Arc<dyn Mailer>,
        suppressed: bool,
    ) -> Arc<AssistantMirror> {
        crate::service::secret_mask::set_extra_secrets(SECRET_OWNER, settings.secret_values());
        let (slack_tx, slack_rx) = mpsc::channel(QUEUE_CAP);
        let (discord_tx, discord_rx) = mpsc::channel(QUEUE_CAP);
        let me = Arc::new(AssistantMirror {
            suppressed,
            db,
            http: reqwest::Client::new(),
            mailer,
            settings: RwLock::new(settings),
            update_lock: tokio::sync::Mutex::new(()),
            statuses: Mutex::new(Statuses::default()),
            last_visible: Mutex::new(None),
            digest: Mutex::new(DigestBuf::default()),
            digest_wake: Notify::new(),
            timings: Mutex::new(DigestTimings::default()),
            masker: Mutex::new(None),
            slack_tx,
            discord_tx,
        });
        tokio::spawn(webhook_worker(
            Arc::downgrade(&me),
            Channel::Slack,
            slack_rx,
        ));
        tokio::spawn(webhook_worker(
            Arc::downgrade(&me),
            Channel::Discord,
            discord_rx,
        ));
        tokio::spawn(digest_loop(Arc::downgrade(&me)));
        me
    }

    pub fn settings(&self) -> MirrorSettings {
        self.settings
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn statuses(&self) -> Statuses {
        lock(&self.statuses).clone()
    }

    /// The `GET /api/assistant/mirror` body (secrets reported as set/unset).
    pub fn wire(&self) -> serde_json::Value {
        self.settings().wire(&self.statuses(), self.is_watched())
    }

    /// Validate, persist, and apply a partial update.
    pub async fn update(&self, patch: MirrorPatch) -> Result<MirrorSettings, UpdateError> {
        let _guard = self.update_lock.lock().await;
        let next = self.settings().apply(patch).map_err(UpdateError::Invalid)?;
        let db = self.db.clone();
        let value = serde_json::to_string(&next).map_err(|e| UpdateError::Storage(e.into()))?;
        tokio::task::spawn_blocking(move || {
            db.plugin_store_put_blocking(SETTINGS_NS, SETTINGS_COLLECTION, SETTINGS_KEY, &value)
        })
        .await
        .map_err(|e| UpdateError::Storage(e.into()))?
        .map_err(UpdateError::Storage)?;
        crate::service::secret_mask::set_extra_secrets(SECRET_OWNER, next.secret_values());
        *self.settings.write().unwrap_or_else(|e| e.into_inner()) = next.clone();
        // New credentials must be in the masker before the next send.
        *lock(&self.masker) = None;
        Ok(next)
    }

    /// The Assistant panel is open and visible in some browser.
    pub fn note_panel_visible(&self) {
        *lock(&self.last_visible) = Some(Instant::now());
    }

    pub fn is_watched(&self) -> bool {
        watched_at(*lock(&self.last_visible), Instant::now())
    }

    /// Shorten the digest timers. For tests; production uses
    /// [`DIGEST_DELAY`] / [`DIGEST_MIN_INTERVAL`].
    pub fn set_digest_timings(&self, delay: Duration, min_interval: Duration) {
        *lock(&self.timings) = DigestTimings {
            delay,
            min_interval,
        };
        self.digest_wake.notify_one();
    }

    fn set_status(&self, channel: Channel, result: &Result<(), String>) {
        let status = ChannelStatus {
            state: if result.is_ok() {
                StatusState::Ok
            } else {
                StatusState::Error
            },
            at: Some(chrono::Utc::now().to_rfc3339()),
            error: result.as_ref().err().map(short_error),
        };
        let mut s = lock(&self.statuses);
        match channel {
            Channel::Slack => s.slack = status,
            Channel::Discord => s.discord = status,
            Channel::Email => s.email = status,
        }
    }

    /// Scrub `text` for sending (masker built or reused on a blocking
    /// thread). `None` if scrubbing failed — then nothing is sent.
    async fn scrub(&self, text: String, redact_code_blocks: bool) -> Option<String> {
        let cached = lock(&self.masker)
            .as_ref()
            .filter(|(at, _)| at.elapsed() < MASKER_TTL)
            .map(|(_, m)| m.clone());
        let fresh = cached.is_none();
        let db = self.db.clone();
        let (masker, out) = tokio::task::spawn_blocking(move || {
            let masker = cached
                .unwrap_or_else(|| Arc::new(crate::service::secret_mask::masker_blocking(&db)));
            let out = format::scrub(&text, &masker, redact_code_blocks);
            (masker, out)
        })
        .await
        .map_err(|e| tracing::warn!("assistant mirror: scrub failed: {e}"))
        .ok()?;
        if fresh {
            *lock(&self.masker) = Some((std::time::Instant::now(), masker));
        }
        Some(out)
    }

    /// Mirror one line to every enabled channel. Never blocks on delivery.
    pub async fn publish(&self, line: Line) {
        if self.suppressed {
            return;
        }
        let s = self.settings();
        let email = s.email.enabled && !self.is_watched();
        if !s.slack.enabled && !s.discord.enabled && !email {
            return;
        }
        let Some(text) = self.scrub(line.render(), s.redact_code_blocks).await else {
            return;
        };
        if s.slack.enabled {
            let payloads = split_message(&text, SLACK_LIMIT)
                .iter()
                .map(|p| slack_payload(p))
                .collect();
            self.enqueue(Channel::Slack, &s.slack.webhook_url, payloads);
        }
        if s.discord.enabled {
            let payloads = split_message(&text, DISCORD_LIMIT)
                .iter()
                .map(|p| discord_payload(p))
                .collect();
            self.enqueue(Channel::Discord, &s.discord.webhook_url, payloads);
        }
        if email {
            self.push_digest(text);
        }
    }

    fn enqueue(&self, channel: Channel, url: &str, payloads: Vec<serde_json::Value>) {
        let tx = match channel {
            Channel::Slack => &self.slack_tx,
            Channel::Discord => &self.discord_tx,
            Channel::Email => return,
        };
        let job = Job {
            url: url.to_string(),
            payloads,
        };
        if tx.try_send(job).is_err() {
            tracing::warn!(
                ?channel,
                "assistant mirror: delivery queue full, message dropped"
            );
            self.set_status(
                channel,
                &Err("Delivery queue full; a message was dropped".into()),
            );
        }
    }

    fn push_digest(&self, text: String) {
        lock(&self.digest).push(text, Instant::now());
        self.digest_wake.notify_one();
    }

    /// Post one payload, retrying per the module rules.
    async fn post(
        &self,
        kind: WebhookKind,
        url: &str,
        payload: &serde_json::Value,
        attempts: u32,
    ) -> Result<(), String> {
        // Re-checked on every send: a stored URL from an old build or a
        // test run is never followed anywhere else.
        validate_webhook_url(kind, url)?;
        let mut backoff = FIRST_BACKOFF;
        let mut last_err = String::new();
        for attempt in 1..=attempts {
            let wait = match self
                .http
                .post(url)
                .json(payload)
                .timeout(HTTP_TIMEOUT)
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => return Ok(()),
                Ok(resp) if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS => {
                    last_err = "Rate limited (HTTP 429)".into();
                    retry_after(resp).await.unwrap_or(backoff)
                }
                Ok(resp) if resp.status().is_client_error() => {
                    // Permanent (bad or deleted webhook): no retry.
                    let status = resp.status();
                    let body = resp.text().await.unwrap_or_default();
                    return Err(format!("HTTP {status}: {}", body.trim()));
                }
                Ok(resp) => {
                    last_err = format!("HTTP {}", resp.status());
                    backoff
                }
                Err(e) => {
                    // `without_url`: the URL is the secret.
                    last_err = e.without_url().to_string();
                    backoff
                }
            };
            if attempt == attempts {
                break;
            }
            tokio::time::sleep(wait.min(MAX_WAIT)).await;
            backoff *= 2;
        }
        Err(last_err)
    }

    /// `POST /api/assistant/mirror/test`: one message now, outside the
    /// queues and the digest. Updates the channel status.
    pub async fn send_test(&self, channel: Channel) -> Result<(), String> {
        let s = self.settings();
        if self.suppressed {
            let result = Err(suppressed_reason());
            self.set_status(channel, &result);
            return result;
        }
        let text = "Peckboard Assistant mirror test: this channel is connected.";
        let result = match channel {
            Channel::Slack if s.slack.webhook_url.is_empty() => {
                Err("No Slack webhook URL is set".to_string())
            }
            Channel::Slack => {
                self.post(
                    WebhookKind::Slack,
                    &s.slack.webhook_url,
                    &slack_payload(text),
                    2,
                )
                .await
            }
            Channel::Discord if s.discord.webhook_url.is_empty() => {
                Err("No Discord webhook URL is set".to_string())
            }
            Channel::Discord => {
                self.post(
                    WebhookKind::Discord,
                    &s.discord.webhook_url,
                    &discord_payload(text),
                    2,
                )
                .await
            }
            Channel::Email => {
                let html = format!("<p>{}</p>", html_escape(text));
                self.mailer
                    .send(
                        &s.smtp,
                        &s.email.to,
                        "Peckboard Assistant: test message",
                        text,
                        &html,
                    )
                    .await
            }
        };
        self.set_status(channel, &result);
        result
    }

    /// Send the pending digest now (the loop decided it is due).
    async fn send_digest(&self) {
        let s = self.settings();
        let (items, dropped) = lock(&self.digest).take();
        if items.is_empty() || !s.email.enabled {
            return;
        }
        let (subject, text, html) = digest_bodies(&items, dropped);
        let result = self
            .mailer
            .send(&s.smtp, &s.email.to, &subject, &text, &html)
            .await;
        {
            let mut d = lock(&self.digest);
            let now = Instant::now();
            if result.is_ok() {
                d.last_sent = Some(now);
                d.failed_at = None;
            } else {
                d.failed_at = Some(now);
                d.restore(items, dropped);
            }
        }
        if let Err(e) = &result {
            tracing::warn!("assistant mirror: digest email failed: {e}");
        }
        self.set_status(Channel::Email, &result);
    }
}

async fn load_settings(db: &Db) -> MirrorSettings {
    let db = db.clone();
    let raw = tokio::task::spawn_blocking(move || {
        db.plugin_store_get_blocking(SETTINGS_NS, SETTINGS_COLLECTION, SETTINGS_KEY)
    })
    .await;
    match raw {
        Ok(Ok(Some(json))) => serde_json::from_str(&json).unwrap_or_else(|e| {
            tracing::warn!("assistant mirror: stored settings unreadable ({e}); using defaults");
            MirrorSettings::default()
        }),
        _ => MirrorSettings::default(),
    }
}

/// Seconds to wait from a 429: the `Retry-After` header (Slack), else the
/// JSON body's `retry_after` (Discord, seconds as a float).
async fn retry_after(resp: reqwest::Response) -> Option<Duration> {
    let header = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<f64>().ok());
    let secs = match header {
        Some(s) => s,
        None => resp
            .json::<serde_json::Value>()
            .await
            .ok()?
            .get("retry_after")?
            .as_f64()?,
    };
    (secs.is_finite() && secs >= 0.0).then(|| Duration::from_secs_f64(secs.min(3600.0)))
}

async fn webhook_worker(
    weak: Weak<AssistantMirror>,
    channel: Channel,
    mut rx: mpsc::Receiver<Job>,
) {
    let kind = match channel {
        Channel::Slack => WebhookKind::Slack,
        _ => WebhookKind::Discord,
    };
    while let Some(job) = rx.recv().await {
        let Some(me) = weak.upgrade() else { return };
        let mut result = Ok(());
        for payload in &job.payloads {
            result = me.post(kind, &job.url, payload, MAX_ATTEMPTS).await;
            if result.is_err() {
                break;
            }
        }
        if let Err(e) = &result {
            tracing::warn!(?channel, "assistant mirror: delivery failed: {e}");
        }
        me.set_status(channel, &result);
    }
}

async fn digest_loop(weak: Weak<AssistantMirror>) {
    loop {
        let Some(me) = weak.upgrade() else { return };
        let timings = *lock(&me.timings);
        let due = lock(&me.digest).due(timings);
        let now = Instant::now();
        if due.is_some_and(|at| at <= now) {
            me.send_digest().await;
            continue;
        }
        let wait = due
            .map_or(DIGEST_IDLE_RECHECK, |at| at - now)
            .min(DIGEST_IDLE_RECHECK);
        tokio::select! {
            _ = me.digest_wake.notified() => {}
            _ = tokio::time::sleep(wait) => {}
        }
    }
}

/// Where the tap is in the voice session's event log.
#[derive(Default)]
struct Cursor {
    voice_id: Option<String>,
    seq: i32,
    turns: TurnAssembler,
}

impl Cursor {
    fn at(voice_id: String, seq: i32) -> Self {
        Cursor {
            voice_id: Some(voice_id),
            seq,
            turns: TurnAssembler::default(),
        }
    }
}

/// Start after the existing voice session's last event: history is never
/// re-posted.
async fn discover(db: &Db) -> Cursor {
    let Ok(Some(session)) = db.find_expert_session(VOICE_EXPERT_KIND).await else {
        return Cursor::default();
    };
    let seq = db
        .events_tail(&session.id, 1)
        .await
        .ok()
        .and_then(|e| e.last().map(|e| e.seq))
        .unwrap_or(0);
    Cursor::at(session.id, seq)
}

async fn tap(weak: Weak<AssistantMirror>, mut rx: broadcast::Receiver<WsEvent>) {
    let mut cur = match weak.upgrade() {
        Some(me) => discover(&me.db).await,
        None => return,
    };
    loop {
        let ev = match rx.recv().await {
            Ok(ev) => Some(ev),
            Err(broadcast::error::RecvError::Lagged(n)) => {
                tracing::debug!(skipped = n, "assistant mirror: tap lagged, re-reading log");
                None
            }
            Err(broadcast::error::RecvError::Closed) => return,
        };
        let Some(me) = weak.upgrade() else { return };
        let Some(ev) = ev else {
            if cur.voice_id.is_none() {
                cur = discover(&me.db).await;
            }
            me.catch_up(&mut cur).await;
            continue;
        };
        if ev.event_type != "event" {
            continue;
        }
        let kind = ev.data.get("kind").and_then(|k| k.as_str()).unwrap_or("");
        if kind != "user" && kind != "agent-end" {
            continue;
        }
        if cur.voice_id.as_deref() != Some(ev.session_id.as_str()) {
            // A user message elsewhere: is that session the (new) voice
            // session? One primary-key read per user message.
            if kind != "user" {
                continue;
            }
            match me.db.get_session(&ev.session_id).await {
                Ok(Some(s)) if s.expert_kind.as_deref() == Some(VOICE_EXPERT_KIND) => {
                    let seq = ev.data.get("seq").and_then(|v| v.as_i64()).unwrap_or(1);
                    cur = Cursor::at(s.id, i32::try_from(seq - 1).unwrap_or(0));
                }
                _ => continue,
            }
        } else if kind == "user" {
            // "Clear session" deletes the log and seq starts over at 1: a
            // user event at or below the cursor means the log was reset.
            let seq = ev.data.get("seq").and_then(|v| v.as_i64()).unwrap_or(0);
            if seq > 0 && seq <= i64::from(cur.seq) {
                cur = Cursor::at(ev.session_id.clone(), i32::try_from(seq - 1).unwrap_or(0));
            }
        }
        me.catch_up(&mut cur).await;
    }
}

impl AssistantMirror {
    /// Feed every voice-session event after the cursor to the assembler
    /// and publish what it completes.
    async fn catch_up(&self, cur: &mut Cursor) {
        let Some(id) = cur.voice_id.clone() else {
            return;
        };
        loop {
            let page = match self.db.events_since_page(&id, cur.seq, EVENT_PAGE).await {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!("assistant mirror: event read failed: {e}");
                    return;
                }
            };
            for e in &page {
                cur.seq = e.seq;
                let data: serde_json::Value = serde_json::from_str(&e.data).unwrap_or_default();
                if let Some(line) = cur.turns.feed(&e.kind, &data) {
                    self.publish(line).await;
                }
            }
            if (page.len() as i64) < EVENT_PAGE {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use format::Speaker;

    #[derive(Default)]
    struct FakeMailer {
        sent: Mutex<Vec<(String, String)>>,
    }

    #[async_trait::async_trait]
    impl Mailer for FakeMailer {
        async fn send(
            &self,
            _smtp: &SmtpSettings,
            _to: &str,
            subject: &str,
            text: &str,
            _html: &str,
        ) -> Result<(), String> {
            lock(&self.sent).push((subject.to_string(), text.to_string()));
            Ok(())
        }
    }

    #[test]
    fn watched_window_is_75s() {
        let t0 = Instant::now();
        assert!(!watched_at(None, t0));
        assert!(watched_at(Some(t0), t0 + Duration::from_secs(75)));
        assert!(!watched_at(Some(t0), t0 + Duration::from_secs(76)));
    }

    #[test]
    fn digest_buffer_is_bounded_and_notes_drops() {
        let mut d = DigestBuf::default();
        let now = Instant::now();
        for i in 0..DIGEST_CAP + 3 {
            d.push(format!("m{i}"), now);
        }
        let (items, dropped) = d.take();
        assert_eq!(items.len(), DIGEST_CAP);
        assert_eq!(dropped, 3);
        assert_eq!(items[0], "m3");
        let (subject, text, html) = digest_bodies(&items[..2], dropped);
        assert_eq!(subject, "Peckboard Assistant: 2 new messages");
        assert!(text.starts_with("(3 older"), "{text}");
        assert!(html.contains("<p style"));
        assert_eq!(
            digest_bodies(&items[..1], 0).0,
            "Peckboard Assistant: 1 new message"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn digest_waits_for_quiet_then_rate_limits() {
        let mailer = Arc::new(FakeMailer::default());
        let mut settings = MirrorSettings::default();
        settings.email.enabled = true;
        settings.email.to = "me@example.com".into();
        let m = AssistantMirror::new(Db::in_memory().unwrap(), settings, mailer.clone());
        let sent = || lock(&mailer.sent).len();

        m.push_digest("You: one".into());
        tokio::time::sleep(Duration::from_secs(60)).await;
        // A second turn restarts the 2-minute quiet timer.
        m.push_digest("Assistant: two".into());
        tokio::time::sleep(Duration::from_secs(119)).await;
        assert_eq!(sent(), 0);
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(sent(), 1);
        assert_eq!(
            lock(&mailer.sent)[0].0,
            "Peckboard Assistant: 2 new messages"
        );

        // The next turn rolls into a digest no sooner than 15 min later.
        m.push_digest("You: three".into());
        tokio::time::sleep(Duration::from_secs(5 * 60)).await;
        assert_eq!(sent(), 1);
        tokio::time::sleep(Duration::from_secs(10 * 60)).await;
        assert_eq!(sent(), 2);
        assert_eq!(lock(&mailer.sent)[1].1, "You: three");
    }

    #[tokio::test]
    async fn watched_turns_skip_the_digest() {
        let mailer = Arc::new(FakeMailer::default());
        let mut settings = MirrorSettings::default();
        settings.email.enabled = true;
        settings.email.to = "me@example.com".into();
        let m = AssistantMirror::new(Db::in_memory().unwrap(), settings, mailer);
        let line = Line {
            speaker: Speaker::You,
            text: "hello".into(),
        };
        m.note_panel_visible();
        m.publish(line.clone()).await;
        assert!(lock(&m.digest).items.is_empty());
        *lock(&m.last_visible) = None;
        m.publish(line).await;
        assert_eq!(lock(&m.digest).items.len(), 1);
    }

    #[tokio::test]
    async fn suppressed_mirror_sends_nothing() {
        let mailer = Arc::new(FakeMailer::default());
        let mut settings = MirrorSettings::default();
        settings.email.enabled = true;
        settings.email.to = "me@example.com".into();
        let m = AssistantMirror::build(Db::in_memory().unwrap(), settings, mailer.clone(), true);
        m.publish(Line {
            speaker: Speaker::You,
            text: "hello".into(),
        })
        .await;
        assert!(lock(&m.digest).items.is_empty());
        let err = m.send_test(Channel::Email).await.unwrap_err();
        assert!(err.contains(NO_RESUME_ENV), "{err}");
        assert!(lock(&mailer.sent).is_empty());
        assert_eq!(m.statuses().email.state, StatusState::Error);
    }
}
