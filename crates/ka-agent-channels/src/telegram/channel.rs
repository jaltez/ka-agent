// TelegramChannel: the long-poll loop, gate, dispatch and callbacks. Split from telegram.rs (moves only).

use super::client::TelegramClient;
use super::surface::{SurfaceInner, TrackedKind, TrackedMsg, take_surface_state};
use super::*;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ka_agent_core::audit::AuditRecord;
use ka_agent_core::ids::id_matches;
use ka_agent_core::{ApprovalDecision, ApprovalStatus, DaemonConfig, OutboxChunk};
use ka_agent_runtime::registry::{ModelsListing, ProviderModels};
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
    /// `media_group_id` → files saved so far (an album in flight). The
    /// first member schedules one flush a debounce later, so an N-file
    /// album gets a single ack naming every file.
    media_groups: Arc<Mutex<HashMap<String, MediaGroup>>>,
}

/// One album being debounced: the saved `@attachments/…` refs, and the
/// caption (on whichever member carried one) that turns the flush into a
/// caption turn instead of a plain ack.
#[derive(Default)]
struct MediaGroup {
    refs: Vec<String>,
    caption: Option<String>,
    caption_message_id: Option<i64>,
}

/// How long the channel waits for further members of a media group
/// before answering once for the whole album. Telegram delivers an
/// album's messages back-to-back; the window only needs to span the
/// batches, not human typing.
const MEDIA_GROUP_DEBOUNCE: Duration = Duration::from_millis(750);

/// The native pickers (bare `/model` `/mode` `/effort`): the provider
/// grid and each vendor page cap how many buttons one card shows.
const PROVIDER_COLS: usize = 3;
const PROVIDER_PAGE: usize = 24;
const MODEL_COLS: usize = 2;
const MODEL_PAGE: usize = 12;
/// Lines of the shared vendor listing a picker card carries (the
/// keyboard pages the models; the text is a window, not the catalogue).
const PICKER_TEXT_LINES: usize = 12;
/// `answerCallbackQuery` toasts cap at 200 characters.
const TOAST_CHARS: usize = 200;

/// What one callback press produced: the toast for
/// `answerCallbackQuery` (empty = just close the spinner) and, when the
/// card must change, its new text plus the keyboard to render under it
/// (`None` drops the keyboard — a settled card).
struct CallbackReply {
    toast: String,
    edit: Option<String>,
    markup: Option<Value>,
}

impl CallbackReply {
    /// Answer with no toast and leave the card alone.
    fn none() -> Self {
        Self {
            toast: String::new(),
            edit: None,
            markup: None,
        }
    }

    /// Answer with `text` and leave the card alone.
    fn toast(text: &str) -> Self {
        Self {
            toast: text.to_string(),
            edit: None,
            markup: None,
        }
    }

    /// Answer with `toast`, replace the card's text and drop its keyboard
    /// (the outcome of a decided card).
    fn settle(toast: &str, text: String) -> Self {
        Self {
            toast: toast.to_string(),
            edit: Some(text),
            markup: None,
        }
    }

