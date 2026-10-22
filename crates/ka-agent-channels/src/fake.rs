//! An in-process fake Telegram Bot API server for tests and app-level e2e.
//! Hand-rolled HTTP/1.1 on a tokio TcpListener: one request per connection
//! (request line + headers + Content-Length body), one JSON response.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// One recorded Bot API call (`method` is the Telegram method name).
#[derive(Debug, Clone)]
pub struct CapturedRequest {
    pub method: String,
    pub body: Value,
}

#[derive(Debug, Default)]
struct State {
    updates: Vec<Value>,
    captured: Vec<CapturedRequest>,
    next_message_id: i64,
    last_offset: Option<i64>,
    /// Downloadable files: (file_id, file_path, bytes) — what `getFile`
    /// resolves and `/file/bot<token>/<file_path>` serves.
    files: Vec<(String, String, Vec<u8>)>,
    /// When set, any request carrying `parse_mode` is refused the way
    /// Telegram refuses malformed HTML.
    reject_html: bool,
    /// When set, `sendMessageDraft` fails like a Bot API server that
    /// predates it.
    reject_drafts: bool,
}

/// A fake `api.telegram.org`. Spawn on an ephemeral local port and point
/// `TelegramConfig::api_base` at the returned base URL.
#[derive(Debug, Clone)]
pub struct FakeBotServer {
    state: Arc<Mutex<State>>,
}

impl FakeBotServer {
    /// Bind `127.0.0.1:0` and start serving. Returns the server and its
    /// `http://127.0.0.1:<port>` base URL.
    pub fn spawn() -> (Self, String) {
        let state = Arc::new(Mutex::new(State {
            next_message_id: 1,
            ..State::default()
        }));
        let server = Self {
            state: Arc::clone(&state),
        };
        let rt_state = Arc::clone(&state);
        // Bind synchronously (spawn() is called from anywhere); hand the
        // socket to tokio inside the spawned accept loop.
        let std_listener = match std::net::TcpListener::bind("127.0.0.1:0") {
            Ok(l) => l,
            Err(e) => {
                // an ephemeral-port bind never legitimately fails
                eprintln!("fake bot server: bind failed: {e}");
                std::process::exit(1);
            }
        };
        let addr = std_listener
            .local_addr()
            .map(|a| a.to_string())
            .unwrap_or_else(|_| "127.0.0.1:0".to_string());
        tokio::spawn(async move {
            let _ = std_listener.set_nonblocking(true);
            let Ok(listener) = tokio::net::TcpListener::from_std(std_listener) else {
                return;
            };
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    continue;
                };
                let state = Arc::clone(&rt_state);
                tokio::spawn(async move {
                    if let Some((path, body)) = read_request(&mut socket).await {
                        // File downloads bypass the JSON method table.
                        let (status, content_type, response): (&str, &str, Vec<u8>) =
                            if let Some(rest) = path.strip_prefix("/file/") {
                                // /file/bot<token>/<file_path> — match the
                                // full API-relative file_path, not a basename.
                                let file_path = rest.split_once('/').map_or("", |x| x.1);
                                let bytes = {
                                    let st = state.lock().unwrap_or_else(|e| e.into_inner());
                                    st.files
                                        .iter()
                                        .find(|(_, p, _)| p == file_path)
                                        .map(|(_, _, b)| b.clone())
                                        .unwrap_or_default()
                                };
                                let status = if bytes.is_empty() {
                                    "404 Not Found"
                                } else {
                                    "200 OK"
                                };
                                (status, "application/octet-stream", bytes)
                            } else {
                                (
                                    "200 OK",
                                    "application/json",
                                    handle(&state, &path, &body).into_bytes(),
                                )
                            };
                        let head = format!(
                            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            response.len()
                        );
                        let _ = socket.write_all(head.as_bytes()).await;
                        let _ = socket.write_all(&response).await;
                    }
                });
            }
        });
        (server, format!("http://{addr}"))
    }

    /// Queue a raw update (as `getUpdates` would return it).
    pub fn push_update(&self, update: Value) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .updates
            .push(update);
    }

    /// Queue a private-chat text message.
    pub fn push_text_message(&self, update_id: i64, chat_id: i64, user_id: i64, text: &str) {
        self.push_update(json!({
            "update_id": update_id,
            "message": {
                "message_id": 1,
                "from": { "id": user_id, "is_bot": false, "first_name": "Tester" },
                "chat": { "id": chat_id, "type": "private" },
                "date": 0,
                "text": text,
            }
        }));
    }

    /// Queue a callback query answering a card in `chat_id`.
    pub fn push_callback(&self, update_id: i64, chat_id: i64, user_id: i64, callback_data: &str) {
        self.push_update(json!({
            "update_id": update_id,
            "callback_query": {
                "id": format!("cq{update_id}"),
                "from": { "id": user_id, "is_bot": false },
                "message": { "message_id": 1, "chat": { "id": chat_id, "type": "private" } },
                "data": callback_data,
            }
        }));
    }

    /// All Bot API calls seen so far, in order.
    pub fn captured(&self) -> Vec<CapturedRequest> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .captured
            .clone()
    }

    /// Serve a downloadable file: `getFile` resolves `file_id` to
    /// `file_path`, and `GET <base>/file/bot<token>/<file_path>` returns
    /// `bytes`.
    pub fn push_file(&self, file_id: &str, file_path: &str, bytes: &[u8]) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .files
            .push((file_id.to_string(), file_path.to_string(), bytes.to_vec()));
    }

    /// Queue a private-chat photo message (size-ascending variants, as the
    /// Bot API sends them).
    pub fn push_photo_message(&self, update_id: i64, chat_id: i64, user_id: i64, file_id: &str) {
        self.push_update(json!({
            "update_id": update_id,
            "message": {
                "message_id": 1,
                "from": { "id": user_id, "is_bot": false },
                "chat": { "id": chat_id, "type": "private" },
                "date": 0,
                "photo": [
                    { "file_id": format!("{file_id}-small"), "width": 90, "height": 90 },
                    { "file_id": file_id, "width": 1280, "height": 720 },
                ],
            }
        }));
    }

    /// Queue a private-chat document message.
    pub fn push_document_message(
        &self,
        update_id: i64,
        chat_id: i64,
        user_id: i64,
        file_id: &str,
        file_name: &str,
    ) {
        self.push_update(json!({
            "update_id": update_id,
            "message": {
                "message_id": 1,
                "from": { "id": user_id, "is_bot": false },
                "chat": { "id": chat_id, "type": "private" },
                "date": 0,
                "document": { "file_id": file_id, "file_name": file_name },
            }
        }));
    }

    /// Poll (50 ms) until a capture matching `pred` lands, or `timeout`.
    pub fn await_capture(
        &self,
        pred: impl Fn(&CapturedRequest) -> bool + Send,
        timeout: Duration,
    ) -> Option<CapturedRequest> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let found = self
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .captured
                .iter()
                .find(|c| pred(c))
                .cloned();
            if let Some(hit) = found {
                return Some(hit);
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Max `offset` seen in any `getUpdates` body.
    pub fn last_offset(&self) -> Option<i64> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .last_offset
    }

    /// Refuse every request that sets `parse_mode` with
    /// `Bad Request: can't parse entities` (the request is still captured).
    pub fn set_reject_html(&self, on: bool) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .reject_html = on;
    }

    /// Fail `sendMessageDraft` like a Bot API server that predates it.
    pub fn set_reject_drafts(&self, on: bool) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .reject_drafts = on;
    }
}

