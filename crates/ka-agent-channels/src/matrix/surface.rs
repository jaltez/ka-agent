// MatrixSurface: the outbound Surface impl and sender queue. Split from matrix.rs (moves only).

use super::MatrixConfig;
use super::client::Client;
use crate::commands;
use ka_agent_runtime::Envelope;
use ka_agent_runtime::event::AgentEvent;
use ka_agent_runtime::surface::Surface;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Notify, mpsc, watch};

/// The first streamed message waits for this much text (or a short pause)
/// so a one-liner still arrives as one ordinary message.
const STREAM_MIN_CHARS: usize = 80;
/// Minimum time between streamed sends/edits.
const STREAM_MIN_INTERVAL_MS: i64 = 2000;
/// Without enough text, the first draft is flushed after this pause.
const STREAM_FIRST_DELAY_MS: i64 = 2000;
/// `m.typing` is a heartbeat: the homeserver drops it after the timeout.
const TYPING_TIMEOUT_MS: u64 = 30_000;
const TYPING_REFRESH: Duration = Duration::from_secs(20);
/// Bounded card-event → approval map (oldest evicted).
const CARD_CAP: usize = 256;

/// One queued outbound item. Approval cards must keep their sent event id
/// so a ✅/❌ reaction can be resolved back to the approval.
pub(super) enum Outbound {
    Text(String),
    Card { id: String, text: String },
}

/// The reply being streamed in the current turn.
#[derive(Default)]
struct Stream {
    /// A turn is live on this surface.
    active: bool,
    /// The turn this stream belongs to (a repeated `TurnStarted` is a no-op).
    turn: Option<String>,
    /// The turn is scheduled (`cron`/`briefing`): text is delivered once,
    /// at the end, with no typing indicator.
    scheduled: bool,
    /// Everything the model streamed so far.
    text: String,
    /// When the first delta arrived (0 before it).
    first_ms: i64,
    /// The room the draft went to: every edit and the final delivery of
    /// this turn target it, even if the operator writes elsewhere.
    room: Option<String>,
    /// The message being streamed, once it exists.
    event_id: Option<String>,
    /// Text length already on the homeserver.
    pushed_len: usize,
    /// Last send/edit time.
    last_ms: i64,
    /// The pump task is running.
    pumping: bool,
    /// A send/edit is in flight; a settlement waits for its write-back.
    sending: bool,
    /// The turn settled while a send was in flight: the pump delivers this
    /// text once it knows whether a draft exists.
    pending_final: Option<String>,
    /// Turn counter: a pump from an older turn stops at once.
    epoch: u64,
}

/// Stream + typing + card state shared with the channel's tasks.
pub(super) struct Shared {
    pub(super) client: Arc<Client>,
    pub(super) room: Arc<Mutex<Option<String>>>,
    user: String,
    tx: mpsc::UnboundedSender<Outbound>,
    rx: Mutex<Option<mpsc::UnboundedReceiver<Outbound>>>,
    stream: Mutex<Stream>,
    wake: Arc<Notify>,
    /// One stream request at a time: a final edit never gets overtaken by
    /// an older snapshot.
    gate: Arc<tokio::sync::Mutex<()>>,
    typing: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

/// The Matrix surface: notices, answers and approval cards go to the room
/// the operator last wrote from (one operator per instance). Replies on
/// operator turns stream as an edited message; scheduled turns arrive once.
pub struct MatrixSurface {
    pub(super) client: Arc<Client>,
    pub(super) room: Arc<Mutex<Option<String>>>,
    pub(super) room_known: Arc<Notify>,
    pub(super) cards: Arc<Mutex<VecDeque<(String, String)>>>,
    inner: Arc<Shared>,
}

impl MatrixSurface {
    pub fn new(cfg: MatrixConfig, initial_room: Option<String>) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let client = Arc::new(Client::new(cfg.homeserver, cfg.token));
        let room = Arc::new(Mutex::new(initial_room.filter(|r| !r.is_empty())));
        let room_known = Arc::new(Notify::new());
        let cards = Arc::new(Mutex::new(VecDeque::new()));
        let inner = Arc::new(Shared {
            client: Arc::clone(&client),
            room: Arc::clone(&room),
            user: cfg.user_id,
            tx,
            rx: Mutex::new(Some(rx)),
            stream: Mutex::new(Stream::default()),
            wake: Arc::new(Notify::new()),
            gate: Arc::new(tokio::sync::Mutex::new(())),
            typing: Mutex::new(None),
        });
        Self {
            client,
            room,
            room_known,
            cards,
            inner,
        }
    }

