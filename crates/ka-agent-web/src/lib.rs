//! Ka Agent web UI: a loopback-only HTTP server with session auth
//! (SEED.md §10 Phase 4). Server-rendered HTML, no external assets, no
//! JS frameworks. Every user-derived string goes through `escape_html`.

pub mod pages;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ka_agent_runtime::{EventBus, Runtime, SurfaceEvent};
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
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn headers_get(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub body: String,
    pub set_cookies: Vec<String>,
    pub redirect: Option<String>,
    /// Extra headers on this response only (CSP is added automatically on
    /// every HTML response).
    pub extra_headers: Vec<(&'static str, String)>,
}

/// The default security policy: only this origin, no third-party anything,
/// frames denied, no JS except our own static file.
pub const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'";

impl Response {
    fn base(status: u16, content_type: &'static str, body: String) -> Self {
        Self {
            status,
            content_type,
            body,
            set_cookies: Vec::new(),
            redirect: None,
            extra_headers: Vec::new(),
        }
    }

    pub fn html(body: String) -> Self {
        Self::base(200, "text/html; charset=utf-8", body)
    }

    pub fn json(body: String) -> Self {
        Self::base(200, "application/json", body)
    }

    /// POST replies use 303 so a browser reload does not re-submit.
    pub fn see_other(location: &str) -> Self {
        let mut r = Self::base(303, "text/plain", String::new());
        r.redirect = Some(location.to_string());
        r
    }

    pub fn redirect(location: &str) -> Self {
        Self::see_other(location)
    }

    pub fn unauthorized() -> Self {
        Self::base(
            401,
            "text/html; charset=utf-8",
            "<!DOCTYPE html><html><body><p>Session expired. On your computer run <code>ka-agent dashboard</code>.</p></body></html>".into(),
        )
    }

    pub fn not_found() -> Self {
        Self::base(404, "text/plain", "not found\n".into())
    }

    pub fn bad_request(message: &str) -> Self {
        Self::base(400, "text/plain", format!("{message}\n"))
    }

    pub fn forbidden(message: &str) -> Self {
        Self::base(403, "text/plain", format!("{message}\n"))
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
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        if buf.len() > MAX {
            return None;
        }
        let n = socket.read(&mut tmp).await.ok()?;
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
            let mut headers: Vec<(String, String)> = Vec::new();
            let mut content_length: Option<u64> = None;
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
                if let Some((name, value)) = line.split_once(": ") {
                    headers.push((name.trim().to_string(), value.trim().to_string()));
                }
                if let Some(v) = line
                    .strip_prefix("Content-Length: ")
                    .or_else(|| line.strip_prefix("content-length: "))
                {
                    // One declaration only (smuggling hardening), and the
                    // value is validated before any arithmetic: `usize::MAX`
                    // must not reach an addition or a slice range.
                    if content_length.is_some() {
                        return None;
                    }
                    let cl: u64 = v.trim().parse().unwrap_or(u64::MAX);
                    if cl > MAX as u64 {
                        return None;
                    }
                    content_length = Some(cl);
                }
            }
            let content_length = content_length.unwrap_or(0) as usize;
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
                let n = socket.read(&mut tmp).await.ok()?;
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
                headers,
                body,
            });
        }
    }
}

