//! Telegram channel e2e against the in-process fake Bot API server. The
//! full engine path (WorkerPort) is covered by app-level e2e; here only
//! routing + surface behavior is exercised.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use ka_agent_channels::fake::FakeBotServer;
use ka_agent_channels::{TelegramChannel, TelegramConfig, TelegramSurface};
use ka_agent_core::DaemonConfig;
use ka_agent_core::config::Telegram as TelegramSettings;
use ka_agent_port::WorkerPort;
use ka_agent_runtime::surface::{ApprovalCard, Surface};
use ka_agent_runtime::{Paths, Runtime};
use ka_agent_store::Store;
use tokio::sync::watch;

static DIR_SEQ: AtomicU32 = AtomicU32::new(0);

fn temp_dir() -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "ka-ch-tg-{}-{}",
        std::process::id(),
        DIR_SEQ.fetch_add(1, Ordering::Relaxed)
    ))
}

struct Stack {
    server: FakeBotServer,
    db_path: std::path::PathBuf,
    store: Arc<Store>,
    runtime: Runtime,
    shutdown: watch::Sender<bool>,
    handle: tokio::task::JoinHandle<()>,
}

async fn spawn_channel(allow: Vec<i64>, token: &str) -> Stack {
    let (server, base) = FakeBotServer::spawn();
    let dir = temp_dir();
    let db_path = dir.join("state.db");
    std::fs::create_dir_all(&dir).unwrap();
    let store = Arc::new(Store::open(&db_path).unwrap());
    // The gate reads the identities table (§8.1): link what the caller
    // allows, exactly as the CLI would.
    for uid in &allow {
        store
            .link_identity("telegram", &uid.to_string(), "test")
            .unwrap();
    }
    let surface = TelegramSurface::new(base.clone(), token.to_string());
    let cfg = DaemonConfig {
        workspace: dir.clone(),
        telegram: TelegramSettings {
            enabled: true,
            token_secret: None,
            allow_user_ids: allow,
            api_base: Some(base),
        },
        ..DaemonConfig::default()
    };
    let runtime = Runtime::new(
        cfg.clone(),
        Paths::new(dir),
        Arc::clone(&store),
        WorkerPort::default(),
        Arc::new(surface) as Arc<dyn Surface>,
    )
    .unwrap();
    let channel = TelegramChannel::new(
        TelegramConfig::from_config(&cfg, token.to_string()),
        runtime.clone(),
        Arc::clone(&store),
    );
    let (shutdown, rx) = watch::channel(false);
    let handle = tokio::spawn(channel.run(rx));
    Stack {
        server,
        db_path,
        store,
        runtime,
        shutdown,
        handle,
    }
}

async fn stop(stack: &mut Stack) {
    stack.shutdown.send(true).unwrap();
    let handle = std::mem::replace(&mut stack.handle, tokio::spawn(async {}));
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .unwrap()
        .unwrap();
}

