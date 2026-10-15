//! Ka Agent web UI: a loopback-only HTTP server with session auth
//! (SEED.md §10 Phase 4). Server-rendered HTML, no external assets, no
//! JS frameworks. Every user-derived string goes through `escape_html`.

pub mod pages;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ka_agent_runtime::Runtime;
use ka_agent_store::Store;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Debug, Error)]
pub enum WebError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("bind: {0}")]
    Bind(String),
}

// ---------------------------------------------------------------------------
// HTTP primitives
// ---------------------------------------------------------------------------

pub struct Request {
    method: String,
    path: String,
    query: HashMap<String, String>,
    cookies: HashMap<String, String>,
    body: Vec<u8>,
}

pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub body: String,
    pub set_cookies: Vec<String>,
    pub redirect: Option<String>,
}

impl Response {
    pub fn html(body: String) -> Self {
        Self {
            status: 200,
            content_type: "text/html; charset=utf-8",
            body,
            set_cookies: Vec::new(),
            redirect: None,
        }
    }

    pub fn redirect(location: &str) -> Self {
        Self {
            status: 302,
            content_type: "text/plain",
            body: String::new(),
            set_cookies: Vec::new(),
            redirect: Some(location.to_string()),
        }
    }

    pub fn unauthorized() -> Self {
        Self {
            status: 401,
            content_type: "text/plain",
            body: "unauthorized\n".into(),
            set_cookies: Vec::new(),
            redirect: None,
        }
    }

    pub fn not_found() -> Self {
        Self {
            status: 404,
            content_type: "text/plain",
            body: "not found\n".into(),
            set_cookies: Vec::new(),
            redirect: None,
        }
    }
}

fn status_reason(code: u16) -> &'static str {
    match code {
        200 => "OK",
        302 => "Found",
        303 => "See Other",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        _ => "Unknown",
    }
}

/// Parse one HTTP/1.1 request from a stream (bounded, timeout-guarded).
pub async fn parse_request(socket: &mut tokio::net::TcpStream) -> Option<Request> {
    const MAX: usize = 1024 * 1024;
    const TIMEOUT: Duration = Duration::from_secs(10);
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        if buf.len() > MAX {
            return None;
        }
        let n = tokio::time::timeout(TIMEOUT, socket.read(&mut tmp))
            .await
            .ok()?
            .ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(head_end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
            let mut lines = head.lines();
            let req_line = lines.next()?;
            let mut parts = req_line.split_whitespace();
            let method = parts.next()?.to_string();
            let target = parts.next()?.to_string();
            let mut cookies = HashMap::new();
            let mut content_length = 0usize;
            for line in lines {
                if let Some(v) = line
                    .strip_prefix("Cookie: ")
                    .or_else(|| line.strip_prefix("cookie: "))
                {
                    for pair in v.split("; ") {
                        if let Some((k, val)) = pair.split_once('=') {
                            cookies.insert(k.trim().to_string(), val.trim().to_string());
                        }
                    }
                }
                if let Some(v) = line
                    .strip_prefix("Content-Length: ")
                    .or_else(|| line.strip_prefix("content-length: "))
                {
                    content_length = v.trim().parse().unwrap_or(0);
                }
            }
            let (path, query_str) = target.split_once('?').unwrap_or((target.as_str(), ""));
            let mut query = HashMap::new();
            for pair in query_str.split('&') {
                if let Some((k, v)) = pair.split_once('=') {
                    query.insert(url_decode(k), url_decode(v));
                }
            }
            let body_start = head_end + 4;
            // A declared `Content-Length` is not trusted: stop at the same
            // cap as the head, so an oversized request cannot grow the buffer
            // without bound.
            if body_start + content_length > MAX {
                return None;
            }
            while buf.len() < body_start + content_length {
                let n = tokio::time::timeout(TIMEOUT, socket.read(&mut tmp))
                    .await
                    .ok()?
                    .ok()?;
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
            }
            let body = buf[body_start..(body_start + content_length).min(buf.len())].to_vec();
            return Some(Request {
                method,
                path: path.to_string(),
                query,
                cookies,
                body,
            });
        }
    }
}

pub fn url_decode(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '%' => {
                let hex: String = chars.by_ref().take(2).collect();
                if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                    out.push(byte as char);
                }
            }
            '+' => out.push(' '),
            _ => out.push(c),
        }
    }
    out
}

pub async fn write_response(socket: &mut tokio::net::TcpStream, resp: Response) {
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        resp.status,
        status_reason(resp.status),
        resp.content_type,
        resp.body.len()
    );
    for cookie in &resp.set_cookies {
        head.push_str(&format!("Set-Cookie: {cookie}\r\n"));
    }
    if let Some(loc) = &resp.redirect {
        head.push_str(&format!("Location: {loc}\r\n"));
    }
    head.push_str("\r\n");
    let _ = socket.write_all(head.as_bytes()).await;
    let _ = socket.write_all(resp.body.as_bytes()).await;
    let _ = socket.flush().await;
}

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

struct Auth {
    sessions: Mutex<HashMap<String, i64>>,
}

