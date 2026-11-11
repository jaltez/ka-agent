// TelegramSurface: the outbound Surface impl (drafts, progress, cards). Split from telegram.rs (moves only).

use super::*;

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use ka_agent_core::ApprovalStatus;
use ka_agent_runtime::Envelope;
use ka_agent_runtime::event::AgentEvent;
use ka_agent_runtime::surface::Surface;
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
    /// The chat a linked sender writes from, keyed by source
    /// (`telegram:<uid>`): with several identities on one bot, replies go
    /// back to whoever asked, never to whoever wrote last.
    chats: Mutex<HashMap<String, i64>>,
    /// The chat of the turn being streamed (0 outside a turn).
    turn_chat: AtomicI64,
    /// Set between the turn's start and finish envelopes; notes outside
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
    /// Set after a 429: no draft request before this instant (`retry_after`).
    draft_blocked_until_ms: AtomicI64,
    /// One draft request at a time: a slow one is never overtaken by an
    /// older snapshot.
    draft_in_flight: AtomicBool,
    draft: Mutex<Draft>,
    /// The messages this surface may clean up later (`/cleanup`, or any
    /// successful turn): bot-sent error cards and notices, newest last.
    pub(super) tracked: Mutex<VecDeque<TrackedMsg>>,
    /// Wall clock of the current turn's start — the auto-clean boundary
    /// (tracked messages older than it go when the turn succeeds).
    turn_started_ms: AtomicI64,
    /// The daemon's store for outbox requeue of undeliverable final
    /// chunks; `None` in tests.
    pub(super) store: Option<Arc<ka_agent_store::Store>>,
}

/// Why a sent message is tracked for later cleanup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TrackedKind {
    Error,
    Notice,
}

/// One bot-sent message worth deleting later. In-memory only: a restart
/// loses the ids (acceptable — Telegram's delete window is 48 h, and
/// `/cleanup` simply finds nothing).
pub(super) struct TrackedMsg {
    pub message_id: i64,
    pub at_ms: i64,
    pub kind: TrackedKind,
}

/// The tracked-message ring cap: old entries drop off the front.
const TRACKED_MAX: usize = 200;

/// The reply being streamed in the current turn.
#[derive(Default)]
struct Draft {
    /// Non-zero while the turn has a draft; scheduled turns (cron, the
    /// briefing) keep it at zero and never open one.
    id: i64,
    /// Streamed answer text; once any arrived it is what the draft shows.
    text: String,
    /// Newest tool activity (`⋯ label`) while no answer text has streamed.
    activity: Option<String>,
    pushed_len: usize,
    pushed_activity: Option<String>,
    pushed_ms: i64,
}

/// Throttle for `sendMessageDraft`: at most one per second, and only when
/// the text grew enough to be worth a request.
const DRAFT_MIN_INTERVAL_MS: i64 = 1000;
const DRAFT_MIN_GROWTH: usize = 40;

/// The text one `sendMessageDraft` carries, plus the length the throttle
/// compares: plain text, at most `MAX_MESSAGE_CHARS - 1` characters (the
/// Bot API cap is 4096, drafts stay one under it).
fn draft_text(full: &str) -> (String, usize) {
    let text: String = full.chars().take(MAX_MESSAGE_CHARS - 1).collect();
    let len = text.chars().count();
    (text, len)
}

/// Telegram caps an uploaded photo at 10 MB; bigger images go as documents.
const PHOTO_MAX_BYTES: usize = 10 * 1024 * 1024;

/// Telegram caps an uploaded video/animation at 50 MB; bigger ones go as
/// documents (the tool cap is 50 MB too).
const VIDEO_MAX_BYTES: usize = 50 * 1024 * 1024;

