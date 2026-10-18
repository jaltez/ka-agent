//! The Telegram channel (SEED.md §5): Bot API long polling in, throttled
//! messages + approval keyboards out. Final answers and notices render a
//! safe HTML subset (`tg_format`, everything escaped first) with a
//! plain-text fallback; router replies and approval cards stay plain.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use ka_agent_core::audit::AuditRecord;
use ka_agent_core::{ApprovalDecision, ApprovalStatus, DaemonConfig, OutboxChunk, SessionStatus};
use ka_agent_runtime::surface::{ApprovalCard, Surface};
use ka_agent_runtime::{Error as RtError, Runtime};
use ka_agent_store::Store;
use serde_json::{Value, json};
use thiserror::Error;
use tokio::sync::{Mutex as AsyncMutex, watch};

use crate::pairing::{self, Redeem};
use crate::tg_format::to_telegram_html;

const POLL_TIMEOUT_SECS: u64 = 25;
const POLL_HTTP_TIMEOUT: Duration = Duration::from_secs(40);
const TYPING_INTERVAL: Duration = Duration::from_secs(4);
const NOTE_EDIT_INTERVAL_MS: i64 = 3_000;
const SEND_SPACING_MS: i64 = 1_000;
const UNAUTHORIZED_NOTE_SECS: i64 = 60;
const START_REPLY_INTERVAL_MS: i64 = 60 * 60 * 1000;
const MAX_MESSAGE_CHARS: usize = 4096;
const START_REPLY: &str = "This is a private assistant. If you are its owner, run `ka-agent pair telegram` on your computer and send the code here.";

#[derive(Debug, Error)]
pub(crate) enum ChannelError {
    #[error("telegram api: {description}")]
    Api { description: String },
    #[error("telegram http: {0}")]
    Http(#[from] reqwest::Error),
}

impl ChannelError {
    fn api(description: impl Into<String>) -> Self {
        Self::Api {
            description: description.into(),
        }
    }

