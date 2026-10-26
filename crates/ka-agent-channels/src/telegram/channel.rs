// TelegramChannel: the long-poll loop, gate, dispatch and callbacks. Split from telegram.rs (moves only).

use super::client::TelegramClient;
use super::surface::{SurfaceInner, take_surface_state};
use super::*;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ka_agent_core::audit::AuditRecord;
use ka_agent_core::{ApprovalDecision, ApprovalStatus, DaemonConfig, OutboxChunk};
use ka_agent_runtime::{Error as RtError, Runtime};
use ka_agent_store::Store;
use serde_json::{Value, json};
use tokio::sync::watch;

use crate::pairing::{self, Redeem};
// ---------------------------------------------------------------------------
// Channel (inbound poller)
// ---------------------------------------------------------------------------

pub struct TelegramConfig {
    pub api_base: String,
    pub token: String,
    pub allow_user_ids: Vec<i64>,
    /// Voice-note transcription; `None` answers voice notes with the
    /// setup hint.
    pub transcriber: Option<crate::voice::Transcriber>,
}

impl TelegramConfig {
    pub fn from_config(cfg: &DaemonConfig, token: String) -> Self {
        Self {
            api_base: cfg.telegram.api_base().to_string(),
            token,
            allow_user_ids: cfg.telegram.allow_user_ids.clone(),
            transcriber: None,
        }
    }
}

pub struct TelegramChannel {
    pub(super) cfg: TelegramConfig,
    runtime: Runtime,
    store: Arc<Store>,
    /// uid → last `/start` reply (ms); strangers get at most one an hour.
    start_replied: Mutex<HashMap<i64, i64>>,
    /// Chunks recovered at startup while no chat was known; delivered on
    /// the first `set_chat`.
    undelivered: Mutex<Vec<OutboxChunk>>,
}

/// What one callback press produced: the toast for
/// `answerCallbackQuery` (empty = just close the spinner) and, when the
/// card must change, its new text.
struct CallbackReply {
    toast: String,
    edit: Option<String>,
}

impl CallbackReply {
    /// Answer with no toast and leave the card alone.
    fn none() -> Self {
        Self {
            toast: String::new(),
            edit: None,
        }
    }

    /// Answer with `text` and leave the card alone.
    fn toast(text: &str) -> Self {
        Self {
            toast: text.to_string(),
            edit: None,
        }
    }
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

        let voice = message.get("voice").or_else(|| message.get("audio"));
        let transcript = match voice {
            Some(v) => match self.transcribe_voice(&client, chat_id, v).await {
                Some(t) => Some(t),
                None => return,
            },
            None => None,
        };

        if transcript.is_none()
            && (message.get("photo").is_some() || message.get("document").is_some())
        {
            let reply = match crate::attachments::save_attachment(
                &client,
                &self.runtime.workspace(),
                message,
            )
            .await
            {
                Ok(rel) => format!("saved {rel} — reference it with @{rel} in your next message"),
                Err(e) => format!("! [attachment] {e}"),
            };
            let _ = client.send_message(chat_id, &reply, None).await;
            return;
        }

