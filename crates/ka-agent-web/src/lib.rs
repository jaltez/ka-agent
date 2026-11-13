//! Ka Agent web UI: a loopback-only HTTP server with session auth
//! (SEED.md §10 Phase 4). Server-rendered HTML, no external assets, no
//! JS frameworks. Every user-derived string goes through `escape_html`.

pub mod pages;
pub mod pages_accounts;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ka_agent_runtime::event::AgentEvent;
use ka_agent_runtime::{Envelope, EventBus, Runtime};
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

    /// First value of a query parameter.
    fn query_param(&self, name: &str) -> Option<&str> {
        self.query.get(name).map(String::as_str)
    }
}

pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub body: String,
    /// Binary payload (file downloads); takes precedence over `body`.
    pub body_bytes: Option<Vec<u8>>,
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
            body_bytes: None,
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

    /// A binary download (`Content-Disposition` is the caller's header).
    pub fn bytes(content_type: &'static str, bytes: Vec<u8>) -> Self {
        let mut r = Self::base(200, content_type, String::new());
        r.body_bytes = Some(bytes);
        r
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
            "<!DOCTYPE html><html><head><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><title>Ka Agent</title><style>body{font:15px/1.6 system-ui,sans-serif;max-width:32rem;margin:15vh auto;padding:0 1.2rem;color:#16181d}a{color:#4f46e5}code{background:#f2f2f4;padding:.1rem .3rem;border-radius:4px}</style></head><body>             <h1>Ka Agent</h1><p>This browser has no live session.</p>             <p><a href=\"/pair\">Pair this browser</a> with a code from the panel, or on your computer run <code>ka-agent dashboard</code>.</p></body></html>".into(),
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
        403 => "Forbidden",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "Unknown",
    }
}

