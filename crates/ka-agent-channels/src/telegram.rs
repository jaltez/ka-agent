//! The Telegram channel (SEED.md §5): Bot API long polling in, throttled
//! messages + approval keyboards out. Plain text only — `parse_mode` is
//! never set, so model output is never interpreted as markup.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use ka_agent_core::audit::AuditRecord;
use ka_agent_core::{ApprovalDecision, ApprovalStatus, DaemonConfig, SessionStatus};
use ka_agent_runtime::surface::{ApprovalCard, Surface};
use ka_agent_runtime::{Error as RtError, Runtime};
use ka_agent_store::Store;
use serde_json::{Value, json};
use thiserror::Error;
use tokio::sync::{Mutex as AsyncMutex, watch};

const POLL_TIMEOUT_SECS: u64 = 25;
const POLL_HTTP_TIMEOUT: Duration = Duration::from_secs(40);
const TYPING_INTERVAL: Duration = Duration::from_secs(4);
const NOTE_EDIT_INTERVAL_MS: i64 = 3_000;
const SEND_SPACING_MS: i64 = 1_000;
const UNAUTHORIZED_NOTE_SECS: i64 = 60;

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
    fn is_auth_failure(&self) -> bool {
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
    if text.matches("```").count() % 2 == 0 {
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
}

impl TelegramClient {
    pub(crate) fn new(api_base: &str, token: &str) -> Self {
        Self {
            http: reqwest::Client::new(),
            base: format!("{api_base}/bot{token}"),
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

struct SurfaceInner {
    client: TelegramClient,
    chat_id: AtomicI64,
    progress_message_id: Mutex<Option<i64>>,
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
            progress_message_id: Mutex::new(None),
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

    fn set_chat(&self, chat_id: i64) {
        self.chat_id
            .compare_exchange(0, chat_id, Ordering::AcqRel, Ordering::Acquire)
            .ok();
    }

    fn spawn_send(&self, text: String) {
        if let Some(chat) = self.chat() {
            let client = self.client.clone();
            tokio::spawn(async move {
                let _ = client.send_message(chat, &text, None).await;
            });
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
    /// No chat yet — the surface no-ops until the poller learns the chat id
    /// from the first allowed inbound message (`set_chat`).
    pub fn new(api_base: String, token: String) -> Self {
        let inner = Arc::new(SurfaceInner::new(TelegramClient::new(&api_base, &token)));
        surfaces()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(token, Arc::clone(&inner));
        Self { inner }
    }

    /// Called by the poller on the first allowed inbound message.
    pub fn set_chat(&self, chat_id: i64) {
        self.inner.set_chat(chat_id);
    }
}

impl Surface for TelegramSurface {
    fn on_turn_started(&self) {
        let Some(chat) = self.inner.chat() else {
            return;
        };
        self.inner
            .last_progress_edit_ms
            .store(now_ms(), Ordering::Release);
        // typing indicator until the turn ends
        {
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
        // progress message
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move {
            if let Ok(message_id) = inner.client.send_message(chat, "…", None).await {
                *inner
                    .progress_message_id
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some(message_id);
            }
        });
    }

    /// Notes are folded into the progress message at most every 3 s; the
    /// last 3 buffered notes are shown on the next edit.
    fn on_note(&self, text: &str) {
        let Some(chat) = self.inner.chat() else {
            return;
        };
        {
            let mut notes = self
                .inner
                .pending_notes
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            notes.push(text.to_string());
        }
        let last = self.inner.last_progress_edit_ms.load(Ordering::Acquire);
        let now = now_ms();
        if now - last < NOTE_EDIT_INTERVAL_MS {
            return;
        }
        self.inner
            .last_progress_edit_ms
            .store(now, Ordering::Release);
        let notes: Vec<String> = {
            let mut notes = self
                .inner
                .pending_notes
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let keep = notes.len().saturating_sub(3);
            let shown = notes.split_off(keep);
            *notes = Vec::new();
            shown
        };
        if notes.is_empty() {
            return;
        }
        let message_id = *self
            .inner
            .progress_message_id
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let Some(message_id) = message_id else { return };
        let text = format!("…\n{}", notes.join("\n"));
        let client = self.inner.client.clone();
        tokio::spawn(async move {
            let _ = client
                .edit_message_text(chat, message_id, &text, None)
                .await;
        });
    }

    /// No per-delta edits: streaming text over Bot API edits would be a
    /// throttle hazard; final chunks carry the content.
    fn on_delta_text(&self, _text: &str) {}

    fn on_delta_thought(&self, _text: &str) {}

    fn on_final_chunk(&self, _index: u32, text: &str) {
        let Some(_chat) = self.inner.chat() else {
            return;
        };
        let inner = Arc::clone(&self.inner);
        let text = balance_fences(text);
        tokio::spawn(async move {
            // ≥1 s spacing between sends
            let mut last = inner.last_send_ms.lock().await;
            let wait = SEND_SPACING_MS - (now_ms() - *last);
            if wait > 0 {
                tokio::time::sleep(Duration::from_millis(wait as u64)).await;
            }
            if let Some(chat) = inner.chat() {
                if inner.client.send_message(chat, &text, None).await.is_ok() {
                    *last = now_ms();
                }
            }
        });
    }

    fn on_turn_finished(&self, stop: &str, cost_usd: f64, tokens: u64) {
        if let Some(handle) = self
            .inner
            .typing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            handle.abort();
        }
        let Some(chat) = self.inner.chat() else {
            return;
        };
        let message_id = *self
            .inner
            .progress_message_id
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let Some(message_id) = message_id else { return };
        let text = format!("· turn {stop} · ${cost_usd:.4} · {tokens} tk");
        let client = self.inner.client.clone();
        tokio::spawn(async move {
            let _ = client
                .edit_message_text(chat, message_id, &text, None)
                .await;
        });
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
        let mins = card.expires_at_ms / 60_000;
        let local = mins.rem_euclid(24 * 60);
        text.push_str(&format!("\n\nexpires {:02}:{:02}", local / 60, local % 60));
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
        // out first, oldest first, before any new traffic.
        let recover = self
            .store
            .pending_chunks_for_session(&self.runtime.session_id())
            .unwrap_or_default();
        for chunk in &recover {
            if let Some(chat) = surface.chat() {
                if client.send_message(chat, &chunk.text, None).await.is_ok() {
                    self.store
                        .mark_sent(&chunk.turn, chunk.chunk_index, chunk.chunk_index as i64)
                        .ok();
                }
            } else {
                eprintln!(
                    "telegram: {} undelivered chunk(s) from before the crash, \
                     but no chat to send them to; they stay queued",
                    recover.len()
                );
                break;
            }
        }
        if !recover.is_empty() {
            eprintln!("telegram: recovered {} queued chunk(s)", recover.len());
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
        if !self.gate(uid, chat_type, unauthorized_last).await {
            return;
        }
        let chat_id = message["chat"]["id"].as_i64().unwrap_or(0);
        surface.set_chat(chat_id);
        let client = surface.client.clone();

        if message.get("photo").is_some() || message.get("document").is_some() {
            let _ = client
                .send_message(chat_id, "file uploads arrive in the next milestone", None)
                .await;
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
                if let Err(e) = self
                    .runtime
                    .handle_message(text, &format!("telegram:{uid}"))
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
        surface.set_chat(chat_id);
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