impl SurfaceInner {
    fn new(client: TelegramClient) -> Self {
        Self {
            client,
            chat_id: AtomicI64::new(0),
            chats: Mutex::new(HashMap::new()),
            turn_chat: AtomicI64::new(0),
            turn_active: AtomicBool::new(false),
            progress: Mutex::new(Progress::None),
            last_progress_edit_ms: AtomicI64::new(0),
            pending_notes: Mutex::new(Vec::new()),
            cards: Mutex::new(HashMap::new()),
            last_send_ms: AsyncMutex::new(0),
            typing: Mutex::new(None),
            drafts: AtomicBool::new(true),
            draft_blocked_until_ms: AtomicI64::new(0),
            draft_in_flight: AtomicBool::new(false),
            draft: Mutex::new(Draft::default()),
            tracked: Mutex::new(VecDeque::new()),
            turn_started_ms: AtomicI64::new(0),
            store: None,
        }
    }

    /// The chat of the live turn, else the last adopted chat (today's
    /// behavior for local sources).
    pub(super) fn chat(&self) -> Option<i64> {
        match self.turn_chat.load(Ordering::Acquire) {
            0 => self.last_chat(),
            id => Some(id),
        }
    }

    fn last_chat(&self) -> Option<i64> {
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

    /// Adopt one sender's chat: remembered per source, and as the last
    /// chat for sources that carry no identity (local turns, reminders).
    pub(super) fn set_chat_for(&self, source: &str, chat_id: i64) -> bool {
        if chat_id == 0 {
            return false;
        }
        self.chats
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(source.to_string(), chat_id);
        self.set_chat(chat_id)
    }

    /// The admin's chat: every linked identity with role `admin` whose
    /// chat is known, else the last adopted chat. Used for admin-only
    /// events (approval cards, memory proposals) even during a subject's
    /// turn.
    fn admin_chat(&self) -> Option<i64> {
        let chats = self.chats.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(store) = &self.store {
            for identity in store.list_identities().unwrap_or_default() {
                if identity.role == ka_agent_core::ADMIN_ROLE && identity.channel == "telegram" {
                    let source = format!("telegram:{}", identity.address);
                    if let Some(chat) = chats.get(&source) {
                        return Some(*chat);
                    }
                }
            }
        }
        // No admin chat is known. Fail closed: the last chat may belong to
        // a subject, and an admin-only card must never land there.
        let last = self.last_chat()?;
        if let Some(store) = &self.store
            && let Some((source, _)) = chats.iter().find(|(_, chat)| **chat == last)
            && let Some((channel, address)) = source.split_once(':')
            && store
                .identity_role(channel, address)
                .ok()
                .flatten()
                .is_some_and(|role| role != ka_agent_core::ADMIN_ROLE)
        {
            return None;
        }
        Some(last)
    }

    /// Where one event goes: its source's chat when known; `None`/local
    /// sources (and admin-only events) to the admin chat, else the last.
    fn dest(&self, env: &Envelope) -> Option<i64> {
        match env.source.as_deref() {
            Some(source) => {
                // Drop the map guard before `admin_chat` re-locks it: the
                // mutex is not reentrant.
                let known = self
                    .chats
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(source)
                    .copied();
                known.or_else(|| self.admin_chat())
            }
            None => self.admin_chat(),
        }
    }

    /// A notice the surface tracks for later cleanup: sent like
    /// [`Self::spawn_send`] renders, and the returned message id is
    /// recorded. The ring entry is stamped at EVENT time, not send
    /// completion — a card whose send is delayed (flood-control sleep)
    /// must still count as older than a turn that started after the
    /// event, or the auto-clean boundary would keep it forever.
    fn spawn_send_tracked(self: &Arc<Self>, kind: TrackedKind, text: String) {
        let inner = Arc::clone(self);
        let at_ms = now_ms();
        tokio::spawn(async move {
            if let Some(message_id) = inner.send_rich(&text).await {
                inner.track_at(kind, message_id, at_ms);
            }
        });
    }

    /// Record one bot-sent message for later cleanup (bounded ring),
    /// stamped at event time (see [`Self::spawn_send_tracked`]).
    pub(super) fn track_at(&self, kind: TrackedKind, message_id: i64, at_ms: i64) {
        let mut tracked = self.tracked.lock().unwrap_or_else(|e| e.into_inner());
        tracked.push_back(TrackedMsg {
            message_id,
            at_ms,
            kind,
        });
        while tracked.len() > TRACKED_MAX {
            tracked.pop_front();
        }
    }

    /// Delete every tracked message recorded before `boundary_ms`
    /// (the start of a turn that then succeeded), sequentially and
    /// best-effort: a refusal past Telegram's 48 h window is skipped.
    pub(super) fn delete_tracked_before(&self, boundary_ms: i64) {
        let Some(chat) = self.chat() else { return };
        let due: Vec<i64> = {
            let mut tracked = self.tracked.lock().unwrap_or_else(|e| e.into_inner());
            let split = tracked
                .iter()
                .position(|m| m.at_ms >= boundary_ms)
                .unwrap_or(tracked.len());
            tracked.drain(..split).map(|m| m.message_id).collect()
        };
        if due.is_empty() {
            return;
        }
        let client = self.client.clone();
        tokio::spawn(async move {
            for message_id in due {
                let _ = client.delete_message(chat, message_id).await;
            }
        });
    }

    /// Send one message as rendered HTML, keeping ≥1 s between sends,
    /// and return its message id (for later cleanup tracking).
    async fn send_rich(&self, text: &str) -> Option<i64> {
        let plain = balance_fences(text);
        let html =
            ka_agent_core::markdown::render(&plain, ka_agent_core::markdown::Target::Telegram);
        self.send_card(&html, &plain, None).await
    }

    /// One HTML-or-plain send with ≥1 s spacing: HTML first, plain-text
    /// fallback when the HTML is over-length or Telegram refuses it
    /// (`can't parse entities`). The card form of [`Self::send_rich`] —
    /// error cards pass their own markup.
    async fn send_card(&self, html: &str, plain: &str, markup: Option<Value>) -> Option<i64> {
        let mut last = self.last_send_ms.lock().await;
        let wait = SEND_SPACING_MS - (now_ms() - *last);
        if wait > 0 {
            tokio::time::sleep(Duration::from_millis(wait as u64)).await;
        }
        let chat = self.chat()?;
        let mut sent = None;
        let mut fallback = true;
        if html.chars().count() <= MAX_MESSAGE_CHARS {
            match self
                .client
                .send_message_html(chat, html, markup.clone())
                .await
            {
                Ok(id) => {
                    fallback = false;
                    sent = Some(id);
                }
                Err(ChannelError::Api { description, .. }) => {
                    eprintln!("telegram: HTML refused ({description}); sending plain text");
                }
                Err(e) => {
                    eprintln!("telegram: send: {e}");
                    fallback = false;
                }
            }
        }
        if fallback {
            // Over-length plain text is split at paragraph boundaries
            // and sent piece by piece with the usual spacing; the card's
            // keyboard rides on the first piece.
            for (i, piece) in split_plain(plain).into_iter().enumerate() {
                if i > 0 {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
                match self
                    .client
                    .send_message(chat, &piece, if i == 0 { markup.clone() } else { None })
                    .await
                {
                    Ok(id) => {
                        if sent.is_none() {
                            sent = Some(id);
                        }
                    }
                    Err(e) => eprintln!("telegram: send: {e}"),
                }
            }
        }
        *last = now_ms();
        sent
    }

    /// Add streamed text to the draft and push it when the throttle allows.
    fn push_draft(self: &Arc<Self>, delta: &str) {
        if !self.drafts.load(Ordering::Acquire) {
            return;
        }
        let Some(chat) = self.chat() else { return };
        {
            let mut d = self.draft.lock().unwrap_or_else(|e| e.into_inner());
            if d.id == 0 {
                return; // no draft for this turn (none started, or scheduled)
            }
            d.text.push_str(delta);
        }
        self.flush_draft(chat);
    }

    /// Show `⋯ label` for the newest tool call while no answer text has
    /// streamed; the answer text replaces it once it arrives.
    fn show_activity(self: &Arc<Self>, label: &str) {
        if !self.drafts.load(Ordering::Acquire) {
            return;
        }
        let Some(chat) = self.chat() else { return };
        {
            let mut d = self.draft.lock().unwrap_or_else(|e| e.into_inner());
            if d.id == 0 || !d.text.is_empty() {
                return;
            }
            d.activity = Some(format!("⋯ {label}"));
        }
        self.flush_draft(chat);
    }

    /// A tool finished: the next tool or the answer text replaces the label.
    fn clear_activity(&self) {
        let mut d = self.draft.lock().unwrap_or_else(|e| e.into_inner());
        d.activity = None;
    }

    /// Send the draft when the throttle allows: at most one request per
    /// second, and only when it changed enough to be worth it.
    fn flush_draft(self: &Arc<Self>, chat: i64) {
        let snapshot = {
            let mut d = self.draft.lock().unwrap_or_else(|e| e.into_inner());
            if d.id == 0 {
                return;
            }
            let now = now_ms();
            // Flood control: no request before the wait Telegram asked for
            // has elapsed; streaming itself stays on.
            if now < self.draft_blocked_until_ms.load(Ordering::Acquire) {
                return;
            }
            // Drafts are plain text (a half-written reply has unbalanced
            // Markdown); Telegram caps a message at 4096 characters.
            let (text, len, activity) = if d.text.is_empty() {
                (None, 0, d.activity.clone())
            } else {
                let (text, len) = draft_text(&d.text);
                (Some(text), len, None)
            };
            let content = match (&text, &activity) {
                (Some(text), _) => text.clone(),
                (None, Some(activity)) => activity.clone(),
                (None, None) => return,
            };
            let due = now - d.pushed_ms >= DRAFT_MIN_INTERVAL_MS;
            // Streamed text has to grow; the comparison uses the truncated
            // length, so text past the cap never re-sends an identical
            // draft. An activity label replaces the previous one wholesale,
            // so any change is worth a request.
            let worth = match &text {
                Some(_) => len.saturating_sub(d.pushed_len) >= DRAFT_MIN_GROWTH,
                None => d.pushed_activity.as_deref() != Some(content.as_str()),
            };
            if !(due && worth) || self.draft_in_flight.swap(true, Ordering::AcqRel) {
                return;
            }
            d.pushed_len = len;
            d.pushed_activity = activity;
            d.pushed_ms = now;
            (d.id, content)
        };
        let inner = Arc::clone(self);
        tokio::spawn(async move {
            let (id, text) = snapshot;
            match inner.client.send_message_draft(chat, id, &text).await {
                Ok(()) => {}
                // 429 = flood control: transient. Wait what Telegram asks
                // for and keep streaming; the refused text was not shown, so
                // the next flush may resend it.
                Err(ChannelError::Api {
                    description,
                    error_code: Some(429),
                    retry_after,
                }) => {
                    let wait = retry_after.unwrap_or(1).clamp(1, 86_400);
                    inner
                        .draft_blocked_until_ms
                        .store(now_ms() + wait * 1000, Ordering::Release);
                    {
                        let mut d = inner.draft.lock().unwrap_or_else(|e| e.into_inner());
                        // Only rewind the draft this request belonged to: a
                        // newer turn may already have pushed its own text.
                        if d.id == id {
                            d.pushed_len = 0;
                            d.pushed_activity = None;
                        }
                    }
                    eprintln!("telegram: draft flood control ({description}); retrying in {wait}s");
                }
                // 400/404: the method itself is not there (a Bot API server
                // that predates drafts). Streaming stays off for the process.
                Err(ChannelError::Api {
                    description,
                    error_code: Some(400 | 404),
                    ..
                }) => {
                    if inner.drafts.swap(false, Ordering::AcqRel) {
                        eprintln!(
                            "telegram: streaming off ({description}); replies arrive when finished"
                        );
                    }
                }
                // Any other code: log it and leave the flag unchanged.
                Err(ChannelError::Api {
                    description,
                    error_code,
                    ..
                }) => {
                    eprintln!("telegram: draft refused ({error_code:?}): {description}");
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
    /// allowed inbound message (`set_chat`). `store` enables the outbox
    /// requeue of final chunks a send could not deliver (tests pass `None`).
    pub fn new(
        api_base: String,
        token: String,
        initial_chat: Option<i64>,
        store: Option<Arc<ka_agent_store::Store>>,
    ) -> Self {
        let mut inner = SurfaceInner::new(TelegramClient::new(&api_base, &token));
        inner.store = store;
        let inner = Arc::new(inner);
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

    /// Adopt one sender's chat (per-identity delivery).
    pub fn set_chat_for(&self, source: &str, chat_id: i64) {
        self.inner.set_chat_for(source, chat_id);
    }

    /// `telegram.stream`: stream replies as drafts (default on).
    pub fn set_streaming(&self, on: bool) {
        self.inner.drafts.store(on, Ordering::Release);
    }
}

impl Surface for TelegramSurface {
    fn emit(&self, env: &Envelope) {
        match &env.ev {
            // Typing indicator only; a progress message appears with the
            // first note.
            AgentEvent::TurnStarted { source, .. } => {
                self.inner
                    .turn_started_ms
                    .store(env.at_ms, Ordering::Release);
                self.inner.turn_active.store(true, Ordering::Release);
                // Pin the turn's chat: every chunk, note and file of this
                // turn goes back to the chat that asked.
                let dest = self.inner.dest(env).unwrap_or(0);
                self.inner.turn_chat.store(dest, Ordering::Release);
                // A fresh draft per turn: a non-zero id the API keys the
                // bubble by. Scheduled turns (cron, the briefing) get no
                // draft at all — their output arrives as notices, and the
                // runtime already suppresses their deltas.
                let scheduled = source == "briefing" || source.starts_with("cron");
                *self.inner.draft.lock().unwrap_or_else(|e| e.into_inner()) = if scheduled {
                    Draft::default()
                } else {
                    Draft {
                        id: (now_ms() & 0x7fff_ffff).max(1),
                        ..Draft::default()
                    }
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
                        let _ = client.send_chat_action(chat, "typing").await;
                        tokio::time::sleep(TYPING_INTERVAL).await;
                    }
                }));
            }
            // In-turn progress: the first note sends `⏳ {note}`; later
            // notes edit that message (last 3 shown) at most every 3 s.
            // Outside a turn notes are dropped here.
            AgentEvent::Note { text } => {
                if !self.inner.turn_active.load(Ordering::Acquire) {
                    return;
                }
                let Some(chat) = self.inner.chat() else {
                    return;
                };
                // The engine's retry note ends "(esc cancels)" — a local
                // keyboard hint with no meaning on the phone (there Esc
                // does nothing; /stop is the remote interrupt).
                let text = &text.replace(" (esc cancels)", "");
                let shown = {
                    let mut notes = self
                        .inner
                        .pending_notes
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    notes.push(text.clone());
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
                                let mut p =
                                    inner.progress.lock().unwrap_or_else(|e| e.into_inner());
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
            // Operator-must-receive messages (reminders, cron output,
            // pauses): always their own message.
            AgentEvent::Notice { text } => {
                // Tracked: `/cleanup` or the next successful turn may
                // delete it (stale status noise).
                self.inner
                    .spawn_send_tracked(TrackedKind::Notice, text.clone());
            }
            // Streamed as a Bot API draft (throttled, plain text); the
            // final chunks still carry the rendered answer.
            AgentEvent::Delta { text } => self.inner.push_draft(text),
            AgentEvent::FinalChunk { index, turn, text } => {
                if self.inner.chat().is_none() {
                    return;
                }
                let inner = Arc::clone(&self.inner);
                let text = text.clone();
                let turn = turn.clone();
                let chunk_index = index.saturating_sub(1);
                tokio::spawn(async move {
                    if inner.send_rich(&text).await.is_none()
                        && let Some(store) = inner.store.as_ref()
                    {
                        // The send still failed after the flood-control
                        // retry: requeue the outbox row so the startup
                        // drain redelivers it (private sessions write no
                        // rows — the requeue is a no-op then).
                        if let Err(e) =
                            store.requeue_chunk(&ka_agent_core::TurnId(turn.clone()), chunk_index)
                        {
                            eprintln!("telegram: requeue chunk {turn}/{}: {e}", chunk_index + 1);
                        }
                    }
                });
            }
            // Stops typing and deletes the progress message; cost lives
            // in /status.
            AgentEvent::TurnFinished { stop, .. } => {
                *self.inner.draft.lock().unwrap_or_else(|e| e.into_inner()) = Draft::default();
                self.inner.stop_typing();
                self.inner.turn_active.store(false, Ordering::Release);
                self.inner.turn_chat.store(0, Ordering::Release);
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
                // Any successful turn clears stale error/notice cards
                // recorded before it started (a retried turn succeeding
                // is the common case).
                if matches!(stop.as_str(), "done" | "length") {
                    let boundary = self.inner.turn_started_ms.load(Ordering::Acquire);
                    if boundary > 0 {
                        self.inner.delete_tracked_before(boundary);
                    }
                }
            }
            AgentEvent::MemoryProposal { id, text } => {
                let Some(chat) = self.inner.dest(env) else {
                    return;
                };
                let short = ka_agent_core::ids::short_id(id.as_str()).to_string();
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
            // A model-proposed skill: one tap installs (same path as
            // `/skills accept <id>`) or rejects it.
            AgentEvent::SkillProposal {
                id,
                name,
                description,
            } => {
                let Some(chat) = self.inner.dest(env) else {
                    return;
                };
                // `sk:<verdict>:<id>` must fit the 64-byte callback cap;
                // ids are short (`skill_xxxx`) but a long one falls back to
                // its suffix, which `id_matches` accepts.
                let target = if id.len() <= 59 {
                    id.clone()
                } else {
                    ka_agent_core::ids::short_id(id).to_string()
                };
                let markup = json!({
                    "inline_keyboard": [[
                        { "text": "✅ Accept", "callback_data": format!("sk:a:{target}") },
                        { "text": "✖ Reject", "callback_data": format!("sk:r:{target}") }
                    ]]
                });
                let body = format!("🧩 Proposed skill: {name}\n\n{description}");
                let client = self.inner.client.clone();
                tokio::spawn(async move {
                    let _ = client.send_message(chat, &body, Some(markup)).await;
                });
            }
            // A file for the operator: a photo when Telegram can take it as
            // one, else a document. A refusal falls back to a notice with
            // the absolute path, so the file is never lost silently.
            AgentEvent::File {
                path,
                name,
                caption,
            } => {
                let Some(chat) = self.inner.chat() else {
                    return;
                };
                let client = self.inner.client.clone();
                let (path, name, caption) = (path.clone(), name.clone(), caption.clone());
                tokio::spawn(async move {
                    // The phone shows an upload indicator instead of a
                    // minutes-long "typing…" while a big file goes out;
                    // one action covers the whole upload.
                    let action = if photo_mime(&name).is_some() {
                        "upload_photo"
                    } else {
                        "upload_document"
                    };
                    let _ = client.send_chat_action(chat, action).await;
                    if let Err(e) = deliver_file(&client, chat, &path, &name, &caption).await {
                        eprintln!("telegram: file {name}: {e}");
                        let notice = format!("! [file] {name}: {e}\n{path}");
                        let _ = client.send_message(chat, &notice, None).await;
                    }
                });
            }
            AgentEvent::Approval {
                id,
                question,
                detail,
                expires_at_ms,
                ..
            } => {
                let Some(chat) = self.inner.dest(env) else {
                    return;
                };
                let full = id.clone();
                let short = short_id(&full, 12);
                let mut text = question.clone();
                if let Some(detail) = detail {
                    let head: Vec<&str> = detail.lines().take(20).collect();
                    if !head.is_empty() {
                        text.push_str("\n\n");
                        text.push_str(&head.join("\n"));
                    }
                }
                text.push_str(&format!(
                    "\n\nexpires in {}",
                    crate::commands::fmt_interval(expires_at_ms - now_ms())
                ));
                let first_line = question.lines().next().unwrap_or("").to_string();
                let markup = json!({
                    "inline_keyboard": [
                        [{ "text": "✅ Allow", "callback_data": format!("a:{short}:y") }],
                        [{ "text": "⛔ Deny", "callback_data": format!("a:{short}:n") }]
                    ]
                });
                let inner = Arc::clone(&self.inner);
                tokio::spawn(async move {
                    if let Ok(message_id) =
                        inner.client.send_message(chat, &text, Some(markup)).await
                    {
                        inner
                            .cards
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(short, (full, message_id, first_line));
                    }
                });
            }
            AgentEvent::ApprovalSettled { id, status } => {
                let parsed = match status.as_str() {
                    "Approved" => ApprovalStatus::Approved,
                    "Denied" => ApprovalStatus::Denied,
                    "Cancelled" => ApprovalStatus::Cancelled,
                    "Superseded" => ApprovalStatus::Superseded,
                    "Expired" => ApprovalStatus::Expired,
                    _ => ApprovalStatus::Pending,
                };
                self.inner.settle_card(&short_id(id, 12), parsed);
            }
            AgentEvent::Error {
                class,
                message,
                retryable,
            } => {
                // A structured card, not a raw provider dump: headline,
                // what it means, the remedy, and ≤300 chars of detail.
                let card = crate::errfmt::friendly_error(class, message);
                let html = card.html();
                let plain = card.plain();
                // ka-port.md §7: a retryable failure offers ↻ (never
                // automatic). One tap re-sends the interrupted question
                // via the shared /retry path.
                let markup = if *retryable {
                    Some(json!({
                        "inline_keyboard": [[
                            { "text": "↻ Retry", "callback_data": "retry:last" }
                        ]]
                    }))
                } else {
                    None
                };
                let inner = Arc::clone(&self.inner);
                let at_ms = now_ms();
                tokio::spawn(async move {
                    if let Some(message_id) = inner.send_card(&html, &plain, markup).await {
                        inner.track_at(TrackedKind::Error, message_id, at_ms);
                    }
                });
            }
            // While tools run and no answer text has streamed, the draft
            // shows the newest tool call; the text replaces it later.
            AgentEvent::ToolStarted { tool, detail, .. } => {
                if !self.inner.turn_active.load(Ordering::Acquire) {
                    return;
                }
                let label = ka_agent_core::labels::tool_label(tool, detail);
                self.inner.show_activity(&label);
            }
            // The activity is cleared: the next tool or the answer text
            // replaces it (no extra request).
            AgentEvent::ToolFinished { .. } => self.inner.clear_activity(),
            // Reasoning, plans and status are not drawn on the phone
            // (today's behavior).
            AgentEvent::Thought { .. }
            | AgentEvent::Plan { .. }
            | AgentEvent::Context { .. }
            | AgentEvent::Compacting { .. }
            | AgentEvent::Inbound { .. }
            | AgentEvent::Session { .. } => {}
            // A fired reminder is its own notification, never a model turn.
            AgentEvent::Reminder { text, .. } => {
                if let Some(chat) = self.inner.dest(env) {
                    let client = self.inner.client.clone();
                    let text = format!("⏰ {text}");
                    tokio::spawn(async move {
                        let _ = client.send_message(chat, &text, None).await;
                    });
                }
            }
            // Later steps wire these to real deliveries.
            AgentEvent::CronFinished { .. }
            | AgentEvent::Queue { .. }
            | AgentEvent::Settings { .. } => {}
            // Per-connection events never reach a broadcast surface.
            AgentEvent::Hello { .. } | AgentEvent::Reply { .. } => {}
        }
    }
}

/// Send one event file as the inline method Telegram can play/show:
/// `sendPhoto` for an image Telegram accepts (jpg, jpeg, png, webp
/// ≤ 10 MB), `sendVideo` for a video (mp4, m4v, webm, mkv ≤ 50 MB),
/// `sendAnimation` for a GIF, `sendDocument` otherwise. A refused inline
/// send (a server that predates the method, a codec Telegram rejects,
/// an over-limit file) falls back to `sendDocument`, so the file is
/// never lost. The caption rides along when non-empty.
async fn deliver_file(
    client: &TelegramClient,
    chat: i64,
    path: &str,
    name: &str,
    caption: &str,
) -> Result<(), ChannelError> {
    let bytes = read_file(path).await?;
    if let Some((method, field, mime)) = inline_send(name, bytes.len()) {
        if let Err(e) = client
            .send_media(chat, method, field, name, mime, bytes, caption)
            .await
        {
            eprintln!("telegram: {method} refused ({e}); sending as a document");
            let bytes = read_file(path).await?;
            return client
                .send_media(chat, "sendDocument", "document", name, None, bytes, caption)
                .await
                .map(|_| ());
        }
        return Ok(());
    }
    client
        .send_media(chat, "sendDocument", "document", name, None, bytes, caption)
        .await
        .map(|_| ())
}

/// Read one outbound file off the runtime thread.
async fn read_file(path: &str) -> Result<Vec<u8>, ChannelError> {
    let read = path.to_string();
    tokio::task::spawn_blocking(move || std::fs::read(&read))
        .await
        .map_err(|e| ChannelError::api(format!("read task: {e}")))?
        .map_err(|e| ChannelError::api(format!("{path}: {e}")))
}

/// The inline send (`method`, multipart `field`, MIME) Telegram accepts
/// for `name` at `len` bytes, if any — else the caller sends a document.
fn inline_send(
    name: &str,
    len: usize,
) -> Option<(&'static str, &'static str, Option<&'static str>)> {
    if let Some(mime) = photo_mime(name)
        && len <= PHOTO_MAX_BYTES
    {
        return Some(("sendPhoto", "photo", Some(mime)));
    }
    if let Some(mime) = video_mime(name)
        && len <= VIDEO_MAX_BYTES
    {
        let (method, field) = if mime == "image/gif" {
            ("sendAnimation", "animation")
        } else {
            ("sendVideo", "video")
        };
        return Some((method, field, Some(mime)));
    }
    None
}

/// The image MIME type Telegram accepts for `sendPhoto`, by extension.
fn photo_mime(name: &str) -> Option<&'static str> {
    let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" => Some("image/jpeg"),
        "png" => Some("image/png"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

/// The video/animation MIME type, by extension: `sendVideo` plays mp4/
/// m4v/webm/mkv inline, `sendAnimation` plays a GIF — both up to 50 MB
/// (bigger files go out as documents, the tool cap is 50 MB).
fn video_mime(name: &str) -> Option<&'static str> {
    let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
    match ext.as_str() {
        "mp4" | "m4v" => Some("video/mp4"),
        "webm" => Some("video/webm"),
        "mkv" => Some("video/x-matroska"),
        "gif" => Some("image/gif"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::{MAX_MESSAGE_CHARS, draft_text};

    #[test]
    fn draft_length_stops_growing_at_the_cap() {
        assert_eq!(draft_text("short"), ("short".to_string(), 5));

        let full = "a".repeat(MAX_MESSAGE_CHARS + 400);
        let (text, len) = draft_text(&full);
        assert_eq!(len, MAX_MESSAGE_CHARS - 1);
        assert_eq!(text.chars().count(), len);

        // Growing only past the cap leaves the compared length alone, so
        // the throttle sees no growth and sends nothing.
        let (_, longer) = draft_text(&format!("{full}bbbb"));
        assert_eq!(longer, len);

        // Characters, not bytes: multi-byte text never splits a glyph.
        let wide = "🙂".repeat(MAX_MESSAGE_CHARS + 5);
        let (text, len) = draft_text(&wide);
        assert_eq!(len, MAX_MESSAGE_CHARS - 1);
        assert_eq!(text.chars().count(), len);
    }
}