    /// Bad-token failures: Telegram answers 401/404 with these descriptions.
    pub(crate) fn is_auth_failure(&self) -> bool {
        match self {
            Self::Api { description } => {
                description.contains("Unauthorized")
                    || description.contains("Not Found")
                    || description.contains("401")
                    || description.contains("404")
            }
            Self::Http(_) => false,
        }
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Count of ``` fences in `text` is odd → make the chunk render as a
/// balanced block: if the chunk itself opens the block (starts with a
/// fence) append a closer; if it continues a block opened in an earlier
/// chunk, prepend the opener.
pub(crate) fn balance_fences(text: &str) -> String {
    if text.matches("```").count().is_multiple_of(2) {
        return text.to_string();
    }
    if text.trim_start().starts_with("```") {
        format!("{text}\n```")
    } else {
        format!("```\n{text}")
    }
}

/// Last `n` characters of an id, for compact rendering and callback data.
fn short_id(id: &str, n: usize) -> String {
    id.chars()
        .rev()
        .take(n)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

// ---------------------------------------------------------------------------
// Bot API client
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub(crate) struct TelegramClient {
    http: reqwest::Client,
    base: String,
    download_base: String,
}

impl TelegramClient {
    pub(crate) fn new(api_base: &str, token: &str) -> Self {
        Self {
            http: reqwest::Client::new(),
            base: format!("{api_base}/bot{token}"),
            download_base: format!("{api_base}/file/bot{token}"),
        }
    }

    async fn call(&self, method: &str, body: Value) -> Result<Value, ChannelError> {
        let resp = self
            .http
            .post(format!("{}/{method}", self.base))
            .timeout(POLL_HTTP_TIMEOUT)
            .json(&body)
            .send()
            .await?;
        let v: Value = resp.json().await?;
        if v["ok"].as_bool() == Some(true) {
            Ok(v["result"].clone())
        } else {
            Err(ChannelError::api(
                v["description"].as_str().unwrap_or("unknown api error"),
            ))
        }
    }

    pub(crate) async fn get_me(&self) -> Result<Value, ChannelError> {
        self.call("getMe", json!({})).await
    }

    pub(crate) async fn get_updates(
        &self,
        offset: i64,
        timeout_secs: u64,
    ) -> Result<Vec<Value>, ChannelError> {
        let result = self
            .call(
                "getUpdates",
                json!({
                    "offset": offset,
                    "timeout": timeout_secs,
                    "allowed_updates": ["message", "callback_query"],
                }),
            )
            .await?;
        Ok(result.as_array().cloned().unwrap_or_default())
    }

    pub(crate) async fn send_message(
        &self,
        chat_id: i64,
        text: &str,
        reply_markup: Option<Value>,
    ) -> Result<i64, ChannelError> {
        let mut body = json!({ "chat_id": chat_id, "text": text });
        if let Some(markup) = reply_markup {
            body["reply_markup"] = markup;
        }
        let result = self.call("sendMessage", body).await?;
        Ok(result["message_id"].as_i64().unwrap_or(0))
    }

    /// `sendMessage` with `parse_mode: HTML`; the caller supplies markup
    /// built by `tg_format` (input already escaped).
    pub(crate) async fn send_message_html(
        &self,
        chat_id: i64,
        html: &str,
        reply_markup: Option<Value>,
    ) -> Result<i64, ChannelError> {
        let mut body = json!({ "chat_id": chat_id, "text": html, "parse_mode": "HTML" });
        if let Some(markup) = reply_markup {
            body["reply_markup"] = markup;
        }
        let result = self.call("sendMessage", body).await?;
        Ok(result["message_id"].as_i64().unwrap_or(0))
    }

    pub(crate) async fn delete_message(
        &self,
        chat_id: i64,
        message_id: i64,
    ) -> Result<(), ChannelError> {
        self.call(
            "deleteMessage",
            json!({ "chat_id": chat_id, "message_id": message_id }),
        )
        .await
        .map(|_| ())
    }

    pub(crate) async fn edit_message_text(
        &self,
        chat_id: i64,
        message_id: i64,
        text: &str,
        reply_markup: Option<Value>,
    ) -> Result<(), ChannelError> {
        let mut body = json!({ "chat_id": chat_id, "message_id": message_id, "text": text });
        if let Some(markup) = reply_markup {
            body["reply_markup"] = markup;
        }
        self.call("editMessageText", body).await.map(|_| ())
    }

    pub(crate) async fn answer_callback_query(
        &self,
        callback_query_id: &str,
        text: &str,
    ) -> Result<(), ChannelError> {
        self.call(
            "answerCallbackQuery",
            json!({ "callback_query_id": callback_query_id, "text": text }),
        )
        .await
        .map(|_| ())
    }

    pub(crate) async fn send_chat_action(&self, chat_id: i64) -> Result<(), ChannelError> {
        self.call(
            "sendChatAction",
            json!({ "chat_id": chat_id, "action": "typing" }),
        )
        .await
        .map(|_| ())
    }

    pub(crate) async fn set_my_commands(&self) -> Result<Value, ChannelError> {
        // Built from the shared command table so the menu can never drift
        // from what `/help` says (fixes /skills and /skill missing here).
        let commands: Vec<serde_json::Value> = crate::commands::COMMANDS
            .iter()
            .filter(|c| c.telegram)
            .map(|c| {
                json!({
                    "command": c.name,
                    "description": c.summary.chars().take(256).collect::<String>(),
                })
            })
            .collect();
        self.call(
            "setMyCommands",
            json!({
                "commands": commands
            }),
        )
        .await
    }
    /// `getFile`: resolve a file_id to the API-relative `file_path`.
    pub(crate) async fn get_file(&self, file_id: &str) -> Result<String, ChannelError> {
        let result = self.call("getFile", json!({ "file_id": file_id })).await?;
        result["file_path"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| ChannelError::api("getFile: no file_path"))
    }

    /// Download one file by its API-relative path. Bot API downloads cap
    /// at 20 MB; anything bigger is refused before it is written anywhere.
    pub(crate) async fn download_file(&self, file_path: &str) -> Result<Vec<u8>, ChannelError> {
        const MAX: usize = 20 * 1024 * 1024;
        let resp = self
            .http
            .get(format!("{}/{file_path}", self.download_base))
            .timeout(POLL_HTTP_TIMEOUT)
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(ChannelError::api("download failed"));
        }
        let bytes = resp.bytes().await?;
        if bytes.len() > MAX {
            return Err(ChannelError::api("file exceeds the 20 MB download cap"));
        }
        Ok(bytes.to_vec())
    }
}

/// Validate a bot token with `getMe`; returns the bot's username.
pub async fn telegram_get_me(api_base: &str, token: &str) -> Result<String, String> {
    let me = TelegramClient::new(api_base, token)
        .get_me()
        .await
        .map_err(|e| e.to_string())?;
    me["username"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "telegram getMe: no username in the answer".to_string())
}

// ---------------------------------------------------------------------------
// Surface (outbound, throttled)
// ---------------------------------------------------------------------------

/// The daemon constructs the surface before the runtime exists, then the
/// poller picks the same state up by token (see `take_surface_state`), so
/// callbacks can resolve the cards the runtime rendered.
fn surfaces() -> &'static Mutex<HashMap<String, Arc<SurfaceInner>>> {
    static REGISTRY: LazyLock<Mutex<HashMap<String, Arc<SurfaceInner>>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    &REGISTRY
}

fn take_surface_state(client: TelegramClient, token: &str) -> Arc<SurfaceInner> {
    let mut map = surfaces().lock().unwrap_or_else(|e| e.into_inner());
    map.remove(token)
        .unwrap_or_else(|| Arc::new(SurfaceInner::new(client)))
}

