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
                        let response = handle(&state, &path, &body);
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
                "from": { "id": user_id, "is_bot": false },
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
        "getMe" => json!({ "id": 1, "is_bot": true, "first_name": "fake" }),
        _ => Value::Bool(true),
    };
    st.captured.push(CapturedRequest {
        method,
        body: body.clone(),
    });
    json!({ "ok": true, "result": result }).to_string()
}