/// Read one request: request line + headers, then Content-Length body.
async fn read_request(socket: &mut tokio::net::TcpStream) -> Option<(String, Value)> {
    const MAX_REQUEST: usize = 1024 * 1024;
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    let head_end;
    loop {
        if buf.len() > MAX_REQUEST {
            return None;
        }
        let n = socket.read(&mut tmp).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
        let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n").or_else(|| {
            buf.windows(2)
                .position(|w| w == b"\n\n")
                .map(|p| p.saturating_sub(2))
        }) else {
            continue;
        };
        head_end = end;
        break;
    }
    let sep_len = if buf.len() >= head_end + 4 && &buf[head_end..head_end + 4] == b"\r\n\r\n" {
        4
    } else {
        2
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let content_length: usize = head
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse().ok())
                .flatten()
        })
        .unwrap_or(0);
    while buf.len() < head_end + sep_len + content_length {
        let n = socket.read(&mut tmp).await.ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    let body_bytes = buf.get(head_end + sep_len..)?.to_vec();
    let request_line = head.lines().next()?;
    let path = request_line.split_whitespace().nth(1)?.to_string();
    let body: Value = serde_json::from_slice(&body_bytes).unwrap_or(Value::Null);
    Some((path, body))
}

fn handle(state: &Arc<Mutex<State>>, path: &str, body: &Value) -> String {
    // path: /bot<token>/<method>
    let method = path.rsplit('/').next().unwrap_or("").to_string();
    let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
    if st.reject_html && body.get("parse_mode").is_some() {
        st.captured.push(CapturedRequest {
            method,
            body: body.clone(),
        });
        return json!({ "ok": false, "description": "Bad Request: can't parse entities" })
            .to_string();
    }
    if st.reject_drafts && method == "sendMessageDraft" {
        st.captured.push(CapturedRequest {
            method,
            body: body.clone(),
        });
        return json!({ "ok": false, "description": "Not Found: method not found" }).to_string();
    }
    let result = match method.as_str() {
        "getUpdates" => {
            let offset = body["offset"].as_i64().unwrap_or(0);
            st.last_offset = Some(st.last_offset.map_or(offset, |prev| prev.max(offset)));
            Value::Array(
                st.updates
                    .iter()
                    .filter(|u| u["update_id"].as_i64().unwrap_or(0) >= offset)
                    .cloned()
                    .collect(),
            )
        }
        "sendMessage" => {
            let id = st.next_message_id;
            st.next_message_id += 1;
            json!({ "message_id": id })
        }
        "editMessageText" => Value::Bool(true),
        "answerCallbackQuery" => Value::Bool(true),
        "sendChatAction" => Value::Bool(true),
        "getMe" => json!({ "id": 1, "is_bot": true, "first_name": "fake", "username": "fake_bot" }),
        "getFile" => {
            let file_id = body["file_id"].as_str().unwrap_or("");
            st.files
                .iter()
                .find(|(id, _, _)| id == file_id)
                .map(|(_, path, _)| json!({ "file_id": file_id, "file_path": path }))
                .unwrap_or(Value::Null)
        }
        _ => Value::Bool(true),
    };
    st.captured.push(CapturedRequest {
        method,
        body: body.clone(),
    });
    json!({ "ok": true, "result": result }).to_string()
}

