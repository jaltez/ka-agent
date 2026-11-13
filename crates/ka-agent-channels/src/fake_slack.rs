#![allow(clippy::unwrap_used, clippy::expect_used)]

//! An in-process fake Slack: the Web API (JSON over HTTP) plus one fake
//! Socket Mode endpoint (WSS) for tests. Hand-rolled on a tokio
//! `TcpListener`: a request whose head asks for a WebSocket upgrade becomes
//! the socket; everything else is answered from the API table.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

/// One recorded Web API call.
#[derive(Debug, Clone)]
pub struct SlackCall {
    pub method: String,
    pub body: Value,
}

#[derive(Default)]
struct State {
    calls: Vec<SlackCall>,
    acks: Vec<String>,
    /// Frames the connector should receive, in order.
    outbound: VecDeque<String>,
    /// Channel to push new frames while a socket is connected.
    tx: Option<mpsc::UnboundedSender<Message>>,
    connected: bool,
    /// Connections opened so far (a reconnect bumps it).
    sessions: usize,
    /// This fake's own `ws://host:port/socket` URL.
    socket_url: String,
}

/// The fake. Cloneable; every clone shares the state.
#[derive(Clone)]
pub struct FakeSlack {
    state: Arc<Mutex<State>>,
}

impl FakeSlack {
    pub fn spawn() -> (Self, String) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let state = Arc::new(Mutex::new(State {
            socket_url: format!("ws://{addr}/socket"),
            ..State::default()
        }));
        let fake = Self {
            state: Arc::clone(&state),
        };
        let rt_state = Arc::clone(&state);
        tokio::spawn(async move {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    continue;
                };
                let st = Arc::clone(&rt_state);
                tokio::spawn(async move {
                    let _ = serve(socket, st).await;
                });
            }
        });
        (fake, format!("http://{addr}"))
    }

    /// Wait until the connector opened its socket.
    pub async fn wait_socket(&self) {
        for _ in 0..600 {
            if self
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .connected
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("the connector never opened the socket");
    }

    /// Push one JSON frame to the connector.
    pub fn send(&self, frame: Value) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let text = frame.to_string();
        if let Some(tx) = &st.tx
            && tx.send(Message::Text(text.clone().into())).is_ok()
        {
            return;
        }
        st.outbound.push_back(text);
    }

    /// Close the socket (the connector must reconnect).
    pub fn drop_socket(&self) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(tx) = st.tx.take() {
            let _ = tx.send(Message::Close(None));
        }
        st.connected = false;
    }

    /// Envelope ids the connector acked, in order.
    pub fn acks(&self) -> Vec<String> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .acks
            .clone()
    }

    /// Web API calls matching `method`.
    pub fn calls(&self, method: &str) -> Vec<SlackCall> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .calls
            .iter()
            .filter(|c| c.method == method)
            .cloned()
            .collect()
    }

    /// How many sockets were opened.
    pub fn sessions(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .sessions
    }
}

/// A stream that yields the bytes already read (`head`) before continuing
/// with the socket: the WebSocket handshake parser needs the request line
/// this listener consumed to decide the request's kind.
struct Replay {
    head: std::io::Cursor<Vec<u8>>,
    inner: tokio::net::TcpStream,
}

impl Replay {
    fn new(head: Vec<u8>, inner: tokio::net::TcpStream) -> Self {
        Self {
            head: std::io::Cursor::new(head),
            inner,
        }
    }
}