/// The in-turn progress message: none yet, being sent, or on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Progress {
    None,
    Sending,
    Shown(i64),
}

struct SurfaceInner {
    client: TelegramClient,
    chat_id: AtomicI64,
    /// Set between `on_turn_started` and `on_turn_finished`; notes outside
    /// a turn are dropped (the journal surface keeps them).
    turn_active: AtomicBool,
    progress: Mutex<Progress>,
    last_progress_edit_ms: AtomicI64,
    pending_notes: Mutex<Vec<String>>,
    /// short approval id → (full id, message id, question first line)
    cards: Mutex<HashMap<String, (String, i64, String)>>,
    last_send_ms: AsyncMutex<i64>,
    typing: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl SurfaceInner {
    fn new(client: TelegramClient) -> Self {
        Self {
            client,
            chat_id: AtomicI64::new(0),
            turn_active: AtomicBool::new(false),
            progress: Mutex::new(Progress::None),
            last_progress_edit_ms: AtomicI64::new(0),
            pending_notes: Mutex::new(Vec::new()),
            cards: Mutex::new(HashMap::new()),
            last_send_ms: AsyncMutex::new(0),
            typing: Mutex::new(None),
        }
    }

    fn chat(&self) -> Option<i64> {
        match self.chat_id.load(Ordering::Acquire) {
            0 => None,
            id => Some(id),
        }
    }

    /// Remember the operator's chat (the last private chat of a linked
    /// user wins); `true` when it changed.
    fn set_chat(&self, chat_id: i64) -> bool {
        chat_id != 0 && self.chat_id.swap(chat_id, Ordering::AcqRel) != chat_id
    }

    fn spawn_send(&self, text: String) {
        if let Some(chat) = self.chat() {
            let client = self.client.clone();
            tokio::spawn(async move {
                let _ = client.send_message(chat, &text, None).await;
            });
        }
    }

    /// Send one message as rendered HTML, keeping ≥1 s between sends.
    /// Falls back to plain text when the HTML is too long or Telegram
    /// refuses it (`can't parse entities`).
    async fn send_rich(&self, text: &str) {
        let mut last = self.last_send_ms.lock().await;
        let wait = SEND_SPACING_MS - (now_ms() - *last);
        if wait > 0 {
            tokio::time::sleep(Duration::from_millis(wait as u64)).await;
        }
        let Some(chat) = self.chat() else { return };
        let plain = balance_fences(text);
        let html = to_telegram_html(&plain);
        let mut fallback = true;
        if html.chars().count() <= MAX_MESSAGE_CHARS {
            match self.client.send_message_html(chat, &html, None).await {
                Ok(_) => fallback = false,
                Err(ChannelError::Api { description }) => {
                    eprintln!("telegram: HTML refused ({description}); sending plain text");
                }
                Err(e) => {
                    eprintln!("telegram: send: {e}");
                    fallback = false;
                }
            }
        }
        if fallback && let Err(e) = self.client.send_message(chat, &plain, None).await {
            eprintln!("telegram: send: {e}");
        }
        *last = now_ms();
    }

    fn stop_typing(&self) {
        if let Some(handle) = self.typing.lock().unwrap_or_else(|e| e.into_inner()).take() {
            handle.abort();
        }
    }

    fn settle_card(&self, short: &str, status: ApprovalStatus) {
        let Some(chat) = self.chat() else { return };
        let mut cards = self.cards.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_full, message_id, first_line)) = cards.remove(short) {
            let text = format!("[{status:?}] {first_line}");
            let client = self.client.clone();
            tokio::spawn(async move {
                let _ = client
                    .edit_message_text(
                        chat,
                        message_id,
                        &text,
                        Some(json!({ "inline_keyboard": [] })),
                    )
                    .await;
            });
        }
    }
}

/// Outbound Telegram surface. All methods are cheap and non-blocking:
/// network work is spawned; throttling state lives in the shared inner.
pub struct TelegramSurface {
    inner: Arc<SurfaceInner>,
}

impl TelegramSurface {
    /// `initial_chat` is the persisted `telegram.chat_id` meta value, so
    /// proactive notices reach the operator right after a restart. Without
    /// it the surface no-ops until the poller learns the chat id from an
    /// allowed inbound message (`set_chat`).
    pub fn new(api_base: String, token: String, initial_chat: Option<i64>) -> Self {
        let inner = Arc::new(SurfaceInner::new(TelegramClient::new(&api_base, &token)));
        if let Some(chat) = initial_chat {
            inner.set_chat(chat);
        }
        surfaces()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(token, Arc::clone(&inner));
        Self { inner }
    }

    /// Called by the poller on allowed inbound messages.
    pub fn set_chat(&self, chat_id: i64) {
        self.inner.set_chat(chat_id);
    }
}