// ---------------------------------------------------------------------------
// Fake Matrix homeserver (Phase 6): the CS-API subset the channel uses —
// `GET /sync` (returns queued events once per `since`, then long-poll
// empties) and `PUT …/send` (records outbound bodies).
// ---------------------------------------------------------------------------

#[derive(Default)]
struct MatrixState {
    /// The room timeline the server is holding, FIFO: every sync delivers
    /// whatever arrived since the last one served (a homeserver queues
    /// server-side; tokens do not "attach" to events).
    timeline: Vec<(String, Value)>,
    next_batch: u64,
    since_seen: Vec<Option<String>>,
    sent: Vec<CapturedRequest>,
    /// Pending invites `(room, sender)`, emitted under `rooms.invite`.
    invites: Vec<(String, String)>,
    /// Rooms joined via `POST …/rooms/{room}/join` (decoded ids).
    joins: Vec<String>,
    /// `/sync` requests currently held open (long-poll).
    syncs_pending: usize,
}

/// The bot account invites are addressed to (`state_key`); configure the
/// channel's `user_id` with it.
pub const FAKE_MATRIX_BOT: &str = "@ka-agent:example.org";

/// Longest the fake holds an empty `/sync` open (the client asks for more).
const FAKE_SYNC_HOLD_MS: u64 = 5_000;

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn query_param(path: &str, key: &str) -> Option<String> {
    let query = path.split_once('?')?.1;
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then(|| percent_decode(v))
    })
}

