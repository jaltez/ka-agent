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

/// A port that was free when checked and is unlikely to be handed out
/// again: the tests run in parallel, so binding :0 and releasing it (the
/// usual trick) can give two of them the same port before either daemon
/// binds. Each call takes a unique slot: within one test binary the
/// candidate ports never repeat (`slot * 11`), and the per-process base
/// (20 apart) keeps adjacent processes' 10-port check windows disjoint.
fn free_port() -> u16 {
    use std::sync::atomic::{AtomicU16, Ordering};
    static NEXT: AtomicU16 = AtomicU16::new(0);
    let slot = NEXT.fetch_add(1, Ordering::Relaxed) as u32;
    let base = 20_000 + (std::process::id() % 400) * 20;
    let candidate = (base + slot * 11) as u16;
    for i in 0..10 {
        let port = candidate + i;
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
    panic!("no free port in the test range");
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
            // Both wordings mean "the URL is not published yet": the old
            // "off" one (web.enabled false) and the C4 one (enabled but the
            // listener has not bound). A real bind failure keeps the same
            // C4 wording and fails after the deadline.
            assert!(
                (stderr.contains("web UI is off") || stderr.contains("web UI failed to start"))
                    && Instant::now() < deadline,
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
    spawn_daemon_extra(root, port, "")
}

/// The same daemon with extra config text (roles, tools) appended.
fn spawn_daemon_extra(root: &std::path::Path, port: u16, extra: &str) -> Daemon {
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let cfg = root.join("ka-agent.toml");
    std::fs::write(
        &cfg,
        format!(
            "workspace = {:?}\nmode = \"guarded\"\nturn_grace_secs = 5\n{extra}",
            ws.display().to_string()
        ),
    )
    .unwrap();
    let data = root.join("data");
    // A scripted speaker: web chat turns must stream answers without keys.
    let sc = root.join("web.json");
    std::fs::write(
        &sc,
        // The scripted speaker drains every remaining step on the first
        // model request, so only the first turn ever streams text; the live
        // SSE assertions below therefore watch the new turn's own events.
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

/// `ka-agent pair web --role <role>` against the daemon's data dir.
fn pair_web(daemon: &Daemon, role: &str) -> String {
    let config = daemon
        .data
        .parent()
        .expect("data root has a parent")
        .join("ka-agent.toml");
    let out = Command::new(env!("CARGO_BIN_EXE_ka-agent"))
        .args(["pair", "web", "--role", role])
        .arg("--config")
        .arg(&config)
        .envs(daemon.env())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "pair web: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout
        .lines()
        .find_map(|l| l.strip_prefix("web pairing code: "))
        .expect("pairing code on stdout")
        .split_whitespace()
        .next()
        .expect("code word")
        .to_string()
}

/// ADR-0002: a paired browser gets a role session — chat only, its own
/// history, and never an admin page. The operator's own session still sees
/// everything.
#[test]
fn web_role_pairing_is_chat_only() {
    let root = temp_root("roleweb");
    let port = free_port();
    let mut daemon = spawn_daemon_extra(
        &root,
        port,
        "[[role]]\nname = \"guest\"\nmode = \"readonly\"\nallow = [\"read\"]\n",
    );
    // Wait for the listener (and consume one admin login URL).
    let _admin_url = daemon.web_token();

    let code = pair_web(&daemon, "guest");
    let resp = request(
        port,
        "POST",
        "/pair",
        None,
        &format!("code={}", code.replace('-', "%2D")),
    );
    assert_eq!(resp.status, 303, "pairing redirects: {}", resp.body);
    let set = resp.header("set-cookie").expect("Set-Cookie on pairing");
    assert!(set.contains("HttpOnly"), "cookie not HttpOnly: {set}");
    assert!(set.contains("SameSite=Strict"), "cookie not Strict: {set}");
    let cookie = resp.cookie();

    // The role chat page renders, with its badge and no admin nav.
    let page = request(port, "GET", "/", Some(&cookie), "");
    assert_eq!(page.status, 200);
    assert!(page.body.contains("role guest"), "{}", page.body);
    assert!(
        !page.body.contains("href=\"/approvals\""),
        "no admin navigation on a role page"
    );
    // Every admin surface is refused.
    for path in ["/status", "/approvals", "/audit", "/memory", "/contacts"] {
        let resp = request(port, "GET", path, Some(&cookie), "");
        assert_eq!(resp.status, 403, "{path} must be admin-only: {}", resp.body);
    }
    let resp = request(port, "POST", "/settings", Some(&cookie), "mode=allow");
    assert_eq!(resp.status, 403, "a role cannot mutate settings");

    // A chat post routes as `web:<id>`, so the role gates apply server-side.
    let resp = request_h(
        port,
        "POST",
        "/chat/send",
        Some(&cookie),
        "text=%2Fmodel",
        &[("X-Ka-Agent", "1")],
    );
    assert_eq!(resp.status, 200);
    assert!(
        resp.body.contains("not available for role guest"),
        "{}",
        resp.body
    );

    // The operator's session is unaffected and sees the profiles page.
    let admin = login(&daemon);
    let status = request(port, "GET", "/status", Some(&admin), "");
    assert_eq!(status.status, 200);
    let profiles = request(port, "GET", "/profiles", Some(&admin), "");
    assert_eq!(profiles.status, 200);
    assert!(
        profiles.body.contains("linked identities"),
        "{}",
        profiles.body
    );
    assert!(
        profiles.body.contains("guest"),
        "the configured role is listed: {}",
        profiles.body
    );
    daemon.child.kill().ok();
    daemon.child.wait().ok();
}

/// Uploads, downloads and admin-issued pairing codes: the web chat's
/// Telegram-parity file path, scoped to the caller role.
#[test]
fn web_uploads_files_and_pairing_codes() {
    let root = temp_root("webup");
    let port = free_port();
    let mut daemon = spawn_daemon_extra(
        &root,
        port,
        "[[role]]\nname = \"guest\"\nmode = \"readonly\"\nallow = [\"read\"]\n",
    );
    let admin = login(&daemon);

    // Issue a guest pairing code from the admin UI (no CLI needed).
    let issued = request(port, "POST", "/pair/issue", Some(&admin), "role=guest");
    assert_eq!(issued.status, 200, "{}", issued.body);
    let start = issued.body.find("<code>").expect("code block") + "<code>".len();
    let end = issued.body[start..].find("</code>").unwrap() + start;
    let code = issued.body[start..end].to_string();
    assert!(code.contains('-'), "pairing code shape: {code}");
    let paired = request(port, "POST", "/pair", None, &format!("code={code}"));
    assert_eq!(paired.status, 303);
    let guest = paired.cookie();

    // Guest uploads one file; it lands under the role's own workspace.
    let up = request_h(
        port,
        "POST",
        "/upload",
        Some(&guest),
        "hello from the web",
        &[("X-Ka-Agent", "1"), ("X-Ka-Filename", "note.txt")],
    );
    assert_eq!(up.status, 200, "{}", up.body);
    let name = up
        .body
        .split("\"name\":\"")
        .nth(1)
        .and_then(|r| r.split('"').next())
        .expect("upload name")
        .to_string();
    assert!(name.contains("note.txt"), "{name}");
    let role_attachments = daemon.data.join("roles/guest/workspace/attachments");
    assert!(
        role_attachments.join(&name).is_file(),
        "stored under the role tree: {}",
        role_attachments.display()
    );

    // The guest can download its own file, but no traversal, and never a
    // file the operator uploaded.
    let file = request(port, "GET", &format!("/file/{name}"), Some(&guest), "");
    assert_eq!(file.status, 200);
    assert_eq!(file.body, "hello from the web");
    let trav = request(port, "GET", "/file/..%2Fconfig.toml", Some(&guest), "");
    assert_eq!(trav.status, 400, "{}", trav.body);
    let admin_up = request_h(
        port,
        "POST",
        "/upload",
        Some(&admin),
        "operator secret",
        &[("X-Ka-Agent", "1"), ("X-Ka-Filename", "op.txt")],
    );
    assert_eq!(admin_up.status, 200);
    let admin_name = admin_up
        .body
        .split("\"name\":\"")
        .nth(1)
        .and_then(|r| r.split('"').next())
        .unwrap()
        .to_string();
    let denied = request(
        port,
        "GET",
        &format!("/file/{admin_name}"),
        Some(&guest),
        "",
    );
    assert_eq!(
        denied.status, 404,
        "a role never reads the operator's files"
    );

    // Profile lifecycle is refused under a pinned data root (the e2e's
    // shape): the page says so instead of spawning the CLI.
    for (path, body) in [
        ("/profiles/create", "name=other"),
        ("/profiles/remove", "name=default&confirm=default"),
    ] {
        let resp = request(port, "POST", path, Some(&admin), body);
        assert_eq!(resp.status, 403, "{path}: {}", resp.body);
        assert!(resp.body.contains("pinned"), "{}", resp.body);
    }

    // The uploaded @ref is a normal reference for the chat.
    let chat = request_h(
        port,
        "POST",
        "/chat/send",
        Some(&guest),
        &format!(
            "text={}",
            urlencode(&format!("what is in @attachments/{name}?")).as_str()
        ),
        &[("X-Ka-Agent", "1")],
    );
    assert_eq!(chat.status, 200, "{}", chat.body);
    daemon.child.kill().ok();
    daemon.child.wait().ok();
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push_str("%20"),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// The admin surface added for TUI/CLI parity: models/mode/effort/persona,
/// config, queue, grants, keys, tasks (create + cron actions), per-role
/// skills, memory remember/undo — every page renders and every action
/// answers.
#[test]
fn admin_pages_cover_the_cli_surface() {
    let root = temp_root("adminpages");
    let port = free_port();
    let mut daemon = spawn_daemon_extra(
        &root,
        port,
        "[[role]]\nname = \"guest\"\nmode = \"readonly\"\nallow = [\"read\"]\n",
    );
    // A skill in the profile library (enable/disable targets).
    let skill = daemon.data.join("skills/demo/SKILL.md");
    std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
    std::fs::write(
        &skill,
        "---\nname: demo\ndescription: demo skill\n---\nbody\n",
    )
    .unwrap();
    let admin = login(&daemon);

    for (path, marker) in [
        ("/models", "Provider catalog"),
        ("/config", "Every key"),
        ("/sessions", "Recent conversations"),
        ("/queue", "queue is empty"),
        ("/grants", "scope grants"),
        ("/keys", "provider keys"),
        ("/tasks", "new cron"),
        ("/skills", "demo"),
        ("/extensions", "Live tools"),
        ("/doctor", "data root"),
        ("/logs", "daemon log tail"),
    ] {
        let page = request(port, "GET", path, Some(&admin), "");
        assert_eq!(page.status, 200, "{path}");
        assert!(
            page.body.contains(marker),
            "{path} misses {marker:?}: {}",
            page.body
        );
    }
    // Role sessions still cannot see any of it.
    let guest = {
        let code = {
            let issued = request(port, "POST", "/pair/issue", Some(&admin), "role=guest");
            let start = issued.body.find("<code>").unwrap() + 6;
            let end = issued.body[start..].find("</code>").unwrap() + start;
            issued.body[start..end].to_string()
        };
        request(port, "POST", "/pair", None, &format!("code={code}")).cookie()
    };
    let denied = request(port, "GET", "/models", Some(&guest), "");
    assert_eq!(denied.status, 403);

    // Mode/effort/model switches answer with the applied value.
    let mode = request(port, "POST", "/models/mode", Some(&admin), "mode=readonly");
    assert_eq!(mode.status, 200);
    assert!(mode.body.contains("mode → readonly"), "{}", mode.body);
    let effort = request(port, "POST", "/models/effort", Some(&admin), "level=low");
    assert_eq!(effort.status, 200);
    assert!(effort.body.contains("effort"), "{}", effort.body);

    // Grants add/remove round-trip.
    let add = request(
        port,
        "POST",
        "/grants/add",
        Some(&admin),
        "scope=http%3Aget%3Aexample.com",
    );
    assert_eq!(add.status, 303);
    let grants = request(port, "GET", "/grants", Some(&admin), "");
    assert!(grants.body.contains("http:get:example.com"));
    let rm = request(
        port,
        "POST",
        "/grants/rm",
        Some(&admin),
        "scope=http%3Aget%3Aexample.com",
    );
    assert_eq!(rm.status, 303);

    // Reminder creation through the shared router, then the list shows it.
    let add = request(
        port,
        "POST",
        "/tasks/add",
        Some(&admin),
        "kind=reminder&args=in%202h%20web%20test%20reminder",
    );
    assert_eq!(add.status, 200, "{}", add.body);
    let tasks = request(port, "GET", "/tasks", Some(&admin), "");
    assert!(tasks.body.contains("web test reminder"), "{}", tasks.body);

    // Memory: remember now, then the fact is listed.
    let remember = request(
        port,
        "POST",
        "/memory/remember",
        Some(&admin),
        "text=the%20web%20remembers",
    );
    assert_eq!(remember.status, 200, "{}", remember.body);
    let memory = request(port, "GET", "/memory", Some(&admin), "");
    assert!(memory.body.contains("the web remembers"), "{}", memory.body);

    // Per-role skills: enable demo for guest only.
    let toggle = request(
        port,
        "POST",
        "/skills/toggle",
        Some(&admin),
        "name=demo&role=guest&enable=1",
    );
    assert_eq!(toggle.status, 200, "{}", toggle.body);
    assert!(
        daemon
            .data
            .join("roles/guest/engine-home/.config/ka/skills/demo/SKILL.md")
            .is_file(),
        "skill provisioned into the role's engine home"
    );

    // Provider keys and secrets can be set from the panel; values never echo.
    let key = request(
        port,
        "POST",
        "/keys/provider",
        Some(&admin),
        "name=DEMO_KEY&value=sk-test-value",
    );
    assert_eq!(key.status, 200, "{}", key.body);
    assert!(
        !key.body.contains("sk-test-value"),
        "the value is never rendered"
    );
    let env = std::fs::read_to_string(daemon.data.join("engine-home/.config/ka/.env"))
        .unwrap_or_default();
    assert!(env.contains("DEMO_KEY=sk-test-value"), "{env}");
    let secret = request(
        port,
        "POST",
        "/keys/secret",
        Some(&admin),
        "name=demo.token&value=abc123",
    );
    assert_eq!(secret.status, 200, "{}", secret.body);
    assert!(!secret.body.contains("abc123"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = daemon.data.join("secrets/demo.token");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "secret file is 0600");
    }

    // Roles: create, edit page, delete (config `[[role]]`).
    let role = request(
        port,
        "POST",
        "/roles/save",
        Some(&admin),
        "original=&name=editor&mode=ask&allow=read%2Cgrep&disable=&workspace=&budget=&commands=status%2Chelp",
    );
    assert_eq!(role.status, 200, "{}", role.body);
    let profiles = request(port, "GET", "/profiles", Some(&admin), "");
    assert!(profiles.body.contains("editor"), "{}", profiles.body);
    let edit = request(port, "GET", "/roles/edit/editor", Some(&admin), "");
    assert_eq!(edit.status, 200);
    assert!(edit.body.contains("editor"), "{}", edit.body);
    let deleted = request(
        port,
        "POST",
        "/roles/delete",
        Some(&admin),
        "name=editor&force=1",
    );
    assert_eq!(deleted.status, 200, "{}", deleted.body);
    let profiles = request(port, "GET", "/profiles", Some(&admin), "");
    assert!(!profiles.body.contains(">editor<"), "role removed");

    // Profile-wide tool baseline.
    let tools = request(
        port,
        "POST",
        "/tools/disable",
        Some(&admin),
        "specs=tool%3Abash",
    );
    assert_eq!(tools.status, 200, "{}", tools.body);
    let cfg_text = std::fs::read_to_string(root.join("ka-agent.toml")).unwrap();
    assert!(cfg_text.contains("tool:bash"), "{cfg_text}");
    let tools = request(port, "POST", "/tools/disable", Some(&admin), "specs=");
    assert_eq!(tools.status, 200);

    // Identity link/unlink from the panel.
    let linked = request(
        port,
        "POST",
        "/identities/link",
        Some(&admin),
        "channel=matrix&address=%40web%3Aexample.org&role=guest",
    );
    assert_eq!(linked.status, 200, "{}", linked.body);
    let profiles = request(port, "GET", "/profiles", Some(&admin), "");
    assert!(
        profiles.body.contains("@web:example.org"),
        "{}",
        profiles.body
    );
    let unlinked = request(
        port,
        "POST",
        "/identities/unlink",
        Some(&admin),
        "channel=matrix&address=%40web%3Aexample.org",
    );
    assert_eq!(unlinked.status, 200, "{}", unlinked.body);

    // MCP add/remove through the config.
    let mcp = request(
        port,
        "POST",
        "/mcp/add",
        Some(&admin),
        "name=demo&kind=http&url=http%3A%2F%2F127.0.0.1%3A9%2Fmcp&command=&env=",
    );
    assert_eq!(mcp.status, 200, "{}", mcp.body);
    let ext = request(port, "GET", "/extensions", Some(&admin), "");
    assert!(ext.body.contains("demo"), "{}", ext.body);
    let mcp_rm = request(port, "POST", "/mcp/rm", Some(&admin), "name=demo");
    assert_eq!(mcp_rm.status, 200, "{}", mcp_rm.body);

    // Theme toggle ships on every page.
    let chat = request(port, "GET", "/", Some(&admin), "");
    assert!(chat.body.contains("theme-toggle"), "{}", chat.body);
    assert!(chat.body.contains("/static/ui.js"), "{}", chat.body);
    let ui = request(port, "GET", "/static/ui.js", None, "");
    assert_eq!(ui.status, 200);
    assert!(ui.body.contains("ka-theme"), "{}", ui.body);

    // Doctor --fix runs (exit code may be non-zero with no model; the page
    // still answers with the child's report).
    let fix = request(port, "POST", "/doctor/fix", Some(&admin), "");
    assert_eq!(fix.status, 200, "{}", fix.body);

    // Secret removal.
    let rm = request(
        port,
        "POST",
        "/keys/secret/rm",
        Some(&admin),
        "name=demo.token",
    );
    assert_eq!(rm.status, 200, "{}", rm.body);
    assert!(!daemon.data.join("secrets/demo.token").exists());

    // The full doctor runs the binary's own checks beside the in-process ones.
    let doctor_page = request(port, "GET", "/doctor", Some(&admin), "");
    assert!(
        doctor_page.body.contains("full doctor"),
        "{}",
        doctor_page.body
    );

    // Backup now writes a restorable snapshot under <data>/backups.
    let backup = request(port, "POST", "/backup", Some(&admin), "");
    assert_eq!(backup.status, 200, "{}", backup.body);
    assert!(
        backup.body.contains("backups") || backup.body.contains("backup"),
        "{}",
        backup.body
    );
    let backups = std::fs::read_dir(daemon.data.join("backups"))
        .map(|d| d.flatten().count())
        .unwrap_or(0);
    assert!(backups >= 1, "a backup directory exists");

    // Sessions: switch to guest and back through the page action.
    let open = request(port, "POST", "/sessions/open", Some(&admin), "role=guest");
    assert_eq!(open.status, 200, "{}", open.body);
    assert!(
        open.body.contains("now acting as role guest"),
        "{}",
        open.body
    );
    let back = request(port, "POST", "/sessions/open", Some(&admin), "role=admin");
    assert_eq!(back.status, 200, "{}", back.body);

    // Persona edit recompiles IDENTITY.md.
    let persona = request(
        port,
        "POST",
        "/persona",
        Some(&admin),
        "text=%23%20Test%20Bot%0Acalm%20and%20clear",
    );
    assert_eq!(persona.status, 200, "{}", persona.body);
    let models = request(port, "GET", "/models", Some(&admin), "");
    assert!(models.body.contains("Test Bot"), "{}", models.body);
    daemon.child.kill().ok();
    daemon.child.wait().ok();
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
            role: ka_agent_core::ADMIN_ROLE.to_string(),
            private: false,
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
    assert!(
        chat.body.contains("id='private'"),
        "the composer carries the private-conversation toggle: {}",
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

    // The private toggle's command path: `/new private` answers with the
    // incognito notice, `/status` reports it, and `/new` returns to a
    // normal conversation before the turn tests below.
    let priv_new = request_h(
        port,
        "POST",
        "/chat/send",
        Some(&cookie),
        "text=%2Fnew+private",
        &[("X-Ka-Agent", "1")],
    );
    assert_eq!(priv_new.status, 200);
    assert!(
        priv_new.body.contains("private session"),
        "private reply: {}",
        priv_new.body
    );
    let priv_status = request_h(
        port,
        "POST",
        "/chat/send",
        Some(&cookie),
        "text=%2Fstatus",
        &[("X-Ka-Agent", "1")],
    );
    assert!(
        priv_status.body.contains("private"),
        "status reports the private conversation: {}",
        priv_status.body
    );
    let back = request_h(
        port,
        "POST",
        "/chat/send",
        Some(&cookie),
        "text=%2Fnew",
        &[("X-Ka-Agent", "1")],
    );
    assert!(
        back.body.contains("new session"),
        "back to normal: {}",
        back.body
    );

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

    // Live stream: an open /events socket sees the new message's inbound
    // echo and its turn's start/finish. Wait for idle first — a message
    // that arrives while a turn runs is folded into it ("added while
    // working") and emits no events of its own. The delta is not asserted
    // here: the scripted speaker drains all steps on the first request, so
    // the second turn's answer is empty; streaming text over SSE is the
    // same broadcast the console path exercises.
    wait_until_idle(port, &cookie);
    let (events, ready) = open_events(port, &cookie);
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
        ready,
        &[
            "\"ev\":\"inbound\"",
            "\"ev\":\"turn_started\"",
            "\"ev\":\"turn_finished\"",
        ],
        Duration::from_secs(120),
    );
    assert!(stream.contains("\"ev\":\"inbound\""), "stream: {stream}");
    assert!(
        stream.contains("\"ev\":\"turn_started\""),
        "stream: {stream}"
    );
    assert!(
        stream.contains("\"ev\":\"turn_finished\""),
        "stream: {stream}"
    );
}

/// Poll `/status` until the session reports idle (bounded), so the next
/// message starts its own turn instead of folding into a running one.
fn wait_until_idle(port: u16, cookie: &str) {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let page = request(port, "GET", "/status", Some(cookie), "");
        if page.body.contains("● idle") {
            return;
        }
        assert!(Instant::now() < deadline, "session never went idle");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Open a raw SSE connection and wait for the response head: the server
/// subscribes before it flushes the head, so once the head is read no event
/// can be missed by a later POST (read timeout 60 s).
fn open_events(port: u16, cookie: &str) -> (std::net::TcpStream, String) {
    use std::io::Read as _;
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .unwrap();
    let req = format!(
        "GET /events HTTP/1.1\r\nHost: 127.0.0.1\r\nCookie: {cookie}\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).unwrap();
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    while !String::from_utf8_lossy(&buf).contains("\r\n\r\n") {
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(_) => break,
        }
    }
    (stream, String::from_utf8_lossy(&buf).into_owned())
}

/// Read the SSE stream until every marker is seen (or `timeout` passes);
/// returns everything received.
fn read_events_until(
    mut stream: std::net::TcpStream,
    mut seen: String,
    markers: &[&str],
    timeout: Duration,
) -> String {
    use std::io::Read as _;
    let deadline = Instant::now() + timeout;
    let mut buf: Vec<u8> = seen.clone().into_bytes();
    seen.clear();
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
        model: None,
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
            model: None,
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
    // Registry bounds are enforced server-side, not just by HTML: the
    // registry declares approval_ttl_secs max 86_400, so 999_999 is
    // refused — and the valid key submitted alongside it is not written
    // either. (min is 1 since the registry was widened; the old `=1`
    // refusal case moved with it.)
    let refused = request_h(
        port,
        "POST",
        "/settings",
        Some(&cookie),
        "approval_ttl_secs=999999&budgets_daily_usd=9",
        &[("X-Ka-Agent", "1")],
    );
    assert_eq!(refused.status, 200, "{}", refused.body);
    assert!(
        refused.body.contains("nothing was written"),
        "refusal must say nothing was written: {}",
        refused.body
    );
    assert!(
        refused.body.contains("approval_ttl_secs"),
        "refusal must name the key: {}",
        refused.body
    );
    let cfg = std::fs::read_to_string(root.join("ka-agent.toml")).unwrap();
    assert!(
        !cfg.contains("approval_ttl_secs = 999999"),
        "out-of-range value written: {cfg}"
    );
    assert!(
        cfg.contains("daily_usd = 3") && !cfg.contains("daily_usd = 9"),
        "a refused form must not write its other fields: {cfg}"
    );
    // Model selectors are checked against the catalog, like the CLI's
    // `config set`; an unknown vendor/model is refused before any write.
    let bad_model = request_h(
        port,
        "POST",
        "/settings",
        Some(&cookie),
        "model=nope/zzz",
        &[("X-Ka-Agent", "1")],
    );
    assert_eq!(bad_model.status, 200, "{}", bad_model.body);
    assert!(
        bad_model.body.contains("nothing was written") && bad_model.body.contains("model"),
        "selector refusal: {}",
        bad_model.body
    );
    assert!(
        !std::fs::read_to_string(root.join("ka-agent.toml"))
            .unwrap()
            .contains("nope/zzz"),
        "an unknown selector must not be persisted"
    );
    // The persona name in the page title is user-influenced and escaped.
    std::fs::create_dir_all(daemon.data.join("memory")).unwrap();
    std::fs::write(
        daemon.data.join("memory/persona.md"),
        "# Ka <b>bold</b>\nbody\n",
    )
    .unwrap();
    let titled = request(port, "GET", "/", Some(&cookie), "");
    assert_eq!(titled.status, 200);
    assert!(
        titled
            .body
            .contains("<title>Chat — Ka &lt;b&gt;bold&lt;/b&gt;</title>"),
        "persona title must be escaped: {}",
        titled.body
    );
    assert!(
        !titled.body.contains("<title>Chat — Ka <b>bold</b></title>"),
        "raw persona markup leaked into the title: {}",
        titled.body
    );
    // The form is built from the key registry: a web-editable key the old
    // hand-written form never offered is present, with its apply timing.
    let page = request(port, "GET", "/status", Some(&cookie), "");
    for key in [
        "search.provider",
        "notify.quiet_hours",
        "approvals.auto_review",
    ] {
        assert!(page.body.contains(key), "form missing {key}");
    }
    assert!(
        page.body.contains("name='notify_quiet_hours'"),
        "field name: {}",
        page.body
    );
    // A POST of such a registry key writes it (string → quoted TOML).
    let resp = request_h(
        port,
        "POST",
        "/settings",
        Some(&cookie),
        "search_provider=brave",
        &[("X-Ka-Agent", "1")],
    );
    assert_eq!(resp.status, 200, "{}", resp.body);
    let cfg = std::fs::read_to_string(root.join("ka-agent.toml")).unwrap();
    assert!(cfg.contains("provider = \"brave\""), "{cfg}");
    // A key outside the registry is ignored, not written.
    request_h(
        port,
        "POST",
        "/settings",
        Some(&cookie),
        "not_a_key=1",
        &[("X-Ka-Agent", "1")],
    );
    assert!(
        !std::fs::read_to_string(root.join("ka-agent.toml"))
            .unwrap()
            .contains("not_a_key")
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

/// E2/E3: every page renders inside the sidebar — including the four that
/// used to dead-end — and the Intents page is linked from the nav.
#[test]
fn every_page_keeps_the_sidebar_and_intents_is_linked() {
    let root = temp_root("nav");
    let port = free_port();
    let mut daemon = spawn_daemon(&root, port);
    wait_web(port, Duration::from_secs(30));
    let cookie = login(&daemon);

    // Seed one turn + episode so /history has a row to render.
    {
        let store = ka_agent_store::Store::open(&daemon.data.join("state.db")).unwrap();
        seed_session(&store, "web-nav", &root.join("ws"));
        let turn = ka_agent_core::Turn::new(
            ka_agent_core::SessionId("web-nav".to_string()),
            "nav seed".into(),
        );
        store.insert_turn(&turn).unwrap();
        store
            .index_episode(
                &ka_agent_core::SessionId("web-nav".to_string()),
                &turn.id,
                "user",
                "nav seed turn",
            )
            .unwrap();
    }

    for path in [
        "/",
        "/status",
        "/tasks",
        "/memory",
        "/skills",
        "/approvals",
        "/history",
        "/usage",
        "/contacts",
        "/audit",
        "/intents",
    ] {
        let resp = request(port, "GET", path, Some(&cookie), "");
        assert_eq!(resp.status, 200, "{path}");
        assert_eq!(
            resp.body.matches("<aside>").count(),
            1,
            "{path} must render the sidebar exactly once"
        );
    }

    // E3: the intents page is reachable from the nav on the chat page.
    let chat = request(port, "GET", "/", Some(&cookie), "");
    assert_eq!(
        chat.body.matches("href=\"/intents\"").count(),
        1,
        "intents link: {}",
        chat.body
    );
    daemon.child.kill().ok();
    daemon.child.wait().ok();
    let _ = std::fs::remove_dir_all(&root);
}

/// E8: a brand-new root's chat page has guidance inside `#log`, not an
/// empty box.
#[test]
fn fresh_chat_page_shows_a_first_visit_hint() {
    let root = temp_root("fresh");
    let port = free_port();
    let mut daemon = spawn_daemon(&root, port);
    wait_web(port, Duration::from_secs(30));
    let cookie = login(&daemon);

    let resp = request(port, "GET", "/", Some(&cookie), "");
    assert_eq!(resp.status, 200);
    let log = resp
        .body
        .split("<div id='log'>")
        .nth(1)
        .and_then(|rest| rest.split("</div>").next())
        .unwrap_or_default();
    assert!(
        log.trim().contains("bubble system"),
        "the fresh #log must carry the system hint, got: {log}"
    );
    assert!(
        log.contains("say hi"),
        "the hint must say what to do, got: {log}"
    );
    daemon.child.kill().ok();
    daemon.child.wait().ok();
    let _ = std::fs::remove_dir_all(&root);
}

/// E4: two keywords that only appear together non-contiguously in one
/// message match; a missing term finds nothing.
#[test]
fn history_search_ands_keywords() {
    let root = temp_root("search");
    let port = free_port();
    let mut daemon = spawn_daemon(&root, port);
    wait_web(port, Duration::from_secs(30));
    let cookie = login(&daemon);

    {
        let store = ka_agent_store::Store::open(&daemon.data.join("state.db")).unwrap();
        seed_session(&store, "web-search", &root.join("ws"));
        let sid = ka_agent_core::SessionId("web-search".to_string());
        let turn = ka_agent_core::Turn::new(sid.clone(), "deploy please".into());
        store.insert_turn(&turn).unwrap();
        store
            .index_episode(&sid, &turn.id, "user", "please deploy nginx to staging")
            .unwrap();
        store
            .index_episode(
                &sid,
                &turn.id,
                "assistant",
                "the cache is warm — safe to deploy now",
            )
            .unwrap();
    }

    // Two words, one message, not contiguous: both must be required.
    let both = request(port, "GET", "/history?q=deploy+cache", Some(&cookie), "");
    assert_eq!(both.status, 200);
    assert!(
        both.body.contains("/history/show/"),
        "deploy+cache must hit the turn that mentions both: {}",
        both.body
    );
    // A word that appears nowhere narrows to the no-results copy.
    let none = request(port, "GET", "/history?q=deploy+postgres", Some(&cookie), "");
    assert!(
        none.body.contains("no results"),
        "a missing term must find nothing: {}",
        none.body
    );
    daemon.child.kill().ok();
    daemon.child.wait().ok();
    let _ = std::fs::remove_dir_all(&root);
}

/// E5: usage splits per model — two seeded turns under different models
/// render as two rows on the same day.
#[test]
fn usage_page_splits_spend_per_model() {
    let root = temp_root("usage-models");
    let port = free_port();
    let mut daemon = spawn_daemon(&root, port);
    wait_web(port, Duration::from_secs(30));
    let cookie = login(&daemon);
    {
        let store = ka_agent_store::Store::open(&daemon.data.join("state.db")).unwrap();
        let session = ka_agent_core::Session::new(root.join("ws"));
        store.insert_session(&session).unwrap();
        for (model, cost) in [
            ("anthropic/claude-sonnet-4-5", 0.0123_f64),
            ("google/gemini-2.5-flash", 0.0456_f64),
        ] {
            let mut t = ka_agent_core::Turn::new(session.id.clone(), "q".into());
            t.model = Some(model.to_string());
            t.cost_usd = cost;
            store.insert_turn(&t).unwrap();
        }
    }
    let usage = request(port, "GET", "/usage", Some(&cookie), "");
    assert_eq!(usage.status, 200);
    assert!(
        usage.body.contains("anthropic/claude-sonnet-4-5"),
        "usage page must list the model: {}",
        usage.body
    );
    assert!(
        usage.body.contains("google/gemini-2.5-flash"),
        "usage page must split per model: {}",
        usage.body
    );
    assert!(
        usage.body.contains("0.0123") && usage.body.contains("0.0456"),
        "each model's cost stands on its own row: {}",
        usage.body
    );
    daemon.child.kill().ok();
    daemon.child.wait().ok();
    let _ = std::fs::remove_dir_all(&root);
}

/// E7: forgetting an unknown id reports the failure instead of a silent
/// redirect.
#[test]
fn memory_forget_unknown_id_says_so() {
    let root = temp_root("forget");
    let port = free_port();
    let mut daemon = spawn_daemon(&root, port);
    wait_web(port, Duration::from_secs(30));
    let cookie = login(&daemon);

    let resp = request(port, "POST", "/memory/forget", Some(&cookie), "id=bogus123");
    assert_eq!(resp.status, 200, "an error page, not a bare 303");
    assert_ne!(resp.status, 303);
    assert!(
        resp.body.contains("no memory entry matches bogus123"),
        "the failure must be spelled out: {}",
        resp.body
    );
    daemon.child.kill().ok();
    daemon.child.wait().ok();
    let _ = std::fs::remove_dir_all(&root);
}

/// E11: a reminder can be postponed from the tasks page — the snooze form
/// posts a when expression and the next column shows the new time.
#[test]
fn reminder_snooze_moves_the_next_fire() {
    let root = temp_root("snooze");
    let port = free_port();
    let mut daemon = spawn_daemon(&root, port);
    wait_web(port, Duration::from_secs(30));
    let cookie = login(&daemon);

    // Schedule through the same /remind command path the operator uses.
    let resp = request_h(
        port,
        "POST",
        "/chat/send",
        Some(&cookie),
        "text=%2Fremind+in+1h+stretch",
        &[("X-Ka-Agent", "1")],
    );
    assert_eq!(resp.status, 200, "{}", resp.body);
    assert!(
        resp.body.contains("id "),
        "the reply carries the reminder id: {}",
        resp.body
    );
    // "⏰ <when> — stretch · id abc12345" → take the id after "· id ".
    let id = resp
        .body
        .rsplit("· id ")
        .next()
        .and_then(|rest| rest.split('"').next().map(str::to_string))
        .unwrap_or_default()
        .trim()
        .to_string();
    assert!(!id.is_empty(), "reminder id from /remind reply");

    // The tasks page renders the snooze form for the reminder row.
    let tasks = request(port, "GET", "/tasks", Some(&cookie), "");
    assert!(
        tasks.body.contains("action=/tasks/snooze"),
        "snooze form on the row: {}",
        tasks.body
    );

    // Snooze it by 30 minutes and read the new time back.
    let snoozed = request_h(
        port,
        "POST",
        "/tasks/snooze",
        Some(&cookie),
        &format!("id={id}&when=30m"),
        &[],
    );
    assert_eq!(
        snoozed.status, 303,
        "snooze redirects back: {}",
        snoozed.body
    );
    let after = request(port, "GET", "/tasks", Some(&cookie), "");
    assert!(
        after.body.contains("(in 29m)") || after.body.contains("(in 30m)"),
        "the next column must show the snoozed time: {}",
        after.body
    );

    // An unparseable expression is refused with help, not a redirect.
    let bad = request_h(
        port,
        "POST",
        "/tasks/snooze",
        Some(&cookie),
        &format!("id={id}&when=sometime"),
        &[],
    );
    assert_eq!(bad.status, 200);
    assert!(
        bad.body.contains("could not read"),
        "bad when expression: {}",
        bad.body
    );
    daemon.child.kill().ok();
    daemon.child.wait().ok();
    let _ = std::fs::remove_dir_all(&root);
}

/// E9: the settled answer rides the stream as `final_chunk` — the event
/// chat.js now consumes so a reconnect that missed the deltas still ends
/// with the full text.
#[test]
fn events_stream_carries_the_final_chunk() {
    let root = temp_root("final-chunk");
    let port = free_port();
    let mut daemon = spawn_daemon(&root, port);
    wait_web(port, Duration::from_secs(30));
    let cookie = login(&daemon);

    // First turn against the scripted speaker streams real text.
    let (events, ready) = open_events(port, &cookie);
    let sent = request_h(
        port,
        "POST",
        "/chat/send",
        Some(&cookie),
        "text=hello+settled",
        &[("X-Ka-Agent", "1")],
    );
    assert_eq!(sent.status, 200);
    let stream = read_events_until(
        events,
        ready,
        &["\"ev\":\"final_chunk\"", "\"ev\":\"turn_finished\""],
        Duration::from_secs(120),
    );
    assert!(
        stream.contains("\"ev\":\"final_chunk\""),
        "the settled text must ride the stream: {stream}"
    );
    // The final chunk carries the scripted answer whole.
    let chunk_line = stream
        .lines()
        .find(|l| l.contains("\"ev\":\"final_chunk\""))
        .unwrap_or_default();
    assert!(
        chunk_line.contains("web-hello"),
        "final_chunk carries the full answer: {chunk_line}"
    );
    assert!(
        stream.contains("\"ev\":\"turn_finished\""),
        "the turn settles: {stream}"
    );
    daemon.child.kill().ok();
    daemon.child.wait().ok();
    let _ = std::fs::remove_dir_all(&root);
}