        let Some(text) = transcript.as_deref().or_else(|| message["text"].as_str()) else {
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
                // 👀 on the message: received and accepted, before any
                // engine start-up or queueing makes the chat look dead.
                if let Some(message_id) = message["message_id"].as_i64() {
                    let client = client.clone();
                    tokio::spawn(async move {
                        let _ = client.set_message_reaction(chat_id, message_id, "👀").await;
                    });
                }
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

    /// ✅ / ✖ on a memory proposal: accept or reject, then turn the card
    /// into its outcome (buttons removed) so it cannot be pressed twice.
    async fn memory_callback(&self, cb: &Value, rest: &str) -> CallbackReply {
        let Some((short, verdict)) = rest.rsplit_once(':') else {
            return CallbackReply::none();
        };
        let text = cb["message"]["text"]
            .as_str()
            .unwrap_or("")
            .trim_start_matches("💡 Remember this?")
            .trim()
            .to_string();
        let (outcome, toast) = match verdict {
            "y" => match self.runtime.accept_candidate(short) {
                Ok(true) => (format!("✅ Remembered: {text}"), "remembered"),
                Ok(false) => (format!("(already decided) {text}"), "already decided"),
                Err(e) => (format!("! [memory] {e}"), "error"),
            },
            "n" => match self.runtime.reject_candidate(short) {
                Ok(true) => (format!("✖ Discarded: {text}"), "discarded"),
                Ok(false) => (format!("(already decided) {text}"), "already decided"),
                Err(e) => (format!("! [memory] {e}"), "error"),
            },
            _ => return CallbackReply::none(),
        };
        CallbackReply {
            toast: toast.to_string(),
            edit: Some(outcome),
        }
    }

    /// ✅ / ✖ on a skill proposal: accept (install + enable) or reject it
    /// through the very command path `/skills accept|reject <id>` takes,
    /// then turn the card into its outcome. A second press of a double-tap
    /// finds no pending proposal (the first press decided it) and only
    /// answers, leaving the card's outcome untouched.
    async fn skill_callback(&self, cb: &Value, rest: &str) -> CallbackReply {
        use ka_agent_core::ids::id_matches;
        let Some((verdict, id)) = rest.split_once(':') else {
            return CallbackReply::none();
        };
        let verb = match verdict {
            "a" => "accept",
            "r" => "reject",
            _ => return CallbackReply::none(),
        };
        let pending = self.store.pending_skill_candidates().unwrap_or_default();
        let hits: Vec<_> = pending
            .into_iter()
            .filter(|c| id_matches(&c.id, id))
            .collect();
        let id = match hits.as_slice() {
            // Use the full id: the command path then matches exactly one row.
            [one] => one.id.clone(),
            // No pending proposal: the first press already decided it (a
            // double tap), so only answer and leave the card's outcome be.
            [] => {
                return CallbackReply::toast(if verdict == "a" {
                    "already installed"
                } else {
                    "already rejected"
                });
            }
            // Ambiguous query (never produced by a card, which carries the
            // full id): let the command path report it.
            _ => id.to_string(),
        };
        let uid = cb["from"]["id"].as_i64().unwrap_or(0);
        let command = format!("/skills {verb} {id}");
        let (outcome, toast) =
            match crate::commands::route(&self.runtime, &format!("telegram:{uid}"), &command).await
            {
                crate::commands::Outcome::Reply(reply) if !reply.starts_with('!') => {
                    let toast = if verdict == "a" {
                        "accepted"
                    } else {
                        "rejected"
                    };
                    (reply, toast)
                }
                crate::commands::Outcome::Reply(reply) => (reply, "error"),
                crate::commands::Outcome::Silent | crate::commands::Outcome::NotACommand => {
                    (format!("! [skills] no outcome for {id}"), "error")
                }
            };
        CallbackReply {
            toast: toast.to_string(),
            edit: Some(outcome),
        }
    }

    /// Resolve one callback press: the toast for `answerCallbackQuery` and,
    /// when the card changes, its new text. Every path returns a reply (an
    /// empty toast for data this build does not understand), so
    /// [`Self::dispatch_callback`] can answer the query exactly once.
    async fn callback_action(
        &self,
        cb: &Value,
        data: Option<&str>,
        surface: &Arc<SurfaceInner>,
    ) -> CallbackReply {
        let Some(data) = data else {
            return CallbackReply::none();
        };
        // `m:<short>:y|n` — a memory proposal card.
        if let Some(rest) = data.strip_prefix("m:") {
            return self.memory_callback(cb, rest).await;
        }
        // `sk:a|r:<id>` — a skill proposal card.
        if let Some(rest) = data.strip_prefix("sk:") {
            return self.skill_callback(cb, rest).await;
        }
        // `a:<short>:y|n`
        let Some(rest) = data.strip_prefix("a:") else {
            return CallbackReply::none();
        };
        let Some((short, verdict)) = rest.rsplit_once(':') else {
            return CallbackReply::none();
        };
        if !matches!(verdict, "y" | "n") {
            return CallbackReply::none();
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
            return CallbackReply::toast("expired");
        };
        let decision = if verdict == "y" {
            ApprovalDecision::Allow
        } else {
            ApprovalDecision::Deny
        };
        match self.runtime.decide(&full, decision).await {
            Ok(()) => CallbackReply::toast(if verdict == "y" {
                "✔ approved"
            } else {
                "✖ denied"
            }),
            Err(RtError::UnknownApproval(_)) => {
                surface.settle_card(short, ApprovalStatus::Expired);
                CallbackReply::toast("expired")
            }
            Err(e) => {
                eprintln!("telegram: decide {full}: {e}");
                CallbackReply::toast("error")
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
        let client = surface.client.clone();
        let Some(uid) = cb["from"]["id"].as_i64() else {
            // Close the spinner anyway: every callback query is answered.
            if let Some(id) = cb["id"].as_str() {
                let _ = client.answer_callback_query(id, "").await;
            }
            return;
        };
        let chat_type = cb["message"]["chat"]["type"].as_str().unwrap_or("");
        if !self.gate(uid, chat_type, unauthorized_last).await {
            // No text for unauthorized traffic (the audit row is the
            // record), but the spinner still closes: every callback query
            // is answered exactly once.
            if let Some(id) = cb["id"].as_str() {
                let _ = client.answer_callback_query(id, "").await;
            }
            return;
        }
        let chat_id = cb["message"]["chat"]["id"].as_i64().unwrap_or(0);
        self.adopt_chat(surface, chat_id).await;
        let reply = self.callback_action(cb, cb["data"].as_str(), surface).await;
        // Exactly one answer per callback query, on every path.
        if let Some(id) = cb["id"].as_str() {
            let _ = client.answer_callback_query(id, &reply.toast).await;
        }
        if let Some(text) = reply.edit
            && let Some(message_id) = cb["message"]["message_id"].as_i64()
        {
            let _ = client
                .edit_message_text(
                    chat_id,
                    message_id,
                    &text,
                    Some(json!({ "inline_keyboard": [] })),
                )
                .await;
        }
    }
}
