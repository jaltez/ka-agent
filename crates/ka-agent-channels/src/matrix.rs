//! The Matrix channel (SEED.md §10 Phase 6): the second adapter proving the
//! channel trait. Client-Server API v3 subset — `GET /sync` long-poll in,
//! `PUT …/send` out — against one homeserver with one bot account.
//!
//! Identity: inbound senders gate on the store's `identities` table as
//! `matrix:<user_id>` (§8.1); anything else is audited and ignored.
//! Approvals are text commands (`/y`, `/n`) through the shared router —
//! the documented channel feature mismatch (no inline keyboards).
//!
//! Delivery is at-least-once with `next_batch` dedupe, mirroring the
//! Telegram offset: events are dispatched first, then `matrix.next_batch`
//! is persisted; a crash between the two replays the event.

use crate::channel::Channel;
use crate::commands::{self, Outcome};
use ka_agent_core::ApprovalStatus;
use ka_agent_core::AuditRecord;
use ka_agent_runtime::Runtime;
use ka_agent_runtime::surface::{ApprovalCard, Surface};
use ka_agent_store::Store;
use serde_json::{Value, json};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::watch;

/// Long-poll window for `/sync` (the daemon's shutdown latency bound).
const SYNC_TIMEOUT_SECS: u64 = 25;
/// Matrix event bodies cap far above this; keep replies short and split.
const REPLY_CAP: usize = 3500;
const NEXT_BATCH_KEY: &str = "matrix.next_batch";

pub struct MatrixConfig {
    pub homeserver: String,
    pub user_id: String,
    pub token: String,
}

/// A tiny CS-API client over raw TLS-less HTTP/1.1 (tests and loopback
/// homeservers); production homeservers sit behind the operator's reverse
/// proxy or localhost dendrite/synapse. Keep-alive per call.
struct Client {
    base: String,
    token: String,
    txn: AtomicU64,
}

impl Client {
    fn new(base: String, token: String) -> Self {
        Self {
            base,
            token,
            txn: AtomicU64::new(1),
        }
    }

    async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, String> {
        // base must carry a host:port for TcpStream; https is not supported
        // by this client — document that homeserver must be local or
        // terminated.
        let authority = self
            .base
            .trim_start_matches("http://")
            .trim_end_matches('/')
            .to_string();
        let payload = body.map(|b| b.to_string()).unwrap_or_default();
        let mut req = format!(
            "{method} {path} HTTP/1.1\r\nHost: {authority}\r\nAuthorization: Bearer {}\r\n\
             Accept: application/json\r\nConnection: close\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\n\r\n",
            self.token,
            payload.len()
        );
        req.push_str(&payload);
        let mut stream = TcpStream::connect(&authority)
            .await
            .map_err(|e| e.to_string())?;
        stream
            .write_all(req.as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        let mut buf = Vec::new();
        stream
            .read_to_end(&mut buf)
            .await
            .map_err(|e| e.to_string())?;
        let split = buf
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .ok_or("no response head")?;
        let head = String::from_utf8_lossy(&buf[..split]).into_owned();
        let status = head
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or(0);
        let body = String::from_utf8_lossy(&buf[split + 4..]).into_owned();
        if !(200..300).contains(&status) {
            return Err(format!(
                "HTTP {status}: {}",
                body.chars().take(200).collect::<String>()
            ));
        }
        serde_json::from_str(&body).map_err(|e| format!("bad json: {e}"))
    }

    async fn sync(&self, since: Option<&str>) -> Result<Value, String> {
        let mut path = format!(
            "/_matrix/client/v3/sync?timeout={SYNC_TIMEOUT_SECS}\
             &filter={{\"room\":{{\"timeline\":{{\"limit\":50}}}}}}"
        );
        if let Some(s) = since {
            path.push_str(&format!("&since={s}"));
        }
        self.request("GET", &path, None).await
    }

    async fn send(&self, room: &str, text: &str) -> Result<(), String> {
        let txn = self.txn.fetch_add(1, Ordering::Relaxed);
        let path = format!("/_matrix/client/v3/rooms/{room}/send/m.room.message/kagent-{txn}");
        self.request(
            "PUT",
            &path,
            Some(&json!({"msgtype": "m.text", "body": text})),
        )
        .await
        .map(|_| ())
    }
}

/// The Matrix surface: outbound runtime notes, answers and approval cards go
/// to the room the operator last wrote from (one operator per instance).
pub struct MatrixSurface {
    client: Arc<Client>,
    room: Mutex<Option<String>>,
    tx: tokio::sync::mpsc::UnboundedSender<String>,
    rx: Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<String>>>,
}

impl MatrixSurface {
    pub fn new(cfg: MatrixConfig) -> Self {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            client: Arc::new(Client::new(cfg.homeserver, cfg.token)),
            room: Mutex::new(None),
            tx,
            rx: Mutex::new(Some(rx)),
        }
    }

