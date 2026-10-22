//! The Matrix channel (SEED.md §10 Phase 6): the second adapter proving the
//! channel trait. Client-Server API v3 subset — `GET /sync` long-poll in,
//! `PUT …/send` out, `POST …/join` for invites — against one homeserver
//! with one bot account, over `reqwest` (http and https).
//!
//! Identity: inbound senders gate on the store's `identities` table as
//! `matrix:<user_id>` (§8.1); anything else is audited and ignored, except a
//! `/pair <code>` message which redeems a one-time pairing code (§5.2).
//! Approvals are text commands (`/y`, `/n`) through the shared router —
//! the documented channel feature mismatch (no inline keyboards).
//!
//! Delivery is at-least-once with `next_batch` dedupe, mirroring the
//! Telegram offset: events are dispatched first, then `matrix.next_batch`
//! is persisted; a crash between the two replays the event. Outbound
//! messages go through a dedicated sender task, so a reply never waits
//! behind the `/sync` long-poll.

use crate::channel::Channel;
use crate::commands::{self, Outcome};
use ka_agent_core::ApprovalStatus;
use ka_agent_core::AuditRecord;
use ka_agent_runtime::Runtime;
use ka_agent_runtime::surface::{ApprovalCard, Surface};
use ka_agent_store::Store;
use serde_json::{Value, json};
use std::collections::{HashSet, VecDeque};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Notify, mpsc, watch};

/// Long-poll window for `/sync` (the daemon's shutdown latency bound).
const SYNC_TIMEOUT_SECS: u64 = 25;
/// Matrix event bodies cap far above this; keep replies short and split.
const REPLY_CAP: usize = 3500;
/// Outbound messages held while no room is known; the oldest drop first.
const BACKLOG_CAP: usize = 200;
/// On the very first sync, timeline events older than boot minus this are
/// history, not new messages.
const FIRST_SYNC_GRACE_MS: i64 = 60_000;
const NEXT_BATCH_KEY: &str = "matrix.next_batch";
const ROOM_KEY: &str = "matrix.room_id";
const ENCRYPTED_NOTICE: &str = "This room is end-to-end encrypted and Ka Agent can't read it yet — talk to me in an unencrypted room or DM.";
const PAIRED_REPLY: &str = "✓ Paired. I only answer you from now on.";

pub struct MatrixConfig {
    pub homeserver: String,
    pub user_id: String,
    pub token: String,
}

/// A small CS-API client. URLs are built segment by segment so room ids
/// (`!abc:server`) are percent-encoded.
struct Client {
    http: reqwest::Client,
    base: String,
    token: String,
    txn: AtomicU64,
}

fn endpoint(base: &str, segments: &[&str]) -> Result<url::Url, String> {
    let mut url = url::Url::parse(base).map_err(|e| format!("bad homeserver url {base:?}: {e}"))?;
    url.path_segments_mut()
        .map_err(|()| format!("bad homeserver url {base:?}"))?
        .pop_if_empty()
        .extend(segments);
    Ok(url)
}

async fn send_json(req: reqwest::RequestBuilder) -> Result<Value, String> {
    let resp = req.send().await.map_err(|e| e.to_string())?;
    let status = resp.status();
    let body = resp.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!(
            "HTTP {}: {}",
            status.as_u16(),
            body.chars().take(200).collect::<String>()
        ));
    }
    serde_json::from_str(&body).map_err(|e| format!("bad json: {e}"))
}

impl Client {
    fn new(base: String, token: String) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(SYNC_TIMEOUT_SECS + 15))
            .build()
            .unwrap_or_default();
        Self {
            http,
            base,
            token,
            txn: AtomicU64::new(1),
        }
    }

    async fn request(
        &self,
        method: reqwest::Method,
        url: url::Url,
        body: Option<&Value>,
    ) -> Result<Value, String> {
        let mut req = self
            .http
            .request(method, url)
            .bearer_auth(&self.token)
            .header("Accept", "application/json");
        if let Some(b) = body {
            req = req.json(b);
        }
        send_json(req).await
    }

    async fn sync(&self, since: Option<&str>) -> Result<Value, String> {
        let mut url = endpoint(&self.base, &["_matrix", "client", "v3", "sync"])?;
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("timeout", &(SYNC_TIMEOUT_SECS * 1000).to_string());
            q.append_pair("filter", r#"{"room":{"timeline":{"limit":50}}}"#);
            if let Some(s) = since {
                q.append_pair("since", s);
            }
        }
        self.request(reqwest::Method::GET, url, None).await
    }

    async fn send(&self, room: &str, text: &str) -> Result<(), String> {
        let txn = format!("kagent-{}", self.txn.fetch_add(1, Ordering::Relaxed));
        let url = endpoint(
            &self.base,
            &[
                "_matrix",
                "client",
                "v3",
                "rooms",
                room,
                "send",
                "m.room.message",
                &txn,
            ],
        )?;
        // Plain `body` for every client, rendered HTML for the ones that
        // read `formatted_body` (same renderer as Telegram and the web).
        let html = ka_agent_core::markdown::render(text, ka_agent_core::markdown::Target::Html);
        self.request(
            reqwest::Method::PUT,
            url,
            Some(&json!({
                "msgtype": "m.text",
                "body": text,
                "format": "org.matrix.custom.html",
                "formatted_body": html,
            })),
        )
        .await
        .map(|_| ())
    }

    async fn send_split(&self, room: &str, text: &str) {
        for chunk in split_reply(text) {
            if let Err(e) = self.send(room, &chunk).await {
                eprintln!("matrix: send: {e}");
            }
        }
    }

    async fn join(&self, room: &str) -> Result<(), String> {
        let url = endpoint(
            &self.base,
            &["_matrix", "client", "v3", "rooms", room, "join"],
        )?;
        self.request(reqwest::Method::POST, url, Some(&json!({})))
            .await
            .map(|_| ())
    }
}

