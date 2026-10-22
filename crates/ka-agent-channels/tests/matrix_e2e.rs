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
use ka_agent_runtime::surface::Surface;
use ka_agent_runtime::{Paths, Runtime};
use ka_agent_store::Store;
use serde_json::json;
use tokio::sync::watch;

const OPERATOR: &str = "@javier:example.org";
const STRANGER: &str = "@stranger:example.org";
const ROOM: &str = "!room1:example.org";

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
    let (fake, base) = FakeHomeserver::spawn();
    let dir = std::env::temp_dir().join(format!(
        "ka-ch-mx-{}-{}",
        std::process::id(),
        DIR_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let store = Arc::new(Store::open(&dir.join("state.db")).unwrap());
    if link_operator {
        store.link_identity("matrix", OPERATOR, "test").unwrap();
    }
    let mcfg = || MatrixConfig {
        homeserver: base.clone(),
        user_id: FAKE_MATRIX_BOT.to_string(),
        token: "tok".to_string(),
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
        WorkerPort::default(),
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
    stack.surface.on_final_chunk(0, "answer while polling");
    let hit = stack
        .fake
        .await_sent("answer while polling", Duration::from_secs(2));
    assert!(hit.is_some(), "the reply waited behind /sync");
    assert!(started.elapsed() < Duration::from_secs(2));
    // Progress notes are never posted; status/turn summaries neither.
    stack.surface.on_note("progress note");
    stack.surface.on_turn_finished("done", 0.1, 10);
    stack.surface.on_notice("⏰ ping");
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
    stack.surface.on_notice("early notice");
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
    stack.surface.on_final_chunk(0, "**done** with `x < y`");
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