fn audit_actions(db_path: &std::path::Path) -> Vec<(String, String)> {
    let conn = rusqlite::Connection::open(db_path).unwrap();
    let mut stmt = conn.prepare("SELECT action, actor FROM audit").unwrap();
    stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn allowlist_rejects_and_audits() {
    let mut stack = spawn_channel(vec![42], "tok-allow").await;
    // unauthorized user: no reply, but an audit row
    stack
        .server
        .push_text_message(1, 7, 43, "hello there general");
    // authorized user follows up
    stack.server.push_text_message(2, 7, 42, "/status");
    let status = stack
        .server
        .await_capture(
            |c| {
                c.method == "sendMessage"
                    && c.body["text"].as_str().unwrap_or("").contains("session")
            },
            Duration::from_secs(5),
        )
        .expect("status message sent");
    assert_eq!(status.body["chat_id"].as_i64(), Some(7));
    let leaked = stack.server.captured().into_iter().any(|c| {
        c.method == "sendMessage" && c.body["text"].as_str().unwrap_or("").contains("hello")
    });
    assert!(!leaked, "unauthorized text must never be echoed");
    let rows = audit_actions(&stack.db_path);
    assert!(
        rows.iter()
            .any(|(action, actor)| action == "telegram.unauthorized" && actor == "telegram:43"),
        "audit must record the unauthorized attempt, got {rows:?}"
    );
    stop(&mut stack).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn commands_route_locally() {
    let mut stack = spawn_channel(vec![42], "tok-cmd").await;
    stack.server.push_text_message(1, 5, 42, "/help");
    stack.server.push_text_message(2, 5, 42, "/status");
    stack.server.push_text_message(3, 5, 42, "/frobnicate");
    let help = stack
        .server
        .await_capture(
            |c| {
                c.method == "sendMessage" && c.body["text"].as_str().unwrap_or("").contains("/help")
            },
            Duration::from_secs(5),
        )
        .expect("help sent");
    assert!(help.body["text"].as_str().unwrap_or("").contains("/status"));
    let unknown = stack
        .server
        .await_capture(
            |c| {
                c.method == "sendMessage"
                    && c.body["text"].as_str().unwrap_or("") == "unknown command: /frobnicate"
            },
            Duration::from_secs(5),
        )
        .expect("unknown command answered locally");
    assert!(unknown.body["text"].as_str().is_some());
    tokio::time::sleep(Duration::from_millis(300)).await;
    let session = stack.runtime.session_id();
    assert!(
        stack
            .store
            .latest_turns_for_session(&session, 1)
            .unwrap()
            .is_empty()
    );
    assert_eq!(stack.store.queue_len(&session).unwrap(), 0);
    stop(&mut stack).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn offset_persists_and_dedupes() {
    let mut stack = spawn_channel(vec![42], "tok-off").await;
    stack.server.push_text_message(1, 3, 42, "/status");
    let first = stack
        .server
        .await_capture(
            |c| {
                c.method == "sendMessage"
                    && c.body["text"].as_str().unwrap_or("").contains("session")
            },
            Duration::from_secs(5),
        )
        .expect("first status sent");
    assert!(first.body["text"].as_str().unwrap_or("").contains("strand"));
    // The next poll must carry the persisted offset (update_id 1 → 2);
    // wait for it explicitly instead of racing the loop.
    let deadline = Instant::now() + Duration::from_secs(5);
    while stack.server.last_offset() < Some(2) {
        assert!(
            Instant::now() < deadline,
            "the channel never re-polled with offset 2"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    stop(&mut stack).await;
    assert_eq!(
        stack.store.meta_get("telegram.offset").unwrap().as_deref(),
        Some("2")
    );
    // replay the same update_id: nothing re-delivers
    stack.server.push_text_message(1, 3, 42, "/status");
    tokio::time::sleep(Duration::from_millis(500)).await;
    let status_sends = stack
        .server
        .captured()
        .into_iter()
        .filter(|c| {
            c.method == "sendMessage" && c.body["text"].as_str().unwrap_or("").contains("session")
        })
        .count();
    assert_eq!(
        status_sends, 1,
        "duplicate update_id must be deduped by offset"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn approval_card_and_callback_settle() {
    let (server, base) = FakeBotServer::spawn();
    let surface = TelegramSurface::new(base.clone(), "tok-appr".to_string());
    let full_id = "appr_0123456789abcdef";
    let card = ApprovalCard {
        approval_id: full_id.to_string(),
        question: "run `rm -rf /tmp/x`?".to_string(),
        detail: None,
        options: vec!["allow".into(), "deny".into()],
        expires_at_ms: 1_799_999_999_000,
    };
    surface.on_approval(&card);
    // no chat yet: must no-op
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        server.captured().is_empty(),
        "surface must no-op before set_chat, got {:?}",
        server.captured()
    );
    // learn the chat, render the card
    surface.set_chat(9);
    surface.on_approval(&card);
    let sent = server
        .await_capture(
            |c| c.method == "sendMessage" && c.body["reply_markup"].is_object(),
            Duration::from_secs(5),
        )
        .expect("approval card sent");
    let data_y = sent.body["reply_markup"]["inline_keyboard"][0][0]["callback_data"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(data_y, format!("a:{}:y", &full_id[full_id.len() - 12..]));
    assert!(data_y.len() <= 64, "callback_data must fit 64 bytes");
    assert!(sent.body["text"].as_str().unwrap_or("").contains("rm -rf"));
    tokio::time::sleep(Duration::from_millis(200)).await;

    // wire a channel on the same surface state and answer the card. No
    // live worker exists, so `decide` fails with `UnknownApproval` and the
    // card settles as `[Expired]` (the documented fallback path); the live
    // decide flow is covered by app-level e2e.
    let dir = temp_dir();
    let db_path = dir.join("state.db");
    std::fs::create_dir_all(&dir).unwrap();
    let store = Arc::new(Store::open(&db_path).unwrap());
    store.link_identity("telegram", "42", "test").unwrap();
    let cfg = DaemonConfig {
        workspace: dir.clone(),
        telegram: TelegramSettings {
            enabled: true,
            token_secret: None,
            allow_user_ids: vec![42],
            api_base: Some(base),
        },
        ..DaemonConfig::default()
    };
    let runtime = Runtime::new(
        cfg.clone(),
        Paths::new(dir),
        Arc::clone(&store),
        WorkerPort::default(),
        Arc::new(TelegramSurface::new(
            String::new(),
            "unused-token".to_string(),
        )) as Arc<dyn Surface>,
    )
    .unwrap();
    let channel = TelegramChannel::new(
        TelegramConfig::from_config(&cfg, "tok-appr".to_string()),
        runtime,
        Arc::clone(&store),
    );
    let (shutdown, rx) = watch::channel(false);
    let handle = tokio::spawn(channel.run(rx));
    // regression: an authorized message (update 1) advances the offset to 2;
    // the callback (update 2) must still dispatch in the next poll
    server.push_text_message(1, 9, 42, "/help");
    server.push_callback(2, 9, 42, &data_y);
    let answered = server
        .await_capture(
            |c| c.method == "answerCallbackQuery",
            Duration::from_secs(5),
        )
        .expect("callback answered");
    assert!(answered.body["callback_query_id"].as_str().is_some());
    let settled = server
        .await_capture(
            |c| {
                c.method == "editMessageText"
                    && c.body["text"]
                        .as_str()
                        .unwrap_or("")
                        .starts_with("[Expired]")
            },
            Duration::from_secs(5),
        )
        .expect("card settled");
    assert!(
        settled.body["reply_markup"]["inline_keyboard"]
            .as_array()
            .is_some()
    );
    shutdown.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn surface_noops_until_chat_is_known() {
    let (server, base) = FakeBotServer::spawn();
    let surface = TelegramSurface::new(base, "tok-nochat".to_string());
    surface.on_final_chunk(0, "chunk");
    surface.on_error("test", "boom");
    surface.on_turn_finished("done", 0.0, 0);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        server.captured().is_empty(),
        "no outbound traffic before set_chat, got {:?}",
        server.captured()
    );
}
