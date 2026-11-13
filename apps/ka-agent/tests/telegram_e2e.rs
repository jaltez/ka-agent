//! Full-stack e2e: the real daemon (`ka-agent run`) + Telegram channel +
//! engine worker with the scripted speaker, driven against a fake Bot API
//! server (`ka_agent_channels::fake`). Covers the simple turn, the
//! ask-mode approval round-trip over inline keyboards, and offset
//! persistence across daemon restarts.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use ka_agent_channels::fake::FakeBotServer;

/// Workspace-relative marker file: bash runs with the workspace as cwd, and
/// the workspace is writable inside the fs sandbox without any grant
/// expansion. (The old /tmp marker broke under the v0.7.0 engine: its
/// sandbox refuses a writable /tmp that contains the protected engine-home
/// — which the test's temp data root always is.)
const MARKER: &str = "ka-agent-m2-marker";

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
    spawn_daemon_with_env(cfg, data, scenario, &[])
}

fn spawn_daemon_with_env(
    cfg: &Path,
    data: &Path,
    scenario: Option<&Path>,
    extra_env: &[(&str, &str)],
) -> Daemon {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ka-agent"));
    cmd.arg("run")
        .arg("--config")
        .arg(cfg)
        .env("KA_AGENT_DATA_DIR", data)
        .env(
            "KA_AGENT_SECRET_TELEGRAM_BOT_TOKEN",
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
        .stderr(Stdio::null());
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
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
async fn ask_mode_approval_over_inline_keyboard() {
    let root = temp_root("ask");
    let _ = std::fs::remove_file(root.join("ws").join(MARKER));
    let (fake, base) = FakeBotServer::spawn();
    let cfg = write_config(&root, &base);
    let sc = scenario_file(
        &root,
        "ask.json",
        r#"{
            "steps": [
                {"call": {"tool": "bash", "args": {"command": "touch ka-agent-m2-marker"}}},
                {"text": "done anyway"}
            ],
            "stop": "done", "cost_usd": 0.0, "tokens": 10
        }"#,
    );
    let data = root.join("data");

    let mut daemon = spawn_daemon(&cfg, &data, Some(&sc));
    wait_polling(&fake);

    // Ask mode: the bash call surfaces as an approval card with an
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

    // Observed: allowing the card runs the bash call in the workspace,
    // so the marker exists once the turn completes.
    assert!(
        root.join("ws").join(MARKER).exists(),
        "allow did not run the scripted bash call"
    );

    daemon.sigterm_wait(Duration::from_secs(10));
    let _ = std::fs::remove_file(root.join("ws").join(MARKER));
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

/// D2: a voice note is transcribed, echoed back, and becomes a turn whose
/// inbound text is the transcript. Without `[voice]` it gets the setup
/// hint and no turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn voice_note_becomes_a_turn_with_its_transcript() {
    use ka_agent_channels::fake::FakeTranscriber;
    let root = temp_root("voice");
    let (fake, base) = FakeBotServer::spawn();
    let (stt, stt_base) = FakeTranscriber::spawn("remind me to water the plants");
    let cfg = write_config(&root, &base);
    let mut text = std::fs::read_to_string(&cfg).unwrap();
    // Top-level keys first would be cleaner; a table appended at the end
    // is equally valid TOML.
    text.push_str(&format!(
        "\n[voice]\nprovider = \"local\"\nbase_url = {stt_base:?}\n"
    ));
    std::fs::write(&cfg, text).unwrap();
    let sc = scenario_file(
        &root,
        "voice.json",
        r#"{ "steps": [ {"text": "Watering noted"} ], "stop": "done" }"#,
    );
    let data = root.join("data");
    fake.push_file("v-1", "voice/v-1.oga", b"OggS fake audio");
    let mut daemon = spawn_daemon(&cfg, &data, Some(&sc));
    wait_polling(&fake);
    fake.push_voice_message(1, 42, 42, "v-1", 4);
    fake.await_capture(
        |c| send_message_text(c).is_some_and(|t| t.contains("🎙 “remind me to water the plants”")),
        Duration::from_secs(30),
    )
    .expect("transcript echo");
    fake.await_capture(
        |c| send_message_text(c).is_some_and(|t| t.contains("Watering noted")),
        Duration::from_secs(30),
    )
    .expect("answer to the voice note");
    // The fake re-serves an unacked update while the daemon is slow under
    // load, so the transcription endpoint may see the same audio twice;
    // what must NOT happen is a second turn (asserted below).
    let req = stt.requests();
    assert!(!req.is_empty(), "the voice note was transcribed");
    let body = String::from_utf8_lossy(&req[0].body);
    assert!(
        body.contains("whisper-1") && body.contains("OggS fake audio"),
        "{body}"
    );
    let conn = rusqlite::Connection::open(data.join("state.db")).unwrap();
    let inbound: String = conn
        .query_row(
            "SELECT inbound_text FROM turns ORDER BY created_ms DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(inbound, "remind me to water the plants");
    let turns: i64 = conn
        .query_row("SELECT COUNT(*) FROM turns", [], |r| r.get(0))
        .unwrap();
    assert_eq!(turns, 1, "the voice note becomes exactly one turn");
    daemon.sigterm_wait(Duration::from_secs(10));

    // Voice off: the hint, nothing submitted.
    std::fs::write(
        &cfg,
        std::fs::read_to_string(&cfg)
            .unwrap()
            .split("\n[voice]")
            .next()
            .unwrap(),
    )
    .unwrap();
    let mut daemon = spawn_daemon(&cfg, &data, Some(&sc));
    wait_polling(&fake);
    fake.push_voice_message(2, 42, 42, "v-1", 4);
    fake.await_capture(
        |c| send_message_text(c).is_some_and(|t| t.contains("voice notes are off")),
        Duration::from_secs(30),
    )
    .expect("setup hint");
    let turns: i64 = conn
        .query_row("SELECT COUNT(*) FROM turns", [], |r| r.get(0))
        .unwrap();
    assert_eq!(turns, 1, "no turn for a voice note while voice is off");
    daemon.sigterm_wait(Duration::from_secs(10));
    let _ = std::fs::remove_dir_all(&root);
}

/// B1: a captioned photo is submitted as a turn — the caption is the
/// prompt, the file rides along as an `@attachments/…` reference — and
/// the bare "saved" ack is gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn captioned_photo_becomes_a_turn_with_caption_and_ref() {
    let root = temp_root("caption");
    let (fake, base) = FakeBotServer::spawn();
    let cfg = write_config(&root, &base);
    let sc = scenario_file(
        &root,
        "caption.json",
        r#"{ "steps": [ {"text": "Caption noted"} ], "stop": "done" }"#,
    );
    let data = root.join("data");
    fake.push_file("p-cap-1", "photos/p-cap-1.jpg", b"fake jpeg payload");
    let mut daemon = spawn_daemon(&cfg, &data, Some(&sc));
    wait_polling(&fake);
    fake.push_update(serde_json::json!({
        "update_id": 1,
        "message": {
            "message_id": 1,
            "from": { "id": 42, "is_bot": false, "first_name": "Tester" },
            "chat": { "id": 42, "type": "private" },
            "date": 0,
            "photo": [
                { "file_id": "p-cap-1-small", "width": 90, "height": 90 },
                { "file_id": "p-cap-1", "width": 1280, "height": 720 },
            ],
            "caption": "what is this?",
        }
    }));
    fake.await_capture(
        |c| send_message_text(c).is_some_and(|t| t.contains("Caption noted")),
        Duration::from_secs(30),
    )
    .expect("the caption became a turn");
    let conn = rusqlite::Connection::open(data.join("state.db")).unwrap();
    let inbound: String = conn
        .query_row(
            "SELECT inbound_text FROM turns ORDER BY created_ms DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(inbound.contains("what is this?"), "caption kept: {inbound}");
    assert!(
        inbound.contains("@attachments/"),
        "the file rides along as a reference: {inbound}"
    );
    let turns: i64 = conn
        .query_row("SELECT COUNT(*) FROM turns", [], |r| r.get(0))
        .unwrap();
    assert_eq!(turns, 1, "the captioned photo is exactly one turn");
    // The ack (if any) would have been sent before the answer arrived.
    let acked = fake
        .captured()
        .into_iter()
        .any(|c| send_message_text(&c).is_some_and(|t| t.contains("saved attachments/")));
    assert!(!acked, "no bare ack for a captioned photo");
    daemon.sigterm_wait(Duration::from_secs(10));
    let _ = std::fs::remove_dir_all(&root);
}

/// `/new private` from the phone: the reply says so, a captioned photo's
/// turn records usage but no text, the media lands in the ephemeral
/// `.private-*` directory, and ending the conversation destroys it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn private_session_leaves_no_trace_on_the_phone_path() {
    let root = temp_root("private");
    let (fake, base) = FakeBotServer::spawn();
    let cfg = write_config(&root, &base);
    let sc = scenario_file(
        &root,
        "private.json",
        r#"{ "steps": [ {"text": "Private noted"} ], "stop": "done", "cost_usd": 0.0, "tokens": 3 }"#,
    );
    let data = root.join("data");
    fake.push_file("p-priv-1", "photos/p-priv-1.jpg", b"private jpeg payload");
    let mut daemon = spawn_daemon(&cfg, &data, Some(&sc));
    wait_polling(&fake);

    // Enter the private conversation; the reply must say what it is.
    fake.push_update(serde_json::json!({
        "update_id": 1,
        "message": {
            "message_id": 1,
            "from": { "id": 42, "is_bot": false, "first_name": "Tester" },
            "chat": { "id": 42, "type": "private" },
            "date": 0,
            "text": "/new private",
        }
    }));
    fake.await_capture(
        |c| send_message_text(c).is_some_and(|t| t.contains("private session")),
        Duration::from_secs(30),
    )
    .expect("the private session reply arrived");

    // A captioned photo in the private conversation becomes a turn.
    fake.push_update(serde_json::json!({
        "update_id": 2,
        "message": {
            "message_id": 2,
            "from": { "id": 42, "is_bot": false, "first_name": "Tester" },
            "chat": { "id": 42, "type": "private" },
            "date": 0,
            "photo": [
                { "file_id": "p-priv-1-small", "width": 90, "height": 90 },
                { "file_id": "p-priv-1", "width": 1280, "height": 720 },
            ],
            "caption": "my sensitive caption",
        }
    }));
    fake.await_capture(
        |c| send_message_text(c).is_some_and(|t| t.contains("Private noted")),
        Duration::from_secs(30),
    )
    .expect("the private turn answered");

    let conn = rusqlite::Connection::open(data.join("state.db")).unwrap();
    let (inbound, tokens): (String, i64) = conn
        .query_row(
            "SELECT inbound_text, tokens FROM turns ORDER BY created_ms DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(inbound, "", "the private turn row carries no text");
    assert!(tokens > 0, "usage accounting survives");
    let episodes: i64 = conn
        .query_row("SELECT COUNT(*) FROM episodes_fts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(episodes, 0, "nothing entered the episodic index");

    // The media landed in the ephemeral directory, not the shared inbox.
    let attachments = root.join("ws").join("attachments");
    let private_dirs: Vec<_> = std::fs::read_dir(&attachments)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(".private-"))
        .collect();
    assert_eq!(private_dirs.len(), 1, "one ephemeral media directory");
    assert!(
        std::fs::read_dir(private_dirs[0].path())
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().starts_with("photo-")),
        "the photo is inside it"
    );

    // Ending the conversation destroys the ephemeral directory.
    fake.push_update(serde_json::json!({
        "update_id": 3,
        "message": {
            "message_id": 3,
            "from": { "id": 42, "is_bot": false, "first_name": "Tester" },
            "chat": { "id": 42, "type": "private" },
            "date": 0,
            "text": "/new",
        }
    }));
    fake.await_capture(
        |c| send_message_text(c).is_some_and(|t| t.starts_with("new session")),
        Duration::from_secs(30),
    )
    .expect("the normal session reply arrived");
    assert!(
        !private_dirs[0].path().exists(),
        "the ephemeral media directory was destroyed"
    );

    daemon.sigterm_wait(Duration::from_secs(10));
    let _ = std::fs::remove_dir_all(&root);
}

/// Full error-card loop: a scripted engine failure reaches the phone as
/// the structured card; the ↻ callback (`retry:last`) re-sends the
/// question; the successful retry deletes the stale card.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn error_card_retry_and_auto_clean() {
    let root = temp_root("errcard");
    let (fake, base) = FakeBotServer::spawn();
    let cfg = write_config(&root, &base);
    // `protocol` is non-retryable, so the engine fails fast (no wire
    // retries); the recovery text then serves the re-sent question.
    let sc = scenario_file(
        &root,
        "errcard.json",
        r#"{
            "steps": [
                {"fail": {"class": "protocol", "retryable": false, "message": "wire glitch <tag>"}},
                {"text": "recovered answer"}
            ],
            "stop": "done", "cost_usd": 0.0, "tokens": 5
        }"#,
    );
    let data = root.join("data");
    let mut daemon = spawn_daemon(&cfg, &data, Some(&sc));
    wait_polling(&fake);

    // The failing turn surfaces as the structured card (HTML, escaped
    // detail, no retry keyboard on a non-retryable class).
    fake.push_text_message(1, 42, 42, "please answer");
    let card = fake
        .await_capture(
            |c| {
                c.method == "sendMessage"
                    && c.body["parse_mode"].as_str() == Some("HTML")
                    && c.body["text"]
                        .as_str()
                        .is_some_and(|t| t.contains("Engine protocol error"))
            },
            Duration::from_secs(30),
        )
        .expect("the structured error card reached the phone");
    let card_text = card.body["text"].as_str().unwrap();
    assert!(
        card_text.contains("&lt;tag&gt;"),
        "raw detail is escaped: {card_text}"
    );
    assert!(
        card_text.contains("<code>"),
        "detail rides in a code block: {card_text}"
    );
    assert!(
        card.body["reply_markup"].is_null(),
        "no retry keyboard on a non-retryable class"
    );

    // The ↻ affordance (`retry:last`) re-sends the question; the scripted
    // recovery answers it.
    fake.push_callback(2, 42, 42, "retry:last");
    fake.await_capture(
        |c| send_message_text(c).is_some_and(|t| t.contains("recovered answer")),
        Duration::from_secs(30),
    )
    .expect("the retried turn answered");

    // The successful turn cleans the stale card up.
    fake.await_capture(|c| c.method == "deleteMessage", Duration::from_secs(30))
        .expect("the error card was deleted after the successful retry");

    daemon.sigterm_wait(Duration::from_secs(10));
    let _ = std::fs::remove_dir_all(&root);
}

