// TelegramSurface: the outbound Surface impl (drafts, progress, cards). Split from telegram.rs (moves only).

use super::*;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use ka_agent_core::{ApprovalStatus, SessionStatus};
use ka_agent_runtime::surface::{ApprovalCard, Surface};
use serde_json::json;
use tokio::sync::Mutex as AsyncMutex;
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

pub(super) fn take_surface_state(client: TelegramClient, token: &str) -> Arc<SurfaceInner> {
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

pub(super) struct SurfaceInner {
    pub(super) client: TelegramClient,
    chat_id: AtomicI64,
    /// Set between `on_turn_started` and `on_turn_finished`; notes outside
    /// a turn are dropped (the journal surface keeps them).
    turn_active: AtomicBool,
    progress: Mutex<Progress>,
    last_progress_edit_ms: AtomicI64,
    pending_notes: Mutex<Vec<String>>,
    /// short approval id → (full id, message id, question first line)
    pub(super) cards: Mutex<HashMap<String, (String, i64, String)>>,
    last_send_ms: AsyncMutex<i64>,
    typing: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Stream replies with `sendMessageDraft`; cleared for good the first
    /// time the API refuses a draft (older Bot API servers).
    drafts: AtomicBool,
    /// One draft request at a time: a slow one is never overtaken by an
    /// older snapshot.
    draft_in_flight: AtomicBool,
    draft: Mutex<Draft>,
}

/// The reply being streamed in the current turn.
#[derive(Default)]
struct Draft {
    id: i64,
    text: String,
    pushed_len: usize,
    pushed_ms: i64,
}

/// Throttle for `sendMessageDraft`: at most one per second, and only when
/// the text grew enough to be worth a request.
const DRAFT_MIN_INTERVAL_MS: i64 = 1000;
const DRAFT_MIN_GROWTH: usize = 40;

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
            drafts: AtomicBool::new(true),
            draft_in_flight: AtomicBool::new(false),
            draft: Mutex::new(Draft::default()),
        }
    }

    pub(super) fn chat(&self) -> Option<i64> {
        match self.chat_id.load(Ordering::Acquire) {
            0 => None,
            id => Some(id),
        }
    }

    /// Remember the operator's chat (the last private chat of a linked
    /// user wins); `true` when it changed.
    pub(super) fn set_chat(&self, chat_id: i64) -> bool {
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
        let html =
            ka_agent_core::markdown::render(&plain, ka_agent_core::markdown::Target::Telegram);
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

    /// Add streamed text to the draft and push it when the throttle allows.
    fn push_draft(self: &Arc<Self>, delta: &str) {
        if !self.drafts.load(Ordering::Acquire) {
            return;
        }
        let Some(chat) = self.chat() else { return };
        let snapshot = {
            let mut d = self.draft.lock().unwrap_or_else(|e| e.into_inner());
            if d.id == 0 {
                return; // no turn started on this surface
            }
            d.text.push_str(delta);
            let now = now_ms();
            let grew = d.text.len().saturating_sub(d.pushed_len) >= DRAFT_MIN_GROWTH;
            let due = now - d.pushed_ms >= DRAFT_MIN_INTERVAL_MS;
            if !(grew && due) || self.draft_in_flight.swap(true, Ordering::AcqRel) {
                return;
            }
            d.pushed_len = d.text.len();
            d.pushed_ms = now;
            // Drafts are plain text (a half-written reply has unbalanced
            // Markdown); Telegram caps a message at 4096 characters.
            let text: String = d.text.chars().take(MAX_MESSAGE_CHARS - 1).collect();
            (d.id, text)
        };
        let inner = Arc::clone(self);
        tokio::spawn(async move {
            let (id, text) = snapshot;
            match inner.client.send_message_draft(chat, id, &text).await {
                Ok(()) => {}
                Err(ChannelError::Api { description }) => {
                    if inner.drafts.swap(false, Ordering::AcqRel) {
                        eprintln!(
                            "telegram: streaming off ({description}); replies arrive when finished"
                        );
                    }
                }
                Err(e) => eprintln!("telegram: draft: {e}"),
            }
            inner.draft_in_flight.store(false, Ordering::Release);
        });
    }

    fn stop_typing(&self) {
        if let Some(handle) = self.typing.lock().unwrap_or_else(|e| e.into_inner()).take() {
            handle.abort();
        }
    }

    pub(super) fn settle_card(&self, short: &str, status: ApprovalStatus) {
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

    /// `telegram.stream`: stream replies as drafts (default on).
    pub fn set_streaming(&self, on: bool) {
        self.inner.drafts.store(on, Ordering::Release);
    }
}

impl Surface for TelegramSurface {
    /// Typing indicator only; a progress message appears with the first note.
    fn on_turn_started(&self) {
        self.inner.turn_active.store(true, Ordering::Release);
        // A fresh draft per turn: a non-zero id the API keys the bubble by.
        *self.inner.draft.lock().unwrap_or_else(|e| e.into_inner()) = Draft {
            id: (now_ms() & 0x7fff_ffff).max(1),
            ..Draft::default()
        };
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

    /// Streamed as a Bot API draft (throttled, plain text); the final
    /// chunks still carry the rendered answer.
    fn on_delta_text(&self, text: &str) {
        self.inner.push_draft(text);
    }

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
        *self.inner.draft.lock().unwrap_or_else(|e| e.into_inner()) = Draft::default();
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

    fn on_memory_proposal(&self, id: &str, text: &str) {
        let Some(chat) = self.inner.chat() else {
            return;
        };
        let short = ka_agent_core::ids::short_id(id).to_string();
        let markup = json!({
            "inline_keyboard": [[
                { "text": "✅ Remember", "callback_data": format!("m:{short}:y") },
                { "text": "✖ Discard", "callback_data": format!("m:{short}:n") }
            ]]
        });
        let body = format!("💡 Remember this?\n\n{text}");
        let client = self.inner.client.clone();
        tokio::spawn(async move {
            let _ = client.send_message(chat, &body, Some(markup)).await;
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
