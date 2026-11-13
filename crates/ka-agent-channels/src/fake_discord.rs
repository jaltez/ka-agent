#![allow(clippy::unwrap_used, clippy::expect_used)]

//! An in-process fake Discord: REST (messages, interactions, gateway
//! discovery) plus one gateway WebSocket speaking Hello/Ready/Heartbeat/
//! Resume, for tests.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

/// One recorded REST call.
#[derive(Debug, Clone)]
pub struct DiscordCall {
    pub method: String,
    pub path: String,
    pub body: Value,
}

#[derive(Default)]
struct State {
    calls: Vec<DiscordCall>,
    /// Frames queued for the gateway.
    outbound: VecDeque<Value>,
    tx: Option<mpsc::UnboundedSender<Message>>,
    connected: bool,
    sessions: usize,
    /// `(op, session_id)` of every Resume received.
    resumes: Vec<(i64, String)>,
    /// The session id the fake hands out.
    session_id: String,
    hello_ms: u64,
    /// This fake's own gateway URL.
    socket_url: String,
}

#[derive(Clone)]
pub struct FakeDiscord {
    state: Arc<Mutex<State>>,
}

impl FakeDiscord {
    pub fn spawn() -> (Self, String) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let state = Arc::new(Mutex::new(State {
            session_id: "sess-1".into(),
            // A short interval keeps the tests fast; the connector paces
            // itself from Hello.
            hello_ms: 200,
            socket_url: format!("ws://{addr}/gw"),
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

    pub async fn wait_socket(&self) {
        for _ in 0..600 {
            if self.state.lock().unwrap_or_else(|e| e.into_inner()).connected {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("the connector never opened the gateway");
    }

    /// Dispatch one `MESSAGE_CREATE`.
    pub fn send_message(&self, message: Value) {
        self.dispatch("MESSAGE_CREATE", message);
    }

    /// Dispatch one `INTERACTION_CREATE`.
    pub fn send_interaction(&self, interaction: Value) {
        self.dispatch("INTERACTION_CREATE", interaction);
    }

    fn dispatch(&self, kind: &str, d: Value) {
        let frame = json!({"op": 0, "t": kind, "s": 1, "d": d});
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(tx) = &st.tx
            && tx.send(Message::Text(frame.to_string().into())).is_ok()
        {
            return;
        }
        st.outbound.push_back(frame);
    }

    /// Close the socket (the connector must resume).
    pub fn drop_socket(&self) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(tx) = st.tx.take() {
            let _ = tx.send(Message::Close(None));
        }
        st.connected = false;
    }

    pub fn calls(&self, method: &str) -> Vec<DiscordCall> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .calls
            .iter()
            .filter(|c| c.method == method)
            .cloned()
            .collect()
    }

    pub fn sessions(&self) -> usize {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).sessions
    }

    pub fn resumes(&self) -> Vec<(i64, String)> {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).resumes.clone()
    }
}

/// A stream that replays the bytes already read before the socket.
struct Replay {
    head: std::io::Cursor<Vec<u8>>,
    inner: tokio::net::TcpStream,
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
        let mut replay = Replay {
            head: std::io::Cursor::new(head),
            inner: socket,
        };
        return gateway(&mut replay, state).await;
    }
    let head_len = head
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4)
        .unwrap_or(head.len());
    let length = text
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = head[head_len..].to_vec();
    while body.len() < length {
        let mut chunk = vec![0u8; length - body.len()];
        let n = socket.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    let original = String::from_utf8_lossy(&head).into_owned();
    let (method, path) = parse_request(&original);
    let payload = api_response(&method, &path, &body, &state);
    let reply = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        payload.len(),
        payload
    );
    socket.write_all(reply.as_bytes()).await?;
    Ok(())
}

fn parse_request(head: &str) -> (String, String) {
    let mut parts = head.lines().next().unwrap_or("").split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    (method, path)
}

fn api_response(
    method: &str,
    path: &str,
    body: &[u8],
    state: &Arc<Mutex<State>>,
) -> String {
    let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
    let value: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    if path.ends_with("/gateway/bot") {
        return json!({"url": st.socket_url.clone(), "shards": 1}).to_string();
    }
    st.calls.push(DiscordCall {
        method: method.to_string(),
        path: path.to_string(),
        body: value,
    });
    json!({"id": "msg-1", "ok": true}).to_string()
}

async fn gateway<S>(stream: &mut S, state: Arc<Mutex<State>>) -> std::io::Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let ws = match tokio_tungstenite::accept_async(stream).await {
        Ok(ws) => ws,
        Err(_) => return Ok(()),
    };
    let (mut sink, mut source) = ws.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
    let hello_ms = {
        let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
        st.connected = true;
        st.sessions += 1;
        st.tx = Some(tx.clone());
        st.hello_ms
    };
    let hello = json!({"op": 10, "d": {"heartbeat_interval": hello_ms}});
    let _ = sink.send(Message::Text(hello.to_string().into())).await;
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
            incoming = source.next() => {
                let Some(Ok(msg)) = incoming else { break };
                let Message::Text(text) = msg else { continue };
                let Ok(value) = serde_json::from_str::<Value>(&text) else { continue };
                match value["op"].as_i64() {
                    Some(1) => {
                        let _ = sink.send(Message::Text(json!({"op": 11}).to_string().into())).await;
                    }
                    Some(2) => {
                        let ready = json!({
                            "op": 0, "t": "READY", "s": 1,
                            "d": {"session_id": state.lock().unwrap_or_else(|e| e.into_inner()).session_id,
                                  "resume_gateway_url": state.lock().unwrap_or_else(|e| e.into_inner()).socket_url.clone(),
                                  "user": {"id": "UBOT", "username": "ka"}}
                        });
                        let _ = sink.send(Message::Text(ready.to_string().into())).await;
                    }
                    Some(6) => {
                        let session = value["d"]["session_id"].as_str().unwrap_or("").to_string();
                        state.lock().unwrap_or_else(|e| e.into_inner()).resumes.push((6, session));
                        let resumed = json!({"op": 0, "t": "RESUMED", "s": 2, "d": {}});
                        let _ = sink.send(Message::Text(resumed.to_string().into())).await;
                    }
                    _ => {}
                }
            }
        }
    }
    let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
    st.connected = false;
    st.tx = None;
    Ok(())
}