/// `/update` from the phone: the busy gate (an update restarts the
/// daemon — never mid-turn) and the check path against a dead release
/// endpoint (the error reaches the operator, nothing is attempted).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_command_busy_gate_and_error_surface() {
    let root = temp_root("upd");
    let (fake, base) = FakeBotServer::spawn();
    let cfg = write_config(&root, &base);
    let sc = scenario_file(
        &root,
        "stall.json",
        r#"{ "steps": [ {"sleep_ms": 60000} ], "stop": "done", "cost_usd": 0.0, "tokens": 0 }"#,
    );
    let data = root.join("data");
    // A dead release endpoint: deterministic, no network.
    let mut daemon = spawn_daemon_with_env(
        &cfg,
        &data,
        Some(&sc),
        &[("KA_AGENT_UPDATE_API", "http://127.0.0.1:1/dead")],
    );
    wait_polling(&fake);

    // Busy gate: a stalling turn is running; /update must refuse and
    // name the way out.
    fake.push_text_message(1, 42, 42, "stall a moment");
    tokio::time::sleep(Duration::from_secs(2)).await;
    fake.push_text_message(2, 42, 42, "/update");
    fake.await_capture(
        |c| send_message_text(c).is_some_and(|t| t.starts_with("! [update] a turn is running")),
        Duration::from_secs(30),
    )
    .expect("the busy gate refused the update");

    // After the turn is stopped the check path runs — against the dead
    // endpoint the operator gets the release error, not the busy-gate
    // refusal (which also starts with "! [update]") or a stack trace.
    fake.push_text_message(3, 42, 42, "/stop");
    tokio::time::sleep(Duration::from_secs(2)).await;
    fake.push_text_message(4, 42, 42, "/update check");
    fake.await_capture(
        |c| {
            send_message_text(c)
                .is_some_and(|t| t.starts_with("! [update]") && !t.contains("a turn is running"))
        },
        Duration::from_secs(30),
    )
    .expect("the release-check error reached the phone");

    daemon.sigterm_wait(Duration::from_secs(10));
    let _ = std::fs::remove_dir_all(&root);
}