impl Surface for TelegramSurface {
    /// Typing indicator only; a progress message appears with the first note.
    fn on_turn_started(&self) {
        self.inner.turn_active.store(true, Ordering::Release);
        *self
            .inner
            .progress
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Progress::None;
        self.inner
            .pending_notes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        let Some(chat) = self.inner.chat() else {
            return;
        };
        let mut typing = self.inner.typing.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(old) = typing.take() {
            old.abort();
        }
        let client = self.inner.client.clone();
        *typing = Some(tokio::spawn(async move {
            loop {
                let _ = client.send_chat_action(chat).await;
                tokio::time::sleep(TYPING_INTERVAL).await;
            }
        }));
    }

    /// In-turn progress: the first note sends `⏳ {note}`; later notes edit
    /// that message (last 3 shown) at most every 3 s. Outside a turn notes
    /// are dropped here.
    fn on_note(&self, text: &str) {
        if !self.inner.turn_active.load(Ordering::Acquire) {
            return;
        }
        let Some(chat) = self.inner.chat() else {
            return;
        };
        let shown = {
            let mut notes = self
                .inner
                .pending_notes
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            notes.push(text.to_string());
            let drop_n = notes.len().saturating_sub(3);
            notes.drain(..drop_n);
            notes.join("\n")
        };
        let now = now_ms();
        let mut progress = self
            .inner
            .progress
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match *progress {
            Progress::None => {
                *progress = Progress::Sending;
                self.inner
                    .last_progress_edit_ms
                    .store(now, Ordering::Release);
                let inner = Arc::clone(&self.inner);
                let body = format!("⏳ {text}");
                tokio::spawn(async move {
                    let sent = inner.client.send_message(chat, &body, None).await.ok();
                    let orphan = {
                        let mut p = inner.progress.lock().unwrap_or_else(|e| e.into_inner());
                        match (sent, *p) {
                            (Some(id), Progress::Sending) => {
                                *p = Progress::Shown(id);
                                None
                            }
                            (None, Progress::Sending) => {
                                *p = Progress::None;
                                None
                            }
                            // the turn ended while the send was in flight
                            (sent, _) => sent,
                        }
                    };
                    if let Some(id) = orphan {
                        let _ = inner.client.delete_message(chat, id).await;
                    }
                });
            }
            Progress::Sending => {}
            Progress::Shown(message_id) => {
                let last = self.inner.last_progress_edit_ms.load(Ordering::Acquire);
                if now - last < NOTE_EDIT_INTERVAL_MS {
                    return;
                }
                self.inner
                    .last_progress_edit_ms
                    .store(now, Ordering::Release);
                let client = self.inner.client.clone();
                let body = format!("⏳ {shown}");
                tokio::spawn(async move {
                    let _ = client
                        .edit_message_text(chat, message_id, &body, None)
                        .await;
                });
            }
        }
    }

    /// Operator-must-receive messages (reminders, cron output, pauses):
    /// always their own message.
    fn on_notice(&self, text: &str) {
        let inner = Arc::clone(&self.inner);
        let text = text.to_string();
        tokio::spawn(async move { inner.send_rich(&text).await });
    }

    /// No per-delta edits: streaming text over Bot API edits would be a
    /// throttle hazard; final chunks carry the content.
    fn on_delta_text(&self, _text: &str) {}

    fn on_delta_thought(&self, _text: &str) {}

    fn on_final_chunk(&self, _index: u32, text: &str) {
        if self.inner.chat().is_none() {
            return;
        }
        let inner = Arc::clone(&self.inner);
        let text = text.to_string();
        tokio::spawn(async move { inner.send_rich(&text).await });
    }

    /// Stops typing and deletes the progress message; cost lives in /status.
    fn on_turn_finished(&self, _stop: &str, _cost_usd: f64, _tokens: u64) {
        self.inner.stop_typing();
        self.inner.turn_active.store(false, Ordering::Release);
        self.inner
            .pending_notes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        let previous = std::mem::replace(
            &mut *self
                .inner
                .progress
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
            Progress::None,
        );
        if let (Progress::Shown(message_id), Some(chat)) = (previous, self.inner.chat()) {
            let client = self.inner.client.clone();
            tokio::spawn(async move {
                let _ = client.delete_message(chat, message_id).await;
            });
        }
    }