    /// Replace the card's text AND keyboard (a picker page): no toast,
    /// the spinner closes and the card becomes the new page.
    fn page(text: String, markup: Value) -> Self {
        Self {
            toast: String::new(),
            edit: Some(text),
            markup: Some(markup),
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
            media_groups: Arc::new(Mutex::new(HashMap::new())),
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
            .latest_session_for_user(ka_agent_core::OPERATOR_USER)
            .ok()
            .flatten()
            .map(|s| {
                self.store
                    .pending_chunks_for_session(&s.id)
                    .unwrap_or_default()
            })
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
                self.runtime.note_channel_err("telegram", &e.to_string());
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
        // Same policy for the profile texts: the empty-chat "What can
        // this bot do?" screen and the chat-list blurb are set best-effort
        // beside the command menu.
        if let Err(e) = client.set_my_description(BOT_DESCRIPTION).await {
            eprintln!("telegram: setMyDescription: {e} (continuing)");
        }
        if let Err(e) = client.set_my_short_description(BOT_SHORT_DESCRIPTION).await {
            eprintln!("telegram: setMyShortDescription: {e} (continuing)");
        }

        let mut backoff_secs = 1u64;
        loop {
            if *shutdown.borrow() {
                break;
            }
            match client.get_updates(offset, POLL_TIMEOUT_SECS).await {
                Ok(updates) => {
                    backoff_secs = 1;
                    // A recovering or healthy poll clears the health line
                    // `status` shows for a dead phone link (F8).
                    self.runtime.note_channel_ok("telegram");
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
                    self.runtime.note_channel_err("telegram", &e.to_string());
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
        // Resolution-based: an enabled linked identity passes; a *disabled*
        // account is refused silently (no re-pair hint — its identity exists,
        // the account is just off), and only a truly unlinked sender is
        // routed to the pairing flow by the caller.
        let linked = chat_type == "private"
            && self
                .runtime
                .principal_for_source(&format!("telegram:{uid}"))
                .is_some();
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

    /// Adopt `chat_id` for `uid`: remembered per sender (per-identity
    /// delivery), and as the last chat. Persists it when it changed and
    /// flushes chunks recovered while no chat was known.
    async fn adopt_chat(&self, surface: &Arc<SurfaceInner>, uid: i64, chat_id: i64) {
        if surface.set_chat_for(&format!("telegram:{uid}"), chat_id) {
            // The global fallback chat is the operator's alone: a member's
            // DM must never become the target of recovered operator chunks.
            if self
                .runtime
                .principal_for_source(&format!("telegram:{uid}"))
                .is_some_and(|p| p.is_admin())
                && let Err(e) = self
                    .store
                    .meta_set("telegram.chat_id", &chat_id.to_string())
            {
                eprintln!("telegram: persist chat id: {e}");
            }
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
    /// `/pair <code>` (or a `/start <code>` deep link, the one-tap form
    /// the CLI prints) links the sender; plain `/start` gets the one-line
    /// owner hint at most once an hour per uid. `true` = handled (linked).
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
                self.adopt_chat(surface, uid, chat_id).await;
                let _ = surface
                    .client
                    .send_message(chat_id, pairing::TELEGRAM_WELCOME, None)
                    .await;
                return true;
            }
            return false;
        }
        let mut words = text.split_whitespace();
        let first = words.next().unwrap_or("");
        if first.split('@').next() == Some("/start") {
            // `/start <payload>` from a `t.me/<bot>?start=<code>` deep
            // link redeems the payload as a pairing code with `/pair`'s
            // exact semantics (10-min TTL, 5 attempts, constant-time
            // compare). A wrong payload burns an attempt and is answered
            // like a wrong `/pair` — never with the hourly owner hint.
            if let Some(payload) = words.next().filter(|p| !p.is_empty()) {
                return if pairing::redeem_code(&self.store, "telegram", &uid.to_string(), payload)
                    == Redeem::Linked
                {
                    self.adopt_chat(surface, uid, chat_id).await;
                    let _ = surface
                        .client
                        .send_message(chat_id, pairing::TELEGRAM_WELCOME, None)
                        .await;
                    true
                } else {
                    false
                };
            }
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
        // Resolution, not a bare link check: only a sender with NO identity
        // row reaches the pairing flow. A disabled account keeps its row
        // (resolution fails) and falls through to `gate`, which refuses it
        // silently with the audit row — no re-pair hint for an off account.
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
        self.adopt_chat(surface, uid, chat_id).await;
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
            // An album member (a `media_group_id` shared with its
            // siblings): save now, answer once for the whole group after
            // a short debounce.
            if message["media_group_id"].as_str().is_some() {
                self.group_media_message(&client, chat_id, uid, message)
                    .await;
                return;
            }
            // A single photo/document: saved and acknowledged. A caption
            // is the operator's message — it becomes the turn, the file
            // riding along as an `@reference` instead of a bare ack.
            let who = self.who(uid);
            match crate::attachments::save_attachment(
                &client,
                &self.runtime.workspace_for(&who),
                message,
                || self.runtime.private_session_token_for(&who),
            )
            .await
            {
                Ok(rel) => {
                    let caption = message["caption"].as_str().map(str::trim).unwrap_or("");
                    if caption.is_empty() {
                        let reply =
                            format!("saved {rel} — reference it with @{rel} in your next message");
                        let _ = client.send_message(chat_id, &reply, None).await;
                    } else {
                        submit_caption_turn(
                            &client,
                            &self.runtime,
                            &self.store,
                            chat_id,
                            uid,
                            message["message_id"].as_i64(),
                            caption,
                            &[rel],
                        )
                        .await;
                    }
                }
                Err(e) => {
                    let _ = client
                        .send_message(chat_id, &format!("! [attachment] {e}"), None)
                        .await;
                }
            }
            return;
        }

        // Media kinds with no handler (a video note, a sticker, …) get an
        // explicit refusal instead of a silent drop: no turn, no 👀.
        if transcript.is_none()
            && message["text"].as_str().is_none()
            && let Some(kind) = unsupported_media_kind(message)
        {
            let _ = client
                .send_message(
                    chat_id,
                    &format!("! [media] {kind} isn't supported yet"),
                    None,
                )
                .await;
            return;
        }

        let Some(text) = transcript.as_deref().or_else(|| message["text"].as_str()) else {
            return;
        };

        // A bare picker command (`/model` `/mode` `/effort` `/approvals`
        // `/queue` `/forget` `/skills` `/history` `/cleanup`) renders as
        // a native Telegram picker (an inline keyboard or, for
        // `/cleanup`, a direct action) instead of the router's text
        // listing; every argument form still routes below.
        if let Some(cmd) = bare_picker_command(text) {
            let who = self.who(uid);
            let role = who.role.clone();
            let name = cmd.trim_start_matches('/');
            // Pickers bypass the router's dispatch: gate them here too.
            if !crate::commands::allowed_for(&self.runtime, &role, name) {
                let _ = client
                    .send_message(
                        chat_id,
                        &format!("command /{name} is not available for role {role}"),
                        None,
                    )
                    .await;
                return;
            }
            if let Err(e) = self.runtime.ensure_user_state(&who) {
                let _ = client
                    .send_message(chat_id, &format!("{e}"), None)
                    .await;
                return;
            }
            self.send_picker(&client, chat_id, cmd, surface, uid).await;
            return;
        }
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
                let who = self.who(uid);
                let source = format!("telegram:{uid}");
                let (text, refs) =
                    crate::refs::expand(&self.store, &self.runtime.workspace_for(&who), &who, text)
                        .await;
                crate::refs::stage_sources(&self.store, &source, &refs);
                if let Err(e) = self.runtime.handle_message(&text, &source).await
                {
                    eprintln!("telegram: handle_message: {e}");
                }
            }
        }
    }

    /// One member of a media group (a Telegram album): the file is saved
    /// at once — bytes never wait in memory — and recorded under the
    /// group's id. The first member schedules a single flush one
    /// [`MEDIA_GROUP_DEBOUNCE`] later, which answers once for the whole
    /// album: one ack naming every `@attachments/…` ref, or, when any
    /// member carried a caption, one caption turn with all the refs.
    async fn group_media_message(
        &self,
        client: &TelegramClient,
        chat_id: i64,
        uid: i64,
        message: &Value,
    ) {
        let Some(group_id) = message["media_group_id"].as_str() else {
            return;
        };
        let who = self.who(uid);
        let rel = match crate::attachments::save_attachment(
            client,
            &self.runtime.workspace_for(&who),
            message,
            || self.runtime.private_session_token_for(&who),
        )
        .await
        {
            Ok(rel) => rel,
            Err(e) => {
                let _ = client
                    .send_message(chat_id, &format!("! [attachment] {e}"), None)
                    .await;
                return;
            }
        };
        let first = {
            let mut groups = self.media_groups.lock().unwrap_or_else(|e| e.into_inner());
            let entry = groups.entry(group_id.to_string()).or_default();
            if entry.caption.is_none()
                && message["caption"]
                    .as_str()
                    .is_some_and(|c| !c.trim().is_empty())
            {
                entry.caption = message["caption"].as_str().map(|c| c.trim().to_string());
                entry.caption_message_id = message["message_id"].as_i64();
            }
            entry.refs.push(rel);
            entry.refs.len() == 1
        };
        if !first {
            return;
        }
        let client = client.clone();
        let runtime = self.runtime.clone();
        let store = Arc::clone(&self.store);
        let groups = Arc::clone(&self.media_groups);
        let group_id = group_id.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(MEDIA_GROUP_DEBOUNCE).await;
            let Some(group) = groups
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&group_id)
            else {
                return;
            };
            match group.caption {
                Some(caption) => {
                    submit_caption_turn(
                        &client,
                        &runtime,
                        &store,
                        chat_id,
                        uid,
                        group.caption_message_id,
                        &caption,
                        &group.refs,
                    )
                    .await;
                }
                None => {
                    let mentions: Vec<String> =
                        group.refs.iter().map(|r| format!("@{r}")).collect();
                    let reply = format!(
                        "saved {} files — reference them with {} in your next message",
                        group.refs.len(),
                        mentions.join(" ")
                    );
                    let _ = client.send_message(chat_id, &reply, None).await;
                }
            }
        });
    }

    /// ✅ / ✖ on a memory proposal: accept or reject, then turn the card
    /// into its outcome (buttons removed) so it cannot be pressed twice.
    async fn memory_callback(&self, cb: &Value, rest: &str) -> CallbackReply {
        let uid = cb["from"]["id"].as_i64().unwrap_or(0);
        let who = self.who(uid);
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
            "y" => match self.runtime.accept_candidate(short, &who) {
                Ok(true) => (format!("✅ Remembered: {text}"), "remembered"),
                Ok(false) => (format!("(already decided) {text}"), "already decided"),
                Err(e) => (format!("! [memory] {e}"), "error"),
            },
            "n" => match self.runtime.reject_candidate(short, &who) {
                Ok(true) => (format!("✖ Discarded: {text}"), "discarded"),
                Ok(false) => (format!("(already decided) {text}"), "already decided"),
                Err(e) => (format!("! [memory] {e}"), "error"),
            },
            _ => return CallbackReply::none(),
        };
        CallbackReply::settle(toast, outcome)
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
        CallbackReply::settle(toast, outcome)
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
        // Model switching, memory review and skill installation are the
        // operator's, whatever card a subject happens to hold.
        let uid = cb["from"]["id"].as_i64().unwrap_or(0);
        let who = self.who(uid);
        let role = who.role.clone();
        let admin_only = data == "mx"
            || data.starts_with("mp:")
            || data.starts_with("mm:")
            || data.starts_with("m:")
            || data.starts_with("sk:");
        if admin_only && role != ka_agent_core::ADMIN_ROLE {
            return CallbackReply::toast("not available for this role");
        }
        // The native `/model` `/mode` `/effort` pickers (checked first:
        // `mm:`/`mo:` would not collide with `m:`, but the picker owns
        // its prefixes as a block).
        // `mx` — the ✕ on a `/model` card: collapse it to the current
        // model line, keyboard gone.
        if data == "mx" {
            let head = crate::commands::model_header(&self.runtime, &who).await;
            let line = head.lines().next().unwrap_or("model:").to_string();
            return CallbackReply::settle("", line);
        }
        // `mp:<vendor>[:<off>]` — open a provider's models (paged) in the
        // `/model` card; `mp:<off>` (digits only) pages the provider grid.
        if let Some(rest) = data.strip_prefix("mp:") {
            if let Ok(off) = rest.parse::<usize>() {
                return self.provider_page_reply(off, &who).await;
            }
            return match rest.split_once(':') {
                None => self.vendor_page_reply(rest, 0, &who).await,
                Some((vendor, off)) => match off.parse::<usize>() {
                    Ok(off) => self.vendor_page_reply(vendor, off, &who).await,
                    Err(_) => CallbackReply::none(),
                },
            };
        }
        // `mm:<vendor>:<idx>` — pick `models[idx]` of the provider.
        if let Some(rest) = data.strip_prefix("mm:") {
            return self.model_pick_callback(rest, &who).await;
        }
        // `mo:<mode>` — a `/mode` button. The phone may only tighten;
        // the refusal is the outcome (H1, same as `/mode <name>`).
        if let Some(mode) = data.strip_prefix("mo:") {
            if !crate::commands::MODES.iter().any(|(name, _)| *name == mode) {
                return CallbackReply::none();
            }
            let uid = cb["from"]["id"].as_i64().unwrap_or(0);
            return match self
                .runtime
                .set_mode(mode, &format!("telegram:{uid}"), &who)
                .await
            {
                Ok(m) => {
                    let text = format!("mode → {m}");
                    CallbackReply::settle(&text, text.clone())
                }
                Err(e) => {
                    CallbackReply::settle(&toast_cap(&e.to_string()), format!("! [mode] {e}"))
                }
            };
        }
        // `ef:<level>` — an `/effort` button.
        if let Some(level) = data.strip_prefix("ef:") {
            if !crate::commands::EFFORTS.contains(&level) {
                return CallbackReply::none();
            }
            return match self.runtime.set_effort(level, &who).await {
                Ok(reply) => CallbackReply::settle(&toast_cap(&reply), reply),
                Err(e) => {
                    CallbackReply::settle(&toast_cap(&e.to_string()), format!("! [effort] {e}"))
                }
            };
        }
        // `retry:last` — the ↻ button on a rate_limit/network error.
        if data == "retry:last" {
            return match self.runtime.retry_last(&who).await {
                Ok(Some(text)) => {
                    let head: String = text.lines().next().unwrap_or("").chars().take(60).collect();
                    CallbackReply::toast(&format!("↻ re-sending \"{head}\""))
                }
                Ok(None) => CallbackReply::toast("nothing to retry"),
                Err(e) => CallbackReply::toast(&format!("! [retry] {e}")),
            };
        }
        // `m:<short>:y|n` — a memory proposal card.
        if let Some(rest) = data.strip_prefix("m:") {
            return self.memory_callback(cb, rest).await;
        }
        // `sk:a|r:<id>` — a skill proposal card.
        if let Some(rest) = data.strip_prefix("sk:") {
            return self.skill_callback(cb, rest).await;
        }
        // `x` — the ✕ on any picker card: collapse it to its first line.
        if data == "x" {
            let line = cb["message"]["text"]
                .as_str()
                .unwrap_or("")
                .lines()
                .next()
                .unwrap_or("")
                .to_string();
            return CallbackReply::settle("", trunc_chars(&line, 60));
        }
        // `ap:<short12>:y|n` — a row of the `/approvals` card; the same
        // resolution and decision the `a:` handler uses.
        if let Some(rest) = data.strip_prefix("ap:") {
            let Some((short, verdict)) = rest.rsplit_once(':') else {
                return CallbackReply::none();
            };
            if !matches!(verdict, "y" | "n") {
                return CallbackReply::none();
            }
            let sid = self.runtime.session_id_for(&who);
            let full = sid
                .and_then(|sid| self.store.pending_approvals(&sid).ok())
                .and_then(|pending| {
                    pending
                        .into_iter()
                        .map(|a| a.id)
                        .find(|id| id.as_str().ends_with(short))
                });
            let Some(full) = full else {
                return CallbackReply::toast("expired");
            };
            let decision = if verdict == "y" {
                ApprovalDecision::Allow
            } else {
                ApprovalDecision::Deny
            };
            let word = if verdict == "y" { "approved" } else { "denied" };
            let source = format!("telegram:{}", cb["from"]["id"].as_i64().unwrap_or(0));
            return match self.runtime.decide(full.as_str(), decision, &source).await {
                Ok(()) => CallbackReply::settle(
                    if verdict == "y" {
                        "✔ approved"
                    } else {
                        "✖ denied"
                    },
                    format!("approval [{}] → {word}", short_id(&full.0, 8)),
                ),
                Err(RtError::UnknownApproval(_)) => CallbackReply::toast("expired"),
                Err(e) => {
                    eprintln!("telegram: decide {full}: {e}");
                    CallbackReply::toast("error")
                }
            };
        }
        // `qx:<short12>` — remove one queued message (the `/queue` ✕).
        if let Some(rest) = data.strip_prefix("qx:") {
            return match self.runtime.remove_queued(rest, &who) {
                Ok(true) => CallbackReply::toast("removed"),
                Ok(false) => CallbackReply::toast("not queued (already turned)"),
                Err(e) => CallbackReply::toast(&toast_cap(&e.to_string())),
            };
        }
        // `qclr` — the `/queue` card's Clear all row.
        if data == "qclr" {
            return match self.runtime.clear_queue(&who) {
                Ok(0) => CallbackReply::settle("", "the queue is empty".into()),
                Ok(n) => CallbackReply::settle("cleared", format!("cleared {n} queued message(s)")),
                Err(e) => CallbackReply::settle("error", format!("! [queue] {e}")),
            };
        }
        // `fx:<short>` — forget one memory entry (the `/forget` ✖),
        // resolved by prefix exactly like `/forget <id>`.
        if let Some(rest) = data.strip_prefix("fx:") {
            let entries = match self.runtime.memory_entries(&who) {
                Ok(e) => e,
                Err(e) => return CallbackReply::toast(&toast_cap(&e.to_string())),
            };
            let hits: Vec<_> = entries.iter().filter(|e| id_matches(&e.id, rest)).collect();
            return match hits.as_slice() {
                [one] => match self.runtime.forget(&one.id, &who) {
                    Ok(true) => CallbackReply::toast(&format!("forgot {}", short_id(&one.id, 8))),
                    Ok(false) => CallbackReply::toast("already gone"),
                    Err(e) => CallbackReply::toast(&toast_cap(&e.to_string())),
                },
                [] => CallbackReply::toast("not found"),
                _ => CallbackReply::toast("ambiguous id"),
            };
        }
        // `fp:<off>` — page the `/forget` card.
        if let Some(rest) = data.strip_prefix("fp:") {
            let Ok(off) = rest.parse::<usize>() else {
                return CallbackReply::none();
            };
            let who = self.who(uid);
            return match self.forget_card(off, &who) {
                Some((text, markup)) => CallbackReply::page(text, markup),
                None => CallbackReply::none(),
            };
        }
        // `hp:<off>` — page the `/history` card.
        if let Some(rest) = data.strip_prefix("hp:") {
            let Ok(off) = rest.parse::<usize>() else {
                return CallbackReply::none();
            };
            let who = self.who(uid);
            return match self.history_card(off, &who) {
                Some((text, markup)) => CallbackReply::page(text, markup),
                None => CallbackReply::none(),
            };
        }
        // `hv:<off>:<short>` — one history row's detail: inbound + answer,
        // with a back row to the page it came from.
        if let Some(rest) = data.strip_prefix("hv:") {
            let Some((off_s, short)) = rest.split_once(':') else {
                return CallbackReply::none();
            };
            let Ok(off) = off_s.parse::<usize>() else {
                return CallbackReply::none();
            };
            return match self.runtime.history_show(short, &who) {
                Ok(Some((inbound, answer))) => {
                    let detail = trunc_chars(&format!("> {inbound}\n{answer}"), 4000);
                    let text = cap_lines(&detail, 12);
                    let markup = json!({
                        "inline_keyboard": [[
                            kb_button("‹ back", &format!("hp:{off}")),
                            kb_button("✕", "x"),
                        ]]
                    });
                    CallbackReply::page(text, markup)
                }
                Ok(None) => CallbackReply::toast("no such turn"),
                Err(e) => CallbackReply::toast(&toast_cap(&e.to_string())),
            };
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
                    self.runtime
                        .session_id_for(&who)
                        .and_then(|sid| self.store.pending_approvals(&sid).ok())
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
        let source = format!("telegram:{}", cb["from"]["id"].as_i64().unwrap_or(0));
        match self.runtime.decide(&full, decision, &source).await {
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

    /// Send the native picker card for a bare picker command from the
    /// operator: the current value plus an inline keyboard instead of
    /// the router's text listing. Best-effort — a refused send logs and
    /// drops the card, the command stays answered.
    async fn send_picker(
        &self,
        client: &TelegramClient,
        chat_id: i64,
        cmd: &str,
        surface: &Arc<SurfaceInner>,
        uid: i64,
    ) {
        let who = self.who(uid);
        let (text, markup) = match cmd {
            "/mode" => {
                let current = self.runtime.effective_mode_for(&who.username);
                (format!("mode: {current}"), mode_keyboard(&current))
            }
            "/effort" => {
                let current = self
                    .runtime
                    .effort(&who)
                    .unwrap_or_else(|| "model default".into());
                (format!("effort: {current}"), effort_keyboard(&current))
            }
            "/approvals" => match self.approvals_card(&who) {
                Some(card) => card,
                None => {
                    let _ = client
                        .send_message(chat_id, "no pending approvals", None)
                        .await;
                    return;
                }
            },
            "/queue" => match self.queue_card(&who) {
                Some(card) => card,
                None => {
                    let _ = client
                        .send_message(chat_id, "the queue is empty", None)
                        .await;
                    return;
                }
            },
            "/forget" => match self.forget_card(0, &who) {
                Some(card) => card,
                None => {
                    let _ = client
                        .send_message(chat_id, "nothing is remembered yet", None)
                        .await;
                    return;
                }
            },
            "/skills" => self.skills_card(uid).await,
            "/history" => match self.history_card(0, &who) {
                Some(card) => card,
                None => {
                    let _ = client
                        .send_message(chat_id, "no history yet — say something first", None)
                        .await;
                    return;
                }
            },
            "/cleanup" => {
                self.run_cleanup(client, chat_id, surface).await;
                return;
            }
            _ => {
                let listing = self.runtime.models_listing();
                let line = if listing.providers.is_empty() {
                    "no models known — ka-agent models --refresh pulls the live list"
                } else {
                    "pick a provider:"
                };
                let text = format!(
                    "{}\n{line}",
                    crate::commands::model_header(&self.runtime, &who).await
                );
                (text, provider_grid(&listing, 0))
            }
        };
        if let Err(e) = client.send_message(chat_id, &text, Some(markup)).await {
            eprintln!("telegram: {cmd} picker: {e}");
        }
    }

    /// `/cleanup`: delete every tracked error/notice message (sequential,
    /// best-effort — a refusal past Telegram's 48 h window is skipped)
    /// and report what went.
    async fn run_cleanup(
        &self,
        client: &TelegramClient,
        chat_id: i64,
        surface: &Arc<SurfaceInner>,
    ) {
        let drained: Vec<TrackedMsg> = surface
            .tracked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
            .collect();
        if drained.is_empty() {
            let _ = client
                .send_message(chat_id, "nothing to clean up", None)
                .await;
            return;
        }
        let errors = drained
            .iter()
            .filter(|m| matches!(m.kind, TrackedKind::Error))
            .count();
        let notices = drained.len() - errors;
        for m in &drained {
            let _ = client.delete_message(chat_id, m.message_id).await;
        }
        let breakdown = match (errors, notices) {
            (0, _) => String::new(),
            (_, 0) => format!(" ({errors} error card(s))"),
            _ => format!(" ({errors} error, {notices} notice)"),
        };
        let _ = client
            .send_message(
                chat_id,
                &format!("removed {} message(s){breakdown}", drained.len()),
                None,
            )
            .await;
    }

    /// The `/approvals` card: one line per pending approval, one
    /// Allow/Deny row per item (`ap:<short12>:y|n`), ✕ close.
    /// Who a Telegram uid acts as (an unlinked uid never reaches here: both
    /// the message and callback paths gate on the identity link first).
    fn who(&self, uid: i64) -> ka_agent_core::Principal {
        self.runtime
            .principal_for_source(&format!("telegram:{uid}"))
            .unwrap_or_else(ka_agent_core::Principal::operator)
    }

    fn approvals_card(&self, who: &ka_agent_core::Principal) -> Option<(String, Value)> {
        let cards = self.runtime.pending_approval_cards(who).ok()?;
        if cards.is_empty() {
            return None;
        }
        let mut lines = Vec::new();
        let mut rows = Vec::new();
        for c in cards.iter().take(10) {
            let short = short_id(&c.approval_id, 12);
            let question = c.question.lines().next().unwrap_or("").to_string();
            lines.push(format!(
                "[{}] {} — expires in {}",
                short_id(&c.approval_id, 8),
                trunc_chars(&question, 60),
                crate::commands::fmt_interval(c.expires_at_ms - now_ms())
            ));
            rows.push(vec![
                kb_button("✅ Allow", &format!("ap:{short}:y")),
                kb_button("⛔ Deny", &format!("ap:{short}:n")),
            ]);
        }
        if cards.len() > 10 {
            lines.push(format!("{} more — /approvals lists them", cards.len() - 10));
        }
        rows.push(vec![kb_button("✕", "x")]);
        Some((
            format!("pending approvals:\n{}", lines.join("\n")),
            json!({ "inline_keyboard": rows }),
        ))
    }

    /// The `/queue` card: one line per queued message, a ✕ per row
    /// (`qx:<short12>`) and a Clear all row.
    fn queue_card(&self, who: &ka_agent_core::Principal) -> Option<(String, Value)> {
        let items = self.runtime.queued(who).ok()?;
        if items.is_empty() {
            return None;
        }
        let mut lines = Vec::new();
        let mut rows = Vec::new();
        for (i, (id, source, text)) in items.iter().take(10).enumerate() {
            lines.push(format!("#{} [{}] {}", i + 1, source, trunc_chars(text, 80)));
            rows.push(vec![kb_button("✕", &format!("qx:{}", short_id(id, 12)))]);
        }
        if items.len() > 10 {
            lines.push(format!(
                "{} more — /queue clear empties the rest",
                items.len() - 10
            ));
        }
        rows.push(vec![kb_button("Clear all", "qclr"), kb_button("✕", "x")]);
        Some((
            format!("{} queued:\n{}", items.len(), lines.join("\n")),
            json!({ "inline_keyboard": rows }),
        ))
    }

    /// One page of the `/forget` card: eight remembered facts per page
    /// (`[short] first-80`), a ✖ per row (`fx:<short>`), `fp:<off>`
    /// paging.
    fn forget_card(
        &self,
        off: usize,
        who: &ka_agent_core::Principal,
    ) -> Option<(String, Value)> {
        let entries = self.runtime.memory_entries(who).ok()?;
        if entries.is_empty() {
            return None;
        }
        if off >= entries.len() {
            return None;
        }
        const PER: usize = 8;
        let page: Vec<_> = entries.iter().skip(off).take(PER).collect();
        let mut lines = Vec::new();
        let mut rows = Vec::new();
        for e in &page {
            lines.push(format!(
                "[{}] {}",
                short_id(&e.id, 8),
                trunc_chars(&e.text, 80)
            ));
            rows.push(vec![kb_button("✖", &format!("fx:{}", short_id(&e.id, 8)))]);
        }
        let mut nav = Vec::new();
        if off > 0 {
            nav.push(kb_button(
                "‹ prev",
                &format!("fp:{}", off.saturating_sub(PER)),
            ));
        }
        if off + PER < entries.len() {
            nav.push(kb_button("more ›", &format!("fp:{}", off + PER)));
        }
        nav.push(kb_button("✕", "x"));
        rows.push(nav);
        Some((
            format!("remembered ({}):\n{}", entries.len(), lines.join("\n")),
            json!({ "inline_keyboard": rows }),
        ))
    }

    /// The `/skills` card: pending proposals get the existing
    /// `sk:a:<id>` / `sk:r:<id>` rows (the same handler the proposal
    /// cards use); with none pending it lists the installed skills over
    /// a lone ✕ close row.
    async fn skills_card(&self, uid: i64) -> (String, Value) {
        let pending = self.store.pending_skill_candidates().unwrap_or_default();
        if !pending.is_empty() {
            let mut lines = Vec::new();
            let mut rows = Vec::new();
            for c in pending.iter().take(10) {
                lines.push(format!(
                    "[{}] {} — {}",
                    short_id(&c.id, 8),
                    c.name,
                    trunc_chars(&c.description, 60)
                ));
                // The same data shape and handler as proposal cards,
                // including the 64-byte guard: a long id falls back to
                // its short suffix, which `id_matches` resolves.
                let target = if c.id.len() <= 59 {
                    c.id.clone()
                } else {
                    ka_agent_core::ids::short_id(&c.id).to_string()
                };
                rows.push(vec![
                    kb_button("✅ Install", &format!("sk:a:{target}")),
                    kb_button("✖ Reject", &format!("sk:r:{target}")),
                ]);
            }
            if pending.len() > 10 {
                lines.push(format!(
                    "{} more — /skills review lists them",
                    pending.len() - 10
                ));
            }
            return (
                format!("proposed skills:\n{}", lines.join("\n")),
                json!({ "inline_keyboard": rows }),
            );
        }
        let reply = match crate::commands::route(
            &self.runtime,
            &format!("telegram:{uid}"),
            "/skills",
        )
        .await
        {
            crate::commands::Outcome::Reply(reply) => cap_lines(&reply, PICKER_TEXT_LINES),
            _ => "no skills enabled".into(),
        };
        (reply, json!({ "inline_keyboard": [[kb_button("✕", "x")]] }))
    }

    /// One page of the `/history` card: the `run_history` line format,
    /// `hp:<off>` paging and a ▸ detail row per turn
    /// (`hv:<off>:<short>`).
    fn history_card(
        &self,
        off: usize,
        who: &ka_agent_core::Principal,
    ) -> Option<(String, Value)> {
        let who = who.clone();
        let turns = self.runtime.recent_turns(6, off, &who).ok()?;
        if turns.is_empty() {
            return None;
        }
        let now = self.runtime.now_zoned();
        let mut lines = Vec::new();
        let mut rows = Vec::new();
        for (t, answer) in &turns {
            let at = jiff::Timestamp::from_millisecond(t.created_ms)
                .map(|ts| ts.to_zoned(now.time_zone().clone()))
                .unwrap_or_else(|_| now.clone());
            let take = |s: &str, n: usize| -> String { s.chars().take(n).collect() };
            lines.push(format!(
                "[{}] {:02}:{:02} you: {} → {}",
                short_id(t.id.as_str(), 8),
                at.hour(),
                at.minute(),
                take(&t.inbound_text, 60),
                take(answer, 60)
            ));
            rows.push(vec![kb_button(
                "▸",
                &format!("hv:{}:{}", off, short_id(t.id.as_str(), 8)),
            )]);
        }
        let mut nav = Vec::new();
        if off > 0 {
            nav.push(kb_button(
                "‹ prev",
                &format!("hp:{}", off.saturating_sub(6)),
            ));
        }
        if turns.len() == 6
            && self
                .runtime
                .recent_turns(1, off + 6, &who)
                .is_ok_and(|more| !more.is_empty())
        {
            nav.push(kb_button("more ›", &format!("hp:{}", off + 6)));
        }
        nav.push(kb_button("✕", "x"));
        rows.push(nav);
        Some((
            format!("history:\n{}", lines.join("\n")),
            json!({ "inline_keyboard": rows }),
        ))
    }

    /// `mp:<off>` — the provider grid page `off` of the `/model` card.
    async fn provider_page_reply(
        &self,
        off: usize,
        who: &ka_agent_core::Principal,
    ) -> CallbackReply {
        let listing = self.runtime.models_listing();
        if off >= listing.providers.len() {
            return CallbackReply::none();
        }
        let text = format!(
            "{}\npick a provider:",
            crate::commands::model_header(&self.runtime, who).await
        );
        CallbackReply::page(text, provider_grid(&listing, off))
    }

    /// `mp:<vendor>[:<off>]` — one page of a provider's models in the
    /// `/model` card: the shared vendor listing text (capped) over a
    /// keyboard of its models.
    async fn vendor_page_reply(
        &self,
        vendor: &str,
        off: usize,
        who: &ka_agent_core::Principal,
    ) -> CallbackReply {
        let listing = self.runtime.models_listing();
        let Some(p) = listing.providers.iter().find(|p| p.name == vendor) else {
            return CallbackReply::none();
        };
        if off >= p.models.len() {
            return CallbackReply::none();
        }
        let text = cap_lines(
            &crate::commands::model_vendor_listing(&self.runtime, vendor, who).await,
            PICKER_TEXT_LINES,
        );
        CallbackReply::page(text, model_grid(p, off))
    }

    /// `mm:<vendor>:<idx>` — switch to `models[idx]` and settle the card
    /// to the same reply the router's `/model <selector>` produces.
    async fn model_pick_callback(
        &self,
        rest: &str,
        who: &ka_agent_core::Principal,
    ) -> CallbackReply {
        let Some((vendor, idx)) = rest.rsplit_once(':') else {
            return CallbackReply::none();
        };
        let Ok(idx) = idx.parse::<usize>() else {
            return CallbackReply::none();
        };
        let listing = self.runtime.models_listing();
        let Some(p) = listing.providers.iter().find(|p| p.name == vendor) else {
            return CallbackReply::none();
        };
        let Some(sel) = p.models.get(idx) else {
            return CallbackReply::none();
        };
        let sel = sel.clone();
        match self.runtime.set_model(&sel, who).await {
            Ok(id) => {
                let mut reply = format!("model → {id} (next message uses it)");
                if let Ok(Some((_, ka_agent_runtime::ModelKeyState::Missing(env)))) =
                    self.runtime.model_key_state()
                {
                    reply.push_str(&format!("\n✗ {env} missing — ka-agent key set {env}"));
                }
                CallbackReply::settle(&toast_cap(&format!("model → {id}")), reply)
            }
            Err(e) => CallbackReply::settle(&toast_cap(&e.to_string()), format!("! [model] {e}")),
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
        self.adopt_chat(surface, uid, chat_id).await;
        let reply = self.callback_action(cb, cb["data"].as_str(), surface).await;
        // Exactly one answer per callback query, on every path.
        if let Some(id) = cb["id"].as_str() {
            let _ = client.answer_callback_query(id, &reply.toast).await;
        }
        if let Some(text) = reply.edit
            && let Some(message_id) = cb["message"]["message_id"].as_i64()
        {
            // A picker page carries its own keyboard; every other edit
            // removes it (a decided card cannot be pressed again).
            let markup = reply
                .markup
                .unwrap_or_else(|| json!({ "inline_keyboard": [] }));
            let _ = client
                .edit_message_text(chat_id, message_id, &text, Some(markup))
                .await;
        }
    }
}

/// Submit a captioned media message as the operator's turn: 👀 on the
/// message, the caption as free text (the router answers anything that
/// looks like a command), and every saved file as an `@reference` — the
/// same path a typed `caption @attachments/…` message takes.
#[allow(clippy::too_many_arguments)]
async fn submit_caption_turn(
    client: &TelegramClient,
    runtime: &Runtime,
    store: &Store,
    chat_id: i64,
    uid: i64,
    message_id: Option<i64>,
    caption: &str,
    refs: &[String],
) {
    let mentions: Vec<String> = refs.iter().map(|r| format!("@{r}")).collect();
    let text = if mentions.is_empty() {
        caption.to_string()
    } else {
        format!("{caption}\n{}", mentions.join(" "))
    };
    let source = format!("telegram:{uid}");
    match crate::commands::route(runtime, &source, &text).await {
        crate::commands::Outcome::Reply(reply) => {
            let _ = client.send_message(chat_id, &reply, None).await;
        }
        crate::commands::Outcome::Silent => {}
        crate::commands::Outcome::NotACommand => {
            if let Some(message_id) = message_id {
                let client = client.clone();
                tokio::spawn(async move {
                    let _ = client.set_message_reaction(chat_id, message_id, "👀").await;
                });
            }
            let who = runtime
                .principal_for_source(&source)
                .unwrap_or_else(ka_agent_core::Principal::operator);
            let (text, refs) =
                crate::refs::expand(store, &runtime.workspace_for(&who), &who, &text).await;
            crate::refs::stage_sources(store, &source, &refs);
            if let Err(e) = runtime.handle_message(&text, &source).await {
                eprintln!("telegram: handle_message: {e}");
            }
        }
    }
}

/// The lone words that open a native picker instead of the router's
/// text listing: `/model`, `/mode`, `/effort`, `/approvals`, `/queue`,
/// `/forget`, `/skills`, `/history` or `/cleanup`, optionally
/// `@botname`-suffixed (Telegram appends it), with nothing after. Every
/// argument form still routes through the shared router.
fn bare_picker_command(text: &str) -> Option<&'static str> {
    let mut words = text.split_whitespace();
    let first = words.next()?;
    if words.next().is_some() {
        return None;
    }
    match first.split('@').next().unwrap_or(first) {
        "/model" => Some("/model"),
        "/mode" => Some("/mode"),
        "/effort" => Some("/effort"),
        "/approvals" => Some("/approvals"),
        "/queue" => Some("/queue"),
        "/forget" => Some("/forget"),
        "/skills" => Some("/skills"),
        "/history" => Some("/history"),
        "/cleanup" => Some("/cleanup"),
        _ => None,
    }
}

/// One inline-keyboard button.
fn kb_button(text: &str, data: &str) -> Value {
    json!({ "text": text, "callback_data": data })
}

/// Buttons laid out in rows of `cols`.
fn kb_rows(buttons: Vec<Value>, cols: usize) -> Vec<Vec<Value>> {
    buttons.chunks(cols).map(<[Value]>::to_vec).collect()
}

/// `s` capped at `max` characters, an ellipsis marking the cut.
fn trunc_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// A toast (`answerCallbackQuery` text): capped at Telegram's 200 chars.
fn toast_cap(text: &str) -> String {
    trunc_chars(text, TOAST_CHARS)
}

/// The provider grid of the `/model` picker: one `name (N)` button per
/// provider (the current vendor dotted), rows of 3, `‹ prev` / `… more`
/// paging past 24, and the ✕ that collapses the card.
fn provider_grid(listing: &ModelsListing, off: usize) -> Value {
    let current = listing.current_vendor();
    let buttons: Vec<Value> = listing
        .providers
        .iter()
        .skip(off)
        .take(PROVIDER_PAGE)
        .map(|p| {
            let dot = if Some(p.name.as_str()) == current {
                "· "
            } else {
                ""
            };
            kb_button(
                &format!("{dot}{} ({})", p.name, p.models.len()),
                &format!("mp:{}", p.name),
            )
        })
        .collect();
    let mut rows = kb_rows(buttons, PROVIDER_COLS);
    let mut nav = Vec::new();
    if off > 0 {
        nav.push(kb_button(
            "‹ prev",
            &format!("mp:{}", off.saturating_sub(PROVIDER_PAGE)),
        ));
    }
    if off + PROVIDER_PAGE < listing.providers.len() {
        nav.push(kb_button("… more", &format!("mp:{}", off + PROVIDER_PAGE)));
    }
    if !nav.is_empty() {
        rows.push(nav);
    }
    rows.push(vec![kb_button("✕", "mx")]);
    json!({ "inline_keyboard": rows })
}

/// One page of a provider's models in the `/model` picker: rows of 2,
/// the bare model id on each button (≤24 chars, the index global so a
/// later page still names a real model), `‹ prev` / `more ›` over the
/// full list, `‹ providers` back to the grid.
fn model_grid(p: &ProviderModels, off: usize) -> Value {
    let buttons: Vec<Value> = p
        .models
        .iter()
        .enumerate()
        .skip(off)
        .take(MODEL_PAGE)
        .map(|(i, m)| {
            let id = m.strip_prefix(&format!("{}/", p.name)).unwrap_or(m);
            kb_button(&trunc_chars(id, 24), &format!("mm:{}:{i}", p.name))
        })
        .collect();
    let mut rows = kb_rows(buttons, MODEL_COLS);
    let mut nav = Vec::new();
    if off > 0 {
        nav.push(kb_button(
            "‹ prev",
            &format!("mp:{}:{}", p.name, off.saturating_sub(MODEL_PAGE)),
        ));
    }
    if off + MODEL_PAGE < p.models.len() {
        nav.push(kb_button(
            "more ›",
            &format!("mp:{}:{}", p.name, off + MODEL_PAGE),
        ));
    }
    if !nav.is_empty() {
        rows.push(nav);
    }
    rows.push(vec![kb_button("‹ providers", "mp:0"), kb_button("✕", "mx")]);
    json!({ "inline_keyboard": rows })
}

/// The `/mode` keyboard: one row per mode, `name — desc` (the desc
/// capped at 20 chars), the current mode dotted.
fn mode_keyboard(current: &str) -> Value {
    let rows: Vec<Vec<Value>> = crate::commands::MODES
        .iter()
        .map(|(name, desc)| {
            let dot = if *name == current { "· " } else { "" };
            vec![kb_button(
                &format!("{dot}{name} — {}", trunc_chars(desc, 20)),
                &format!("mo:{name}"),
            )]
        })
        .collect();
    json!({ "inline_keyboard": rows })
}

/// The `/effort` keyboard: one row of the five levels, current dotted.
fn effort_keyboard(current: &str) -> Value {
    let buttons: Vec<Value> = crate::commands::EFFORTS
        .iter()
        .map(|level| {
            let dot = if *level == current { "· " } else { "" };
            kb_button(&format!("{dot}{level}"), &format!("ef:{level}"))
        })
        .collect();
    json!({ "inline_keyboard": [buttons] })
}

/// The first `n` lines of `text` — a picker card caps its listing.
fn cap_lines(text: &str, n: usize) -> String {
    text.lines().take(n).collect::<Vec<_>>().join("\n")
}
/// The human name of the first unsupported media kind `message` carries,
/// if any — used for the `! [media] … isn't supported yet` refusal.
fn unsupported_media_kind(message: &Value) -> Option<&'static str> {
    [
        ("video_note", "video note"),
        ("video", "video"),
        ("animation", "animation"),
        ("sticker", "sticker"),
        ("location", "location"),
        ("contact", "contact"),
    ]
    .into_iter()
    .find_map(|(key, kind)| message.get(key).is_some().then_some(kind))
}
