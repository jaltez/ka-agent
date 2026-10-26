// MatrixChannel: the /sync loop, gate, dispatch and the sender task. Split from matrix.rs (moves only).

use super::client::Client;
use super::surface::MatrixSurface;
use super::*;
use crate::channel::Channel;
use crate::commands::{self, Outcome};
use ka_agent_core::AuditRecord;
use ka_agent_runtime::Runtime;
use ka_agent_store::Store;
use serde_json::Value;
use std::collections::{HashSet, VecDeque};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Notify, mpsc, watch};

/// The sender task: sends each queued message to the current room right
/// away; with no room known yet, messages wait (bounded) until one is set.
async fn sender_loop(
    client: Arc<Client>,
    room: Arc<Mutex<Option<String>>>,
    room_known: Arc<Notify>,
    mut rx: mpsc::UnboundedReceiver<String>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut backlog: VecDeque<String> = VecDeque::new();
    let push = |backlog: &mut VecDeque<String>, text: String| {
        if backlog.len() >= BACKLOG_CAP {
            backlog.pop_front();
            eprintln!("matrix: no room known yet; dropped the oldest queued message");
        }
        backlog.push_back(text);
    };
    loop {
        let mut stop = false;
        tokio::select! {
            _ = shutdown.changed() => stop = true,
            msg = rx.recv() => match msg {
                Some(text) => push(&mut backlog, text),
                None => stop = true,
            },
            _ = room_known.notified() => {}
        }
        if stop {
            // Best effort: whatever is already queued still goes out.
            while let Ok(text) = rx.try_recv() {
                push(&mut backlog, text);
            }
        }
        let current = room.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(r) = current {
            while let Some(text) = backlog.pop_front() {
                client.send_split(&r, &text).await;
            }
        }
        if stop {
            break;
        }
    }
}

pub struct MatrixChannel {
    cfg: MatrixConfig,
    runtime: Runtime,
    store: Arc<Store>,
    surface: Arc<MatrixSurface>,
    encrypted_noticed: Mutex<HashSet<String>>,
}

impl MatrixChannel {
    pub fn new(
        cfg: MatrixConfig,
        runtime: Runtime,
        store: Arc<Store>,
        surface: Arc<MatrixSurface>,
    ) -> Self {
        Self {
            cfg,
            runtime,
            store,
            surface,
            encrypted_noticed: Mutex::new(HashSet::new()),
        }
    }

    /// Point delivery at `room`, persisting it so proactive notices reach
    /// the operator after a restart.
    fn set_room(&self, room: &str) {
        if self.surface.set_room(room)
            && let Err(e) = self.store.meta_set(ROOM_KEY, room)
        {
            eprintln!("matrix: persist room: {e}");
        }
    }

    fn audit_unauthorized(&self, sender: &str, detail: String) {
        let mut record = AuditRecord::new(
            &format!("matrix:{sender}"),
            "matrix.unauthorized",
            "",
            Some(detail),
        );
        let _ = self.store.append_audit(&mut record);
    }

    fn is_linked(&self, sender: &str) -> bool {
        self.store.is_linked("matrix", sender).unwrap_or(false)
    }