/// Password login (setup wizard): returns the access token for `user`.
pub async fn login(homeserver: &str, user: &str, password: &str) -> Result<String, String> {
    let url = endpoint(homeserver, &["_matrix", "client", "v3", "login"])?;
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;
    let body = json!({
        "type": "m.login.password",
        "identifier": {"type": "m.id.user", "user": user},
        "password": password,
        "initial_device_display_name": "Ka Agent",
    });
    let v = send_json(http.post(url).json(&body)).await?;
    v["access_token"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "login response has no access_token".to_string())
}

/// The Matrix surface: notices, answers and approval cards go to the room
/// the operator last wrote from (one operator per instance). In-turn
/// progress notes are never sent (the journal keeps them).
pub struct MatrixSurface {
    client: Arc<Client>,
    room: Arc<Mutex<Option<String>>>,
    room_known: Arc<Notify>,
    tx: mpsc::UnboundedSender<String>,
    rx: Mutex<Option<mpsc::UnboundedReceiver<String>>>,
}

impl MatrixSurface {
    pub fn new(cfg: MatrixConfig, initial_room: Option<String>) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            client: Arc::new(Client::new(cfg.homeserver, cfg.token)),
            room: Arc::new(Mutex::new(initial_room.filter(|r| !r.is_empty()))),
            room_known: Arc::new(Notify::new()),
            tx,
            rx: Mutex::new(Some(rx)),
        }
    }

    /// Take the outbound queue (once): the channel's sender task drains it,
    /// keeping sends on one task and in order.
    pub fn take_receiver(&self) -> Option<mpsc::UnboundedReceiver<String>> {
        self.rx.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    /// Set the delivery room; returns whether it changed.
    fn set_room(&self, room: &str) -> bool {
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

    fn enqueue(&self, text: &str) {
        let _ = self.tx.send(text.to_string());
    }
}

fn split_reply(text: &str) -> Vec<String> {
    if text.len() <= REPLY_CAP {
        return vec![text.to_string()];
    }
    let mut out = Vec::new();
    let mut rest = text;
    while rest.len() > REPLY_CAP {
        let capped = ka_agent_core::truncate_at_char(rest, REPLY_CAP);
        let cut = capped.rfind('\n').map(|i| i + 1).unwrap_or(capped.len());
        out.push(rest[..cut].to_string());
        rest = &rest[cut..];
    }
    if !rest.is_empty() {
        out.push(rest.to_string());
    }
    out
}

impl Surface for MatrixSurface {
    /// In-turn progress: never sent (no message edits on Matrix).
    fn on_note(&self, _text: &str) {}

    fn on_notice(&self, text: &str) {
        self.enqueue(text);
    }

    fn on_approval(&self, card: &ApprovalCard) {
        let left = (card.expires_at_ms - ka_agent_core::now_ms()).max(0);
        let text = format!(
            "approval {} — {}\n\nexpires in {}\n/y to allow · /n to deny",
            ka_agent_core::ids::short_id(&card.approval_id),
            card.question,
            commands::fmt_interval(left)
        );
        self.enqueue(&text);
    }

    fn on_approval_settled(&self, approval_id: &str, status: ApprovalStatus) {
        self.enqueue(&format!(
            "approval {} → {status:?}",
            ka_agent_core::ids::short_id(approval_id)
        ));
    }

    fn on_final_chunk(&self, _index: u32, text: &str) {
        self.enqueue(text);
    }

    fn on_error(&self, class: &str, message: &str) {
        self.enqueue(&format!("! [{class}] {message}"));
    }

    /// Cost and token counts live in `/status`; nothing is posted.
    fn on_turn_finished(&self, _stop: &str, _cost_usd: f64, _tokens: u64) {}

    /// Session state changes are not posted (pauses arrive as notices).
    fn on_session_status(&self, _status: ka_agent_core::SessionStatus) {}
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn room_ids_are_percent_encoded() {
        let url = endpoint(
            "https://matrix.example.org/",
            &["_matrix", "client", "v3", "rooms", "!a/b?c:server", "join"],
        )
        .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            url.as_str(),
            "https://matrix.example.org/_matrix/client/v3/rooms/!a%2Fb%3Fc:server/join"
        );
    }
}