    /// Take the outbound queue (once): the channel loop drains it, keeping
    /// sends on one task and in order.
    pub fn take_receiver(&self) -> Option<tokio::sync::mpsc::UnboundedReceiver<String>> {
        self.rx.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    fn set_room(&self, room: &str) {
        *self.room.lock().unwrap_or_else(|e| e.into_inner()) = Some(room.to_string());
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
    fn on_note(&self, text: &str) {
        self.enqueue(text);
    }

    fn on_approval(&self, card: &ApprovalCard) {
        let mins = card.expires_at_ms / 60_000;
        let local = mins.rem_euclid(24 * 60);
        let text = format!(
            "approval {} (expires {:02}:{:02})\n{}\n\n/y to allow · /n to deny",
            card.approval_id,
            local / 60,
            local % 60,
            card.question
        );
        self.enqueue(&text);
    }

    fn on_approval_settled(&self, approval_id: &str, status: ApprovalStatus) {
        self.enqueue(&format!("approval {approval_id} → {status:?}"));
    }

    fn on_final_chunk(&self, _index: u32, text: &str) {
        self.enqueue(text);
    }

    fn on_error(&self, class: &str, message: &str) {
        self.enqueue(&format!("! [{class}] {message}"));
    }

    fn on_turn_finished(&self, stop: &str, cost_usd: f64, tokens: u64) {
        self.enqueue(&format!("turn {stop} · ${cost_usd:.4} · {tokens} tokens"));
    }

    fn on_session_status(&self, status: ka_agent_core::SessionStatus) {
        self.enqueue(&format!("session {status:?}"));
    }
}

pub struct MatrixChannel {
    cfg: MatrixConfig,
    runtime: Runtime,
    store: Arc<Store>,
    surface: Arc<MatrixSurface>,
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
        }
    }

    /// One joined room's timeline: dispatch each `m.room.message`.
    async fn handle_event(&self, room: &str, event: &Value) {
        if event["type"].as_str() != Some("m.room.message") {
            return;
        }
        let sender = event["sender"].as_str().unwrap_or("");
        if sender == self.cfg.user_id || sender.is_empty() {
            return;
        }
        let body = event["content"]["body"].as_str().unwrap_or("").trim();
        if body.is_empty() {
            return;
        }
        // §8.1: linked identities only; everyone else is audited, ignored,
        // never answered.
        if !self.store.is_linked("matrix", sender).unwrap_or(false) {
            eprintln!("matrix: unauthorized sender {sender}");
            let mut record = AuditRecord::new(
                &format!("matrix:{sender}"),
                "matrix.unauthorized",
                "",
                Some(format!("room={room}")),
            );
            let _ = self.store.append_audit(&mut record);
            return;
        }
        self.surface.set_room(room);
        match commands::route(&self.runtime, &format!("matrix:{sender}"), body).await {
            Outcome::Reply(reply) => {
                for chunk in split_reply(&reply) {
                    if let Err(e) = self.surface.client.send(room, &chunk).await {
                        eprintln!("matrix: send: {e}");
                    }
                }
            }
            Outcome::Silent => {}
            Outcome::NotACommand => {
                if let Err(e) = self
                    .runtime
                    .handle_message(body, &format!("matrix:{sender}"))
                    .await
                {
                    eprintln!("matrix: handle_message: {e}");
                }
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
            let mut since = self.store.meta_get(NEXT_BATCH_KEY).ok().flatten();
            let mut shutdown = shutdown;
            let Some(mut outbound) = self.surface.take_receiver() else {
                eprintln!("matrix: outbound queue already taken; not starting");
                return;
            };
            // Crash recovery (SEED §3.7): undelivered chunks go out before
            // any new traffic, once a room is known (the first inbound sets
            // it; until then they stay queued, not dropped).
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
            eprintln!(
                "matrix: syncing as {} (since {:?})",
                self.cfg.user_id, since
            );
            loop {
                // Outbound first: a pending runtime reply outranks the next
                // long-poll, and sends stay strictly ordered.
                while let Ok(text) = outbound.try_recv() {
                    let room = self
                        .surface
                        .room
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone();
                    if let Some(room) = room {
                        for chunk in split_reply(&text) {
                            if let Err(e) = self.surface.client.send(&room, &chunk).await {
                                eprintln!("matrix: send: {e}");
                            }
                        }
                    }
                }
                let sync = tokio::select! {
                    _ = shutdown.changed() => break,
                    r = self.surface.client.sync(since.as_deref()) => r,
                };
                match sync {
                    Ok(value) => {
                        if let Some(next) = value["next_batch"].as_str() {
                            since = Some(next.to_string());
                        }
                        let rooms = value["rooms"]["join"]
                            .as_object()
                            .cloned()
                            .unwrap_or_default();
                        for (room, state) in rooms {
                            let events = state["timeline"]["events"]
                                .as_array()
                                .cloned()
                                .unwrap_or_default();
                            for event in &events {
                                self.handle_event(&room, event).await;
                            }
                        }
                        // Persist only after dispatch (at-least-once).
                        if let Some(next) = &since {
                            if let Err(e) = self.store.meta_set(NEXT_BATCH_KEY, next) {
                                eprintln!("matrix: persist next_batch: {e}");
                            }
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
            eprintln!("matrix: channel stopped");
        })
    }
}