pub fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            // Both hex digits must be present; a truncated escape stays a
            // literal `%` (lossless, never a control byte).
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
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
    if resp.content_type.starts_with("text/html") {
        head.push_str(&format!(
            "Content-Security-Policy: {CSP}\r\nX-Content-Type-Options: nosniff\r\n"
        ));
    }
    for (name, value) in &resp.extra_headers {
        head.push_str(&format!("{name}: {value}\r\n"));
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
        // Constant-time: an unauthenticated caller must not learn the
        // token's prefix through compare timing.
        let same = stored.len() == token.len()
            && stored
                .bytes()
                .zip(token.bytes())
                .fold(0u8, |acc, (x, y)| acc | (x ^ y))
                == 0;
        if !same || ka_agent_core::now_ms() > expires {
            return None;
        }
        // Single-use under concurrent redemption: exactly one caller's
        // compare-and-swap wins, even if several read the token first.
        if !store.consume_meta("web_auth_token", &raw).unwrap_or(false) {
            return None;
        }
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

/// 32 bytes of OS entropy, hex-encoded.
fn random_hex(bytes: usize) -> String {
    use rand_core::RngCore;
    let mut buf = vec![0u8; bytes];
    rand_core::OsRng.fill_bytes(&mut buf);
    let mut out = String::with_capacity(bytes * 2);
    for b in buf {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

pub struct WebServer {
    pub port: u16,
    pub runtime: Runtime,
    pub store: Arc<Store>,
    pub bus: Arc<EventBus>,
}

impl WebServer {
    pub fn new(port: u16, runtime: Runtime, store: Arc<Store>, bus: Arc<EventBus>) -> Self {
        Self {
            port,
            runtime,
            store,
            bus,
        }
    }

    pub async fn run(&self, shutdown: tokio::sync::watch::Receiver<bool>) -> Result<(), WebError> {
        let addr = format!("127.0.0.1:{}", self.port);
        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .map_err(|e| WebError::Bind(format!("bind {addr}: {e}")))?;
        let _ = self.store.meta_set("web.url", &format!("http://{addr}"));
        eprintln!("ka-agent web: listening on {addr}");
        let auth = Arc::new(Auth::new());
        // Bounded concurrency: a local process must not exhaust the shared
        // daemon's tasks and file descriptors with idle connections. Beyond
        // the cap the connection is dropped immediately (fail closed).
        let slots = Arc::new(tokio::sync::Semaphore::new(64));
        let mut shutdown = shutdown;
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    eprintln!("ka-agent web: shutting down");
                    let _ = self.store.meta_set("web.url", "");
                    return Ok(());
                }
                accepted = listener.accept() => {
                    let Ok((mut socket, _)) = accepted else { continue };
                    let Ok(slot) = slots.clone().try_acquire_owned() else {
                        // Over capacity: nothing was read, nothing leaks.
                        continue;
                    };
                    let auth = Arc::clone(&auth);
                    let runtime = self.runtime.clone();
                    let store = Arc::clone(&self.store);
                    let bus = Arc::clone(&self.bus);
                    tokio::spawn(async move {
                        let _slot = slot;
                        // One deadline for head and body alike: a dribbling
                        // connection cannot reset it byte by byte. The SSE
                        // stream is exempt by design (see its own branch).
                        let req = tokio::time::timeout(
                            Duration::from_secs(30),
                            parse_request(&mut socket),
                        )
                        .await
                        .ok()
                        .flatten();
                        if let Some(req) = req {
                            if req.path == "/events" {
                                serve_events(&req, &auth, &runtime, &store, bus, socket).await;
                                return;
                            }
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
        ("GET", "/") => pages::chat_page(runtime, store),
        ("GET", "/static/chat.js") => {
            let mut r = Response::base(
                200,
                "application/javascript",
                include_str!("chat.js").to_string(),
            );
            r.extra_headers
                .push(("Cache-Control", "no-store".to_string()));
            r
        }
        ("POST", "/chat/send") => {
            if req.headers_get("x-ka-agent") != Some("1") {
                return Response::forbidden("missing X-Ka-Agent header");
            }
            let body = String::from_utf8_lossy(&req.body);
            pages::chat_send(runtime, store, &body).await
        }
        ("GET", "/status") => pages::status(runtime),
        ("GET", "/usage") => pages::usage(runtime, store),
        ("POST", "/settings") => {
            let body = String::from_utf8_lossy(&req.body).into_owned();
            pages::settings_apply(runtime, &body).await
        }
        ("GET", "/tasks") => pages::tasks(runtime),
        ("POST", "/tasks/cancel") => {
            let body = String::from_utf8_lossy(&req.body);
            pages::tasks_cancel(runtime, &body).await
        }
        ("GET", "/skills") => pages::skills(runtime),
        ("POST", "/skills/toggle") => {
            let body = String::from_utf8_lossy(&req.body);
            pages::skills_toggle(runtime, &body)
        }
        ("GET", "/contacts") => pages::contacts(runtime, store),
        ("POST", "/memory/forget") => {
            let body = String::from_utf8_lossy(&req.body);
            pages::memory_forget(runtime, &body).await
        }
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
        ("POST", "/memory/candidate") => {
            let body = String::from_utf8_lossy(&req.body);
            pages::memory_candidate(runtime, &body)
        }
        ("GET", "/approvals") => pages::approvals(runtime, store),
        ("POST", "/approvals/decide") => {
            let body = String::from_utf8_lossy(&req.body);
            if req.headers_get("x-ka-agent") == Some("1") {
                pages::approvals_decide_json(runtime, &body).await
            } else {
                pages::approvals_decide(runtime, &body).await
            }
        }
        ("GET", "/audit") => pages::audit(runtime, store),
        ("GET", "/intents") => pages::intents(store),
        _ => Response::not_found(),
    }
}

/// `GET /events`: server-sent events from the bus. Authenticated like any
/// page; the connection lives until the client or the daemon goes away.
async fn serve_events(
    req: &Request,
    auth: &Auth,
    _runtime: &Runtime,
    _store: &Arc<Store>,
    bus: Arc<EventBus>,
    mut socket: tokio::net::TcpStream,
) {
    if !auth.is_valid(&req.cookies) {
        write_response(&mut socket, Response::unauthorized()).await;
        return;
    }
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nConnection: keep-alive\r\n\r\n";
    if socket.write_all(head.as_bytes()).await.is_err() {
        return;
    }
    let mut rx = bus.subscribe();
    let mut ping = tokio::time::interval(Duration::from_secs(25));
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            ev = rx.recv() => {
                let ev = match ev {
                    Ok(ev) => ev,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => SurfaceEvent::Notice {
                        text: format!("(missed {n} events)"),
                    },
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                };
                if matches!(ev, SurfaceEvent::Hello { .. } | SurfaceEvent::Reply { .. }) {
                    continue;
                }
                match serde_json::to_string(&ev) {
                    Ok(json) => {
                        if socket
                            .write_all(format!("data: {json}\n\n").as_bytes())
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                    Err(_) => continue,
                }
            }
            _ = ping.tick() => {
                if socket.write_all(b": ping\n\n").await.is_err() {
                    return;
                }
            }
        }
        let _ = socket.flush().await;
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
