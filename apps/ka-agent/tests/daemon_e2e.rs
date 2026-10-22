//! Daemon e2e (no channels): `ka-agent run` with Telegram disabled —
//! admin socket appears, answers `{"op":"status"}` with live runtime state,
//! socket mode is 0600, and SIGTERM exits 0.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn temp_root(tag: &str) -> std::path::PathBuf {
    let dir =
        std::env::temp_dir().join(format!("ka-agent-daemon-e2e-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct Daemon {
    child: Child,
    data: std::path::PathBuf,
}

impl Daemon {
    fn sock(&self) -> std::path::PathBuf {
        self.data.join("admin.sock")
    }

    fn wait_sock(&self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.sock().exists() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!(
            "admin socket did not appear within {timeout:?} at {}",
            self.sock().display()
        );
    }

    /// SIGTERM the child and wait (bounded) for a clean exit.
    fn sigterm_wait(&mut self, timeout: Duration) {
        std::process::Command::new("kill")
            .arg("-TERM")
            .arg(self.child.id().to_string())
            .status()
            .unwrap();
        let deadline = Instant::now() + timeout;
        loop {
            match self.child.try_wait().unwrap() {
                Some(code) => {
                    assert!(code.success(), "exit was {code:?}");
                    return;
                }
                None if Instant::now() > deadline => {
                    panic!("daemon did not exit within {timeout:?} after SIGTERM");
                }
                None => std::thread::sleep(Duration::from_millis(100)),
            }
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_daemon(root: &std::path::Path) -> Daemon {
    spawn_daemon_with(root, None)
}

fn write_config(root: &std::path::Path) -> std::path::PathBuf {
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let cfg = root.join("ka-agent.toml");
    std::fs::write(
        &cfg,
        format!(
            "workspace = {:?}\nmode = \"guarded\"\nturn_grace_secs = 5\n[web]\nenabled = false\n",
            ws.display().to_string()
        ),
    )
    .unwrap();
    cfg
}

fn spawn_daemon_with(root: &std::path::Path, scenario: Option<&std::path::Path>) -> Daemon {
    let cfg = write_config(root);
    let data = root.join("data");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ka-agent"));
    cmd.arg("run").arg("--config").arg(&cfg);
    if let Some(sc) = scenario {
        cmd.arg("--scenario").arg(sc);
    }
    let child = cmd
        .env("KA_AGENT_DATA_DIR", &data)
        .env_remove("KA_MODEL")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    Daemon { child, data }
}

fn status_over(sock: &std::path::Path) -> serde_json::Value {
    let mut stream = std::os::unix::net::UnixStream::connect(sock).unwrap();
    stream.write_all(b"{\"op\":\"status\"}\n").unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    serde_json::from_str(line.trim()).unwrap()
}

#[test]
fn admin_socket_status_and_clean_sigterm() {
    let root = temp_root("status");
    let mut daemon = spawn_daemon(&root);
    daemon.wait_sock(Duration::from_secs(10));

    // Socket must be owner-only.
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(daemon.sock())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "socket mode is {mode:o}");
    }

    let v = status_over(&daemon.sock());
    assert_eq!(v["ok"], serde_json::json!(true));
    let report = &v["report"];
    assert!(report["session_id"].as_str().is_some_and(|s| !s.is_empty()));
    assert!(report["status"].as_str().is_some_and(|s| !s.is_empty()));
    assert!(report["queue"].is_u64());
    assert!(report["crashes"].is_u64());

    // Unknown op → error.
    let mut stream = std::os::unix::net::UnixStream::connect(daemon.sock()).unwrap();
    stream.write_all(b"{\"op\":\"nope\"}\n").unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(v["error"], serde_json::json!("unknown op"));

    // SIGTERM → exit 0.
    daemon.sigterm_wait(Duration::from_secs(10));
    std::fs::remove_dir_all(&root).ok();
}

/// One runtime per data root: a second `ka-agent run` on the same root is
/// refused (exit 1) and the first daemon keeps its admin socket.
#[test]
fn second_daemon_on_the_same_root_is_refused() {
    let root = temp_root("single");
    let mut daemon = spawn_daemon(&root);
    daemon.wait_sock(Duration::from_secs(10));
    let cfg = root.join("ka-agent.toml");
    let out = Command::new(env!("CARGO_BIN_EXE_ka-agent"))
        .arg("run")
        .arg("--config")
        .arg(&cfg)
        .env("KA_AGENT_DATA_DIR", &daemon.data)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "second daemon must exit 1");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("another Ka Agent is already running"),
        "stderr: {stderr}"
    );
    let v = status_over(&daemon.sock());
    assert_eq!(
        v["ok"],
        serde_json::json!(true),
        "first daemon still answers"
    );
    daemon.sigterm_wait(Duration::from_secs(10));
    std::fs::remove_dir_all(&root).ok();
}

