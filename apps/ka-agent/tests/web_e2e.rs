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

    /// `ka-agent web-token --port <port>` → the login URL it prints.
    fn web_token(&self) -> String {
        let out = Command::new(env!("CARGO_BIN_EXE_ka-agent"))
            .args(["web-token", "--port", &self.port.to_string()])
            .envs(self.env())
            .output()
            .unwrap();
        assert!(out.status.success(), "web-token exited {:?}", out.status);
        let stdout = String::from_utf8_lossy(&out.stdout);
        stdout
            .trim()
            .rsplit(' ')
            .next()
            .expect("login URL on stdout")
            .to_string()
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
    let child = Command::new(env!("CARGO_BIN_EXE_ka-agent"))
        .arg("run")
        .arg("--config")
        .arg(&cfg)
        .arg("--web-port")
        .arg(port.to_string())
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
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n");
    if let Some(c) = cookie {
        req.push_str(&format!("Cookie: {c}\r\n"));
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
    assert_eq!(resp.status, 302, "login should redirect: {url}");
    assert_eq!(resp.header("location"), Some("/"));
    let set = resp.header("set-cookie").expect("Set-Cookie on login");
    assert!(set.contains("HttpOnly"), "cookie not HttpOnly: {set}");
    assert!(set.contains("SameSite=Strict"), "cookie not Strict: {set}");
    resp.cookie()
}

/// The listening socket for `port` in `/proc/net/tcp`, as (address, port).
/// Linux-specific: proves the bind address, which a connect test cannot.
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
    // reports addresses little-endian in hex.
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
    assert_eq!(replay.status, 302, "fresh token should log in");
    let replay = request(port, "GET", path, None, "");
    assert_eq!(
        replay.status, 401,
        "consumed token must not authenticate again"
    );

    // A session reaches every page.
    let dashboard = request(port, "GET", "/", Some(&cookie), "");
    assert_eq!(dashboard.status, 200);
    assert!(dashboard.body.contains("Dashboard"), "dashboard body");
    assert!(
        dashboard.body.contains("session"),
        "dashboard shows session id"
    );
    assert!(dashboard.body.contains("budget"), "dashboard shows budget");

    for (path, needle) in [
        ("/memory", "Memory"),
        ("/history", "History"),
        ("/approvals", "Approvals"),
        ("/audit", "Intents"),
        ("/intents", "Intents"),
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
}

#[test]
fn oversized_body_is_refused_without_buffering() {
    let root = temp_root("oversize");
    let port = free_port();
    let daemon = spawn_daemon(&root, port);
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
    // session the daemon actually runs (its id is on the dashboard).
    let dashboard = request(port, "GET", "/", Some(&cookie), "");
    let marker = "<td>session</td><td>";
    let start = dashboard.body.find(marker).expect("session row") + marker.len();
    let session_id = dashboard.body[start..]
        .split("</td>")
        .next()
        .unwrap()
        .to_string();
    assert!(
        !session_id.is_empty(),
        "session id parsed from the dashboard"
    );

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