    fn on_approval(&self, card: &ApprovalCard) {
        let Some(chat) = self.inner.chat() else {
            return;
        };
        let full = card.approval_id.clone();
        let short = short_id(&full, 12);
        let mut text = card.question.clone();
        if let Some(detail) = &card.detail {
            let head: Vec<&str> = detail.lines().take(20).collect();
            if !head.is_empty() {
                text.push_str("\n\n");
                text.push_str(&head.join("\n"));
            }
        }
        text.push_str(&format!(
            "\n\nexpires in {}",
            crate::commands::fmt_interval(card.expires_at_ms - now_ms())
        ));
        let first_line = card.question.lines().next().unwrap_or("").to_string();
        let markup = json!({
            "inline_keyboard": [
                [{ "text": "✅ Allow", "callback_data": format!("a:{short}:y") }],
                [{ "text": "⛔ Deny", "callback_data": format!("a:{short}:n") }]
            ]
        });
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move {
            if let Ok(message_id) = inner.client.send_message(chat, &text, Some(markup)).await {
                inner
                    .cards
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(short, (full, message_id, first_line));
            }
        });
    }

    fn on_approval_settled(&self, approval_id: &str, status: ApprovalStatus) {
        self.inner.settle_card(&short_id(approval_id, 12), status);
    }

    fn on_session_status(&self, _status: SessionStatus) {}

    fn on_error(&self, class: &str, message: &str) {
        self.inner.spawn_send(format!("! [{class}] {message}"));
    }
}

// ---------------------------------------------------------------------------
// Channel (inbound poller)
// ---------------------------------------------------------------------------

pub struct TelegramConfig {
    pub api_base: String,
    pub token: String,
    pub allow_user_ids: Vec<i64>,
}

impl TelegramConfig {
    pub fn from_config(cfg: &DaemonConfig, token: String) -> Self {
        Self {
            api_base: cfg.telegram.api_base().to_string(),
            token,
            allow_user_ids: cfg.telegram.allow_user_ids.clone(),
        }
    }
}

pub struct TelegramChannel {
    cfg: TelegramConfig,
    runtime: Runtime,
    store: Arc<Store>,
    /// uid → last `/start` reply (ms); strangers get at most one an hour.
    start_replied: Mutex<HashMap<i64, i64>>,
    /// Chunks recovered at startup while no chat was known; delivered on
    /// the first `set_chat`.
    undelivered: Mutex<Vec<OutboxChunk>>,
}

impl crate::channel::Channel for TelegramChannel {
    fn name(&self) -> &'static str {
        "telegram"
    }

    fn run(self, shutdown: watch::Receiver<bool>) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(Self::run(self, shutdown))
    }
}

impl TelegramChannel {
    pub fn new(cfg: TelegramConfig, runtime: Runtime, store: Arc<Store>) -> Self {
        Self {
            cfg,
            runtime,
            store,
            start_replied: Mutex::new(HashMap::new()),
            undelivered: Mutex::new(Vec::new()),
        }
    }

    /// Long-poll loop. Delivery is at-least-once with offset-based dedupe:
    /// each update is dispatched first, and only afterwards is
    /// `telegram.offset` persisted as `update_id + 1`. A crash between the
    /// two re-delivers the update; a duplicate within one process is
    /// filtered by `update_id <= offset`.
    pub async fn run(self, shutdown: watch::Receiver<bool>) {
        let client = TelegramClient::new(&self.cfg.api_base, &self.cfg.token);
        let surface = take_surface_state(client.clone(), &self.cfg.token);
        if surface.chat().is_none()
            && let Some(chat) = self
                .store
                .meta_get("telegram.chat_id")
                .ok()
                .flatten()
                .and_then(|s| s.parse::<i64>().ok())
        {
            surface.set_chat(chat);
        }
        let allow = self.cfg.allow_user_ids.clone();
        let mut unauthorized_last: HashMap<i64, i64> = HashMap::new();

        let mut offset = self
            .store
            .meta_get("telegram.offset")
            .ok()
            .flatten()
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0);

        // Crash recovery (SEED §3.7): chunks queued but never delivered go
        // out first, oldest first, before any new traffic. Without a known
        // chat they wait for the operator's first message.
        let recover = self
            .store
            .pending_chunks_for_session(&self.runtime.session_id())
            .unwrap_or_default();
        if !recover.is_empty() {
            eprintln!("telegram: recovered {} queued chunk(s)", recover.len());
            match surface.chat() {
                Some(chat) => self.deliver_recovered(&client, chat, recover).await,
                None => {
                    eprintln!("telegram: no chat known yet; they go out after your next message");
                    *self.undelivered.lock().unwrap_or_else(|e| e.into_inner()) = recover;
                }
            }
        }

        // Startup token check: fail fast on a bad token instead of
        // discovering it on the first poll.
        if let Err(e) = client.get_me().await {
            if e.is_auth_failure() {
                eprintln!("telegram: auth failure: {e}");
                if let Some(chat) = surface.chat() {
                    let _ = client
                        .send_message(chat, &format!("! [auth] {e}"), None)
                        .await;
                }
                return;
            }
            eprintln!("telegram: getMe: {e}");
        }
        // Register the command list so Telegram clients autocomplete the
        // router's vocabulary. Best-effort: a fake or restricted API that
        // refuses it must not disable the channel.
        if let Err(e) = client.set_my_commands().await {
            eprintln!("telegram: setMyCommands: {e} (continuing)");
        }

