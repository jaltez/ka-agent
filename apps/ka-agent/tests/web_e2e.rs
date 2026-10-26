//! M7 e2e: the web UI over the real binary — loopback bind, single-use login
//! token, session cookie flags, every page reachable, POST-only mutations,
//! and HTML escaping of user-derived text. A raw `TcpStream` client keeps the
//! test dependency-free and lets it assert raw headers (Set-Cookie flags).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn temp_root(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("ka-agent-web-e2e-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

struct Daemon {
    child: Child,
    data: std::path::PathBuf,
    port: u16,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Daemon {
    fn env(&self) -> [(String, std::path::PathBuf); 1] {
        [("KA_AGENT_DATA_DIR".to_string(), self.data.clone())]
    }

    /// `ka-agent dashboard --no-open` → the one-shot login URL.
    fn web_token(&self) -> String {
        // The dashboard needs the daemon's admin socket; wait for it.
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if !self.data.join("admin.sock").exists() {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            break;
        }
        // The web listener binds shortly after the admin socket; retry
        // while the daemon is still publishing `web.url` (a plain
        // "web UI is off" here is a startup race, not a verdict).
        let deadline = Instant::now() + Duration::from_secs(15);
        let out = loop {
            let out = Command::new(env!("CARGO_BIN_EXE_ka-agent"))
                .args(["dashboard", "--no-open"])
                .envs(self.env())
                .output()
                .unwrap();
            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
            if out.status.success() {
                break out;
            }
            assert!(
                stderr.contains("web UI is off") && Instant::now() < deadline,
                "dashboard exited {:?} stderr={stderr}",
                out.status
            );
            std::thread::sleep(Duration::from_millis(100));
        };
        let stdout = String::from_utf8_lossy(&out.stdout);
        let line = stdout
            .lines()
            .find(|l| l.starts_with("open: "))
            .expect("login URL on stdout");
        line["open: ".len()..].trim().to_string()
    }
}

fn spawn_daemon(root: &std::path::Path, port: u16) -> Daemon {
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let cfg = root.join("ka-agent.toml");
    std::fs::write(
        &cfg,
        format!(
            "workspace = {:?}\nmode = \"guarded\"\nturn_grace_secs = 5\n",
            ws.display().to_string()
        ),
    )
    .unwrap();
    let data = root.join("data");
    // A scripted speaker: web chat turns must stream answers without keys.
    let sc = root.join("web.json");
    std::fs::write(
        &sc,
        r#"{ "steps": [ {"text": "web-hello"} ], "stop": "done", "cost_usd": 0.0001, "tokens": 3 }"#,
    )
    .unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_ka-agent"))
        .arg("run")
        .arg("--config")
        .arg(&cfg)
        .arg("--web-port")
        .arg(port.to_string())
        .arg("--scenario")
        .arg(&sc)
        .env("KA_AGENT_DATA_DIR", &data)
        .env_remove("KA_MODEL")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    Daemon { child, data, port }
}

// ---------------------------------------------------------------------------
// Minimal HTTP/1.1 client (raw sockets: exact status line + headers)
// ---------------------------------------------------------------------------

struct Http {
    status: u16,
    headers: Vec<String>,
    body: String,
}

impl Http {
    fn header(&self, name: &str) -> Option<&str> {
        let prefix = format!("{}: ", name.to_ascii_lowercase());
        self.headers
            .iter()
            .find(|h| h.to_ascii_lowercase().starts_with(&prefix))
            .map(|h| h[prefix.len()..].trim())
    }