    /// Take the outbound queue (once): the channel's sender task drains it,
    /// keeping sends on one task and in order.
    pub(super) fn take_receiver(&self) -> Option<mpsc::UnboundedReceiver<Outbound>> {
        self.inner
            .rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    /// Set the delivery room; returns whether it changed.
    pub(super) fn set_room(&self, room: &str) -> bool {
        let changed = {
            let mut cur = self.room.lock().unwrap_or_else(|e| e.into_inner());
            if cur.as_deref() == Some(room) {
                false
            } else {
                *cur = Some(room.to_string());
                true
            }
        };
        self.room_known.notify_one();
        changed
    }

    pub(super) fn enqueue(&self, text: &str) {
        let _ = self.inner.tx.send(Outbound::Text(text.to_string()));
    }

    fn enqueue_card(&self, id: &str, text: &str) {
        let _ = self.inner.tx.send(Outbound::Card {
            id: id.to_string(),
            text: text.to_string(),
        });
    }

    /// The approval a card's event id stands for (a ✅/❌ reaction target).
    pub(super) fn approval_for_card(&self, event_id: &str) -> Option<String> {
        self.cards
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|(card, _)| card == event_id)
            .map(|(_, id)| id.clone())
    }

    /// Drop a card (reacted to, or settled).
    pub(super) fn forget_card(&self, event_id: &str) {
        self.cards
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(card, _)| card != event_id);
    }

    fn forget_approval(&self, approval_id: &str) {
        self.cards
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(_, id)| id != approval_id);
    }
}

