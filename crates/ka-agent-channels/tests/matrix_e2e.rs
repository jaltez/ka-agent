//! Matrix channel e2e against the in-process fake homeserver: invites,
//! immediate outbound delivery during a pending `/sync`, room persistence,
//! first-sync backlog skip, the encrypted-room notice and `/pair` pairing.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use ka_agent_channels::fake::{FAKE_MATRIX_BOT, FakeHomeserver};
use ka_agent_channels::{Channel, MatrixChannel, MatrixConfig, MatrixSurface, pairing};
use ka_agent_core::DaemonConfig;
use ka_agent_port::WorkerPort;
use ka_agent_runtime::event::AgentEvent;
use ka_agent_runtime::surface::Surface;
use ka_agent_runtime::{Envelope, Paths, Runtime};

/// A numbered envelope (seqs only matter to the bus, not the surface).
fn env(seq: u64, ev: AgentEvent) -> Envelope {
    Envelope::new(seq, ev)
}
use ka_agent_store::Store;
use serde_json::json;
use tokio::sync::watch;

const OPERATOR: &str = "@javier:example.org";
const STRANGER: &str = "@stranger:example.org";
const ROOM: &str = "!room1:example.org";
const ROOM2: &str = "!room2:example.org";

static DIR_SEQ: AtomicU32 = AtomicU32::new(0);

struct Stack {
    fake: FakeHomeserver,
    store: Arc<Store>,
    surface: Arc<MatrixSurface>,
    shutdown: watch::Sender<bool>,
    handle: tokio::task::JoinHandle<()>,
}

impl Stack {
    async fn stop(self) {
        self.shutdown.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(10), self.handle)
            .await
            .unwrap()
            .unwrap();
    }
}