    fn cookie(&self) -> String {
        let set = self.header("set-cookie").expect("Set-Cookie header");
        set.split(';').next().unwrap().trim().to_string()
    }
}

fn request(port: u16, method: &str, path: &str, cookie: Option<&str>, body: &str) -> Http {
    request_h(port, method, path, cookie, body, &[])
}

fn request_h(
    port: u16,
    method: &str,
    path: &str,
    cookie: Option<&str>,
    body: &str,
    extra: &[(&str, &str)],
) -> Http {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n");
    if let Some(c) = cookie {
        req.push_str(&format!("Cookie: {c}\r\n"));
    }
    for (name, value) in extra {
        req.push_str(&format!("{name}: {value}\r\n"));
    }
    if !body.is_empty() {
        req.push_str("Content-Type: application/x-www-form-urlencoded\r\n");
        req.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    req.push_str("\r\n");
    req.push_str(body);
    stream.write_all(req.as_bytes()).unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).unwrap();
    let split = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("response head terminator");
    let head = String::from_utf8_lossy(&buf[..split]).into_owned();
    let mut lines = head.lines();
    let status_line = lines.next().expect("status line");
    let status = status_line
        .split_whitespace()
        .nth(1)
        .expect("status code")
        .parse()
        .expect("numeric status");
    Http {
        status,
        headers: lines.map(str::to_string).collect(),
        body: String::from_utf8_lossy(&buf[split + 4..]).into_owned(),
    }
}

fn wait_web(port: u16, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("web UI did not start on port {port} within {timeout:?}");
}

/// Insert a session row so turns/approvals can reference it (FK).
fn seed_session(store: &ka_agent_store::Store, id: &str, workspace: &std::path::Path) {
    store
        .insert_session(&ka_agent_core::Session {
            id: ka_agent_core::SessionId(id.to_string()),
            workspace: workspace.to_path_buf(),
            strand_id: None,
            strand_path: None,
            status: ka_agent_core::SessionStatus::Idle,
            created_ms: ka_agent_core::now_ms(),
            updated_ms: ka_agent_core::now_ms(),
        })
        .unwrap();
}

/// Log in and return the session cookie value.
fn login(daemon: &Daemon) -> String {
    let url = daemon.web_token();
    let path = url.split("127.0.0.1").nth(1).expect("URL host");
    let path = path.trim_start_matches(|c: char| c.is_ascii_digit() || c == ':');
    let resp = request(daemon.port, "GET", path, None, "");
    assert_eq!(resp.status, 303, "login should redirect: {url}");
    assert_eq!(resp.header("location"), Some("/"));
    let set = resp.header("set-cookie").expect("Set-Cookie on login");
    assert!(set.contains("HttpOnly"), "cookie not HttpOnly: {set}");
    assert!(set.contains("SameSite=Strict"), "cookie not Strict: {set}");
    resp.cookie()
}

/// The listening socket for `port` in `/proc/net/tcp`, as (address, port).
/// Linux-specific: proves the bind address, which a connect test cannot.
#[cfg(target_os = "linux")]
fn listening_addr(port: u16) -> Option<String> {
    let table = std::fs::read_to_string("/proc/net/tcp").ok()?;
    for line in table.lines().skip(1) {
        let mut cols = line.split_whitespace();
        let _sl = cols.next()?;
        let local = cols.next()?;
        let (addr, p) = local.split_once(':')?;
        let state = cols.nth(1)?;
        if state == "0A" && u16::from_str_radix(p, 16) == Ok(port) {
            return Some(addr.to_string());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn auth_flow_gates_every_page_and_token_is_single_use() {
    let root = temp_root("auth");
    let port = free_port();
    let daemon = spawn_daemon(&root, port);
    wait_web(port, Duration::from_secs(30));

    // Exit criterion: the listener is loopback-only (127.0.0.1) — /proc/net/tcp
    // reports addresses little-endian in hex. (Linux-only probe.)
    #[cfg(target_os = "linux")]
    match listening_addr(port) {
        Some(addr) => assert_eq!(
            addr.to_ascii_uppercase(),
            "0100007F",
            "web UI must bind 127.0.0.1 only, found {addr}"
        ),
        None => panic!("no LISTEN socket for port {port} in /proc/net/tcp"),
    }

    // Unauthenticated: every page refuses.
    for path in [
        "/",
        "/history",
        "/memory",
        "/approvals",
        "/audit",
        "/intents",
    ] {
        let resp = request(port, "GET", path, None, "");
        assert_eq!(
            resp.status, 401,
            "unauthenticated {path} was {}",
            resp.status
        );
    }

    let cookie = login(&daemon);

    // The token is single-use: replaying the URL must not mint a new session.
    let url = daemon.web_token();
    let path = url.split("127.0.0.1").nth(1).unwrap();
    let path = path.trim_start_matches(|c: char| c.is_ascii_digit() || c == ':');
    let replay = request(port, "GET", path, None, "");
    assert_eq!(replay.status, 303, "fresh token should log in");
    let replay = request(port, "GET", path, None, "");
    assert_eq!(
        replay.status, 401,
        "consumed token must not authenticate again"
    );

    // A session reaches every page. The chat page loads our one JS file
    // under a strict CSP.
    let chat = request(port, "GET", "/", Some(&cookie), "");
    assert_eq!(chat.status, 200);
    assert!(
        chat.body.contains("script src=\"/static/chat.js\""),
        "chat page body: {}",
        chat.body
    );
    let csp = chat
        .header("content-security-policy")
        .expect("CSP on HTML responses");
    assert!(csp.contains("script-src 'self'"), "CSP: {csp}");
    assert!(csp.contains("frame-ancestors 'none'"), "CSP: {csp}");
    let js = request(port, "GET", "/static/chat.js", Some(&cookie), "");
    assert_eq!(js.status, 200);
    assert!(js.body.contains("EventSource"), "chat.js served");

    // The status page (the old dashboard) renders the operator report.
    let dashboard = request(port, "GET", "/status", Some(&cookie), "");
    assert_eq!(dashboard.status, 200);
    // The redesign renders the report as stat cards (today's spend, the
    // worker state) instead of one <pre>.
    assert!(
        dashboard.body.contains("today") && dashboard.body.contains("$"),
        "status shows spend: {}",
        dashboard.body
    );
    assert!(
        dashboard.body.contains("stopped") || dashboard.body.contains("running"),
        "status shows worker: {}",
        dashboard.body
    );

    for (path, needle) in [
        ("/memory", "Memory"),
        ("/history", "History"),
        ("/approvals", "Approvals"),
        ("/audit", "Audit"),
        ("/intents", "Intents"),
        ("/tasks", "Tasks"),
        ("/contacts", "Contacts"),
        ("/skills", "Skills"),
        ("/history?q=anything", "History"),
    ] {
        let resp = request(port, "GET", path, Some(&cookie), "");
        assert_eq!(resp.status, 200, "{path} was {}", resp.status);
        assert!(resp.body.contains(needle), "{path} missing {needle:?}");
    }

    // Mutations are POST-only: no GET route may perform one.
    let get_decide = request(
        port,
        "GET",
        "/approvals/decide?id=x&decision=y",
        Some(&cookie),
        "",
    );
    assert_eq!(
        get_decide.status, 404,
        "GET /approvals/decide must not exist"
    );
    let get_edit = request(port, "GET", "/memory/edit", Some(&cookie), "");
    assert_eq!(get_edit.status, 404, "GET /memory/edit must not exist");

    // The memory editor accepts a proposal and redirects (POST mutation).
    let edited = request(
        port,
        "POST",
        "/memory/edit",
        Some(&cookie),
        "text=A+durable+fact",
    );
    assert_eq!(
        edited.status, 303,
        "memory edit should redirect, got {}",
        edited.status
    );
    assert_eq!(edited.header("location"), Some("/memory"));
    let page = request(port, "GET", "/memory", Some(&cookie), "");
    assert!(
        page.body.contains("A durable fact"),
        "accepted entry must render on /memory"
    );

    // An empty proposal is rejected without touching the store.
    let empty = request(port, "POST", "/memory/edit", Some(&cookie), "text=");
    assert_eq!(empty.status, 200, "empty edit renders a page");

    // Unknown paths 404 rather than fall through to a page.
    assert_eq!(
        request(port, "GET", "/../etc/passwd", Some(&cookie), "").status,
        404
    );

    // The chat-send endpoint is CSRF-guarded: no header, no service.
    let no_header = request(port, "POST", "/chat/send", Some(&cookie), "text=hi");
    assert_eq!(no_header.status, 403, "missing X-Ka-Agent must be refused");
    assert!(no_header.body.contains("X-Ka-Agent"));

    // A command through the router replies as JSON.
    let cmd = request_h(
        port,
        "POST",
        "/chat/send",
        Some(&cookie),
        "text=%2Fstatus",
        &[("X-Ka-Agent", "1")],
    );
    assert_eq!(cmd.status, 200);
    cmd.header("content-type").expect("json content type");
    assert!(
        cmd.header("content-type")
            .unwrap()
            .starts_with("application/json"),
        "chat/send replies JSON"
    );
    // (The reply body carries the rendered status; the queue line proves it.)
    assert!(cmd.body.contains("queue"), "status JSON: {}", cmd.body);

    // Non-ASCII form input arrives intact (percent-decoded as UTF-8).
    let accented = request_h(
        port,
        "POST",
        "/chat/send",
        Some(&cookie),
        "text=%C3%A9",
        &[("X-Ka-Agent", "1")],
    );
    assert_eq!(accented.status, 200, "accented send: {}", accented.body);

    // Live stream: an open /events socket sees the inbound echo and the
    // scripted answer's delta for a message sent while it is open. Slow
    // runners may still be finishing the previous turn (this one queues),
    // so read until both markers arrive, not for a fixed window.
    let events = open_events(port, &cookie);
    let sent = request_h(
        port,
        "POST",
        "/chat/send",
        Some(&cookie),
        "text=hello+stream",
        &[("X-Ka-Agent", "1")],
    );
    assert_eq!(sent.status, 200);
    let stream = read_events_until(
        events,
        &["\"ev\":\"inbound\"", "\"ev\":\"delta\""],
        Duration::from_secs(30),
    );
    assert!(stream.contains("\"ev\":\"inbound\""), "stream: {stream}");
    assert!(stream.contains("\"ev\":\"delta\""), "stream: {stream}");
}

/// Open a raw SSE connection (blocking socket, read timeout 60 s).
fn open_events(port: u16, cookie: &str) -> std::net::TcpStream {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .unwrap();
    let req = format!(
        "GET /events HTTP/1.1\r\nHost: 127.0.0.1\r\nCookie: {cookie}\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).unwrap();
    stream
}

/// Read the SSE stream until every marker is seen (or `timeout` passes);
/// returns everything received.
fn read_events_until(
    mut stream: std::net::TcpStream,
    markers: &[&str],
    timeout: Duration,
) -> String {
    use std::io::Read as _;
    let deadline = Instant::now() + timeout;
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        let text = String::from_utf8_lossy(&buf).into_owned();
        if markers.iter().all(|m| text.contains(m)) {
            return text;
        }
        if Instant::now() >= deadline {
            return text;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        stream
            .set_read_timeout(Some(remaining.min(Duration::from_millis(500))))
            .ok();
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(_) if Instant::now() < deadline => continue,
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

#[test]
fn memory_editor_stages_diffs_and_applies_decisions() {
    let root = temp_root("editor");
    let port = free_port();
    let daemon = spawn_daemon(&root, port);
    wait_web(port, Duration::from_secs(30));
    let cookie = login(&daemon);

    // A proposal is staged for review, not written.
    let staged = request(
        port,
        "POST",
        "/memory/edit",
        Some(&cookie),
        "text=Facts+stay+<i>escaped</i>",
    );
    assert_eq!(staged.status, 303, "proposal should redirect");
    let page = request(port, "GET", "/memory", Some(&cookie), "");
    assert_eq!(page.status, 200);
    assert!(
        page.body.contains("proposes:"),
        "the review inbox must list the proposal"
    );
    // The diff preview carries the added line, escaped.
    assert!(
        page.body
            .contains("+ - Facts stay &lt;i&gt;escaped&lt;/i&gt;"),
        "diff must show the escaped addition"
    );
    assert!(
        !page.body.contains("<i>escaped</i>"),
        "raw markup leaked into the diff"
    );
    let id = page
        .body
        .split("name=id value=\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("candidate id in the form")
        .to_string();

    // Accept: the entry lands and the queue empties.
    let accepted = request(
        port,
        "POST",
        "/memory/candidate",
        Some(&cookie),
        &format!("id={id}&decision=y"),
    );
    assert_eq!(accepted.status, 303, "accept should redirect");
    let after = request(port, "GET", "/memory", Some(&cookie), "");
    assert!(
        after.body.contains("none awaiting review"),
        "review queue must drain: {}",
        after.body
    );
    assert!(
        after.body.contains("&lt;i&gt;escaped&lt;/i&gt;"),
        "accepted entry must render"
    );
    let facts = std::fs::read_to_string(daemon.data.join("memory/facts.md")).unwrap_or_default();
    assert!(
        facts.contains("Facts stay <i>escaped</i>"),
        "facts.md must carry the entry: {facts}"
    );

    // Reject: staged and dropped without touching the file.
    request(
        port,
        "POST",
        "/memory/edit",
        Some(&cookie),
        "text=Rejected+proposal",
    );
    let page = request(port, "GET", "/memory", Some(&cookie), "");
    let id = page
        .body
        .split("name=id value=\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("second candidate id")
        .to_string();
    let rejected = request(
        port,
        "POST",
        "/memory/candidate",
        Some(&cookie),
        &format!("id={id}&decision=n"),
    );
    assert_eq!(rejected.status, 303, "reject should redirect");
    // An unknown id is reported escaped — it is operator input, and the
    // crate's invariant is that every user-derived string is escaped.
    let reflected = request(
        port,
        "POST",
        "/memory/candidate",
        Some(&cookie),
        "id=<img src=x onerror=alert(1)>&decision=n",
    );
    assert_eq!(reflected.status, 200, "unknown id renders a page");
    assert!(
        reflected.body.contains("&lt;img"),
        "reflected id must be escaped: {}",
        reflected.body
    );
    assert!(
        !reflected.body.contains("<img "),
        "raw markup leaked through the unknown-id path"
    );
    let facts = std::fs::read_to_string(daemon.data.join("memory/facts.md")).unwrap_or_default();
    assert!(
        !facts.contains("Rejected proposal"),
        "a rejected candidate must not touch facts.md: {facts}"
    );
}

#[test]
fn malformed_requests_do_not_kill_the_server() {
    let root = temp_root("malformed");
    let port = free_port();
    // Held for the lifetime of the test: dropping it kills the server.
    let _daemon_holder = spawn_daemon(&root, port);
    wait_web(port, Duration::from_secs(30));

    // A header flood plus a cookie flood, built once and borrowed below.
    let mut flood = b"GET / HTTP/1.1\r\n".to_vec();
    for _ in 0..2000 {
        flood.extend_from_slice(b"X-Pad: 0\r\n");
    }
    flood.extend_from_slice(b"Cookie: ");
    for _ in 0..500 {
        flood.extend_from_slice(b"a=1; ");
    }
    flood.extend_from_slice(b"ka_session=x\r\nConnection: close\r\n\r\n");

    let cases: Vec<&[u8]> = vec![
        // Not HTTP at all.
        b"\x00\x01garbage\x00\xff\xfe",
        // Request line with NULs and no terminator shape.
        b"GET /\x00 HTTP/1.1\r\n\r\n",
        // Headers only, no request line.
        b"Host: x\r\n\r\n",
        // Absolute-form target (must not match a route → 404, no bypass).
        b"GET http://127.0.0.1/ HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        // Path normalization attempts.
        b"GET /history/../ HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        b"GET //memory HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        // Header flood (many small headers) and a cookie flood.
        &flood,
        // POST without Content-Length (empty body) and with a lying one.
        b"POST /memory/edit HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        b"POST /memory/edit HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\nConnection: close\r\n\r\nab",
        // Chunked encoding is not supported: no Content-Length → empty body.
        b"POST /memory/edit HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n5\r\nhello\r\n0\r\n\r\n",
        // usize::MAX in Content-Length: must be refused before any
        // arithmetic (overflow → panic), not just as "too large".
        b"POST /x HTTP/1.1\r\nHost: x\r\nContent-Length: 18446744073709551615\r\nConnection: close\r\n\r\n",
        // Two declarations of Content-Length (request smuggling shape).
        b"POST /memory/edit HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\nContent-Length: 0\r\nConnection: close\r\n\r\nhello",
    ];
    for (i, raw) in cases.iter().enumerate() {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.write_all(raw).unwrap();
        // Done writing: EOF lets the parser settle a short body immediately
        // instead of holding the connection to its deadline.
        use std::net::Shutdown;
        let _ = stream.shutdown(Shutdown::Write);
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).unwrap();
        // Whatever comes back (a response or a closed connection), the server
        // must survive: check liveness after each case.
        let alive = request(port, "GET", "/healthz", None, "");
        assert_eq!(alive.status, 200, "server died after case {i}");
        if !buf.is_empty() {
            let head = String::from_utf8_lossy(&buf[..buf.len().min(40)]).into_owned();
            assert!(
                head.starts_with("HTTP/1.1 "),
                "case {i}: unexpected bytes {head:?}"
            );
        }
    }
}

#[test]
fn oversized_body_is_refused_without_buffering() {
    let root = temp_root("oversize");
    let port = free_port();
    // Held for the lifetime of the test: dropping it kills the server.
    let _daemon = spawn_daemon(&root, port);
    wait_web(port, Duration::from_secs(30));

    // A declared Content-Length beyond the cap must not be buffered: the
    // server drops the connection instead of reading gigabytes.
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .write_all(
            b"POST /memory/edit HTTP/1.1\r\nHost: 127.0.0.1\r\n\
              Content-Length: 400000000\r\nConnection: close\r\n\r\n",
        )
        .unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).unwrap();
    assert!(
        buf.is_empty(),
        "server should close without a response, got {} bytes",
        buf.len()
    );
}

#[test]
fn user_derived_text_is_html_escaped() {
    let root = temp_root("xss");
    let port = free_port();
    let daemon = spawn_daemon(&root, port);
    wait_web(port, Duration::from_secs(30));
    let cookie = login(&daemon);

    // Seed a turn whose inbound text carries script tags, straight into the
    // store the daemon is reading (WAL: one writer, concurrent readers).
    let store = ka_agent_store::Store::open(&daemon.data.join("state.db")).unwrap();
    seed_session(&store, "web-xss", &root.join("ws"));
    let session = ka_agent_core::SessionId("web-xss".to_string());
    let turn = ka_agent_core::Turn {
        id: ka_agent_core::TurnId("t-xss".to_string()),
        session: session.clone(),
        inbound_text: "hello <script>alert('pwned')</script>".to_string(),
        status: ka_agent_core::TurnStatus::Done,
        stop: Some("done".to_string()),
        error_class: None,
        error_message: None,
        cost_usd: 0.0,
        tokens: 0,
        created_ms: ka_agent_core::now_ms(),
        updated_ms: ka_agent_core::now_ms(),
    };
    store.insert_turn(&turn).unwrap();
    let chunk = ka_agent_core::OutboxChunk {
        turn: turn.id.clone(),
        chunk_index: 0,
        text: "<img src=x onerror=alert(1)>".to_string(),
        status: ka_agent_core::OutboxStatus::Sent,
        channel_message_id: None,
        created_ms: ka_agent_core::now_ms(),
    };
    store.push_chunk(&chunk).unwrap();

    let resp = request(port, "GET", "/history/show/t-xss", Some(&cookie), "");
    assert_eq!(resp.status, 200, "turn view status");
    assert!(
        resp.body.contains("&lt;script&gt;"),
        "script tag must be escaped: {}",
        resp.body
    );
    assert!(
        !resp.body.contains("<script>alert"),
        "raw script tag leaked into the page"
    );
    assert!(resp.body.contains("&lt;img"), "img tag must be escaped");
    assert!(
        !resp.body.contains("onerror=alert(1)>"),
        "raw event handler leaked into the page"
    );

    // Unknown turn → 404, not a page.
    assert_eq!(
        request(port, "GET", "/history/show/nope", Some(&cookie), "").status,
        404
    );
}

#[test]
fn approvals_card_renders_escaped_and_only_live_asks_decide() {
    let root = temp_root("approvals");
    let port = free_port();
    let daemon = spawn_daemon(&root, port);
    wait_web(port, Duration::from_secs(30));
    let cookie = login(&daemon);

    // The approvals inbox lists the live session's asks, so seed into the
    // session the daemon actually runs (adopted = newest row in the store).
    let session_id: String = {
        let db = rusqlite::Connection::open(daemon.data.join("state.db")).unwrap();
        db.query_row(
            "SELECT id FROM sessions ORDER BY created_ms DESC, rowid DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert!(!session_id.is_empty(), "session id read from the store");

    let store = ka_agent_store::Store::open(&daemon.data.join("state.db")).unwrap();
    let session = ka_agent_core::SessionId(session_id);
    let turn = ka_agent_core::TurnId("t-ask".to_string());
    store
        .insert_turn(&ka_agent_core::Turn {
            id: turn.clone(),
            session: session.clone(),
            inbound_text: "write the file".to_string(),
            status: ka_agent_core::TurnStatus::Running,
            stop: None,
            error_class: None,
            error_message: None,
            cost_usd: 0.0,
            tokens: 0,
            created_ms: ka_agent_core::now_ms(),
            updated_ms: ka_agent_core::now_ms(),
        })
        .unwrap();
    let approval = ka_agent_core::Approval::new(
        session.clone(),
        turn.clone(),
        ka_agent_core::ApprovalKind::KaAsk {
            ask_id: "ask-1".to_string(),
            question: "write <b>bold.txt</b>?".to_string(),
            detail: Some("diff <script>x</script>".to_string()),
            options: vec!["allow".to_string(), "deny".to_string()],
        },
        300,
    );
    let id = approval.id.as_str().to_string();
    store.insert_approval(&approval).unwrap();

    let page = request(port, "GET", "/approvals", Some(&cookie), "");
    assert_eq!(page.status, 200);
    assert!(
        page.body.contains("write &lt;b&gt;bold.txt&lt;/b&gt;?"),
        "question must be escaped: {}",
        page.body
    );
    assert!(
        !page.body.contains("<b>bold.txt</b>"),
        "raw question markup leaked"
    );
    assert!(page.body.contains("diff &lt;script&gt;"), "detail escaped");
    assert!(
        page.body.contains("expires "),
        "card shows expiry (parity with Telegram)"
    );
    assert!(
        page.body.contains("Allow") && page.body.contains("Deny"),
        "buttons"
    );
    assert!(page.body.contains(&id), "card carries the approval id");

    // The engine has no such live ask, so the decision must not be applied —
    // and must not 500 either.
    let decide = request(
        port,
        "POST",
        "/approvals/decide",
        Some(&cookie),
        &format!("id={id}&decision=y"),
    );
    assert_eq!(decide.status, 200, "decide must render, not 500");
    assert!(
        decide.body.contains("decision not applied"),
        "undeliverable decision is reported"
    );
    let row = store
        .pending_approvals(&session)
        .unwrap()
        .into_iter()
        .find(|a| a.id.as_str() == id)
        .expect("seeded ask still pending");
    assert_eq!(
        row.status,
        ka_agent_core::ApprovalStatus::Pending,
        "an undeliverable decision must not settle the ask"
    );
}

/// I1: the redesigned panel — chat bubbles with rendered (escaped) Markdown,
/// the sidebar, one stylesheet, strict CSP.
#[test]
fn chat_page_renders_markdown_and_keeps_csp() {
    let root = temp_root("redesign");
    let port = free_port();
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let cfg = root.join("ka-agent.toml");
    std::fs::write(
        &cfg,
        format!(
            "workspace = {:?}\nmode = \"guarded\"\nturn_grace_secs = 5\n",
            ws.display().to_string()
        ),
    )
    .unwrap();
    // The scripted ANSWER carries the markup the page must render.
    let sc = root.join("md.json");
    std::fs::write(
        &sc,
        r#"{ "steps": [ {"text": "**bold** and `code` and <script>alert(1)</script>"} ], "stop": "done", "cost_usd": 0.0001, "tokens": 3 }"#,
    )
    .unwrap();
    let data = root.join("data");
    let child = Command::new(env!("CARGO_BIN_EXE_ka-agent"))
        .arg("run")
        .arg("--config")
        .arg(&cfg)
        .arg("--web-port")
        .arg(port.to_string())
        .arg("--scenario")
        .arg(&sc)
        .env("KA_AGENT_DATA_DIR", &data)
        .env_remove("KA_MODEL")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut daemon = Daemon { child, data, port };
    wait_web(port, Duration::from_secs(30));
    let cookie = login(&daemon);
    request_h(
        port,
        "POST",
        "/chat/send",
        Some(&cookie),
        "text=render+please",
        &[("X-Ka-Agent", "1")],
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    let page = loop {
        let page = request(port, "GET", "/", Some(&cookie), "");
        if page.body.contains("<strong>bold</strong>") || Instant::now() > deadline {
            break page;
        }
        std::thread::sleep(Duration::from_millis(300));
    };
    assert!(page.body.contains("<style>"), "stylesheet embedded");
    assert!(page.body.contains("class=\"layout\""), "sidebar layout");
    assert!(page.body.contains("bubble"), "chat bubbles");
    assert!(page.body.contains("<strong>bold</strong>"), "rendered");
    assert!(page.body.contains("<code>code</code>"), "code rendered");
    assert!(
        !page.body.contains("<script>alert"),
        "script text stays text"
    );
    assert!(
        page.header("Content-Security-Policy")
            .unwrap_or("")
            .contains("script-src 'self'"),
        "CSP present"
    );
    daemon.child.kill().ok();
    daemon.child.wait().ok();
    let _ = std::fs::remove_dir_all(&root);
}

/// I2/I3: the settings form writes valid config and applies it live; the
/// usage page lists seeded days.
#[test]
fn settings_form_and_usage_page() {
    let root = temp_root("settings");
    let port = free_port();
    let mut daemon = spawn_daemon(&root, port);
    wait_web(port, Duration::from_secs(30));
    let cookie = login(&daemon);
    // Seed two days of spend straight into the store.
    {
        let store = ka_agent_store::Store::open(&daemon.data.join("state.db")).unwrap();
        let now = ka_agent_core::now_ms();
        let session = ka_agent_core::Session::new(root.join("ws"));
        store.insert_session(&session).unwrap();
        for (i, cost) in [0.0123_f64, 0.0456].into_iter().enumerate() {
            let mut t = ka_agent_core::Turn::new(session.id.clone(), "q".into());
            t.cost_usd = cost;
            t.created_ms = now - i as i64 * 86_400_000;
            // insert_turn then a raw update of created/cost is store-level;
            // reuse usage accounting via a direct row.
            store.insert_turn(&t).unwrap();
            let conn = rusqlite::Connection::open(daemon.data.join("state.db")).unwrap();
            conn.execute(
                "UPDATE turns SET cost_usd = ?2 WHERE id = ?1",
                rusqlite::params![t.id.as_str(), cost],
            )
            .unwrap();
        }
    }
    // Save a budget through the form.
    let resp = request_h(
        port,
        "POST",
        "/settings",
        Some(&cookie),
        "budgets_daily_usd=3",
        &[("X-Ka-Agent", "1")],
    );
    assert_eq!(resp.status, 200, "{}", resp.body);
    let cfg = std::fs::read_to_string(root.join("ka-agent.toml")).unwrap();
    assert!(cfg.contains("daily_usd = 3"), "{cfg}");
    // The POST answers the rendered status page with the confirmation.
    assert!(
        resp.body.contains("saved: budgets.daily_usd = 3"),
        "flash: {}",
        resp.body
    );
    // An invalid value is refused and nothing is written.
    request_h(
        port,
        "POST",
        "/settings",
        Some(&cookie),
        "mode=nonsense",
        &[("X-Ka-Agent", "1")],
    );
    assert!(
        !std::fs::read_to_string(root.join("ka-agent.toml"))
            .unwrap()
            .contains("nonsense")
    );
    // Usage page lists both days.
    let usage = request(port, "GET", "/usage", Some(&cookie), "");
    assert!(usage.body.contains("0.0456"), "usage: {}", usage.body);
    daemon.child.kill().ok();
    daemon.child.wait().ok();
    let _ = std::fs::remove_dir_all(&root);
}

/// 2.7 loopback API: /api/notify and /api/ask answer only with the right
/// bearer token — 404 while `web.api_token` is unset, 401 for a wrong
/// token, 200 (empty ok) for the right one. No cookie involved.
#[test]
fn api_requires_the_bearer_token() {
    let root = temp_root("api");
    let port = free_port();
    let daemon = spawn_daemon(&root, port);
    wait_web(port, Duration::from_secs(15));
    let body = "{\"text\":\"ping\"}";

    // No token configured: the API does not exist.
    let r = request_h(port, "POST", "/api/notify", None, body, &[]);
    assert_eq!(r.status, 404, "unset api_token must 404: {}", r.body);

    // Configure one (written where the daemon reads secrets).
    let secrets = daemon.data.join("secrets");
    std::fs::create_dir_all(&secrets).unwrap();
    std::fs::write(secrets.join("web.api_token"), "test-token-123").unwrap();

    let r = request_h(port, "POST", "/api/notify", None, body, &[]);
    assert_eq!(r.status, 401, "{}", r.body);
    let r = request_h(
        port,
        "POST",
        "/api/notify",
        None,
        body,
        &[("Authorization", "Bearer wrong-token")],
    );
    assert_eq!(r.status, 401, "{}", r.body);
    let r = request_h(
        port,
        "POST",
        "/api/notify",
        None,
        body,
        &[("Authorization", "Bearer test-token-123")],
    );
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(r.body.contains("\"ok\":true"), "{}", r.body);
    // Dropping the daemon kills it.
    drop(daemon);
    std::fs::remove_dir_all(&root).ok();
}
