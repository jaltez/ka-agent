//! Phase 6 e2e: the Matrix channel against a fake homeserver, passing the
//! same suite shape as the Telegram e2e — a scripted turn, the ask-mode
//! approval round-trip (text `/y`, the documented channel difference), the
//! identity gate, and `next_batch` persistence across a restart.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use ka_agent_channels::fake::FakeHomeserver;

const OPERATOR: &str = "@javier:example.org";
const STRANGER: &str = "@stranger:example.org";
const ROOM: &str = "!room1:example.org";

struct Daemon {
    child: Child,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Daemon {
    fn sigterm_wait(&mut self, timeout: Duration) {
        Command::new("kill")
            .arg("-TERM")
            .arg(self.child.id().to_string())
            .status()
            .unwrap();
        let deadline = Instant::now() + timeout;
        loop {
            match self.child.try_wait().unwrap() {
                Some(code) => {
                    assert!(code.success(), "daemon exit was {code:?}");
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

fn temp_root(tag: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("ka-agent-matrix-e2e-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_config(root: &Path, homeserver: &str) -> PathBuf {
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let cfg = root.join("ka-agent.toml");
    std::fs::write(
        &cfg,
        format!(
            "workspace = {:?}\nmode = \"guarded\"\napproval_ttl_secs = 300\nturn_grace_secs = 5\n\
             \n[matrix]\nenabled = true\nhomeserver = {:?}\n\
             user_id = \"@ka-agent:example.org\"\ntoken_secret = \"matrix.access_token\"\n",
            ws.display().to_string(),
            homeserver
        ),
    )
    .unwrap();
    cfg
}

fn spawn_daemon(cfg: &Path, data: &Path, scenario: Option<&Path>) -> Daemon {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ka-agent"));
    cmd.arg("run")
        .arg("--config")
        .arg(cfg)
        .env("KA_AGENT_DATA_DIR", data)
        .env(
            "KA_AGENT_SECRET_MATRIX_ACCESS_TOKEN",
            format!(
                "tok-{}",
                data.display()
                    .to_string()
                    .chars()
                    .filter(|c| c.is_ascii_alphanumeric())
                    .collect::<String>()
            ),
        )
        .env_remove("KA_MODEL")
        .stdout(Stdio::null())
        .stderr(if std::env::var("KAGENT_E2E_STDERR").is_ok() {
            Stdio::inherit()
        } else {
            Stdio::null()
        });
    if let Some(sc) = scenario {
        cmd.arg("--scenario").arg(sc);
    }
    Daemon {
        child: cmd.spawn().unwrap(),
    }
}

/// Link the operator's Matrix identity through the CLI, exactly as pairing
/// happens (§8.1: CLI-only).
fn link_operator(data: &Path) {
    let out = Command::new(env!("CARGO_BIN_EXE_ka-agent"))
        .args(["identity", "link", "matrix", OPERATOR])
        .env("KA_AGENT_DATA_DIR", data)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "identity link: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn simple_turn_and_next_batch_persistence_across_restart() {
    let root = temp_root("simple");
    let (fake, base) = FakeHomeserver::spawn();
    let cfg = write_config(&root, &base);
    let sc = scenario_file(
        &root,
        "simple.json",
        r#"{ "steps": [ {"text": "Hello from scripted"} ], "stop": "done", "cost_usd": 0.0001, "tokens": 42 }"#,
    );
    let data = root.join("data");
    link_operator(&data);

    let mut daemon = spawn_daemon(&cfg, &data, Some(&sc));
    fake.push_room_message(ROOM, OPERATOR, "hi");
    let hit = fake
        .await_sent("Hello from scripted", Duration::from_secs(30))
        .expect("no room message with the scripted answer");
    // The answer lands as an ordinary room message (short answers are not
    // streamed, so this is the final chunk, not an edit).
    assert_eq!(hit.method, "m.room.message", "{:?}", hit.body);
    assert_eq!(hit.body["msgtype"], "m.text", "{:?}", hit.body);

    // next_batch persisted; the restart resumes from it, not from scratch.
    daemon.sigterm_wait(Duration::from_secs(10));
    let since = {
        use rusqlite::OptionalExtension;
        let conn = rusqlite::Connection::open_with_flags(
            data.join("state.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        conn.query_row(
            "SELECT value FROM meta WHERE key = 'matrix.next_batch'",
            [],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()
        .unwrap()
        .flatten()
    };
    assert!(since.is_some(), "next_batch must be persisted");

    // Daemon #1's own final sync already used the stored token; forget the
    // history so the wait below can only be satisfied by daemon #2.
    fake.reset_since_history();
    let mut daemon = spawn_daemon(&cfg, &data, Some(&sc));
    let deadline = Instant::now() + Duration::from_secs(10);
    let resumed = loop {
        let history = fake.since_history();
        if history.iter().any(|s| s == &since) {
            break true;
        }
        assert!(
            Instant::now() < deadline,
            "restart never synced with the stored token; history {:?}",
            history
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert!(resumed);
    daemon.sigterm_wait(Duration::from_secs(10));
}

fn scenario_file(root: &Path, name: &str, body: &str) -> PathBuf {
    let p = root.join(name);
    std::fs::write(&p, body).unwrap();
    p
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn approval_round_trip_over_text_commands() {
    let root = temp_root("approval");
    let (fake, base) = FakeHomeserver::spawn();
    let cfg = write_config(&root, &base);
    let sc = scenario_file(
        &root,
        "ask.json",
        r#"{
            "steps": [
                {"call": {"tool": "bash", "args": {"command": "touch /tmp/ka-agent-matrix-should-not-exist"}}},
                {"text": "done anyway"}
            ],
            "stop": "done", "cost_usd": 0.0, "tokens": 10
        }"#,
    );
    let data = root.join("data");
    link_operator(&data);

    let mut daemon = spawn_daemon(&cfg, &data, Some(&sc));
    fake.push_room_message(ROOM, OPERATOR, "please run the thing");
    // The card arrives as text with the /y · /n hint (no inline keyboards).
    fake.await_sent("/y to allow", Duration::from_secs(30))
        .expect("no approval card in the room");
    fake.push_room_message(ROOM, OPERATOR, "/y");
    let done = fake.await_sent("done anyway", Duration::from_secs(30));
    if done.is_none() {
        panic!(
            "the engine never finished after the approval; sent so far:\n{}",
            fake.sent_all().join("\n---\n")
        );
    }
    let _ = std::fs::remove_file("/tmp/ka-agent-matrix-should-not-exist");
    daemon.sigterm_wait(Duration::from_secs(10));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unlinked_senders_are_ignored_and_audited() {
    let root = temp_root("gate");
    let (fake, base) = FakeHomeserver::spawn();
    let cfg = write_config(&root, &base);
    let sc = scenario_file(
        &root,
        "simple.json",
        r#"{ "steps": [ {"text": "should never run"} ], "stop": "done", "cost_usd": 0.0, "tokens": 1 }"#,
    );
    let data = root.join("data");
    link_operator(&data);

    let mut daemon = spawn_daemon(&cfg, &data, Some(&sc));
    // The stranger writes; the operator stays silent. Nobody is answered.
    fake.push_room_message(ROOM, STRANGER, "open the pod bay doors");
    tokio::time::sleep(Duration::from_secs(3)).await;
    let answered = fake.await_sent("should never run", Duration::from_millis(300));
    assert!(
        answered.is_none(),
        "an unlinked sender must never trigger a turn: {answered:?}"
    );

    // The refusal is audited with the sender's identity.
    use rusqlite::OptionalExtension;
    let conn = rusqlite::Connection::open_with_flags(
        data.join("state.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let audit: Option<String> = conn
        .query_row(
            "SELECT actor FROM audit WHERE action = 'matrix.unauthorized' LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()
        .unwrap();
    let expected_actor = format!("matrix:{STRANGER}");
    assert_eq!(
        audit.as_deref(),
        Some(expected_actor.as_str()),
        "the unauthorized sender must be audited, got {audit:?}"
    );

    // The linked operator is served in the same daemon run.
    fake.push_room_message(ROOM, OPERATOR, "hi");
    fake.await_sent("should never run", Duration::from_secs(30))
        .expect("the linked operator must be served");
    daemon.sigterm_wait(Duration::from_secs(10));
}