impl Auth {
    fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
        }
    }

    fn issue_token(store: &Store) -> Result<String, ka_agent_store::StoreError> {
        let token = random_hex(32);
        let payload = serde_json::json!({
            "token": token,
            "expires_ms": ka_agent_core::now_ms() + 600_000,
        });
        store.meta_set("web_auth_token", &payload.to_string())?;
        Ok(token)
    }

    fn consume_token(&self, token: &str, store: &Store) -> Option<String> {
        let raw = store.meta_get("web_auth_token").ok().flatten()?;
        let parsed: serde_json::Value = serde_json::from_str(&raw).ok()?;
        let stored = parsed["token"].as_str()?;
        let expires = parsed["expires_ms"].as_i64()?;
        if stored != token || ka_agent_core::now_ms() > expires {
            return None;
        }
        let _ = store.meta_set("web_auth_token", "");
        let session = random_hex(32);
        let now = ka_agent_core::now_ms();
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        // Drop expired sessions on every login: the map has no other
        // eviction point and would otherwise grow with each login.
        sessions.retain(|_, expiry| now < *expiry);
        sessions.insert(session.clone(), now + 86_400_000);
        drop(sessions);
        Some(session)
    }

    fn is_valid(&self, cookies: &HashMap<String, String>) -> bool {
        let Some(session) = cookies.get("ka_session") else {
            return false;
        };
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(session)
            .is_some_and(|expiry| ka_agent_core::now_ms() < *expiry)
    }
}

/// Issue a one-shot login URL for the web UI. The token is stored in the
/// meta table with a 10-minute TTL and consumed on first use.
pub fn issue_auth_url(store: &Store, port: u16) -> Result<String, WebError> {
    let token = Auth::issue_token(store).map_err(|e| WebError::Bind(format!("auth token: {e}")))?;
    Ok(format!("http://127.0.0.1:{port}/auth?token={token}"))
}

/// 32 bytes of OS entropy, hex-encoded. Falls back to per-thread hasher
/// output (seeded from the OS by `RandomState`) if `/dev/urandom` is absent.
fn random_hex(bytes: usize) -> String {
    use std::io::Read;
    let mut buf = vec![0u8; bytes];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_ok()
    {
        let mut out = String::with_capacity(bytes * 2);
        for b in buf {
            out.push_str(&format!("{b:02x}"));
        }
        return out;
    }
    let mut out = String::with_capacity(bytes * 2 + 16);
    while out.len() < bytes * 2 {
        out.push_str(&format!("{:016x}", random_u64()));
    }
    out.truncate(bytes * 2);
    out
}

fn random_u64() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(ka_agent_core::now_ms() as u64);
    h.write_u64(std::process::id() as u64);
    h.finish()
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

pub struct WebServer {
    pub port: u16,
    pub runtime: Runtime,
    pub store: Arc<Store>,
}

impl WebServer {
    pub fn new(port: u16, runtime: Runtime, store: Arc<Store>) -> Self {
        Self {
            port,
            runtime,
            store,
        }
    }

    pub async fn run(&self, shutdown: tokio::sync::watch::Receiver<bool>) -> Result<(), WebError> {
        let addr = format!("127.0.0.1:{}", self.port);
        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .map_err(|e| WebError::Bind(format!("bind {addr}: {e}")))?;
        eprintln!("ka-agent web: listening on {addr}");
        let auth = Arc::new(Auth::new());
        let mut shutdown = shutdown;
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    eprintln!("ka-agent web: shutting down");
                    return Ok(());
                }
                accepted = listener.accept() => {
                    let Ok((mut socket, _)) = accepted else { continue };
                    let auth = Arc::clone(&auth);
                    let runtime = self.runtime.clone();
                    let store = Arc::clone(&self.store);
                    tokio::spawn(async move {
                        if let Some(req) = parse_request(&mut socket).await {
                            let resp = route(&req, &auth, &runtime, &store).await;
                            write_response(&mut socket, resp).await;
                        }
                    });
                }
            }
        }
    }
}

async fn route(req: &Request, auth: &Auth, runtime: &Runtime, store: &Arc<Store>) -> Response {
    if req.path == "/healthz" {
        return Response::html("ok".into());
    }
    if req.path == "/auth" {
        let token = req.query.get("token").map(String::as_str).unwrap_or("");
        if let Some(session) = auth.consume_token(token, store) {
            let mut resp = Response::redirect("/");
            resp.set_cookies.push(format!(
                "ka_session={session}; HttpOnly; SameSite=Strict; Path=/; Max-Age=86400"
            ));
            return resp;
        }
        return Response::unauthorized();
    }
    if !auth.is_valid(&req.cookies) {
        return Response::unauthorized();
    }
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") => pages::dashboard(runtime, store),
        ("GET", "/history") => pages::history(runtime, req.query.get("q").map(String::as_str)),
        ("GET", p) if p.starts_with("/history/show/") => {
            let turn_id = p.strip_prefix("/history/show/").unwrap_or("");
            pages::history_show(store, turn_id)
        }
        ("GET", "/memory") => pages::memory(runtime),
        ("POST", "/memory/edit") => {
            let body = String::from_utf8_lossy(&req.body);
            pages::memory_edit(runtime, &body)
        }
        ("GET", "/approvals") => pages::approvals(runtime, store),
        ("POST", "/approvals/decide") => {
            let body = String::from_utf8_lossy(&req.body);
            pages::approvals_decide(runtime, &body).await
        }
        ("GET", "/audit") => pages::audit(store),
        ("GET", "/intents") => pages::intents(store),
        _ => Response::not_found(),
    }
}

/// Escape user-derived text for safe HTML rendering.
pub fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}