    /// One pending invite: join when a linked user invited us.
    async fn handle_invite(&self, room: &str, state: &Value) {
        let events = state["invite_state"]["events"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let Some(sender) = events.iter().find_map(|e| {
            (e["type"].as_str() == Some("m.room.member")
                && e["state_key"].as_str() == Some(self.cfg.user_id.as_str())
                && e["content"]["membership"].as_str() == Some("invite"))
            .then(|| e["sender"].as_str().unwrap_or("").to_string())
        }) else {
            return;
        };
        if sender.is_empty() || !self.is_linked(&sender) {
            eprintln!("matrix: ignoring invite to {room} from {sender}");
            self.audit_unauthorized(&sender, format!("invite room={room}"));
            return;
        }
        match self.surface.client.join(room).await {
            Ok(()) => self.set_room(room),
            Err(e) => eprintln!("matrix: join {room}: {e}"),
        }
    }

    /// One joined room's timeline event.
    async fn handle_event(&self, room: &str, event: &Value) {
        let kind = event["type"].as_str().unwrap_or("");
        let sender = event["sender"].as_str().unwrap_or("");
        if sender == self.cfg.user_id || sender.is_empty() {
            return;
        }
        if kind == "m.room.encrypted" {
            if self.is_linked(sender) {
                let first = self
                    .encrypted_noticed
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(room.to_string());
                if first {
                    self.surface.client.send_split(room, ENCRYPTED_NOTICE).await;
                }
            }
            return;
        }
        if kind != "m.room.message" {
            return;
        }
        let body = event["content"]["body"].as_str().unwrap_or("").trim();
        if body.is_empty() {
            return;
        }
        // §8.1: linked identities only; everyone else is audited, ignored,
        // never answered — except a pairing code (§5.2), silent unless it
        // links.
        if !self.is_linked(sender) {
            if body
                .get(..5)
                .is_some_and(|p| p.eq_ignore_ascii_case("/pair"))
            {
                if crate::pairing::redeem(&self.store, "matrix", sender, body)
                    == crate::pairing::Redeem::Linked
                {
                    self.set_room(room);
                    self.surface.client.send_split(room, PAIRED_REPLY).await;
                }
                return;
            }
            eprintln!("matrix: unauthorized sender {sender}");
            self.audit_unauthorized(sender, format!("room={room}"));
            return;
        }
        self.set_room(room);
        let transcript;
        let body = if event["content"]["msgtype"].as_str() == Some("m.audio") {
            match self.transcribe_audio(room, &event["content"]).await {
                Some(t) => {
                    transcript = t;
                    transcript.as_str()
                }
                None => return,
            }
        } else {
            body
        };
        match commands::route(&self.runtime, &format!("matrix:{sender}"), body).await {
            Outcome::Reply(reply) => self.surface.client.send_split(room, &reply).await,
            Outcome::Silent => {}
            Outcome::NotACommand => {
                let body = crate::refs::expand(&self.store, &self.runtime.workspace(), body).await;
                if let Err(e) = self
                    .runtime
                    .handle_message(&body, &format!("matrix:{sender}"))
                    .await
                {
                    eprintln!("matrix: handle_message: {e}");
                }
            }
        }
    }

    /// An `m.audio` message → its transcript (echoed back); `None` when
    /// nothing should be submitted (voice off, too long, failure — each
    /// answered in the room).
    async fn transcribe_audio(&self, room: &str, content: &Value) -> Option<String> {
        let client = &self.surface.client;
        let Some(t) = &self.cfg.transcriber else {
            client
                .send_split(
                    room,
                    "🎙 voice messages are off — `ka-agent setup voice` turns them on",
                )
                .await;
            return None;
        };
        let ms = content["info"]["duration"].as_i64().unwrap_or(0);
        if ms / 1000 > crate::voice::MAX_SECONDS {
            client
                .send_split(room, "🎙 too long — voice messages up to 10 minutes")
                .await;
            return None;
        }
        let heard = match client
            .download_media(content["url"].as_str().unwrap_or(""))
            .await
        {
            Ok(bytes) => t.transcribe(bytes, "voice.ogg").await,
            Err(e) => Err(e),
        };
        match heard {
            Ok(text) if !text.is_empty() => {
                client.send_split(room, &format!("🎙 “{text}”")).await;
                Some(text)
            }
            Ok(_) => {
                client.send_split(room, "🎙 (nothing heard)").await;
                None
            }
            Err(e) => {
                client.send_split(room, &format!("! [voice] {e}")).await;
                None
            }
        }
    }

    async fn handle_sync(&self, value: &Value, first: bool, boot_ms: i64) {
        if let Some(invites) = value["rooms"]["invite"].as_object() {
            for (room, state) in invites {
                self.handle_invite(room, state).await;
            }
        }
        let Some(rooms) = value["rooms"]["join"].as_object() else {
            return;
        };
        for (room, state) in rooms {
            let Some(events) = state["timeline"]["events"].as_array() else {
                continue;
            };
            for event in events {
                // First-ever sync: the timeline is history, not new mail.
                if first
                    && event["origin_server_ts"]
                        .as_i64()
                        .is_some_and(|ts| ts < boot_ms - FIRST_SYNC_GRACE_MS)
                {
                    continue;
                }
                self.handle_event(room, event).await;
            }
        }
    }
}

impl Channel for MatrixChannel {
    fn name(&self) -> &'static str {
        "matrix"
    }

    fn run(self, shutdown: watch::Receiver<bool>) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(async move {
            let boot_ms = ka_agent_core::now_ms();
            let mut since = self
                .store
                .meta_get(NEXT_BATCH_KEY)
                .ok()
                .flatten()
                .filter(|s| !s.is_empty());
            let mut shutdown = shutdown;
            let Some(outbound) = self.surface.take_receiver() else {
                eprintln!("matrix: outbound queue already taken; not starting");
                return;
            };
            // Crash recovery (SEED §3.7): undelivered chunks go out before
            // any new traffic, once a room is known (until then they wait in
            // the sender's backlog, not dropped).
            let recover = self
                .store
                .pending_chunks_for_session(&self.runtime.session_id())
                .unwrap_or_default();
            for chunk in &recover {
                self.surface.enqueue(&chunk.text);
                self.store
                    .mark_sent(&chunk.turn, chunk.chunk_index, chunk.chunk_index as i64)
                    .ok();
            }
            if !recover.is_empty() {
                eprintln!("matrix: recovered {} queued chunk(s)", recover.len());
            }
            let sender = tokio::spawn(sender_loop(
                Arc::clone(&self.surface.client),
                Arc::clone(&self.surface.room),
                Arc::clone(&self.surface.room_known),
                outbound,
                shutdown.clone(),
            ));
            eprintln!(
                "matrix: syncing as {} (since {:?})",
                self.cfg.user_id, since
            );
            loop {
                let sync = tokio::select! {
                    _ = shutdown.changed() => break,
                    r = self.surface.client.sync(since.as_deref()) => r,
                };
                match sync {
                    Ok(value) => {
                        let first = since.is_none();
                        if let Some(next) = value["next_batch"].as_str() {
                            since = Some(next.to_string());
                        }
                        self.handle_sync(&value, first, boot_ms).await;
                        // Persist only after dispatch (at-least-once).
                        if let Some(next) = &since
                            && let Err(e) = self.store.meta_set(NEXT_BATCH_KEY, next)
                        {
                            eprintln!("matrix: persist next_batch: {e}");
                        }
                    }
                    Err(e) => {
                        eprintln!("matrix: sync error: {e}; backing off");
                        tokio::select! {
                            _ = shutdown.changed() => break,
                            _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                        }
                    }
                }
            }
            let _ = tokio::time::timeout(Duration::from_secs(5), sender).await;
            eprintln!("matrix: channel stopped");
        })
    }
}