        let mut backoff_secs = 1u64;
        loop {
            if *shutdown.borrow() {
                break;
            }
            match client.get_updates(offset, POLL_TIMEOUT_SECS).await {
                Ok(updates) => {
                    backoff_secs = 1;
                    // dedupe against the offset this batch was fetched
                    // with, not the running one: every update in the batch
                    // is new relative to the batch start
                    let batch_offset = offset;
                    for update in &updates {
                        let Some(update_id) = update["update_id"].as_i64() else {
                            continue;
                        };
                        // `offset` is "next expected update id"; the server
                        // already excludes `update_id < offset`, this is the
                        // belt-and-braces at-least-once dedupe
                        if update_id < batch_offset {
                            continue;
                        }
                        self.dispatch(update, &allow, &surface, &mut unauthorized_last)
                            .await;
                        offset = update_id + 1;
                        if let Err(e) = self.store.meta_set("telegram.offset", &offset.to_string())
                        {
                            eprintln!("telegram: persist offset: {e}");
                        }
                    }
                    if updates.is_empty() {
                        // The real server blocks for the long-poll window;
                        // a fake (or instantly-empty) reply gets a short
                        // pacing sleep so the loop and shutdown stay cheap.
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
                Err(e) if e.is_auth_failure() => {
                    eprintln!("telegram: auth failure: {e}");
                    if let Some(chat) = surface.chat() {
                        let _ = client
                            .send_message(chat, &format!("! [auth] {e}"), None)
                            .await;
                    }
                    break;
                }
                Err(e) => {
                    eprintln!("telegram: poll error: {e}; backing off {backoff_secs}s");
                    tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                    backoff_secs = (backoff_secs * 2).clamp(1, 60);
                }
            }
        }
    }

    async fn dispatch(
        &self,
        update: &Value,
        allow: &[i64],
        surface: &Arc<SurfaceInner>,
        unauthorized_last: &mut HashMap<i64, i64>,
    ) {
        if let Some(message) = update.get("message") {
            self.dispatch_message(message, allow, surface, unauthorized_last)
                .await;
        } else if let Some(cb) = update.get("callback_query") {
            self.dispatch_callback(cb, allow, surface, unauthorized_last)
                .await;
        }
    }

    /// Private chats from **linked identities** only (SEED.md §8.1): the
    /// config allowlist is a migration source, the identities table is the
    /// authority. Everything else is dropped with (rate-limited) stderr
    /// noise + an audit row and NO reply.
    async fn gate(
        &self,
        uid: i64,
        chat_type: &str,
        unauthorized_last: &mut HashMap<i64, i64>,
    ) -> bool {
        let linked = chat_type == "private"
            && self
                .store
                .is_linked("telegram", &uid.to_string())
                .unwrap_or(false);
        if linked {
            return true;
        }
        let now = now_ms();
        let should_note = unauthorized_last
            .get(&uid)
            .is_none_or(|last| now - *last >= UNAUTHORIZED_NOTE_SECS * 1000);
        if should_note {
            eprintln!("telegram: unauthorized user {uid} (chat type {chat_type:?})");
            unauthorized_last.insert(uid, now);
        }
        let mut record = AuditRecord::new(
            &format!("telegram:{uid}"),
            "telegram.unauthorized",
            "",
            Some(format!("chat_type={chat_type}")),
        );
        if let Err(e) = self.store.append_audit(&mut record) {
            eprintln!("telegram: audit unauthorized: {e}");
        }
        false
    }

    /// Adopt `chat_id` as the operator's chat: persist it when it changed
    /// and flush chunks recovered while no chat was known.
    async fn adopt_chat(&self, surface: &Arc<SurfaceInner>, chat_id: i64) {
        if surface.set_chat(chat_id)
            && let Err(e) = self
                .store
                .meta_set("telegram.chat_id", &chat_id.to_string())
        {
            eprintln!("telegram: persist chat id: {e}");
        }
        let pending =
            std::mem::take(&mut *self.undelivered.lock().unwrap_or_else(|e| e.into_inner()));
        if !pending.is_empty() {
            self.deliver_recovered(&surface.client, chat_id, pending)
                .await;
        }
    }

    async fn deliver_recovered(
        &self,
        client: &TelegramClient,
        chat: i64,
        chunks: Vec<OutboxChunk>,
    ) {
        for chunk in &chunks {
            match client.send_message(chat, &chunk.text, None).await {
                Ok(message_id) => {
                    self.store
                        .mark_sent(&chunk.turn, chunk.chunk_index, message_id)
                        .ok();
                }
                Err(e) => eprintln!("telegram: recovered chunk: {e}"),
            }
        }
    }

    /// Unlinked private messages that may still get an answer: a correct
    /// `/pair <code>` links the sender; `/start` gets the one-line owner
    /// hint at most once an hour per uid. `true` = handled (linked).
    async fn unlinked_private(
        &self,
        message: &Value,
        uid: i64,
        text: &str,
        surface: &Arc<SurfaceInner>,
    ) -> bool {
        let chat_id = message["chat"]["id"].as_i64().unwrap_or(uid);
        if pairing::is_pair_command(text) {
            if pairing::redeem(&self.store, "telegram", &uid.to_string(), text) == Redeem::Linked {
                self.adopt_chat(surface, chat_id).await;
                let _ = surface
                    .client
                    .send_message(chat_id, pairing::TELEGRAM_WELCOME, None)
                    .await;
                return true;
            }
            return false;
        }
        let first = text.split_whitespace().next().unwrap_or("");
        if first.split('@').next() == Some("/start") {
            let now = now_ms();
            let reply = {
                let mut replied = self.start_replied.lock().unwrap_or_else(|e| e.into_inner());
                let due = replied
                    .get(&uid)
                    .is_none_or(|last| now - *last >= START_REPLY_INTERVAL_MS);
                if due {
                    replied.insert(uid, now);
                }
                due
            };
            if reply {
                let _ = surface
                    .client
                    .send_message(chat_id, START_REPLY, None)
                    .await;
            }
        }
        false
    }

    async fn dispatch_message(
        &self,
        message: &Value,
        _allow: &[i64],
        surface: &Arc<SurfaceInner>,
        unauthorized_last: &mut HashMap<i64, i64>,
    ) {
        let chat_type = message["chat"]["type"].as_str().unwrap_or("");
        let Some(uid) = message["from"]["id"].as_i64() else {
            return;
        };
        if chat_type == "private"
            && let Some(text) = message["text"].as_str()
            && !self
                .store
                .is_linked("telegram", &uid.to_string())
                .unwrap_or(false)
            && self.unlinked_private(message, uid, text, surface).await
        {
            return;
        }
        if !self.gate(uid, chat_type, unauthorized_last).await {
            return;
        }
        let chat_id = message["chat"]["id"].as_i64().unwrap_or(0);
        self.adopt_chat(surface, chat_id).await;
        let client = surface.client.clone();

        if message.get("photo").is_some() || message.get("document").is_some() {
            let reply = match self.save_attachment(&client, message).await {
                Ok(rel) => format!("saved {rel} — reference it with @{rel} in your next message"),
                Err(e) => format!("! [attachment] {e}"),
            };
            let _ = client.send_message(chat_id, &reply, None).await;
            return;
        }

        let Some(text) = message["text"].as_str() else {
            return;
        };

        // Commands route through the shared router; replies go to the same
        // chat. Approvals additionally offer the inline keyboard below.
        // Only free text reaches the engine (the router's contract): a
        // command both answered *and* submitted used to burn a real turn —
        // `/new` answered, then sent the literal "/new" as the fresh
        // session's first prompt.
        match crate::commands::route(&self.runtime, &format!("telegram:{uid}"), text).await {
            crate::commands::Outcome::Reply(reply) => {
                let _ = client.send_message(chat_id, &reply, None).await;
            }
            crate::commands::Outcome::Silent => {}
            crate::commands::Outcome::NotACommand => {
                let text = crate::refs::expand(&self.store, &self.runtime.workspace(), text).await;
                if let Err(e) = self
                    .runtime
                    .handle_message(&text, &format!("telegram:{uid}"))
                    .await
                {
                    eprintln!("telegram: handle_message: {e}");
                }
            }
        }
    }

    async fn dispatch_callback(
        &self,
        cb: &Value,
        _allow: &[i64],
        surface: &Arc<SurfaceInner>,
        unauthorized_last: &mut HashMap<i64, i64>,
    ) {
        let Some(uid) = cb["from"]["id"].as_i64() else {
            return;
        };
        let chat_type = cb["message"]["chat"]["type"].as_str().unwrap_or("");
        if !self.gate(uid, chat_type, unauthorized_last).await {
            return;
        }
        let chat_id = cb["message"]["chat"]["id"].as_i64().unwrap_or(0);
        self.adopt_chat(surface, chat_id).await;
        let client = surface.client.clone();
        let Some(data) = cb["data"].as_str() else {
            return;
        };
        // `a:<short>:y|n`
        let Some(rest) = data.strip_prefix("a:") else {
            return;
        };
        let Some((short, verdict)) = rest.rsplit_once(':') else {
            return;
        };
        if !matches!(verdict, "y" | "n") {
            return;
        }
        let full = {
            let cards = surface.cards.lock().unwrap_or_else(|e| e.into_inner());
            cards
                .get(short)
                .map(|(full, _, _)| full.clone())
                .or_else(|| {
                    // fallback: suffix match against pending approvals
                    self.store
                        .pending_approvals(&self.runtime.session_id())
                        .ok()
                        .and_then(|pending| {
                            pending
                                .into_iter()
                                .map(|a| a.id)
                                .find(|id| id.as_str().ends_with(short))
                                .map(|id| id.as_str().to_string())
                        })
                })
        };
        let Some(full) = full else {
            let _ = client
                .answer_callback_query(cb["id"].as_str().unwrap_or(""), "expired")
                .await;
            return;
        };
        let decision = if verdict == "y" {
            ApprovalDecision::Allow
        } else {
            ApprovalDecision::Deny
        };
        match self.runtime.decide(&full, decision).await {
            Ok(()) => {
                let _ = client
                    .answer_callback_query(
                        cb["id"].as_str().unwrap_or(""),
                        if verdict == "y" {
                            "✔ approved"
                        } else {
                            "✖ denied"
                        },
                    )
                    .await;
            }
            Err(RtError::UnknownApproval(_)) => {
                surface.settle_card(short, ApprovalStatus::Expired);
                let _ = client
                    .answer_callback_query(cb["id"].as_str().unwrap_or(""), "expired")
                    .await;
            }
            Err(e) => {
                eprintln!("telegram: decide {full}: {e}");
                let _ = client
                    .answer_callback_query(cb["id"].as_str().unwrap_or(""), "error")
                    .await;
            }
        }
    }

    /// File inbox (SEED.md §5.4): download the message's largest photo or
    /// its document into `<workspace>/attachments/` and return the
    /// workspace-relative path. `@reference` expansion resolves against
    /// the workspace, so the operator's next message can use the file by
    /// pasting the returned `@attachments/…` token.
    async fn save_attachment(
        &self,
        client: &TelegramClient,
        message: &Value,
    ) -> Result<String, String> {
        // Photos arrive as size-ascending variants; the last is the best.
        let (file_id, name): (String, String) = if let Some(sizes) = message["photo"].as_array() {
            let best = sizes
                .iter()
                .filter_map(|p| p["file_id"].as_str())
                .next_back()
                .unwrap_or("");
            if best.is_empty() {
                return Err("photo without a file_id".into());
            }
            (best.to_string(), format!("photo-{}.jpg", short_id(best, 8)))
        } else if let Some(doc) = message["document"].as_object() {
            let file_id = doc
                .get("file_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if file_id.is_empty() {
                return Err("document without a file_id".into());
            }
            let raw = doc.get("file_name").and_then(Value::as_str).unwrap_or("");
            let name = sanitize_attachment_name(raw)
                .unwrap_or_else(|| format!("doc-{}.bin", short_id(&file_id, 8)));
            (file_id, name)
        } else {
            return Err("message carries no photo or document".into());
        };
        let file_path = client.get_file(&file_id).await.map_err(|e| e.to_string())?;
        let bytes = client
            .download_file(&file_path)
            .await
            .map_err(|e| e.to_string())?;
        let dir = self.runtime.workspace().join("attachments");
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        // Collisions never overwrite: first free `-N` suffix wins.
        let mut path = dir.join(&name);
        let mut n = 1;
        while path.exists() {
            n += 1;
            let stem = name.rsplit_once('.').map_or(name.as_str(), |(s, _)| s);
            let ext = name.rsplit_once('.').map_or("", |(_, e)| e);
            let candidate = if ext.is_empty() {
                format!("{stem}-{n}")
            } else {
                format!("{stem}-{n}.{ext}")
            };
            path = dir.join(candidate);
        }
        std::fs::write(&path, &bytes).map_err(|e| e.to_string())?;
        path.strip_prefix(self.runtime.workspace())
            .map(|p| p.display().to_string())
            .map_err(|_| "attachment escaped the workspace".to_string())
    }
}

/// `[A-Za-z0-9._-]` only, no leading dot, ≤ 64 chars, never empty.
fn sanitize_attachment_name(raw: &str) -> Option<String> {
    let mut out = String::new();
    for c in raw.chars() {
        if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    while out.starts_with('.') {
        out.remove(0);
    }
    while out.contains("..") {
        out = out.replace("..", ".");
    }
    let trimmed: String = out.chars().take(64).collect();
    (!trimmed.is_empty()).then_some(trimmed)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::balance_fences;

    #[test]
    fn fence_balancing() {
        // even count: untouched
        assert_eq!(balance_fences("plain"), "plain");
        assert_eq!(balance_fences("```a\nb``` tail"), "```a\nb``` tail");
        // odd count, block opened in this chunk: closer appended
        assert_eq!(
            balance_fences("```rust\nfn main() {}"),
            "```rust\nfn main() {}\n```"
        );
        // odd count, chunk continues an earlier block: opener prepended
        assert_eq!(
            balance_fences("still in the block```"),
            "```\nstill in the block```"
        );
        // every balanced result has an even fence count
        for balanced in [
            balance_fences("```rust\nfn main() {}"),
            balance_fences("still in the block```"),
        ] {
            assert_eq!(balanced.matches("```").count() % 2, 0);
        }
    }
}