/// Serve one `/sync`: long-poll until something is queued or the hold ends.
async fn matrix_sync(state: &Arc<Mutex<MatrixState>>, path: &str) -> String {
    let wait_ms = query_param(path, "timeout")
        .and_then(|t| t.parse::<u64>().ok())
        .unwrap_or(0)
        .min(FAKE_SYNC_HOLD_MS);
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(wait_ms);
    {
        let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
        st.since_seen.push(query_param(path, "since"));
        st.syncs_pending += 1;
    }
    loop {
        {
            let st = state.lock().unwrap_or_else(|e| e.into_inner());
            if !st.timeline.is_empty() || !st.invites.is_empty() {
                break;
            }
        }
        if std::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
    st.syncs_pending = st.syncs_pending.saturating_sub(1);
    // Deliver everything that arrived since the last sync served
    // (server-side FIFO timeline; tokens do not attach to events).
    let events: Vec<(String, Value)> = std::mem::take(&mut st.timeline);
    let invites: Vec<(String, String)> = std::mem::take(&mut st.invites);
    st.next_batch += 1;
    let next = format!("s{}", st.next_batch);
    let mut rooms = serde_json::Map::new();
    for (room, event) in events {
        if let Some(arr) = rooms
            .entry(room)
            .or_insert_with(|| serde_json::json!({"timeline": {"events": []}}))["timeline"]
            ["events"]
            .as_array_mut()
        {
            arr.push(event);
        }
    }
    let mut invite = serde_json::Map::new();
    for (room, sender) in invites {
        invite.insert(
            room,
            serde_json::json!({"invite_state": {"events": [{
                "type": "m.room.member",
                "sender": sender,
                "state_key": FAKE_MATRIX_BOT,
                "content": {"membership": "invite"},
            }]}}),
        );
    }
    serde_json::json!({
        "next_batch": next,
        "rooms": {"join": rooms, "invite": invite, "leave": {}},
    })
    .to_string()
}

pub struct FakeHomeserver {
    state: Arc<Mutex<MatrixState>>,
}

impl FakeHomeserver {
    pub fn spawn() -> (Self, String) {
        let state = Arc::new(Mutex::new(MatrixState::default()));
        let server = Self {
            state: Arc::clone(&state),
        };
        let rt_state = Arc::clone(&state);
        let Ok(std_listener) = std::net::TcpListener::bind("127.0.0.1:0") else {
            eprintln!("fake homeserver: bind failed");
            std::process::exit(1);
        };
        let addr = std_listener
            .local_addr()
            .map(|a| a.to_string())
            .unwrap_or_else(|_| "127.0.0.1:0".to_string());
        tokio::spawn(async move {
            let _ = std_listener.set_nonblocking(true);
            let Ok(listener) = tokio::net::TcpListener::from_std(std_listener) else {
                return;
            };
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    continue;
                };
                let state = Arc::clone(&rt_state);
                tokio::spawn(async move {
                    if let Some((path, body)) = read_request(&mut socket).await {
                        let response = if path.starts_with("/_matrix/client/v3/sync") {
                            matrix_sync(&state, &path).await
                        } else {
                            matrix_handle(&state, &path, &body)
                        };
                        let _ = socket
                            .write_all(
                                format!(
                                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                                    response.len()
                                )
                                .as_bytes(),
                            )
                            .await;
                    }
                });
            }
        });
        (server, format!("http://{addr}"))
    }

    /// Queue one room message from `sender`: before the first sync it waits
    /// with the initial batch; afterwards it waits for the token the client
    /// will sync with next (the latest `next_batch` handed out).
    pub fn push_room_message(&self, room: &str, sender: &str, body: &str) {
        let event = serde_json::json!({
            "type": "m.room.message",
            "sender": sender,
            "content": {"msgtype": "m.text", "body": body},
        });
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .timeline
            .push((room.to_string(), event));
    }

    /// Queue a raw timeline event (e.g. with `origin_server_ts`, or an
    /// `m.room.encrypted` event).
    pub fn push_room_event(&self, room: &str, event: Value) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .timeline
            .push((room.to_string(), event));
    }

    /// Queue an invite of the bot to `room` from `sender`, emitted under
    /// `rooms.invite` on the next sync.
    pub fn push_invite(&self, room: &str, sender: &str) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .invites
            .push((room.to_string(), sender.to_string()));
    }

    /// Rooms the client joined, in order (decoded room ids).
    pub fn joins(&self) -> Vec<String> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .joins
            .clone()
    }

    /// Whether a `/sync` long-poll is currently held open.
    pub fn sync_pending(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .syncs_pending
            > 0
    }

    /// Wait until the homeserver saw a send with this text fragment.
    pub fn await_sent(
        &self,
        needle: &str,
        timeout: std::time::Duration,
    ) -> Option<CapturedRequest> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let hit = self
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .sent
                .iter()
                .find(|c| c.body["body"].as_str().is_some_and(|t| t.contains(needle)))
                .cloned();
            if let Some(hit) = hit {
                return Some(hit);
            }
            if std::time::Instant::now() > deadline {
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// Every recorded send, oldest first (test diagnostics).
    pub fn sent_all(&self) -> Vec<String> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .sent
            .iter()
            .map(|c| c.body["body"].as_str().unwrap_or("<no body>").to_string())
            .collect()
    }

    /// Forget the sync history (between daemon restarts in a test).
    pub fn reset_since_history(&self) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .since_seen
            .clear();
    }

    /// Every `since` token the channel has synced with, in order.
    pub fn since_history(&self) -> Vec<Option<String>> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .since_seen
            .clone()
    }
}

fn matrix_handle(state: &Arc<Mutex<MatrixState>>, path: &str, body: &Value) -> String {
    let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
    if path.contains("/send/m.room.message/") {
        st.sent.push(CapturedRequest {
            method: "PUT".into(),
            body: body.clone(),
        });
        serde_json::json!({"event_id": format!("$k{}", st.sent.len())}).to_string()
    } else if let Some(room) = path
        .strip_prefix("/_matrix/client/v3/rooms/")
        .and_then(|r| r.strip_suffix("/join"))
    {
        st.joins.push(percent_decode(room));
        serde_json::json!({"room_id": percent_decode(room)}).to_string()
    } else {
        serde_json::json!({}).to_string()
    }
}
