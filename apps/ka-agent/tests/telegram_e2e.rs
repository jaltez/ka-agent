//! Full-stack e2e: the real daemon (`ka-agent run`) + Telegram channel +
//! engine worker with the scripted speaker, driven against a fake Bot API
//! server (`ka_agent_channels::fake`). Covers the simple turn, the guarded
//! approval round-trip over inline keyboards, and offset persistence across
//! daemon restarts.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use ka_agent_channels::fake::FakeBotServer;

const MARKER: &str = "/tmp/ka-agent-m2-marker";

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
    /// SIGTERM and wait (bounded) for a clean exit.
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
    let dir = std::env::temp_dir().join(format!(
        "ka-agent-telegram-e2e-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn scenario_file(root: &Path, name: &str, body: &str) -> PathBuf {
    let p = root.join(name);
    std::fs::write(&p, body).unwrap();
    p
}

fn write_config(root: &Path, api_base: &str) -> PathBuf {
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let cfg = root.join("ka-agent.toml");
    std::fs::write(
        &cfg,
        format!(
            "workspace = {:?}\nmode = \"guarded\"\napproval_ttl_secs = 300\nturn_grace_secs = 5\n\
             \n[telegram]\nenabled = true\napi_base = {:?}\n\
             token_secret = \"telegram.bot_token\"\nallow_user_ids = [42]\n",
            ws.display().to_string(),
            api_base
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
        .env("KA_AGENT_SECRET_TELEGRAM_BOT_TOKEN", "tok")
        .env_remove("KA_MODEL")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(sc) = scenario {
        cmd.arg("--scenario").arg(sc);
    }
    Daemon {
        child: cmd.spawn().unwrap(),
    }
}

/// Wait until the daemon's Telegram poller is live (it starts getUpdates).
fn wait_polling(fake: &FakeBotServer) {
    fake.await_capture(|c| c.method == "getUpdates", Duration::from_secs(10))
        .expect("daemon never started polling getUpdates");
}

fn send_message_text(c: &ka_agent_channels::fake::CapturedRequest) -> Option<&str> {
    if c.method == "sendMessage" {
        c.body["text"].as_str()
    } else {
        None
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn simple_turn_and_offset_persistence_across_restart() {
    let root = temp_root("simple");
    let (fake, base) = FakeBotServer::spawn();
    let cfg = write_config(&root, &base);
    let sc = scenario_file(
        &root,
        "simple.json",
        r#"{ "steps": [ {"text": "Hello from scripted"} ], "stop": "done", "cost_usd": 0.0001, "tokens": 42 }"#,
    );
    let data = root.join("data");

    // Flow 1: one scripted turn, driven by a Telegram message.
    let mut daemon = spawn_daemon(&cfg, &data, Some(&sc));
    wait_polling(&fake);
    fake.push_text_message(1, 42, 42, "hi");
    let hit = fake
        .await_capture(
            |c| send_message_text(c).is_some_and(|t| t.contains("Hello from scripted")),
            Duration::from_secs(30),
        )
        .expect("no sendMessage with the scripted answer");
    assert!(hit.body["chat_id"].as_i64() == Some(42));
    // The poller must acknowledge update 1 (offset advances past it).
    let deadline = Instant::now() + Duration::from_secs(10);
    while fake.last_offset().is_none_or(|o| o < 2) {
        assert!(
            Instant::now() < deadline,
            "offset never advanced past update 1"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    daemon.sigterm_wait(Duration::from_secs(10));

    // Flow 3: a fresh daemon on the SAME data dir; re-pushing update 1 must
    // not re-deliver (persisted offset), a new update id must.
    let mut daemon2 = spawn_daemon(&cfg, &data, Some(&sc));
    wait_polling(&fake);
    let before = fake.captured().len();
    fake.push_text_message(1, 42, 42, "hi");
    tokio::time::sleep(Duration::from_secs(2)).await;
    let replayed = fake.captured()[before..]
        .iter()
        .any(|c| send_message_text(c).is_some_and(|t| t.contains("Hello from scripted")));
    assert!(!replayed, "stale update 1 was re-delivered after restart");
    // A /status reply in the post-restart window only (the fake's capture
    // history already holds one from before; matching on "session" alone
    // would be satisfied instantly by it).
    fake.push_text_message(5, 42, 42, "/memory");
    fake.await_capture(
        |c| send_message_text(c).is_some_and(|t| t.contains("remembered yet")),
        Duration::from_secs(30),
    )
    .expect("no /memory reply after restart");
    // And the stale-id replay produced no answer in the same window.
    let stale_answer = fake.captured()[before..]
        .iter()
        .any(|c| send_message_text(c).is_some_and(|t| t.contains("Hello from scripted")));
    assert!(!stale_answer, "stale update answered after restart");
    daemon2.sigterm_wait(Duration::from_secs(10));

    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guarded_approval_over_inline_keyboard() {
    let root = temp_root("ask");
    let _ = std::fs::remove_file(MARKER);
    let (fake, base) = FakeBotServer::spawn();
    let cfg = write_config(&root, &base);
    let sc = scenario_file(
        &root,
        "ask.json",
        r#"{
            "steps": [
                {"call": {"tool": "bash", "args": {"command": "touch /tmp/ka-agent-m2-marker"}}},
                {"text": "done anyway"}
            ],
            "stop": "done", "cost_usd": 0.0, "tokens": 10
        }"#,
    );
    let data = root.join("data");

    let mut daemon = spawn_daemon(&cfg, &data, Some(&sc));
    wait_polling(&fake);

    // Guarded mode: the bash call surfaces as an approval card with an
    // inline keyboard.
    fake.push_text_message(1, 42, 42, "run it");
    let card = fake
        .await_capture(
            |c| c.method == "sendMessage" && c.body["reply_markup"]["inline_keyboard"].is_array(),
            Duration::from_secs(30),
        )
        .expect("no approval card with inline_keyboard");
    let callback_data = card.body["reply_markup"]["inline_keyboard"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|row| row.as_array().unwrap())
        .filter_map(|b| b["callback_data"].as_str())
        .find(|d| d.ends_with(":y"))
        .expect("no allow (:y) callback button")
        .to_string();

    // Answer the card; the turn completes with the scripted final text.
    fake.push_callback(2, 42, 42, &callback_data);
    fake.await_capture(
        |c| c.method == "answerCallbackQuery",
        Duration::from_secs(30),
    )
    .expect("no answerCallbackQuery after pressing allow");
    fake.await_capture(
        |c| send_message_text(c).is_some_and(|t| t.contains("done anyway")),
        Duration::from_secs(30),
    )
    .expect("no final answer after allow");

    // Observed: allowing the card runs the bash call in the guarded
    // workspace, so the marker exists once the turn completes.
    assert!(
        Path::new(MARKER).exists(),
        "allow did not run the scripted bash call"
    );

    daemon.sigterm_wait(Duration::from_secs(10));
    let _ = std::fs::remove_file(MARKER);
    std::fs::remove_dir_all(&root).ok();
}

/// F4 (review): a `/command` from Telegram must be answered by the router and
/// must NOT be submitted to the engine. The old code did both: `/status`
/// answered and then burned a real turn (cost, strand, a duplicate
/// "session…" answer from the model), and `/new` sent the literal "/new" as
/// the fresh session's first prompt.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn commands_never_reach_the_engine() {
    let root = temp_root("cmdonly");
    let (fake, base) = FakeBotServer::spawn();
    let cfg = write_config(&root, &base);
    // The scenario's text is unique: if any command leaks to the engine, this
    // text comes back and the test fails.
    let sc = scenario_file(
        &root,
        "leak.json",
        r#"{ "steps": [ {"text": "ENGINE-LEAK-MARKER"} ], "stop": "done", "cost_usd": 0.0, "tokens": 1 }"#,
    );
    let data = root.join("data");
    let mut daemon = spawn_daemon(&cfg, &data, Some(&sc));
    wait_polling(&fake);

    for (update_id, cmd) in [
        (10, "/status"),
        (11, "/help"),
        (12, "/memory"),
        (13, "/history leak"),
        (14, "/y"),
    ] {
        let before = fake.captured().len();
        fake.push_text_message(update_id, 42, 42, cmd);
        // The router answers something (status/help text, an error note)…
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let answered = fake.captured()[before..]
                .iter()
                .any(|c| send_message_text(c).is_some_and(|t| !t.is_empty()));
            if answered {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "{cmd} was never answered; captured: {:?}",
                &fake.captured()[before..]
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        // …and nothing from the engine: the scripted marker must not appear.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let leaked = fake.captured()[before..]
            .iter()
            .any(|c| send_message_text(c).is_some_and(|t| t.contains("ENGINE-LEAK-MARKER")));
        assert!(!leaked, "{cmd} leaked to the engine and ran a turn");
    }

    // Control: free text DOES run the engine (the marker arrives).
    fake.push_text_message(20, 42, 42, "hello");
    fake.await_capture(
        |c| send_message_text(c).is_some_and(|t| t.contains("ENGINE-LEAK-MARKER")),
        Duration::from_secs(30),
    )
    .expect("free text should have run the engine");

    daemon.sigterm_wait(Duration::from_secs(10));
    std::fs::remove_dir_all(&root).ok();
}