fn audit_details(dir: &std::path::Path) -> Vec<(String, String)> {
    let conn = rusqlite::Connection::open(dir.join("state.db")).unwrap();
    let mut stmt = conn
        .prepare("SELECT action, COALESCE(detail, '') FROM audit")
        .unwrap();
    stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

async fn spawn_stack(link_operator: bool) -> (Stack, std::path::PathBuf) {
    spawn_stack_with(link_operator, None).await
}

async fn spawn_stack_with(
    link_operator: bool,
    transcriber: Option<ka_agent_channels::voice::Transcriber>,
) -> (Stack, std::path::PathBuf) {
    spawn_stack_with_port(link_operator, transcriber, WorkerPort::default()).await
}

async fn spawn_stack_with_port(
    link_operator: bool,
    transcriber: Option<ka_agent_channels::voice::Transcriber>,
    port: WorkerPort,
) -> (Stack, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "ka-ch-mx-{}-{}",
        std::process::id(),
        DIR_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let (stack, dir) = build_stack(dir, link_operator, transcriber, port).await;
    (stack, dir)
}

async fn build_stack(
    dir: std::path::PathBuf,
    link_operator: bool,
    transcriber: Option<ka_agent_channels::voice::Transcriber>,
    port: WorkerPort,
) -> (Stack, std::path::PathBuf) {
    let (fake, base) = FakeHomeserver::spawn();
    std::fs::create_dir_all(&dir).unwrap();
    let store = Arc::new(Store::open(&dir.join("state.db")).unwrap());
    if link_operator {
        store.link_identity("matrix", OPERATOR, "test").unwrap();
    }
    let mcfg = || MatrixConfig {
        homeserver: base.clone(),
        user_id: FAKE_MATRIX_BOT.to_string(),
        token: "tok".to_string(),
        transcriber: transcriber.clone(),
    };
    let initial_room = store.meta_get("matrix.room_id").ok().flatten();
    let surface = Arc::new(MatrixSurface::new(mcfg(), initial_room));
    let cfg = DaemonConfig {
        workspace: dir.clone(),
        ..DaemonConfig::default()
    };
    let runtime = Runtime::new(
        cfg,
        Paths::new(dir.clone()),
        Arc::clone(&store),
        port,
        Arc::clone(&surface) as Arc<dyn Surface>,
    )
    .unwrap();
    let channel = MatrixChannel::new(mcfg(), runtime, Arc::clone(&store), Arc::clone(&surface));
    let (shutdown, rx) = watch::channel(false);
    let handle = tokio::spawn(channel.run(rx));
    (
        Stack {
            fake,
            store,
            surface,
            shutdown,
            handle,
        },
        dir,
    )
}

async fn wait_until(what: &str, timeout: Duration, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn invite_from_linked_user_is_joined_and_room_persisted() {
    let (stack, _dir) = spawn_stack(true).await;
    stack.fake.push_invite(ROOM, OPERATOR);
    let fake = &stack.fake;
    wait_until("join", Duration::from_secs(5), || {
        fake.joins().iter().any(|r| r == ROOM)
    })
    .await;
    let store = Arc::clone(&stack.store);
    wait_until("persisted room", Duration::from_secs(5), || {
        store.meta_get("matrix.room_id").unwrap().as_deref() == Some(ROOM)
    })
    .await;
    stack.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn invite_from_unlinked_user_is_ignored_and_audited() {
    let (stack, dir) = spawn_stack(true).await;
    stack.fake.push_invite(ROOM, STRANGER);
    let d = dir.clone();
    wait_until("audit row", Duration::from_secs(5), || {
        audit_details(&d)
            .iter()
            .any(|(a, det)| a == "matrix.unauthorized" && det == &format!("invite room={ROOM}"))
    })
    .await;
    assert!(
        stack.fake.joins().is_empty(),
        "must not join a stranger's room"
    );
    stack.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn outbound_is_sent_while_sync_is_pending() {
    let (stack, _dir) = spawn_stack(true).await;
    // The operator writes: the room becomes known.
    stack.fake.push_room_message(ROOM, OPERATOR, "/status");
    stack
        .fake
        .await_sent("queue", Duration::from_secs(10))
        .expect("no /status reply");
    // The next long-poll is held open by the fake; a notice must not wait.
    let fake = &stack.fake;
    wait_until("pending sync", Duration::from_secs(5), || {
        fake.sync_pending()
    })
    .await;
    let started = Instant::now();
    stack.surface.emit(&env(
        1,
        AgentEvent::FinalChunk {
            index: 0,
            text: "answer while polling".into(),
        },
    ));
    let hit = stack
        .fake
        .await_sent("answer while polling", Duration::from_secs(2));
    assert!(hit.is_some(), "the reply waited behind /sync");
    assert!(started.elapsed() < Duration::from_secs(2));
    // Progress notes are never posted; status/turn summaries neither.
    stack.surface.emit(&env(
        2,
        AgentEvent::Note {
            text: "progress note".into(),
        },
    ));
    stack.surface.emit(&env(
        3,
        AgentEvent::TurnFinished {
            stop: "done".into(),
            cost_usd: 0.1,
            tokens: 10,
            ms: 5,
            text: "done".into(),
        },
    ));
    stack.surface.emit(&env(
        4,
        AgentEvent::Notice {
            text: "⏰ ping".into(),
        },
    ));
    stack
        .fake
        .await_sent("⏰ ping", Duration::from_secs(2))
        .expect("notice not sent");
    let sent = stack.fake.sent_all();
    assert!(
        !sent.iter().any(|s| s.contains("progress note")),
        "{sent:?}"
    );
    assert!(!sent.iter().any(|s| s.contains("10 tokens")), "{sent:?}");
    stack.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn queued_messages_wait_for_a_room_then_flush() {
    let (stack, _dir) = spawn_stack(true).await;
    stack.surface.emit(&env(
        1,
        AgentEvent::Notice {
            text: "early notice".into(),
        },
    ));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(stack.fake.sent_all().is_empty());
    stack.fake.push_room_message(ROOM, OPERATOR, "/status");
    stack
        .fake
        .await_sent("early notice", Duration::from_secs(5))
        .expect("backlog not flushed once the room was known");
    stack.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn first_sync_skips_old_backlog_and_encrypted_rooms_get_one_notice() {
    let (stack, _dir) = spawn_stack(true).await;
    let old = ka_agent_core::now_ms() - 3_600_000;
    stack.fake.push_room_event(
        ROOM,
        json!({"type": "m.room.message", "sender": OPERATOR, "origin_server_ts": old,
               "content": {"msgtype": "m.text", "body": "/help"}}),
    );
    stack.fake.push_room_event(
        "!enc:example.org",
        json!({"type": "m.room.encrypted", "sender": OPERATOR, "content": {}}),
    );
    stack.fake.push_room_event(
        "!enc:example.org",
        json!({"type": "m.room.encrypted", "sender": OPERATOR, "content": {}}),
    );
    stack
        .fake
        .await_sent("end-to-end encrypted", Duration::from_secs(5))
        .expect("no encrypted-room notice");
    stack.fake.push_room_message(ROOM, OPERATOR, "/status");
    stack
        .fake
        .await_sent("queue", Duration::from_secs(10))
        .expect("fresh message not served");
    let sent = stack.fake.sent_all();
    assert_eq!(
        sent.iter()
            .filter(|s| s.contains("end-to-end encrypted"))
            .count(),
        1,
        "{sent:?}"
    );
    assert!(
        !sent.iter().any(|s| s.contains("/help")),
        "old history must not be answered: {sent:?}"
    );
    stack.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pair_code_links_the_sender() {
    let (stack, _dir) = spawn_stack(false).await;
    let code = pairing::issue(&stack.store, "matrix").unwrap();
    stack
        .fake
        .push_room_message(ROOM, OPERATOR, "/pair WRONG-CODE");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        stack.fake.sent_all().is_empty(),
        "wrong code must be silent"
    );
    assert!(!stack.store.is_linked("matrix", OPERATOR).unwrap());

    let typed = code.replace('-', "").to_lowercase();
    stack
        .fake
        .push_room_message(ROOM, OPERATOR, &format!("/pair {typed}"));
    stack
        .fake
        .await_sent("Paired", Duration::from_secs(5))
        .expect("no pairing confirmation");
    assert!(stack.store.is_linked("matrix", OPERATOR).unwrap());
    assert_eq!(
        stack.store.meta_get("matrix.room_id").unwrap().as_deref(),
        Some(ROOM)
    );
    stack.stop().await;
}

/// D5: replies carry rendered HTML (`formatted_body`) next to the plain body.
#[tokio::test(flavor = "multi_thread")]
async fn replies_carry_formatted_html() {
    let (stack, _dir) = spawn_stack(true).await;
    stack.fake.push_room_message(ROOM, OPERATOR, "/status");
    stack
        .fake
        .await_sent("queue", Duration::from_secs(10))
        .expect("no /status reply");
    stack.surface.emit(&env(
        1,
        AgentEvent::FinalChunk {
            index: 0,
            text: "**done** with `x < y`".into(),
        },
    ));
    let hit = stack
        .fake
        .await_sent("**done**", Duration::from_secs(5))
        .expect("reply sent");
    assert_eq!(hit.body["format"].as_str(), Some("org.matrix.custom.html"));
    assert_eq!(
        hit.body["formatted_body"].as_str(),
        Some("<strong>done</strong> with <code>x &lt; y</code>")
    );
    stack.stop().await;
}

/// D2 on Matrix: an `m.audio` message is downloaded (authenticated media),
/// transcribed and echoed; without a transcriber the room gets the hint.
#[tokio::test(flavor = "multi_thread")]
async fn audio_message_is_transcribed_and_echoed() {
    use ka_agent_channels::fake::FakeTranscriber;
    let (stt, stt_base) = FakeTranscriber::spawn("call the plumber");
    let t = ka_agent_channels::voice::Transcriber::new(stt_base, "whisper-1".into(), None);
    let (stack, _dir) = spawn_stack_with(true, Some(t)).await;
    stack
        .fake
        .push_media("example.org/abc", b"OggS matrix audio");
    stack
        .fake
        .push_audio_message(ROOM, OPERATOR, "mxc://example.org/abc");
    stack
        .fake
        .await_sent("🎙 “call the plumber”", Duration::from_secs(10))
        .expect("transcript echo");
    // The fake re-serves an unacked sync event while the box is loaded, so
    // the transcription endpoint may be hit more than once for one audio
    // message; the transcript echo (one send) is the single-delivery proof.
    let req = stt.requests();
    assert!(!req.is_empty(), "the audio was transcribed");
    assert!(String::from_utf8_lossy(&req[0].body).contains("OggS matrix audio"));
    stack.stop().await;

    let (stack, _dir) = spawn_stack_with(true, None).await;
    stack
        .fake
        .push_audio_message(ROOM, OPERATOR, "mxc://example.org/abc");
    stack
        .fake
        .await_sent("voice messages are off", Duration::from_secs(10))
        .expect("setup hint");
    stack.stop().await;
}

/// Not a test in the ordinary sense: when the port spawns this binary as
/// its worker (`WorkerPort { binary: current_exe() }`), the libtest name
/// filter `__worker` lands here and this calls the real worker loop with
/// the scenario the parent left in `KA_DATA_DIR/scenario.json`. Without
/// the `__worker` argument it is a no-op in a normal test run.
#[test]
#[allow(non_snake_case)]
fn mock__worker() {
    if !std::env::args().any(|a| a == "__worker") {
        return;
    }
    let Some(dir) = std::env::var_os("KA_DATA_DIR") else {
        return;
    };
    let scenario = std::path::PathBuf::from(dir).join("scenario.json");
    if !scenario.exists() {
        return;
    }
    let _ = ka_agent_port::run_worker(ka_agent_port::WorkerRun {
        scenario: Some(scenario),
    });
}

/// The port the approval test drives: this test binary as the worker, with
/// the scripted-speaker scenario the mock worker loads from disk.
fn scripted_port(dir: &std::path::Path) -> WorkerPort {
    let scenario = dir.join("ka").join("scenario.json");
    std::fs::create_dir_all(scenario.parent().unwrap()).unwrap();
    std::fs::write(
        &scenario,
        r#"{"steps": [
            {"call": {"tool": "bash", "args": {"command": "touch /tmp/ka-agent-matrix-ask-proof"}}},
            {"text": "done anyway"}
        ], "stop": "done", "cost_usd": 0.0, "tokens": 10}"#,
    )
    .unwrap();
    // The port spawns `<binary> __worker --scenario …`; libtest refuses the
    // unknown flag, so a small wrapper forwards only the filter. The
    // scenario reaches the worker through `KA_DATA_DIR`, which keeps
    // `scenario_override` (the runtime's scripted-mode preflight) honest.
    let wrapper = dir.join("worker.sh");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nexec '{}' __worker\n",
            std::env::current_exe().unwrap().display()
        ),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    WorkerPort {
        binary: wrapper,
        scenario_override: Some(scenario),
    }
}

/// Plan 3.4: an operator turn streams as one message plus `m.replace`
/// edits; the final edit carries the renderer's HTML. Typing runs while
/// the turn does and the final edit clears it.
#[tokio::test(flavor = "multi_thread")]
async fn operator_turn_streams_with_edits() {
    let (stack, _dir) = spawn_stack(true).await;
    // Learn the room (commands answer through the sender task).
    stack.fake.push_room_message(ROOM, OPERATOR, "/status");
    stack
        .fake
        .await_sent("queue", Duration::from_secs(10))
        .expect("no /status reply");

    stack.surface.emit(&env(
        1,
        AgentEvent::TurnStarted {
            turn: "t1".into(),
            source: format!("matrix:{OPERATOR}"),
        },
    ));
    // The first draft: enough text to be worth a message, no relation.
    let first = "a".repeat(100);
    stack.surface.emit(&env(
        2,
        AgentEvent::Delta {
            text: first.clone(),
        },
    ));
    let draft = wait_for_event(&stack.fake, Duration::from_secs(5), |e| {
        e.event_type == "m.room.message" && e.body["body"].as_str() == Some(first.as_str())
    })
    .await
    .expect("no streamed draft");
    assert!(
        draft.body.get("m.relates_to").is_none(),
        "the first draft must be a plain message: {:?}",
        draft.body
    );

    // More text, throttled: the pump edits the draft with `m.replace`.
    tokio::time::sleep(Duration::from_millis(2100)).await;
    stack.surface.emit(&env(
        3,
        AgentEvent::Delta {
            text: "b".repeat(100),
        },
    ));
    let edit = wait_for_event(&stack.fake, Duration::from_secs(6), |e| {
        e.event_type == "m.room.message"
            && e.body["m.relates_to"]["rel_type"].as_str() == Some("m.replace")
    })
    .await
    .expect("no m.replace edit");
    assert_eq!(
        edit.body["m.relates_to"]["event_id"].as_str(),
        Some(draft.event_id.as_str()),
        "an edit must target the streamed message"
    );

    // The final edit carries the shared renderer's HTML.
    let full = format!("**final** {first}");
    stack.surface.emit(&env(
        4,
        AgentEvent::FinalChunk {
            index: 1,
            text: full.clone(),
        },
    ));
    let final_edit = wait_for_event(&stack.fake, Duration::from_secs(5), |e| {
        e.event_type == "m.room.message"
            && e.body["m.new_content"]["formatted_body"]
                .as_str()
                .is_some_and(|h| h.contains("<strong>final</strong>"))
    })
    .await
    .expect("no final edit");
    assert_eq!(
        final_edit.body["m.new_content"]["body"].as_str(),
        Some(full.as_str())
    );
    assert_eq!(
        final_edit.body["format"].as_str(),
        Some("org.matrix.custom.html")
    );

    // Typing: started with the turn, cleared after the final edit.
    wait_until("typing start", Duration::from_secs(5), || {
        stack.fake.typing_events().iter().any(|(room, b)| {
            room == ROOM && b["typing"] == json!(true) && b["timeout"] == json!(30000)
        })
    })
    .await;
    wait_until("typing stop", Duration::from_secs(5), || {
        stack
            .fake
            .typing_events()
            .iter()
            .any(|(_, b)| b["timeout"] == json!(0))
    })
    .await;
    stack.stop().await;
}

/// Plan 3.4: a scheduled turn is delivered once — no streamed draft.
#[tokio::test(flavor = "multi_thread")]
async fn scheduled_turn_is_not_streamed() {
    let (stack, _dir) = spawn_stack(true).await;
    stack.fake.push_room_message(ROOM, OPERATOR, "/status");
    stack
        .fake
        .await_sent("queue", Duration::from_secs(10))
        .expect("no /status reply");
    stack.surface.emit(&env(
        1,
        AgentEvent::TurnStarted {
            turn: "t1".into(),
            source: "cron".into(),
        },
    ));
    stack.surface.emit(&env(
        2,
        AgentEvent::Delta {
            text: "x".repeat(200),
        },
    ));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !stack
            .fake
            .sent_all()
            .iter()
            .any(|s| s.contains(&"x".repeat(50))),
        "a scheduled turn must not stream: {:?}",
        stack.fake.sent_all()
    );
    stack.surface.emit(&env(
        3,
        AgentEvent::FinalChunk {
            index: 1,
            text: "briefing".into(),
        },
    ));
    stack
        .fake
        .await_sent("briefing", Duration::from_secs(5))
        .expect("the final text was not delivered");
    stack.stop().await;
}

/// Plan 3.4: ✅ on an approval card decides it through the runtime (once,
/// exactly like `/y`), so the store row is approved.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn card_reaction_approves_the_approval() {
    let dir = std::env::temp_dir().join(format!(
        "ka-ch-mx-ask-{}-{}",
        std::process::id(),
        DIR_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let port = scripted_port(&dir);
    let (stack, dir) = build_stack(dir, true, None, port).await;

    stack
        .fake
        .push_room_message(ROOM, OPERATOR, "please run the thing");
    let card = match stack
        .fake
        .await_sent("/y to allow", Duration::from_secs(60))
    {
        Some(card) => card,
        None => panic!(
            "no approval card; sent so far:\n{:?}\naudit: {:?}",
            stack.fake.sent_all(),
            audit_details(&dir)
        ),
    };
    assert!(
        card.body["body"]
            .as_str()
            .unwrap_or("")
            .contains("react ✅")
    );
    let card_event = stack
        .fake
        .sent_events()
        .into_iter()
        .find(|e| e.body["body"].as_str() == card.body["body"].as_str())
        .expect("the card has no event id");
    stack
        .fake
        .push_reaction(ROOM, OPERATOR, &card_event.event_id, "✅");

    let db = dir.join("state.db");
    wait_until("approval approved", Duration::from_secs(20), || {
        approval_status(&db).as_deref() == Some("approved")
    })
    .await;
    let d = dir.clone();
    wait_until("decide audit", Duration::from_secs(10), || {
        audit_details(&d)
            .iter()
            .filter(|(a, _)| a == "approval.decide")
            .count()
            == 1
    })
    .await;
    // A second ✅ on the same (settled) card decides nothing.
    stack
        .fake
        .push_reaction(ROOM, OPERATOR, &card_event.event_id, "✅");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        audit_details(&dir)
            .iter()
            .filter(|(a, _)| a == "approval.decide")
            .count(),
        1,
        "a card is single-use"
    );
    assert_eq!(approval_status(&db).as_deref(), Some("approved"));
    stack.stop().await;
}

fn approval_status(db: &std::path::Path) -> Option<String> {
    let conn = rusqlite::Connection::open(db).ok()?;
    conn.query_row("SELECT status FROM approvals LIMIT 1", [], |r| r.get(0))
        .ok()
}

/// Plan 3.4: an `m.file` message lands in `<workspace>/attachments` and
/// the room gets the `@attachments/<file>` reference.
#[tokio::test(flavor = "multi_thread")]
async fn inbound_file_lands_in_attachments() {
    let (stack, dir) = spawn_stack(true).await;
    stack.fake.push_media("example.org/f1", b"the file body");
    stack.fake.push_media_message(
        ROOM,
        OPERATOR,
        "m.file",
        "mxc://example.org/f1",
        "report.txt",
    );
    stack
        .fake
        .await_sent("saved attachments/report.txt", Duration::from_secs(10))
        .expect("no attachment reply");
    let saved = dir.join("attachments").join("report.txt");
    assert_eq!(
        std::fs::read_to_string(&saved).unwrap(),
        "the file body",
        "the file did not land in attachments"
    );
    // The receipt reaction marks the accepted message.
    wait_until("👀 receipt", Duration::from_secs(5), || {
        stack
            .fake
            .sent_events()
            .iter()
            .any(|e| e.event_type == "m.reaction" && e.body["m.relates_to"]["key"] == json!("👀"))
    })
    .await;
    stack.stop().await;
}

/// Plan 3.4: `AgentEvent::File` uploads the bytes and posts media with the
/// caption as the body.
#[tokio::test(flavor = "multi_thread")]
async fn outbound_file_is_uploaded_and_posted() {
    let (stack, dir) = spawn_stack(true).await;
    stack.fake.push_room_message(ROOM, OPERATOR, "/status");
    stack
        .fake
        .await_sent("queue", Duration::from_secs(10))
        .expect("no /status reply");
    let path = dir.join("report.txt");
    std::fs::write(&path, b"the report bytes").unwrap();
    stack.surface.emit(&env(
        1,
        AgentEvent::File {
            path: path.display().to_string(),
            name: "report.txt".into(),
            caption: "the report".into(),
        },
    ));
    wait_until("upload", Duration::from_secs(10), || {
        stack
            .fake
            .uploads()
            .iter()
            .any(|(name, bytes)| name == "report.txt" && bytes == b"the report bytes")
    })
    .await;
    let sent = wait_for_event(&stack.fake, Duration::from_secs(10), |e| {
        e.event_type == "m.room.message" && e.body["msgtype"] == json!("m.file")
    })
    .await
    .expect("no media message");
    assert_eq!(sent.body["body"].as_str(), Some("the report"));
    assert!(
        sent.body["url"]
            .as_str()
            .is_some_and(|u| u.starts_with("mxc://")),
        "{:?}",
        sent.body
    );
    stack.stop().await;
}

/// Learn the delivery room first (commands answer through the sender task).
async fn learn_room(stack: &Stack) {
    stack.fake.push_room_message(ROOM, OPERATOR, "/status");
    stack
        .fake
        .await_sent("queue", Duration::from_secs(10))
        .expect("no /status reply");
}

fn turn_started(turn: &str) -> AgentEvent {
    AgentEvent::TurnStarted {
        turn: turn.into(),
        source: format!("matrix:{OPERATOR}"),
    }
}

/// M3: `FinalChunk` can land while the first draft is still in flight. The
/// settlement must wait for that send's event id and edit the draft — never
/// enqueue the answer as a second message, leaving a partial draft behind.
#[tokio::test(flavor = "multi_thread")]
async fn final_chunk_during_the_first_send_edits_the_draft() {
    let (stack, _dir) = spawn_stack(true).await;
    learn_room(&stack).await;

    let first = "a".repeat(100);
    let full = format!("**final** {first}");
    stack
        .fake
        .delay_next_send(&first, Duration::from_millis(800));
    stack.surface.emit(&env(1, turn_started("t1")));
    stack.surface.emit(&env(
        2,
        AgentEvent::Delta {
            text: first.clone(),
        },
    ));
    wait_until("draft send in flight", Duration::from_secs(5), || {
        stack.fake.sends_in_flight() == 1
    })
    .await;

    // No event id exists yet: this is the race.
    stack.surface.emit(&env(
        3,
        AgentEvent::FinalChunk {
            index: 1,
            text: full.clone(),
        },
    ));

    let edit = wait_for_event(&stack.fake, Duration::from_secs(6), |e| {
        e.event_type == "m.room.message"
            && e.body["m.relates_to"]["rel_type"] == json!("m.replace")
            && e.body["m.new_content"]["body"].as_str() == Some(full.as_str())
    })
    .await
    .expect("the final chunk was not edited into the draft");

    let drafts: Vec<_> = stack
        .fake
        .sent_events()
        .into_iter()
        .filter(|e| {
            e.event_type == "m.room.message" && e.body["body"].as_str() == Some(first.as_str())
        })
        .collect();
    assert_eq!(drafts.len(), 1, "exactly one draft: {drafts:?}");
    assert_eq!(
        edit.body["m.relates_to"]["event_id"].as_str(),
        Some(drafts[0].event_id.as_str()),
        "the final edit must target the draft"
    );
    assert_eq!(edit.room, ROOM);
    // No duplicate answer as its own message.
    assert!(
        !stack
            .fake
            .sent_events()
            .iter()
            .any(|e| e.event_type == "m.room.message"
                && e.body.get("m.relates_to").is_none()
                && e.body["body"].as_str() == Some(full.as_str())),
        "the answer was delivered twice"
    );
    stack.stop().await;
}

/// M4: when the homeserver refuses `m.replace`, the answer still reaches
/// the operator as a visible fallback message instead of staying truncated.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_final_edit_posts_the_answer_as_a_message() {
    let (stack, _dir) = spawn_stack(true).await;
    learn_room(&stack).await;

    let first = "a".repeat(100);
    stack.surface.emit(&env(1, turn_started("t1")));
    stack.surface.emit(&env(
        2,
        AgentEvent::Delta {
            text: first.clone(),
        },
    ));
    wait_for_event(&stack.fake, Duration::from_secs(5), |e| {
        e.event_type == "m.room.message" && e.body["body"].as_str() == Some(first.as_str())
    })
    .await
    .expect("no streamed draft");
    // Let the draft's write-back record its id, then refuse edits.
    tokio::time::sleep(Duration::from_millis(200)).await;
    stack.fake.reject_edits();

    let full = format!("**final** {first}");
    stack.surface.emit(&env(
        3,
        AgentEvent::FinalChunk {
            index: 1,
            text: full.clone(),
        },
    ));
    let fallback = wait_for_event(&stack.fake, Duration::from_secs(5), |e| {
        e.event_type == "m.room.message"
            && e.body.get("m.relates_to").is_none()
            && e.body["body"].as_str() == Some(full.as_str())
    })
    .await
    .expect("a refused edit left the answer truncated");
    assert_eq!(fallback.room, ROOM);
    stack.stop().await;
}

/// M5: a turn is pinned to the room its first draft went to; a message from
/// another linked room does not move its edits or typing.
#[tokio::test(flavor = "multi_thread")]
async fn edits_and_typing_stay_in_the_room_the_turn_started_in() {
    let (stack, _dir) = spawn_stack(true).await;
    learn_room(&stack).await;

    stack.surface.emit(&env(1, turn_started("t1")));
    let first = "a".repeat(100);
    stack.surface.emit(&env(
        2,
        AgentEvent::Delta {
            text: first.clone(),
        },
    ));
    wait_for_event(&stack.fake, Duration::from_secs(5), |e| {
        e.event_type == "m.room.message" && e.body["body"].as_str() == Some(first.as_str())
    })
    .await
    .expect("no streamed draft");

    // The operator writes from another room; delivery now points there.
    stack.fake.push_room_message(ROOM2, OPERATOR, "/status");
    wait_until("room moved", Duration::from_secs(5), || {
        stack.store.meta_get("matrix.room_id").unwrap().as_deref() == Some(ROOM2)
    })
    .await;

    // The next edit of the running turn still targets the draft's room.
    tokio::time::sleep(Duration::from_millis(2100)).await;
    stack.surface.emit(&env(
        3,
        AgentEvent::Delta {
            text: "b".repeat(100),
        },
    ));
    let edit = wait_for_event(&stack.fake, Duration::from_secs(6), |e| {
        e.body["m.relates_to"]["rel_type"] == json!("m.replace")
            && e.body["m.new_content"]["body"]
                .as_str()
                .is_some_and(|b| b.contains("bbbb"))
    })
    .await
    .expect("no edit");
    assert_eq!(edit.room, ROOM, "an edit left the turn's room");

    let full = "**final** answer".to_string();
    stack.surface.emit(&env(
        4,
        AgentEvent::FinalChunk {
            index: 1,
            text: full.clone(),
        },
    ));
    let final_edit = wait_for_event(&stack.fake, Duration::from_secs(5), |e| {
        e.body["m.relates_to"]["rel_type"] == json!("m.replace")
            && e.body["m.new_content"]["body"].as_str() == Some(full.as_str())
    })
    .await
    .expect("no final edit");
    assert_eq!(final_edit.room, ROOM, "the final edit left the turn's room");

    wait_until(
        "typing stop in the turn's room",
        Duration::from_secs(5),
        || {
            stack
                .fake
                .typing_events()
                .iter()
                .any(|(room, body)| room == ROOM && body["timeout"] == json!(0))
        },
    )
    .await;
    stack.stop().await;
}

/// 5a: a send from a replaced turn must not write its event id into the
/// next turn's stream (which would make that turn edit the old draft).
#[tokio::test(flavor = "multi_thread")]
async fn a_stale_send_does_not_seed_the_next_turn() {
    let (stack, _dir) = spawn_stack(true).await;
    learn_room(&stack).await;

    let first = "a".repeat(100);
    stack
        .fake
        .delay_next_send(&first, Duration::from_millis(800));
    stack.surface.emit(&env(1, turn_started("t1")));
    stack.surface.emit(&env(
        2,
        AgentEvent::Delta {
            text: first.clone(),
        },
    ));
    wait_until("first send in flight", Duration::from_secs(5), || {
        stack.fake.sends_in_flight() == 1
    })
    .await;
    // The turn is replaced while its first send is still in flight.
    stack.surface.emit(&env(3, turn_started("t2")));
    wait_for_event(&stack.fake, Duration::from_secs(5), |e| {
        e.event_type == "m.room.message" && e.body["body"].as_str() == Some(first.as_str())
    })
    .await
    .expect("the delayed draft was never sent");
    // Let the stale send's write-back land, then give turn 2 a short answer.
    tokio::time::sleep(Duration::from_millis(200)).await;
    stack.surface.emit(&env(
        4,
        AgentEvent::Delta {
            text: "short turn two text".into(),
        },
    ));
    let answer = "turn two answer";
    stack.surface.emit(&env(
        5,
        AgentEvent::FinalChunk {
            index: 1,
            text: answer.into(),
        },
    ));

    let sent = wait_for_event(&stack.fake, Duration::from_secs(5), |e| {
        e.event_type == "m.room.message" && e.body["body"].as_str() == Some(answer)
    })
    .await
    .expect("turn 2's answer was never posted");
    assert!(
        sent.body.get("m.relates_to").is_none(),
        "turn 2's answer became an edit of turn 1's draft: {:?}",
        sent.body
    );
    assert!(
        !stack
            .fake
            .sent_events()
            .iter()
            .any(|e| e.body["m.relates_to"]["rel_type"] == json!("m.replace")
                && e.body["m.new_content"]["body"].as_str() == Some(answer)),
        "a stale send seeded turn 2's draft"
    );
    stack.stop().await;
}

/// 5b: a failed first-draft send backs off (one attempt per interval), not
/// a tight retry loop; the retry then becomes the turn's draft.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_draft_send_backs_off() {
    let (stack, _dir) = spawn_stack(true).await;
    learn_room(&stack).await;

    let base = stack.fake.send_attempts();
    stack.fake.fail_plain_sends(1);
    let first = "a".repeat(100);
    stack.surface.emit(&env(1, turn_started("t1")));
    stack.surface.emit(&env(
        2,
        AgentEvent::Delta {
            text: first.clone(),
        },
    ));
    wait_until("first draft attempt", Duration::from_secs(5), || {
        stack.fake.send_attempts() > base
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        stack.fake.send_attempts(),
        base + 1,
        "a failed draft must back off instead of spinning"
    );

    wait_until("retry after the backoff", Duration::from_secs(6), || {
        stack.fake.send_attempts() > base + 1
    })
    .await;
    let full = format!("**final** {first}");
    stack.surface.emit(&env(
        3,
        AgentEvent::FinalChunk {
            index: 1,
            text: full.clone(),
        },
    ));
    let edit = wait_for_event(&stack.fake, Duration::from_secs(5), |e| {
        e.body["m.relates_to"]["rel_type"] == json!("m.replace")
            && e.body["m.new_content"]["body"].as_str() == Some(full.as_str())
    })
    .await
    .expect("the retried draft was not finalized");
    assert_eq!(edit.room, ROOM);
    stack.stop().await;
}

/// 5c: finishing a turn clears the typing indicator (not just the local
/// heartbeat, which leaves the indicator up until the homeserver timeout).
#[tokio::test(flavor = "multi_thread")]
async fn typing_stops_when_the_turn_finishes() {
    let (stack, _dir) = spawn_stack(true).await;
    learn_room(&stack).await;

    stack.surface.emit(&env(1, turn_started("t1")));
    wait_until("typing start", Duration::from_secs(5), || {
        stack
            .fake
            .typing_events()
            .iter()
            .any(|(room, body)| room == ROOM && body["typing"] == json!(true))
    })
    .await;

    stack.surface.emit(&env(
        2,
        AgentEvent::TurnFinished {
            stop: "done".into(),
            cost_usd: 0.0,
            tokens: 0,
            ms: 1,
            text: "done".into(),
        },
    ));
    wait_until("typing stop", Duration::from_secs(5), || {
        stack
            .fake
            .typing_events()
            .iter()
            .any(|(room, body)| room == ROOM && body["timeout"] == json!(0))
    })
    .await;
    stack.stop().await;
}

/// M9: a fired reminder reaches the operator on Matrix as its own message.
#[tokio::test(flavor = "multi_thread")]
async fn a_fired_reminder_is_delivered() {
    let (stack, _dir) = spawn_stack(true).await;
    learn_room(&stack).await;

    stack.surface.emit(&env(
        1,
        AgentEvent::Reminder {
            id: "task_1".into(),
            text: "call mom".into(),
        },
    ));
    let sent = stack
        .fake
        .await_sent("⏰ call mom", Duration::from_secs(5))
        .expect("the reminder was not delivered");
    assert_eq!(sent.body["body"].as_str(), Some("⏰ call mom"));
    stack.stop().await;
}

/// Poll until a recorded send matches `pred`.
async fn wait_for_event(
    fake: &FakeHomeserver,
    timeout: Duration,
    pred: impl Fn(&ka_agent_channels::fake::SentEvent) -> bool,
) -> Option<ka_agent_channels::fake::SentEvent> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(hit) = fake.sent_events().into_iter().find(|e| pred(e)) {
            return Some(hit);
        }
        if Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