impl Shared {
    fn room(&self) -> Option<String> {
        self.room.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// A turn started: reset the stream and start typing on operator turns.
    /// Reset happens once per turn id: a repeated `TurnStarted` for the same
    /// turn must not wipe the draft (the runtime announces each turn once).
    fn start(self: &Arc<Self>, turn: &str, scheduled: bool) {
        let room = self.room();
        let pending = {
            let mut s = self.stream.lock().unwrap_or_else(|e| e.into_inner());
            if s.turn.as_deref() == Some(turn) {
                return;
            }
            // A settlement still waiting on the old turn's in-flight send
            // cannot edit its draft once the turn is replaced: post it.
            let pending = s.pending_final.take();
            let epoch = s.epoch + 1;
            *s = Stream {
                active: true,
                turn: Some(turn.to_string()),
                scheduled,
                epoch,
                room: room.clone(),
                ..Stream::default()
            };
            pending
        };
        if let Some(text) = pending {
            let _ = self.tx.send(Outbound::Text(text));
        }
        if scheduled {
            return;
        }
        let Some(room) = room else { return };
        let mut typing = self.typing.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(old) = typing.take() {
            old.abort();
        }
        let client = Arc::clone(&self.client);
        let user = self.user.clone();
        *typing = Some(tokio::spawn(async move {
            loop {
                client.typing(&room, &user, TYPING_TIMEOUT_MS).await;
                tokio::time::sleep(TYPING_REFRESH).await;
            }
        }));
    }

    /// Stop the typing heartbeat; `send_stop` also clears the indicator in
    /// the room the turn is pinned to (falling back to the current room).
    fn stop_typing(&self, send_stop: bool) {
        if let Some(handle) = self.typing.lock().unwrap_or_else(|e| e.into_inner()).take() {
            handle.abort();
        }
        if !send_stop {
            return;
        }
        let pinned = {
            let s = self.stream.lock().unwrap_or_else(|e| e.into_inner());
            s.room.clone()
        };
        let Some(room) = pinned.or_else(|| self.room()) else {
            return;
        };
        let client = Arc::clone(&self.client);
        let user = self.user.clone();
        tokio::spawn(async move { client.typing(&room, &user, 0).await });
    }

    /// Accumulate a streamed delta and make sure a pump is watching it.
    /// The draft stops growing at [`super::REPLY_CAP`]: a longer answer
    /// splits at the final boundary, exactly like a non-streamed reply.
    fn push_delta(self: &Arc<Self>, delta: &str) {
        let spawn = {
            let mut s = self.stream.lock().unwrap_or_else(|e| e.into_inner());
            if !s.active || s.scheduled {
                return;
            }
            if s.text.is_empty() {
                s.first_ms = ka_agent_core::now_ms();
            }
            let room = super::REPLY_CAP.saturating_sub(s.text.len());
            s.text
                .push_str(ka_agent_core::truncate_at_char(delta, room));
            if s.pumping {
                None
            } else {
                s.pumping = true;
                Some(s.epoch)
            }
        };
        if let Some(epoch) = spawn {
            tokio::spawn(Arc::clone(self).pump(epoch));
        } else {
            self.wake.notify_one();
        }
    }

    /// The pump: sends the first draft once it is worth a message, then
    /// edits it at most every [`STREAM_MIN_INTERVAL_MS`] with ≥
    /// [`STREAM_MIN_CHARS`] new bytes, until the turn settles.
    async fn pump(self: Arc<Self>, epoch: u64) {
        enum Step {
            Push,
            Wait(i64),
            Idle,
            Stop,
        }
        loop {
            let now = ka_agent_core::now_ms();
            let step = {
                let s = self.stream.lock().unwrap_or_else(|e| e.into_inner());
                if s.epoch != epoch || !s.active || s.scheduled {
                    Step::Stop
                } else if s.text.is_empty() {
                    Step::Idle
                } else if s.event_id.is_none() {
                    let due = s.text.len() >= STREAM_MIN_CHARS
                        || now - s.first_ms >= STREAM_FIRST_DELAY_MS;
                    if !due {
                        Step::Wait(s.first_ms + STREAM_FIRST_DELAY_MS)
                    } else if s.last_ms != 0 && now - s.last_ms < STREAM_MIN_INTERVAL_MS {
                        // A failed draft send backs off instead of retrying
                        // in a tight loop.
                        Step::Wait(s.last_ms + STREAM_MIN_INTERVAL_MS)
                    } else {
                        Step::Push
                    }
                } else if s.text.len().saturating_sub(s.pushed_len) < STREAM_MIN_CHARS {
                    // Nothing time alone can make due: wait for more text.
                    Step::Idle
                } else if now - s.last_ms >= STREAM_MIN_INTERVAL_MS {
                    Step::Push
                } else {
                    Step::Wait(s.last_ms + STREAM_MIN_INTERVAL_MS)
                }
            };
            match step {
                Step::Stop => break,
                Step::Idle => self.wake.notified().await,
                Step::Wait(at) => {
                    let wait = (at - ka_agent_core::now_ms()).max(0) as u64;
                    tokio::select! {
                        _ = self.wake.notified() => {}
                        _ = tokio::time::sleep(Duration::from_millis(wait)) => {}
                    }
                }
                Step::Push => {
                    let current = self.room();
                    // The room is pinned by the first send of the turn:
                    // edits, the final edit and typing all stay in it even
                    // if the operator writes from another room meanwhile.
                    let pinned = {
                        let s = self.stream.lock().unwrap_or_else(|e| e.into_inner());
                        if s.epoch != epoch || !s.active || s.scheduled {
                            break;
                        }
                        s.room.clone().or(current)
                    };
                    let Some(room) = pinned else {
                        // No room yet: wait for one instead of spinning.
                        self.wake.notified().await;
                        continue;
                    };
                    let (text, event_id) = {
                        let mut s = self.stream.lock().unwrap_or_else(|e| e.into_inner());
                        if s.epoch != epoch || !s.active || s.scheduled {
                            break;
                        }
                        // Pinning the room and marking the send in flight
                        // happen together: a settlement from here on waits
                        // for this send's write-back.
                        s.room = Some(room.clone());
                        s.sending = true;
                        (s.text.clone(), s.event_id.clone())
                    };
                    if !self.push(epoch, &room, &text, event_id.as_deref()).await {
                        break;
                    }
                }
            }
        }
        let mut s = self.stream.lock().unwrap_or_else(|e| e.into_inner());
        if s.epoch == epoch {
            s.pumping = false;
        }
    }

    /// One send/edit and its write-back, under the stream gate. Returns
    /// whether the pump keeps looping.
    async fn push(
        self: &Arc<Self>,
        epoch: u64,
        room: &str,
        text: &str,
        event_id: Option<&str>,
    ) -> bool {
        enum Action {
            Send,
            Settle(String),
            Stop,
        }
        let _gate = self.gate.lock().await;
        // The turn may have settled (and queued its final text) while this
        // push waited for the gate.
        let action = {
            let mut s = self.stream.lock().unwrap_or_else(|e| e.into_inner());
            if s.epoch != epoch {
                Action::Stop
            } else if !s.active {
                s.sending = false;
                match s.pending_final.take() {
                    Some(text) => Action::Settle(text),
                    None => Action::Stop,
                }
            } else {
                Action::Send
            }
        };
        match action {
            Action::Stop => false,
            Action::Settle(text) => {
                self.stop_typing(false);
                self.settle(room, None, &text).await;
                false
            }
            Action::Send => {
                let outcome = match event_id {
                    Some(event_id) => self
                        .client
                        .edit(room, event_id, text, false)
                        .await
                        .map(|()| None),
                    None => self.client.send_plain(room, text).await.map(Some),
                };
                if let Err(e) = &outcome {
                    eprintln!("matrix: stream: {e}");
                }
                // Record the outcome only on the stream this push belongs
                // to: a stale send must not seed the next turn's draft.
                let record = {
                    let mut s = self.stream.lock().unwrap_or_else(|e| e.into_inner());
                    if s.epoch != epoch {
                        None
                    } else {
                        s.sending = false;
                        s.last_ms = ka_agent_core::now_ms();
                        if outcome.is_ok() {
                            s.pushed_len = text.len();
                        }
                        if let Ok(Some(event_id)) = &outcome {
                            s.event_id = Some(event_id.clone());
                        }
                        Some((s.pending_final.take(), s.event_id.clone()))
                    }
                };
                match record {
                    // A newer turn owns the stream; this send changed nothing.
                    None => false,
                    Some((Some(final_text), event_id)) => {
                        // The turn settled while this send was in flight:
                        // settle now that the draft id is known.
                        self.stop_typing(false);
                        self.settle(room, event_id.as_deref(), &final_text).await;
                        false
                    }
                    Some((None, _)) => true,
                }
            }
        }
    }

    /// Deliver a settled turn's final text: edit the streamed draft in
    /// place; with no draft (or when the homeserver refuses the edit) post
    /// the whole text as its own message, so the answer is never left
    /// truncated. Clears the typing indicator in `room` last.
    async fn settle(&self, room: &str, event_id: Option<&str>, text: &str) {
        let edited = match event_id {
            Some(event_id) => match self.client.edit(room, event_id, text, true).await {
                Ok(()) => true,
                Err(e) => {
                    eprintln!("matrix: final edit: {e}");
                    false
                }
            },
            None => false,
        };
        if !edited {
            let _ = self.tx.send(Outbound::Text(text.to_string()));
        }
        self.client.typing(room, &self.user, 0).await;
    }

    /// The final chunk: when a message was streamed, edit it in place with
    /// the rendered final text (and stop typing). `false` when there is
    /// nothing to edit — the caller sends the chunk as a message.
    fn finish(self: &Arc<Self>, text: &str) -> bool {
        enum Settle {
            Edit {
                event_id: String,
                room: String,
            },
            /// A first send is in flight: the pump settles this text after
            /// it records the draft id, under the same gate.
            Deferred,
            /// Nothing was streamed: the caller sends the chunk.
            Message,
        }
        let current_room = self.room();
        let decision = {
            let mut s = self.stream.lock().unwrap_or_else(|e| e.into_inner());
            s.active = false;
            match s.event_id.take() {
                Some(event_id) => match s.room.clone().or(current_room) {
                    Some(room) => Settle::Edit { event_id, room },
                    None => {
                        s.event_id = Some(event_id);
                        Settle::Message
                    }
                },
                None if s.sending => {
                    s.pending_final = Some(text.to_string());
                    Settle::Deferred
                }
                None => Settle::Message,
            }
        };
        // `notify_one` keeps a permit: a wakeup racing the pump's own
        // `notified()` is never lost (`notify_waiters` drops it).
        self.wake.notify_one();
        match decision {
            Settle::Message => false,
            Settle::Deferred => true,
            Settle::Edit { event_id, room } => {
                self.stop_typing(false);
                let inner = Arc::clone(self);
                let text = text.to_string();
                tokio::spawn(async move {
                    let _gate = inner.gate.lock().await;
                    inner.settle(&room, Some(&event_id), &text).await;
                });
                true
            }
        }
    }

    /// An `AgentEvent::File`: upload it and post `m.image`/`m.file`.
    fn send_file(self: &Arc<Self>, path: String, name: String, caption: String) {
        let Some(room) = self.room() else {
            eprintln!("matrix: no room for file {name}");
            return;
        };
        let client = Arc::clone(&self.client);
        tokio::spawn(async move {
            if let Err(e) = client
                .send_file(&room, std::path::Path::new(&path), &name, &caption)
                .await
            {
                eprintln!("matrix: send file {name}: {e}");
            }
        });
    }
}

/// Whether the turn's source is a scheduled job (silent while running,
/// delivered once at the end).
fn is_scheduled(source: &str) -> bool {
    source == "cron" || source.starts_with("cron:") || source == "briefing"
}

impl Surface for MatrixSurface {
    fn emit(&self, env: &Envelope) {
        match &env.ev {
            AgentEvent::TurnStarted { turn, source } => {
                self.inner.start(turn, is_scheduled(source))
            }
            // Streamed assistant text: the draft message + throttled edits.
            AgentEvent::Delta { text } => self.inner.push_delta(text),
            // In-turn progress: never sent (the journal keeps them).
            AgentEvent::Note { .. } => {}
            AgentEvent::Notice { text } => self.enqueue(text),
            AgentEvent::Approval {
                id,
                question,
                expires_at_ms,
                ..
            } => {
                let left = (expires_at_ms - ka_agent_core::now_ms()).max(0);
                let text = format!(
                    "approval {} — {}\n\nexpires in {}\n/y to allow · /n to deny\nreact ✅ to allow · ❌ to deny",
                    ka_agent_core::ids::short_id(id),
                    question,
                    commands::fmt_interval(left)
                );
                self.enqueue_card(id, &text);
            }
            AgentEvent::ApprovalSettled { id, status } => {
                self.forget_approval(id);
                self.enqueue(&format!(
                    "approval {} → {status}",
                    ka_agent_core::ids::short_id(id)
                ));
            }
            AgentEvent::FinalChunk { text, .. } => {
                if !self.inner.finish(text) {
                    // Nothing was streamed: the chunk is the whole message.
                    self.inner.stop_typing(true);
                    self.enqueue(text);
                }
            }
            AgentEvent::Error { class, message } => {
                self.inner.stop_typing(true);
                self.enqueue(&format!("! [{class}] {message}"));
            }
            // A file for the operator: uploaded and posted as media.
            AgentEvent::File {
                path,
                name,
                caption,
            } => self
                .inner
                .send_file(path.clone(), name.clone(), caption.clone()),
            // Cost and token counts live in `/status`; nothing is posted.
            AgentEvent::TurnFinished { .. } => {
                self.inner.stop_typing(true);
            }
            // Session state changes are not posted (pauses arrive as
            // notices).
            AgentEvent::Session { .. } => {}
            AgentEvent::MemoryProposal { id, text } => {
                let short = ka_agent_core::ids::short_id(id);
                self.enqueue(&format!(
                    "💡 remember this? {text}\n   /memory accept {short} · /memory reject {short}"
                ));
            }
            // Reasoning, tool structure and plans are not drawn on Matrix.
            AgentEvent::Thought { .. }
            | AgentEvent::ToolStarted { .. }
            | AgentEvent::ToolFinished { .. }
            | AgentEvent::Plan { .. }
            | AgentEvent::Context { .. }
            | AgentEvent::Compacting { .. }
            | AgentEvent::Inbound { .. } => {}
            // A fired reminder is its own notification, never a model turn.
            AgentEvent::Reminder { text, .. } => self.enqueue(&format!("⏰ {text}")),
            // Later steps wire these to real deliveries.
            AgentEvent::CronFinished { .. }
            | AgentEvent::Queue { .. }
            | AgentEvent::Settings { .. }
            | AgentEvent::SkillProposal { .. } => {}
            // Per-connection events never reach a broadcast surface.
            AgentEvent::Hello { .. } | AgentEvent::Reply { .. } => {}
        }
    }
}

/// Record a card's event id (bounded); the reaction handler resolves it.
fn record_card(cards: &Mutex<VecDeque<(String, String)>>, event_id: String, id: String) {
    let mut cards = cards.lock().unwrap_or_else(|e| e.into_inner());
    while cards.len() >= CARD_CAP {
        cards.pop_front();
    }
    cards.push_back((event_id, id));
}

/// The sender task: sends each queued message to the current room right
/// away; with no room known yet, messages wait (bounded) until one is set.
pub(super) async fn sender_loop(
    client: Arc<Client>,
    room: Arc<Mutex<Option<String>>>,
    room_known: Arc<Notify>,
    cards: Arc<Mutex<VecDeque<(String, String)>>>,
    mut rx: mpsc::UnboundedReceiver<Outbound>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut backlog: VecDeque<Outbound> = VecDeque::new();
    let push = |backlog: &mut VecDeque<Outbound>, item: Outbound| {
        if backlog.len() >= super::BACKLOG_CAP {
            backlog.pop_front();
            eprintln!("matrix: no room known yet; dropped the oldest queued message");
        }
        backlog.push_back(item);
    };
    loop {
        let mut stop = false;
        tokio::select! {
            _ = shutdown.changed() => stop = true,
            msg = rx.recv() => match msg {
                Some(item) => push(&mut backlog, item),
                None => stop = true,
            },
            _ = room_known.notified() => {}
        }
        if stop {
            // Best effort: whatever is already queued still goes out.
            while let Ok(item) = rx.try_recv() {
                push(&mut backlog, item);
            }
        }
        let current = room.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(r) = current {
            while let Some(item) = backlog.pop_front() {
                match item {
                    Outbound::Text(text) => client.send_split(&r, &text).await,
                    Outbound::Card { id, text } => match client.send(&r, &text).await {
                        Ok(event_id) => record_card(&cards, event_id, id),
                        Err(e) => eprintln!("matrix: card: {e}"),
                    },
                }
            }
        }
        if stop {
            break;
        }
    }
}