impl tokio::io::AsyncRead for Replay {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let remaining = self.head.get_ref().len() as u64 - self.head.position();
        if remaining > 0 {
            let take = remaining.min(buf.remaining() as u64) as usize;
            let mut tmp = vec![0u8; take];
            std::io::Read::read_exact(&mut self.head, &mut tmp)?;
            buf.put_slice(&tmp);
            return std::task::Poll::Ready(Ok(()));
        }
        std::pin::Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl tokio::io::AsyncWrite for Replay {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// One connection: decide WebSocket vs HTTP from the request head.
async fn serve(mut socket: tokio::net::TcpStream, state: Arc<Mutex<State>>) -> std::io::Result<()> {
    let mut head = Vec::new();
    let mut buf = [0u8; 1024];
    loop {
        let n = socket.read(&mut buf).await?;
        if n == 0 {
            return Ok(());
        }
        head.extend_from_slice(&buf[..n]);
        if head.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if head.len() > 64 * 1024 {
            return Ok(());
        }
    }
    let text = String::from_utf8_lossy(&head).to_ascii_lowercase();
    if text.contains("upgrade: websocket") {
        // Hand the already-buffered bytes back to the WS layer.
        let mut prefixed = Replay::new(head, socket);
        return run_socket(&mut prefixed, state).await;
    }
    // Plain HTTP: one request per connection. Read the declared body (the
    // head alone carries no JSON).
    let content_length = text
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let head_len = head
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4)
        .unwrap_or(head.len());
    let mut body_bytes = head[head_len..].to_vec();
    while body_bytes.len() < content_length {
        let mut chunk = vec![0u8; content_length - body_bytes.len()];
        let n = socket.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        body_bytes.extend_from_slice(&chunk[..n]);
    }
    let mut full = head[..head_len].to_vec();
    full.extend_from_slice(&body_bytes);
    let original = String::from_utf8_lossy(&head);
    let (method, body) = parse_api_request(&original, &full);
    let response = api_response(&method, &body, &state);
    let payload = response.to_string();
    let reply = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    );
    socket.write_all(reply.as_bytes()).await?;
    Ok(())
}

async fn run_socket<S>(stream: &mut S, state: Arc<Mutex<State>>) -> std::io::Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let ws = match tokio_tungstenite::accept_async(stream).await {
        Ok(ws) => ws,
        Err(_) => return Ok(()),
    };
    let (mut sink, mut source) = ws.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
    let pending: Vec<String> = {
        let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
        st.connected = true;
        st.sessions += 1;
        st.tx = Some(tx.clone());
        st.outbound.drain(..).collect()
    };
    // The socket says hello first, like Slack's own.
    if sink.send(Message::Text(json!({"type": "hello"}).to_string().into())).await.is_err() {
        return Ok(());
    }
    for frame in pending {
        let _ = tx.send(Message::Text(frame.into()));
    }
    loop {
        tokio::select! {
            out = rx.recv() => {
                match out {
                    Some(msg) => {
                        if sink.send(msg).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                }
            }
            incoming = futures_util::StreamExt::next(&mut source) => {
                let Some(Ok(msg)) = incoming else { break };
                if let Message::Text(text) = msg
                    && let Ok(value) = serde_json::from_str::<Value>(&text)
                        && let Some(id) = value["envelope_id"].as_str() {
                            let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
                            st.acks.push(id.to_string());
                        }
            }
        }
    }
    let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
    st.connected = false;
    st.tx = None;
    Ok(())
}

/// `POST /api/<method>` with a JSON body (or the form token). The method is
/// case-sensitive (`chat.postMessage`), so the head is read verbatim.
fn parse_api_request(head: &str, raw: &[u8]) -> (String, Value) {
    let method = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|path| path.strip_prefix("/api/"))
        .unwrap_or("")
        .to_string();
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4)
        .unwrap_or(raw.len());
    let body = String::from_utf8_lossy(&raw[split..]).to_string();
    let value = serde_json::from_str::<Value>(&body).unwrap_or(Value::Null);
    (method, value)
}

fn api_response(method: &str, body: &Value, state: &Arc<Mutex<State>>) -> Value {
    let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
    if method != "apps.connections.open" {
        st.calls.push(SlackCall {
            method: method.to_string(),
            body: body.clone(),
        });
    }
    match method {
        "apps.connections.open" => json!({"ok": true, "url": st.socket_url.clone()}),
        "auth.test" => json!({"ok": true, "user_id": "UBOT", "team_id": "T1"}),
        "chat.postMessage" => json!({"ok": true, "ts": "1700000000.000100", "channel": body["channel"]}),
        "chat.update" => json!({"ok": true, "ts": body["ts"]}),
        "chat.postEphemeral" => json!({"ok": true}),
        "files.info" => json!({"ok": true, "file": {"id": body["file"], "size": 4,
            "url_private_download": "http://example.invalid/file"}}),
        "files.getUploadURLExternal" => {
            json!({"ok": true, "upload_url": "http://example.invalid/upload", "file_id": "F1"})
        }
        "files.completeUploadExternal" => json!({"ok": true}),
        other => json!({"ok": false, "error": format!("unknown_method:{other}")}),
    }
}