/// Parse one HTTP/1.1 request from a stream (bounded, timeout-guarded).
pub async fn parse_request(socket: &mut tokio::net::TcpStream) -> Option<Request> {
    const MAX_HEAD: usize = 64 * 1024;
    /// Uploads (files from the web chat) ride one request body; the route
    /// enforces its own, smaller cap on top.
    const MAX_BODY: usize = 12 * 1024 * 1024;
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        if buf.len() > MAX_HEAD + MAX_BODY {
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
                    if cl > MAX_BODY as u64 {
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
            if body_start + content_length > MAX_HEAD + MAX_BODY {
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
    let payload: &[u8] = resp.body_bytes.as_deref().unwrap_or(resp.body.as_bytes());
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\
         X-Content-Type-Options: nosniff\r\n",
        resp.status,
        status_reason(resp.status),
        resp.content_type,
        payload.len()
    );
    for cookie in &resp.set_cookies {
        head.push_str(&format!("Set-Cookie: {cookie}\r\n"));
    }
    if let Some(loc) = &resp.redirect {
        head.push_str(&format!("Location: {loc}\r\n"));
    }
    if resp.content_type.starts_with("text/html") {
        head.push_str(&format!(
            "Content-Security-Policy: {CSP}\r\nX-Content-Type-Options: nosniff\r\nCache-Control: no-store\r\n"
        ));
    }
    for (name, value) in &resp.extra_headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let _ = socket.write_all(head.as_bytes()).await;
    let _ = socket.write_all(payload).await;
    let _ = socket.flush().await;
}

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

/// Who a web session belongs to: a **user** and the role their account
/// carries (ADR-0004). One login; reach derives from the role — `admin`
/// sees the whole panel, everyone else the chat-only allowlist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    /// The envelope source this session's turns are stamped with
    /// (`web:<username>`), so `/events` filtering needs no new protocol.
    pub source: String,
    pub username: String,
    pub role: String,
}

impl Principal {
    fn new(username: &str, role: &str) -> Self {
        Self {
            source: format!("web:{username}"),
            username: username.to_string(),
            role: role.to_string(),
        }
    }

    pub fn is_admin(&self) -> bool {
        self.role == ka_agent_core::ADMIN_ROLE
    }

    fn as_core(&self) -> ka_agent_core::Principal {
        ka_agent_core::Principal::new(&self.username, &self.role)
    }
}

/// Login and invite attempts allowed per key inside [`FAILURE_WINDOW_MS`].
const MAX_FAILURES: usize = 5;
const FAILURE_WINDOW_MS: i64 = 15 * 60 * 1000;

/// Account failures: 5 wrong passwords (or invite redemptions) per username
/// per 15 minutes, then a lockout that is itself audited.
#[derive(Default)]
struct Failures {
    map: Mutex<HashMap<String, Vec<i64>>>,
}

impl Failures {
    /// Whether `key` is currently locked out.
    fn locked(&self, key: &str) -> bool {
        let now = ka_agent_core::now_ms();
        let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        let hits = map.entry(key.to_string()).or_default();
        hits.retain(|t| now - *t < FAILURE_WINDOW_MS);
        hits.len() >= MAX_FAILURES
    }

    fn record(&self, key: &str) {
        let now = ka_agent_core::now_ms();
        let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        let hits = map.entry(key.to_string()).or_default();
        hits.retain(|t| now - *t < FAILURE_WINDOW_MS);
        hits.push(now);
    }

    fn clear(&self, key: &str) {
        self.map
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(key);
    }
}

/// Accounts: password hashing, sessions and the login/invite flows. Sessions
/// live in `web_sessions` (survive a daemon restart, swept on access); only
/// the SHA-256 of a cookie is stored.
struct Auth {
    failures: Failures,
}

/// Why a login or invite redemption failed (all operator-facing).
#[derive(Debug)]
enum LoginError {
    /// No such user, no password set, or a wrong password — one message, so
    /// the form never enumerates accounts.
    Credentials,
    Locked,
    Invite(String),
}

impl LoginError {
    fn message(&self) -> String {
        match self {
            LoginError::Credentials => "wrong username or password".to_string(),
            LoginError::Locked => {
                "too many attempts — wait 15 minutes or ask an admin to reset the password".to_string()
            }
            LoginError::Invite(reason) => reason.clone(),
        }
    }
}

fn sha256_hex(text: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Hash a password with argon2id (fresh salt from the OS).
fn hash_password(password: &str) -> Result<String, String> {
    use argon2::password_hash::{PasswordHasher, SaltString, rand_core::OsRng};
    let salt = SaltString::generate(&mut OsRng);
    argon2::Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| format!("password hash: {e}"))
}

/// Verify a password against a PHC string (constant-time inside argon2).
fn verify_password(password: &str, phc: &str) -> bool {
    use argon2::password_hash::{PasswordHash, PasswordVerifier};
    let Ok(parsed) = PasswordHash::new(phc) else {
        return false;
    };
    argon2::Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

impl Auth {
    fn new() -> Self {
        Self {
            failures: Failures::default(),
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
        // The one-shot URL belongs to the operator's account. Binding its
        // `web:operator` identity here (not at boot) keeps a fresh profile
        // identity-free until someone actually signs in.
        let role = store
            .get_user(ka_agent_core::OPERATOR_USER)
            .ok()
            .flatten()
            .map(|u| u.role)
            .unwrap_or_else(|| ka_agent_core::ADMIN_ROLE.to_string());
        let _ = store.link_identity_to_user(
            "web",
            ka_agent_core::OPERATOR_USER,
            "panel-login",
            &role,
            ka_agent_core::OPERATOR_USER,
        );
        self.new_session(store, ka_agent_core::OPERATOR_USER, &role)
    }

    /// Persist a fresh session and return the cookie value.
    fn new_session(&self, store: &Store, username: &str, role: &str) -> Option<String> {
        // Every web session speaks as `web:<username>`: make sure the identity
        // row exists (a reset can reopen an account that never had one).
        // Idempotent, like the operator's token path.
        let _ = store.link_identity_to_user("web", username, "panel-login", role, username);
        let token = random_hex(32);
        let now = ka_agent_core::now_ms();
        let _ = store.sweep_web_sessions(now);
        store
            .insert_web_session(&sha256_hex(&token), username, now, now + 86_400_000)
            .ok()?;
        Some(token)
    }

    /// The principal behind the cookie, when the session is live. The role is
    /// re-derived from the user row on every request (never trusted from the
    /// captured session), and a disabled account stops resolving.
    fn principal(&self, store: &Store, cookies: &HashMap<String, String>) -> Option<Principal> {
        let token = cookies.get("ka_session")?;
        let username = store
            .web_session_user(&sha256_hex(token), ka_agent_core::now_ms())
            .ok()
            .flatten()?;
        let user = store.get_user(&username).ok().flatten()?;
        if user.disabled {
            return None;
        }
        Some(Principal::new(&user.username, &user.role))
    }

    /// Username + password login (argon2id verify, lockout after 5 failures).
    fn login(
        &self,
        store: &Store,
        username: &str,
        password: &str,
    ) -> Result<Option<String>, LoginError> {
        let username = username.trim().to_ascii_lowercase();
        let key = format!("login:{username}");
        if self.failures.locked(&key) {
            return Err(LoginError::Locked);
        }
        let user = store
            .get_user(&username)
            .ok()
            .flatten()
            .filter(|u| !u.disabled);
        let Some(user) = user else {
            self.failures.record(&key);
            return Err(LoginError::Credentials);
        };
        let Some(phc) = user.pw_hash.as_deref() else {
            // No password set yet: the account exists but has never redeemed
            // an invite. Same message as a wrong password (no enumeration).
            self.failures.record(&key);
            return Err(LoginError::Credentials);
        };
        if !verify_password(password, phc) {
            self.failures.record(&key);
            let mut rec = ka_agent_core::AuditRecord::new(
                &format!("web:{username}"),
                "user.login_failed",
                &username,
                None,
            )
            .with_role(&user.role)
            .with_user(&username);
            let _ = store.append_audit(&mut rec);
            return Err(LoginError::Credentials);
        }
        self.failures.clear(&key);
        Ok(self.new_session(store, &user.username, &user.role))
    }

    /// Redeem an admin-minted invite: `code` + a chosen username + password.
    /// A code can only ever create a *new* account (never take over one), and
    /// it is single-use.
    fn redeem_invite(
        &self,
        store: &Store,
        code: &str,
        username: &str,
        password: &str,
    ) -> Result<Option<String>, LoginError> {
        let username = username.trim().to_ascii_lowercase();
        // A targeted code names its account; a new-account code needs one.
        let untargeted = store
            .invite_verdict(&sha256_hex(code.trim()), ka_agent_core::now_ms())
            .ok()
            .and_then(|v| match v {
                ka_agent_store::InviteVerdict::Valid(i) => Some(i.username.is_none()),
                _ => None,
            })
            .unwrap_or(true);
        if untargeted && !ka_agent_store::valid_username(&username) {
            return Err(LoginError::Invite(
                "username: lowercase letters, digits, dot, dash or underscore".into(),
            ));
        }
        if password.chars().count() < 8 {
            return Err(LoginError::Invite(
                "password needs at least 8 characters".into(),
            ));
        }
        let code_hash = sha256_hex(code.trim().to_ascii_lowercase().as_str());
        let key = format!("invite:{}", &code_hash[..16]);
        if self.failures.locked(&key) {
            return Err(LoginError::Locked);
        }
        let verdict = store
            .invite_verdict(&code_hash, ka_agent_core::now_ms())
            .map_err(|e| LoginError::Invite(e.to_string()))?;
        let invite = match verdict {
            ka_agent_store::InviteVerdict::Valid(invite) => invite,
            ka_agent_store::InviteVerdict::Unknown => {
                self.failures.record(&key);
                return Err(LoginError::Invite("invite code invalid".into()));
            }
            ka_agent_store::InviteVerdict::Expired(_) => {
                self.failures.record(&key);
                return Err(LoginError::Invite(
                    "invite code expired — ask an admin for a new one".into(),
                ));
            }
            ka_agent_store::InviteVerdict::Used(_) => {
                self.failures.record(&key);
                return Err(LoginError::Invite("invite code already used".into()));
            }
        };
        // A targeted code (an admin reset) only ever sets that account's
        // password; an untargeted one only ever creates a new account, so a
        // leaked code can never take over an existing user.
        if let Some(target) = &invite.username {
            if !username.is_empty() && username != *target {
                return Err(LoginError::Invite(format!(
                    "this code resets {target}, not {username}"
                )));
            }
            let Some(user) = store.get_user(target).ok().flatten() else {
                return Err(LoginError::Invite(format!("{target} no longer exists")));
            };
            let phc = hash_password(password).map_err(LoginError::Invite)?;
            store
                .set_user_password(target, Some(&phc))
                .map_err(|e| LoginError::Invite(e.to_string()))?;
            let _ = store.delete_web_sessions_for(target);
            if !store
                .consume_invite(&code_hash, ka_agent_core::now_ms())
                .unwrap_or(false)
            {
                return Err(LoginError::Invite("invite code already used".into()));
            }
            let mut rec = ka_agent_core::AuditRecord::new(
                "web",
                "user.password_reset",
                target,
                Some("redeemed a reset invite".into()),
            )
            .with_role(&user.role)
            .with_user(target);
            let _ = store.append_audit(&mut rec);
            self.failures.clear(&key);
            return Ok(self.new_session(store, &user.username, &user.role));
        }
        if store.get_user(&username).ok().flatten().is_some() {
            return Err(LoginError::Invite(format!(
                "{username} already exists — use the login form"
            )));
        }
        let phc = hash_password(password).map_err(LoginError::Invite)?;
        let user = ka_agent_store::UserRow {
            username: username.clone(),
            display_name: username.clone(),
            pw_hash: Some(phc),
            role: invite.role.clone(),
            quota_daily_usd: None,
            disabled: false,
            created_ms: ka_agent_core::now_ms(),
            created_by: Some(invite.created_by.clone()),
        };
        store
            .create_user(&user)
            .map_err(|e| LoginError::Invite(e.to_string()))?;
        for group in &invite.groups {
            let _ = store.ensure_group(group);
            let _ = store.add_user_group(&username, group);
        }
        if !store.consume_invite(&code_hash, ka_agent_core::now_ms()).unwrap_or(false) {
            // Someone else consumed it between the verdict and here.
            return Err(LoginError::Invite("invite code already used".into()));
        }
        // The account is reachable from the panel's own identity source.
        let _ = store.link_identity_to_user(
            "web",
            &username,
            "invite",
            &invite.role,
            &username,
        );
        let mut rec = ka_agent_core::AuditRecord::new(
            "web",
            "invite.redeem",
            &username,
            Some(format!("role {}", invite.role)),
        )
        .with_role(&invite.role)
        .with_user(&username);
        let _ = store.append_audit(&mut rec);
        self.failures.clear(&key);
        Ok(self.new_session(store, &user.username, &user.role))
    }
}

/// Issue a one-shot login URL for the web UI. The token is stored in the
/// meta table with a 10-minute TTL and consumed on first use.
pub fn issue_auth_url(store: &Store, port: u16) -> Result<String, WebError> {
    issue_auth_url_at(store, &format!("http://127.0.0.1:{port}"))
}

/// The same, against a public base URL (`https://agent.example.com`).
pub fn issue_auth_url_at(store: &Store, base: &str) -> Result<String, WebError> {
    let token = Auth::issue_token(store).map_err(|e| WebError::Bind(format!("auth token: {e}")))?;
    Ok(format!("{}/auth?token={token}", base.trim_end_matches('/')))
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
    /// Interface to bind (`127.0.0.1` default; `0.0.0.0` only behind TLS
    /// or a reverse proxy).
    pub bind: String,
    /// Public base URL for printed links (`None` = the local URL).
    pub public_url: Option<String>,
    pub runtime: Runtime,
    pub store: Arc<Store>,
    pub bus: Arc<EventBus>,
}

impl WebServer {
    pub fn new(
        port: u16,
        bind: String,
        public_url: Option<String>,
        runtime: Runtime,
        store: Arc<Store>,
        bus: Arc<EventBus>,
    ) -> Self {
        Self {
            port,
            bind,
            public_url,
            runtime,
            store,
            bus,
        }
    }

    pub async fn run(&self, shutdown: tokio::sync::watch::Receiver<bool>) -> Result<(), WebError> {
        // IPv6 literals need brackets in `host:port`.
        let addr = if self.bind.contains(':') {
            format!("[{}]:{}", self.bind, self.port)
        } else {
            format!("{}:{}", self.bind, self.port)
        };
        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .map_err(|e| WebError::Bind(format!("bind {addr}: {e}")))?;
        // A wildcard bind is not a URL anyone can open; the local one is.
        let local = if self.bind == "0.0.0.0" || self.bind == "::" {
            format!("http://127.0.0.1:{}", self.port)
        } else {
            format!("http://{addr}")
        };
        let url = self.public_url.clone().unwrap_or(local);
        let _ = self.store.meta_set("web.url", &url);
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
    if req.path == "/static/ui.js" {
        let mut r = Response::base(
            200,
            "application/javascript",
            include_str!("ui.js").to_string(),
        );
        r.extra_headers
            .push(("Cache-Control", "no-store".to_string()));
        return r;
    }
    if req.path == "/static/chat.js" {
        // Public: the login and pairing pages do not load it, and it
        // carries no data.
        let mut r = Response::base(
            200,
            "application/javascript",
            include_str!("chat.js").to_string(),
        );
        r.extra_headers
            .push(("Cache-Control", "no-store".to_string()));
        return r;
    }
    if req.path == "/login" {
        return match req.method.as_str() {
            "GET" => pages_accounts::login_page(None),
            "POST" => {
                let form = crate::pages_accounts::Form::parse(&req.body);
                match auth.login(store, form.get("username"), form.get("password")) {
                    Ok(Some(token)) => {
                        let mut resp = Response::redirect("/");
                        resp.set_cookies.push(session_cookie(&token, runtime));
                        resp
                    }
                    Ok(None) => pages_accounts::login_page(Some("could not open a session")),
                    Err(e) => pages_accounts::login_page(Some(&e.message())),
                }
            }
            _ => Response::base(405, "text/plain", "method not allowed\n".into()),
        };
    }
    if req.path == "/invite" {
        return match req.method.as_str() {
            "GET" => pages_accounts::invite_page(None),
            "POST" => {
                let form = crate::pages_accounts::Form::parse(&req.body);
                match auth.redeem_invite(
                    store,
                    form.get("code"),
                    form.get("username"),
                    form.get("password"),
                ) {
                    Ok(Some(token)) => {
                        let mut resp = Response::redirect("/");
                        resp.set_cookies.push(session_cookie(&token, runtime));
                        resp
                    }
                    Ok(None) => pages_accounts::invite_page(Some("could not open a session")),
                    Err(e) => pages_accounts::invite_page(Some(&e.message())),
                }
            }
            _ => Response::base(405, "text/plain", "method not allowed\n".into()),
        };
    }
    if req.path == "/auth" {
        let token = req.query.get("token").map(String::as_str).unwrap_or("");
        if let Some(session) = auth.consume_token(token, store) {
            let mut resp = Response::redirect("/");
            resp.set_cookies.push(session_cookie(&session, runtime));
            return resp;
        }
        return Response::unauthorized();
    }
    // Local triggers (plan 2.7): POST /api/notify and POST /api/ask take
    // `Authorization: Bearer <web.api_token>` instead of the panel cookie.
    // No secret configured → 404 (the API is off, not forbidden).
    if req.path.starts_with("/api/") {
        return api_route(req, runtime, store).await;
    }
    let Some(principal) = auth.principal(store, &req.cookies) else {
        return Response::unauthorized();
    };
    // CSRF: every mutating form POST must carry the panel's header (the
    // pages' own `ui.js` submits through fetch with it). SameSite=Strict
    // already blocks cross-site cookies; this is the second lock.
    if req.method == "POST" && req.headers_get("x-ka-agent") != Some("1") {
        return Response::forbidden("missing X-Ka-Agent header");
    }
    let role = principal.role.clone();
    let source = principal.source.clone();
    if req.path == "/upload" {
        if req.method != "POST" {
            return Response::base(405, "text/plain", "method not allowed\n".into());
        }
        let name = req.headers_get("x-ka-filename").unwrap_or("file");
        return pages::web_upload(runtime, &role, &principal.username, name, &req.body);
    }
    if let Some(raw) = req.path.strip_prefix("/file/") {
        if req.method != "GET" {
            return Response::base(405, "text/plain", "method not allowed\n".into());
        }
        return pages::serve_file(runtime, &role, &principal.username, &url_decode(raw));
    }
    // A non-admin session is chat-only: `/`, `/chat/send`, `/events` (SSE),
    // `/upload`, `/file/*` and the static scripts. "Admin" is a property of
    // the user, so the realms collapse to one login.
    if !principal.is_admin() {
        return match (req.method.as_str(), req.path.as_str()) {
            ("GET", "/") => {
                if runtime.activate_user(&principal.as_core()) {
                    pages::chat_page_user(runtime, store, &principal.as_core())
                } else {
                    pages::chat_page_busy(&principal.username)
                }
            }
            ("POST", "/chat/send") => {
                let body = String::from_utf8_lossy(&req.body);
                pages::chat_send_for(runtime, store, &body, &source, &principal.as_core()).await
            }
            _ => Response::forbidden("this web session is chat-only"),
        };
    }
    let who = principal.as_core();
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") => pages::chat_page_user(runtime, store, &who),
        ("POST", "/chat/send") => {
            let body = String::from_utf8_lossy(&req.body);
            pages::chat_send_for(runtime, store, &body, &source, &who).await
        }
        ("GET", "/status") => pages::status(runtime, &who),
        ("GET", "/users") => {
            pages_accounts::users(runtime, store, req.query.get("msg").map(String::as_str))
        }
        ("POST", "/users/save") => {
            let _body = String::from_utf8_lossy(&req.body).into_owned();
            pages_accounts::users_save(runtime, store, &req.body)
        }
        ("POST", "/users/invite") => {
            let _body = String::from_utf8_lossy(&req.body).into_owned();
            pages_accounts::users_invite(store, &req.body)
        }
        ("POST", "/users/disable") => {
            let _body = String::from_utf8_lossy(&req.body).into_owned();
            pages_accounts::users_state(store, &req.body, true)
        }
        ("POST", "/users/enable") => {
            let _body = String::from_utf8_lossy(&req.body).into_owned();
            pages_accounts::users_state(store, &req.body, false)
        }
        ("POST", "/users/reset") => {
            let _body = String::from_utf8_lossy(&req.body).into_owned();
            pages_accounts::users_reset(store, &req.body)
        }
        ("POST", "/users/export") => {
            let _body = String::from_utf8_lossy(&req.body).into_owned();
            pages_accounts::users_export(runtime, &req.body)
        }
        ("POST", "/users/purge") => {
            let _body = String::from_utf8_lossy(&req.body).into_owned();
            pages_accounts::users_purge(runtime, &req.body)
        }
        ("GET", "/agents") => pages_accounts::agents(store, &who),
        ("POST", "/agents/create") => pages_accounts::agents_create(store, &req.body, &who),
        ("POST", "/agents/save") => pages_accounts::agents_save(store, &req.body, &who),
        ("POST", "/agents/rm") => pages_accounts::agents_rm(store, &req.body, &who),
        ("GET", "/groups") => pages_accounts::groups(store),
        ("POST", "/groups/save") => {
            let _body = String::from_utf8_lossy(&req.body).into_owned();
            pages_accounts::groups_save(store, &req.body)
        }
        ("POST", "/groups/delete") => {
            let _body = String::from_utf8_lossy(&req.body).into_owned();
            pages_accounts::groups_delete(store, &req.body)
        }
        ("GET", "/kb") => pages_accounts::kb_page(runtime, store),
        ("POST", "/kb/create") => {
            let _body = String::from_utf8_lossy(&req.body).into_owned();
            pages_accounts::kb_create(store, &req.body, &who)
        }
        ("POST", "/kb/access") => {
            let _body = String::from_utf8_lossy(&req.body).into_owned();
            pages_accounts::kb_access(store, &req.body)
        }
        ("POST", "/kb/rm") => {
            let _body = String::from_utf8_lossy(&req.body).into_owned();
            pages_accounts::kb_rm(store, &req.body)
        }
        ("POST", p) if p.starts_with("/kb/") && p.ends_with("/docs") => {
            let name = p
                .trim_start_matches("/kb/")
                .trim_end_matches("/docs")
                .to_string();
            pages_accounts::kb_upload(
                store,
                &url_decode(&name),
                &req.body,
                req.headers_get("x-ka-filename"),
                req.headers_get("x-ka-title"),
            )
        }
        ("GET", "/profiles") => pages::profiles(runtime, store),
        ("GET", "/roles/new") => pages::role_edit(runtime, None),
        ("GET", p) if p.starts_with("/roles/edit/") => {
            let name = p.strip_prefix("/roles/edit/").unwrap_or("");
            pages::role_edit(runtime, Some(&url_decode(name)))
        }
        ("POST", "/roles/save") => {
            let body = String::from_utf8_lossy(&req.body).into_owned();
            pages::role_save(runtime, &body).await
        }
        ("POST", "/roles/delete") => {
            let body = String::from_utf8_lossy(&req.body).into_owned();
            pages::role_delete(runtime, &body).await
        }
        ("POST", "/tools/disable") => {
            let body = String::from_utf8_lossy(&req.body).into_owned();
            pages::tools_disable(runtime, &body).await
        }
        ("GET", "/models") => pages::models(runtime),
        ("POST", "/models/set") => {
            let body = String::from_utf8_lossy(&req.body).into_owned();
            pages::models_set(runtime, &body).await
        }
        ("POST", "/models/mode") => {
            let body = String::from_utf8_lossy(&req.body).into_owned();
            pages::models_mode(runtime, &body).await
        }
        ("POST", "/models/effort") => {
            let body = String::from_utf8_lossy(&req.body).into_owned();
            pages::models_effort(runtime, &body).await
        }
        ("POST", "/persona") => {
            let body = String::from_utf8_lossy(&req.body);
            pages::persona_set(runtime, &body)
        }
        ("GET", "/config") => pages::config_page(runtime),
        ("GET", "/queue") => pages::queue(runtime, &who),
        ("POST", "/queue/remove") => {
            let body = String::from_utf8_lossy(&req.body);
            pages::queue_remove(runtime, &body, &who)
        }
        ("POST", "/queue/clear") => pages::queue_clear(runtime, &who),
        ("GET", "/grants") => pages::grants(runtime),
        ("POST", "/grants/add") => {
            let body = String::from_utf8_lossy(&req.body);
            pages::grants_add(runtime, &body)
        }
        ("POST", "/grants/rm") => {
            let body = String::from_utf8_lossy(&req.body);
            pages::grants_rm(runtime, &body)
        }
        ("GET", "/keys") => pages::keys(runtime),
        ("POST", "/keys/provider") => {
            let body = String::from_utf8_lossy(&req.body);
            pages::keys_provider(runtime, &body)
        }
        ("POST", "/keys/secret") => {
            let body = String::from_utf8_lossy(&req.body);
            pages::keys_secret(runtime, &body)
        }
        ("GET", "/extensions") => pages::extensions(runtime),
        ("POST", "/mcp/add") => {
            let body = String::from_utf8_lossy(&req.body).into_owned();
            pages::mcp_add(runtime, &body).await
        }
        ("POST", "/mcp/rm") => {
            let body = String::from_utf8_lossy(&req.body).into_owned();
            pages::mcp_rm(runtime, &body).await
        }
        ("GET", "/doctor") => pages::doctor(runtime),
        ("POST", "/backup") => pages::backup_now(runtime),
        ("POST", "/doctor/fix") => pages::doctor_fix(runtime),
        ("POST", "/keys/secret/rm") => {
            let body = String::from_utf8_lossy(&req.body);
            pages::keys_secret_rm(runtime, &body)
        }
        ("GET", "/logs") => pages::logs(runtime),
        ("POST", "/identities/link") => {
            let body = String::from_utf8_lossy(&req.body);
            pages::identity_link(runtime, &body)
        }
        ("POST", "/identities/unlink") => {
            let body = String::from_utf8_lossy(&req.body);
            pages::identity_unlink(runtime, &body)
        }
        ("POST", "/identities/role") => {
            let body = String::from_utf8_lossy(&req.body);
            pages::identity_role(store, &body)
        }
        ("POST", "/profiles/create") => {
            let body = String::from_utf8_lossy(&req.body).into_owned();
            pages::profile_action("create", &body).await
        }
        ("POST", "/profiles/use") => {
            let body = String::from_utf8_lossy(&req.body).into_owned();
            pages::profile_action("use", &body).await
        }
        ("POST", "/profiles/remove") => {
            let body = String::from_utf8_lossy(&req.body).into_owned();
            pages::profile_action("remove", &body).await
        }
        ("GET", "/usage") => pages::usage(
            runtime,
            store,
            req.query.get("format").map(String::as_str),
        ),
        ("POST", "/settings") => {
            let body = String::from_utf8_lossy(&req.body).into_owned();
            pages::settings_apply(runtime, &body).await
        }
        ("GET", "/tasks") => pages::tasks(runtime, &who),
        ("POST", "/tasks/add") => {
            let body = String::from_utf8_lossy(&req.body).into_owned();
            pages::tasks_add(runtime, &body).await
        }
        ("POST", "/tasks/calendar") => {
            let body = String::from_utf8_lossy(&req.body).into_owned();
            pages::tasks_calendar(runtime, &body).await
        }
        ("POST", "/tasks/cron") => {
            let body = String::from_utf8_lossy(&req.body);
            pages::tasks_cron(runtime, &body)
        }
        ("POST", "/tasks/cancel") => {
            let body = String::from_utf8_lossy(&req.body);
            pages::tasks_cancel(runtime, &body).await
        }
        ("POST", "/tasks/snooze") => {
            let body = String::from_utf8_lossy(&req.body);
            pages::tasks_snooze(runtime, &body)
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
        ("GET", "/history") => {
            pages::history(runtime, req.query.get("q").map(String::as_str), &who)
        }
        ("GET", p) if p.starts_with("/history/show/") => {
            let turn_id = p.strip_prefix("/history/show/").unwrap_or("");
            pages::history_show(runtime, store, turn_id, &who)
        }
        ("GET", "/memory") => pages::memory(runtime, &who),
        ("POST", "/memory/remember") => {
            let body = String::from_utf8_lossy(&req.body);
            pages::memory_remember(runtime, &body, &who)
        }
        ("POST", "/memory/undo") => pages::memory_undo(runtime, &who),
        ("POST", "/memory/edit") => {
            let body = String::from_utf8_lossy(&req.body);
            pages::memory_edit(runtime, &body, &who)
        }
        ("POST", "/memory/candidate") => {
            let body = String::from_utf8_lossy(&req.body);
            pages::memory_candidate(runtime, &body, &who)
        }
        ("GET", "/approvals") => pages::approvals(runtime, store, &who),
        ("POST", "/approvals/decide") => {
            let body = String::from_utf8_lossy(&req.body);
            let wants_json = req
                .headers_get("accept")
                .is_some_and(|a| a.contains("application/json"));
            if wants_json {
                pages::approvals_decide_json(runtime, &body, &source).await
            } else {
                pages::approvals_decide(runtime, &body, &source).await
            }
        }
        ("GET", "/audit") => pages::audit(runtime, store),
        ("GET", "/intents") => pages::intents(runtime, store),
        _ => Response::not_found(),
    }
}

/// `GET /events`: server-sent events from the bus. Authenticated like any
/// page; the connection lives until the client or the daemon goes away.
/// Each event carries an SSE `id:` (the envelope's `seq`), so a reconnect
/// with `Last-Event-ID` replays exactly the gap it missed from the bus
/// ring.
async fn serve_events(
    req: &Request,
    auth: &Auth,
    _runtime: &Runtime,
    store: &Arc<Store>,
    bus: Arc<EventBus>,
    mut socket: tokio::net::TcpStream,
) {
    let Some(principal) = auth.principal(store, &req.cookies) else {
        write_response(&mut socket, Response::unauthorized()).await;
        return;
    };
    // A role's stream carries only its own turn-scoped events: approval
    // cards, admin notices and other subjects' turns never cross it.
    let visible = |env: &Envelope| envelope_visible(&principal, env);
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nConnection: keep-alive\r\n\r\n";
    if socket.write_all(head.as_bytes()).await.is_err() {
        return;
    }
    // A reconnect names the last seq it saw (SSE `Last-Event-ID`, or
    // `?since=` for a first page refresh); 0 means live only.
    let since = req
        .headers_get("last-event-id")
        .and_then(|v| v.parse::<u64>().ok())
        .or_else(|| req.query_param("since").and_then(|v| v.parse::<u64>().ok()))
        .unwrap_or(0);
    let (mut rx, replay) = if since > 0 {
        bus.subscribe_since(since)
    } else {
        (bus.subscribe(), Vec::new())
    };
    // Events that raced the snapshot ride both the replay and the live
    // stream; remember the last replayed seq and skip it live.
    let mut last_sent = replay.last().map_or(0, |env| env.seq);
    for env in replay {
        if !visible(&env) {
            continue;
        }
        if write_sse(&mut socket, &env).await.is_err() {
            return;
        }
    }
    let _ = socket.flush().await;
    let mut ping = tokio::time::interval(Duration::from_secs(25));
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            env = rx.recv() => {
                let env = match env {
                    Ok(env) if env.seq > last_sent => {
                        last_sent = env.seq;
                        env
                    }
                    Ok(_) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        std::sync::Arc::new(Envelope::new(
                            0,
                            AgentEvent::Notice {
                                text: format!("(missed {n} events)"),
                            },
                        ))
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                };
                if matches!(env.ev, AgentEvent::Hello { .. } | AgentEvent::Reply { .. }) {
                    continue;
                }
                // The synthetic "missed events" notice has seq 0 and no
                // source; it is safe to send to every stream.
                if env.seq != 0 && !visible(&env) {
                    continue;
                }
                if write_sse(&mut socket, &env).await.is_err() {
                    return;
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

/// The session cookie. `Secure` only when the published URL is https (TLS
/// is terminated by the operator's proxy); a plain-http local access still
/// works.
fn session_cookie(session: &str, runtime: &Runtime) -> String {
    let secure = runtime
        .config()
        .web
        .public_url
        .as_deref()
        .is_some_and(|u| u.starts_with("https://"));
    format!(
        "ka_session={session}; HttpOnly; SameSite=Strict; Path=/; Max-Age=86400{}",
        if secure { "; Secure" } else { "" }
    )
}

/// Whether one envelope may cross a principal's event stream. A role sees
/// only events stamped with its own source; admin-only events (`source:
/// None`) and every other role's turns stay out.
fn envelope_visible(principal: &Principal, env: &Envelope) -> bool {
    if principal.is_admin() {
        return true;
    }
    env.source.as_deref() == Some(principal.source.as_str())
}

/// One SSE frame: `id:` (the seq, for `Last-Event-ID` reconnects) +
/// `data:` (the whole envelope, `seq` and `at_ms` included).
async fn write_sse(socket: &mut tokio::net::TcpStream, env: &Envelope) -> std::io::Result<()> {
    let json = match serde_json::to_string(env) {
        Ok(json) => json,
        Err(_) => return Ok(()),
    };
    socket
        .write_all(format!("id: {}\ndata: {json}\n\n", env.seq).as_bytes())
        .await
}

/// Escape user-derived text for safe HTML rendering.
pub fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

/// `POST /api/notify {"text"}` / `POST /api/ask {"text","background"}`.
/// Bearer-only; 404 when no token is configured; 16 KiB bodies; a small
/// per-process token bucket (30/min).
async fn api_route(req: &Request, runtime: &Runtime, _store: &Arc<Store>) -> Response {
    use std::sync::atomic::{AtomicU64, Ordering};
    if req.method != "POST" {
        return Response::base(405, "application/json", "{}".into());
    }
    let secret = match ka_agent_core::secret::resolve(
        "web.api_token",
        &ka_agent_core::layout::default_root().join("secrets"),
    ) {
        Ok(t) => t,
        Err(_) => return Response::base(404, "application/json", "{}".into()),
    };
    let token = req
        .headers_get("authorization")
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if !constant_time_eq(token.as_bytes(), secret.trim().as_bytes()) {
        return Response::base(
            401,
            "application/json",
            "{\"error\":\"unauthorized\"}".into(),
        );
    }
    // 30 requests/min token bucket (process-wide).
    static WINDOW: AtomicU64 = AtomicU64::new(0);
    static COUNT: AtomicU64 = AtomicU64::new(0);
    let now_s = ka_agent_core::now_ms() as u64 / 1000;
    let window = WINDOW.load(Ordering::Relaxed);
    if now_s / 60 != window / 60 || window == 0 {
        WINDOW.store(now_s, Ordering::Relaxed);
        COUNT.store(0, Ordering::Relaxed);
    }
    if COUNT.fetch_add(1, Ordering::Relaxed) >= 30 {
        return Response::base(429, "application/json", "{\"error\":\"slow down\"}".into());
    }
    if req.body.len() > 16 * 1024 {
        return Response::base(413, "application/json", "{}".into());
    }
    let body: serde_json::Value =
        serde_json::from_slice(&req.body).unwrap_or(serde_json::Value::Null);
    let text = body
        .get("text")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .trim();
    if text.is_empty() {
        return Response::base(
            400,
            "application/json",
            "{\"error\":\"text required\"}".into(),
        );
    }
    match req.path.as_str() {
        "/api/notify" => {
            runtime.emit(ka_agent_runtime::AgentEvent::Notice {
                text: text.to_string(),
            });
            Response::base(200, "application/json", "{\"ok\":true}".into())
        }
        "/api/ask" => {
            let background = body
                .get("background")
                .and_then(|b| b.as_bool())
                .unwrap_or(false);
            let source = if background { "api-bg" } else { "api" };
            match runtime.handle_message(text, source).await {
                Ok(()) => Response::base(
                    200,
                    "application/json",
                    "{\"ok\":true,\"queued\":true}".into(),
                ),
                Err(e) => Response::base(
                    500,
                    "application/json",
                    format!("{{\"error\":{}}}", serde_json::Value::String(e.to_string())),
                ),
            }
        }
        _ => Response::base(404, "application/json", "{}".into()),
    }
}

/// Length-independent comparison for the bearer token.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn env(source: Option<&str>) -> Envelope {
        Envelope::from(1, AgentEvent::Notice { text: "x".into() }, source)
    }

    #[test]
    fn user_streams_only_carry_their_own_source() {
        let guest = Principal::new("guest", "guest");
        assert_eq!(guest.source, "web:guest");
        assert!(envelope_visible(&guest, &env(Some("web:guest"))));
        assert!(!envelope_visible(&guest, &env(Some("web:other"))));
        assert!(!envelope_visible(&guest, &env(Some("telegram:42"))));
        // Escalations (approval cards, memory proposals) carry no source.
        assert!(!envelope_visible(&guest, &env(None)));
        // The operator sees everything.
        let admin = Principal::new(ka_agent_core::OPERATOR_USER, "admin");
        assert!(admin.is_admin());
        assert!(envelope_visible(&admin, &env(None)));
        assert!(envelope_visible(&admin, &env(Some("web:abc"))));
    }

    #[test]
    fn sessions_survive_in_the_store_and_expire() {
        let store = Store::open_in_memory().unwrap();
        let auth = Auth::new();
        let token = auth
            .new_session(&store, ka_agent_core::OPERATOR_USER, "admin")
            .expect("session");
        let mut cookies = HashMap::new();
        cookies.insert("ka_session".to_string(), token.clone());
        let principal = auth.principal(&store, &cookies).expect("principal");
        assert_eq!(principal.username, ka_agent_core::OPERATOR_USER);
        assert!(principal.is_admin());
        // A different Auth (a daemon restart) still resolves it: the table is
        // the state.
        let auth2 = Auth::new();
        assert!(auth2.principal(&store, &cookies).is_some());
        // Only the hash is stored.
        let rows = store.list_web_sessions().unwrap();
        assert_eq!(rows.len(), 1);
        assert_ne!(rows[0].token_hash, token);
        // A disabled account stops resolving.
        store
            .set_user_disabled(ka_agent_core::OPERATOR_USER, true)
            .unwrap();
        assert!(auth.principal(&store, &cookies).is_none());
        store
            .set_user_disabled(ka_agent_core::OPERATOR_USER, false)
            .unwrap();
        // Logout.
        assert!(store.delete_web_session(&sha256_hex(&token)).unwrap());
        assert!(auth.principal(&store, &cookies).is_none());
    }

    #[test]
    fn login_verifies_argon2_and_locks_out_after_five_failures() {
        let store = Store::open_in_memory().unwrap();
        let auth = Auth::new();
        let phc = hash_password("correct horse battery").unwrap();
        assert!(verify_password("correct horse battery", &phc));
        assert!(!verify_password("wrong", &phc));
        store
            .create_user(&ka_agent_store::UserRow {
                username: "ana".into(),
                display_name: "Ana".into(),
                pw_hash: Some(phc),
                role: "member".into(),
                quota_daily_usd: None,
                disabled: false,
                created_ms: 1,
                created_by: Some("test".into()),
            })
            .unwrap();
        // Wrong password: refused, no session.
        assert!(matches!(
            auth.login(&store, "ana", "nope"),
            Err(LoginError::Credentials)
        ));
        // Unknown user: the same message (no enumeration).
        assert!(matches!(
            auth.login(&store, "bob", "nope"),
            Err(LoginError::Credentials)
        ));
        // The fifth failure locks the username out, even with the right
        // password (the first wrong attempt above already counted).
        for _ in 0..4 {
            let _ = auth.login(&store, "ana", "nope");
        }
        assert!(matches!(
            auth.login(&store, "ana", "correct horse battery"),
            Err(LoginError::Locked)
        ));
        // Another account is unaffected.
        assert!(matches!(
            auth.login(&store, "bob", "x"),
            Err(LoginError::Credentials | LoginError::Locked) | Ok(_)
        ));
        // A fresh limiter (daemon restart) lets the right password in.
        let auth = Auth::new();
        assert!(auth.login(&store, "ana", "correct horse battery").unwrap().is_some());
    }

    #[test]
    fn reset_invite_sets_the_password_only_for_its_account() {
        let store = Store::open_in_memory().unwrap();
        let auth = Auth::new();
        store
            .create_user(&ka_agent_store::UserRow {
                username: "ana".into(),
                display_name: "Ana".into(),
                pw_hash: Some(hash_password("old-password").unwrap()),
                role: "member".into(),
                quota_daily_usd: None,
                disabled: false,
                created_ms: 1,
                created_by: Some("test".into()),
            })
            .unwrap();
        let code = "b".repeat(32);
        store
            .insert_invite(&ka_agent_store::InviteRow {
                code_hash: sha256_hex(&code),
                role: "member".into(),
                username: Some("ana".into()),
                groups: Vec::new(),
                created_ms: ka_agent_core::now_ms(),
                expires_ms: ka_agent_core::now_ms() + 60_000,
                used_ms: None,
                created_by: ka_agent_core::OPERATOR_USER.to_string(),
            })
            .unwrap();
        // A reset code cannot be aimed at a different account.
        assert!(matches!(
            auth.redeem_invite(&store, &code, "bob", "newpassword"),
            Err(LoginError::Invite(_))
        ));
        // Redeeming it sets ana's new password and never creates an account.
        let token = auth
            .redeem_invite(&store, &code, "ana", "newpassword")
            .unwrap()
            .expect("session");
        assert!(!token.is_empty());
        assert!(store.get_user("bob").unwrap().is_none());
        let phc = store.get_user("ana").unwrap().unwrap().pw_hash.unwrap();
        assert!(verify_password("newpassword", &phc));
        assert!(!verify_password("old-password", &phc));
        // Single use.
        assert!(auth
            .redeem_invite(&store, &code, "ana", "another-password")
            .is_err());
    }

    #[test]
    fn invite_redeem_creates_the_account_once() {
        let store = Store::open_in_memory().unwrap();
        let auth = Auth::new();
        let code = "a".repeat(32);
        store
            .insert_invite(&ka_agent_store::InviteRow {
                code_hash: sha256_hex(&code),
                role: "member".into(),
                username: None,
                groups: vec!["dev".into()],
                created_ms: ka_agent_core::now_ms(),
                expires_ms: ka_agent_core::now_ms() + 60_000,
                used_ms: None,
                created_by: ka_agent_core::OPERATOR_USER.to_string(),
            })
            .unwrap();
        // A code never takes over an existing account.
        assert!(matches!(
            auth.redeem_invite(&store, &code, "operator", "longpassword"),
            Err(LoginError::Invite(_))
        ));
        let token = auth
            .redeem_invite(&store, &code, "ana", "longpassword")
            .unwrap()
            .expect("session");
        assert!(!token.is_empty());
        let ana = store.get_user("ana").unwrap().unwrap();
        assert_eq!(ana.role, "member");
        assert!(ana.pw_hash.is_some());
        assert_eq!(store.groups_of("ana").unwrap(), vec!["dev"]);
        // The panel's own identity source resolves to the account.
        assert_eq!(
            store.identity_principal("web", "ana").unwrap(),
            Some(("ana".to_string(), "member".to_string()))
        );
        // Single use.
        assert!(matches!(
            auth.redeem_invite(&store, &code, "bea", "longpassword"),
            Err(LoginError::Invite(_))
        ));
        // A wrong code is refused and counted.
        assert!(matches!(
            auth.redeem_invite(&store, "deadbeef", "bea", "longpassword"),
            Err(LoginError::Invite(_))
        ));
    }

}