/// `ka-agent chat` while the daemon runs attaches to it: piped input is
/// answered by the daemon's session, the client leaves by itself once the
/// session is idle, and nothing is reconciled as interrupted.
#[test]
fn chat_attaches_to_the_running_daemon() {
    let root = temp_root("attach");
    let sc = root.join("attach.json");
    std::fs::write(
        &sc,
        r#"{ "steps": [ {"text": "attach-ok"} ], "stop": "done", "cost_usd": 0.0001, "tokens": 7 }"#,
    )
    .unwrap();
    let mut daemon = spawn_daemon_with(&root, Some(&sc));
    daemon.wait_sock(Duration::from_secs(10));
    let cfg = root.join("ka-agent.toml");
    let mut child = Command::new(env!("CARGO_BIN_EXE_ka-agent"))
        .arg("chat")
        .arg("--config")
        .arg(&cfg)
        .env("KA_AGENT_DATA_DIR", &daemon.data)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"hello\n/status\n")
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        assert!(
            Instant::now() < deadline,
            "attached chat did not exit by itself"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    let mut out = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take().unwrap(), &mut out).unwrap();
    assert!(status.success(), "exit {status:?}; output:\n{out}");
    for needle in [
        "connected to the background service",
        "attach-ok",
        "queue 0",
    ] {
        assert!(out.contains(needle), "missing {needle:?} in:\n{out}");
    }
    let v = status_over(&daemon.sock());
    assert_eq!(v["report"]["status"], serde_json::json!("Idle"), "{v}");
    let db = rusqlite::Connection::open(daemon.data.join("state.db")).unwrap();
    let interrupted: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM turns WHERE status = 'interrupted'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        interrupted, 0,
        "the daemon's turn must not be reconciled away"
    );
    daemon.sigterm_wait(Duration::from_secs(10));
    std::fs::remove_dir_all(&root).ok();
}

/// G1: `briefing on` schedules exactly one briefing cron; `briefing now`
/// makes the running daemon send it — the engine sees the briefing prompt
/// with today's context; `briefing off` removes it.
#[test]
fn briefing_on_now_off() {
    let root = temp_root("briefing");
    let sc = root.join("brief.json");
    std::fs::write(
        &sc,
        r#"{ "steps": [ {"text": "Nothing notable today."} ], "stop": "done" }"#,
    )
    .unwrap();
    let mut daemon = spawn_daemon_with(&root, Some(&sc));
    daemon.wait_sock(Duration::from_secs(10));
    let cli = |args: &[&str]| {
        let o = Command::new(env!("CARGO_BIN_EXE_ka-agent"))
            .args(args)
            .env("KA_AGENT_DATA_DIR", &daemon.data)
            .env("KA_AGENT_CONFIG", root.join("ka-agent.toml"))
            .output()
            .unwrap();
        assert!(o.status.success(), "{args:?}: {o:?}");
        String::from_utf8_lossy(&o.stdout).into_owned()
    };
    assert!(cli(&["briefing", "on", "08:00"]).contains("daily briefing on"));
    assert!(cli(&["briefing", "on", "07:30"]).contains("daily briefing on"));
    let store = ka_agent_store::Store::open(&daemon.data.join("state.db")).unwrap();
    let briefings = || {
        store
            .cron_tasks()
            .unwrap()
            .into_iter()
            .filter(|t| {
                t.payload
                    .as_deref()
                    .is_some_and(|p| p.contains("\"briefing\""))
            })
            .count()
    };
    assert_eq!(briefings(), 1, "re-enabling replaces");
    assert!(cli(&["briefing", "now"]).contains("armed"));
    let deadline = Instant::now() + Duration::from_secs(40);
    let conn = rusqlite::Connection::open(daemon.data.join("state.db")).unwrap();
    loop {
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM turns WHERE inbound_text LIKE 'Daily briefing%' AND inbound_text LIKE '%waiting for the operator%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        if n >= 1 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "briefing never reached the engine"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
    // It re-armed for its time instead of disappearing.
    assert_eq!(briefings(), 1);
    assert!(cli(&["briefing", "off"]).contains("off"));
    assert_eq!(briefings(), 0);
    daemon.sigterm_wait(Duration::from_secs(10));
    let _ = std::fs::remove_dir_all(&root);
}

/// B3: crons and approvals from the shell.
#[test]
fn cron_and_approvals_cli() {
    let root = temp_root("cron-cli");
    let mut daemon = spawn_daemon(&root);
    daemon.wait_sock(Duration::from_secs(10));
    let cli = |args: &[&str]| {
        let o = Command::new(env!("CARGO_BIN_EXE_ka-agent"))
            .args(args)
            .env("KA_AGENT_DATA_DIR", &daemon.data)
            .env("KA_AGENT_CONFIG", root.join("ka-agent.toml"))
            .output()
            .unwrap();
        (
            o.status.success(),
            String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr),
        )
    };
    let (ok, out) = cli(&["cron", "add", "every", "day", "at", "08:00", "hi", "there"]);
    assert!(ok && out.contains("scheduled"), "{out}");
    let id = out.split_whitespace().nth(1).unwrap().to_string();
    let (ok, out) = cli(&["cron", "list"]);
    assert!(
        ok && out.contains("hi there") && out.contains("↻ daily 08:00"),
        "{out}"
    );
    let (ok, out) = cli(&["cron", "list", "--json"]);
    let v: serde_json::Value =
        serde_json::from_str(out.trim()).unwrap_or_else(|e| panic!("{e}: {out}"));
    assert!(ok && v["crons"][0]["text"] == "hi there", "{out}");
    assert!(cli(&["cron", "pause", &id]).1.contains("paused"));
    assert!(cli(&["cron", "resume", &id]).1.contains("resumed — next"));
    assert!(cli(&["cron", "runs", &id]).1.contains("no runs yet"));
    let (ok, out) = cli(&["cron", "add", "every", "fortnight", "x"]);
    assert!(!ok && out.contains("bad duration"), "{out}");
    let (ok, out) = cli(&["approvals"]);
    assert!(ok && out.contains("no pending approvals"), "{out}");
    let (ok, out) = cli(&["approvals", "y", "nope"]);
    assert!(
        ok && out.contains("no pending approval matches nope"),
        "{out}"
    );
    assert!(cli(&["cron", "rm", &id]).1.contains("removed"));
    assert!(cli(&["cron", "list"]).1.contains("no crons"));
    daemon.sigterm_wait(Duration::from_secs(10));
    let _ = std::fs::remove_dir_all(&root);
}
