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
use ka_agent_runtime::Envelope;
use ka_agent_runtime::event::AgentEvent;
use ka_agent_runtime::surface::Surface;
use serde_json::json;

/// A numbered envelope (seqs only matter to the bus, not the surface).
fn env(seq: u64, ev: AgentEvent) -> Envelope {
    Envelope::new(seq, ev)
}

fn finished(seq: u64) -> Envelope {
    env(
        seq,
        AgentEvent::TurnFinished {
            stop: "done".into(),
            cost_usd: 0.0,
            tokens: 0,
            ms: 0,
            text: String::new(),
        },
    )
}
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
    spawn_on(server, base, db_path, store, allow, token, None)
}

/// Channel + runtime over an existing store and fake server; the surface
/// starts from `initial_chat` exactly as the daemon passes the meta value.
fn spawn_on(
    server: FakeBotServer,
    base: String,
    db_path: std::path::PathBuf,
    store: Arc<Store>,
    allow: Vec<i64>,
    token: &str,
    initial_chat: Option<i64>,
) -> Stack {
    let dir = db_path.parent().unwrap().to_path_buf();
    let surface = TelegramSurface::new(base.clone(), token.to_string(), initial_chat);
    let cfg = DaemonConfig {
        workspace: dir.clone(),
        telegram: TelegramSettings {
            enabled: true,
            token_secret: None,
            allow_user_ids: allow,
            api_base: Some(base),
            ..TelegramSettings::default()
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
                    && c.body["text"]
                        .as_str()
                        .unwrap_or("")
                        .starts_with("unknown command /frobnicate")
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

/// M7: a Telegram deep link (`/start <payload>`) answers the command list,
/// never `unknown command /<payload>`.
#[tokio::test(flavor = "multi_thread")]
async fn start_deep_link_lists_commands() {
    let mut stack = spawn_channel(vec![42], "tok-start").await;
    stack.server.push_text_message(1, 7, 42, "/start abc123");
    stack
        .server
        .await_capture(
            |c| {
                c.method == "sendMessage"
                    && c.body["text"]
                        .as_str()
                        .unwrap_or("")
                        .contains("Tip: just talk to me")
            },
            Duration::from_secs(5),
        )
        .expect("the command list answers a deep link");
    tokio::time::sleep(Duration::from_millis(200)).await;
    let unknown = stack.server.captured().into_iter().any(|c| {
        c.method == "sendMessage"
            && c.body["text"]
                .as_str()
                .unwrap_or("")
                .starts_with("unknown command /abc123")
    });
    assert!(!unknown, "the payload is not treated as a help topic");
    stop(&mut stack).await;
}

/// File inbox (SEED.md §5.4): a photo is downloaded into the workspace's
/// attachments dir and answered with a usable `@attachments/…` reference;
/// the bytes on disk are exactly what the fake served.
#[tokio::test(flavor = "multi_thread")]
async fn photo_lands_in_attachments_with_ref() {
    let mut stack = spawn_channel(vec![42], "tok-file").await;
    let payload = b"JPEGBYTES-etc".to_vec();
    stack
        .server
        .push_file("f-photo-1", "photos/f-photo-1.jpg", &payload);
    stack.server.push_photo_message(1, 5, 42, "f-photo-1");
    let reply = stack
        .server
        .await_capture(
            |c| {
                c.method == "sendMessage"
                    && c.body["text"]
                        .as_str()
                        .unwrap_or("")
                        .contains("@attachments/")
            },
            Duration::from_secs(5),
        )
        .expect("attachment ack with @ref");
    let text = reply.body["text"].as_str().unwrap_or("");
    assert!(text.contains("saved attachments/"), "reply: {text}");
    let dir = stack.runtime.workspace().join("attachments");
    let saved = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e == "jpg"))
        .expect("one jpg on disk");
    assert_eq!(std::fs::read(&saved).unwrap(), payload, "bytes verbatim");
    stop(&mut stack).await;
}

/// A document with a hostile name is sanitized; a download failure is
/// reported to the operator instead of silently dropped.
#[tokio::test(flavor = "multi_thread")]
async fn document_name_sanitized_and_missing_file_reported() {
    let mut stack = spawn_channel(vec![42], "tok-doc").await;
    stack
        .server
        .push_file("f-doc-1", "docs/notes.txt", b"notes payload\n");
    stack
        .server
        .push_document_message(1, 5, 42, "f-doc-1", "../../my notes!!.txt");
    let reply = stack
        .server
        .await_capture(
            |c| {
                c.method == "sendMessage"
                    && c.body["text"]
                        .as_str()
                        .unwrap_or("")
                        .contains("saved attachments/")
            },
            Duration::from_secs(5),
        )
        .expect("document ack");
    let text = reply.body["text"].as_str().unwrap_or("");
    assert!(!text.contains(".."), "sanitized: {text}");
    let dir = stack.runtime.workspace().join("attachments");
    let names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec!["_._my_notes__.txt".to_string()], "{names:?}");

    // A file_id the server cannot resolve surfaces as an error reply.
    stack
        .server
        .push_document_message(2, 5, 42, "f-ghost", "gone.txt");
    let err = stack
        .server
        .await_capture(
            |c| {
                c.method == "sendMessage"
                    && c.body["text"]
                        .as_str()
                        .unwrap_or("")
                        .contains("! [attachment]")
            },
            Duration::from_secs(5),
        )
        .expect("download failure reported");
    assert!(
        err.body["text"]
            .as_str()
            .unwrap_or("")
            .contains("no file_path"),
        "visible failure: {}",
        err.body["text"]
    );
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
    assert_eq!(
        stack.store.meta_get("telegram.offset").unwrap().as_deref(),
        Some("2")
    );
    // Replay the same update_id WHILE the channel is live: exactly one more
    // reply may appear (the fresh id), never a second for the replayed one.
    let before = stack.server.captured().len();
    stack.server.push_text_message(9, 3, 42, "/memory");
    stack
        .server
        .await_capture(
            |c| {
                c.method == "sendMessage"
                    && c.body["text"].as_str().unwrap_or("").contains("remembered")
            },
            Duration::from_secs(5),
        )
        .expect("the fresh update must be answered");
    stack.server.push_text_message(1, 3, 42, "/status");
    tokio::time::sleep(Duration::from_millis(600)).await;
    let status_sends = stack.server.captured()[before..]
        .iter()
        .filter(|c| {
            c.method == "sendMessage" && c.body["text"].as_str().unwrap_or("").contains("session")
        })
        .count();
    assert_eq!(
        status_sends, 0,
        "duplicate update_id must be deduped by offset while live"
    );
    stop(&mut stack).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn approval_card_and_callback_settle() {
    let (server, base) = FakeBotServer::spawn();
    let surface = TelegramSurface::new(base.clone(), "tok-appr".to_string(), None);
    let full_id = "appr_0123456789abcdef";
    let card = AgentEvent::Approval {
        id: full_id.to_string(),
        question: "run `rm -rf /tmp/x`?".to_string(),
        detail: None,
        options: vec!["allow".into(), "deny".into()],
        expires_at_ms: 1_799_999_999_000,
    };
    surface.emit(&env(1, card.clone()));
    // no chat yet: must no-op
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        server.captured().is_empty(),
        "surface must no-op before set_chat, got {:?}",
        server.captured()
    );
    // learn the chat, render the card
    surface.set_chat(9);
    surface.emit(&env(2, card));
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
            ..TelegramSettings::default()
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
            None,
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

/// M3: every callback query is answered exactly once. Data this build does
/// not understand closes the spinner with an empty answer and changes
/// nothing (no card edit, no other traffic).
#[tokio::test(flavor = "multi_thread")]
async fn unknown_callback_prefix_is_answered_once_and_changes_nothing() {
    let mut stack = spawn_channel(vec![42], "tok-cb").await;
    // Learn the chat first so the callback takes the normal dispatch path.
    stack.server.push_text_message(1, 7, 42, "/status");
    stack
        .server
        .await_capture(|c| c.method == "sendMessage", Duration::from_secs(5))
        .expect("status reply");
    let before = stack.server.captured().len();
    stack.server.push_callback(2, 7, 42, "zz:bogus:data");
    let answered = stack
        .server
        .await_capture(
            |c| c.method == "answerCallbackQuery",
            Duration::from_secs(5),
        )
        .expect("callback answered");
    assert_eq!(
        answered.body["text"].as_str(),
        Some(""),
        "an empty answer still closes the spinner"
    );
    tokio::time::sleep(Duration::from_millis(400)).await;
    let after: Vec<_> = stack.server.captured()[before..].to_vec();
    assert_eq!(
        after
            .iter()
            .filter(|c| c.method == "answerCallbackQuery")
            .count(),
        1,
        "answered exactly once: {after:?}"
    );
    assert!(
        after.iter().all(|c| c.method != "editMessageText"),
        "no card is touched: {after:?}"
    );
    stop(&mut stack).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn surface_noops_until_chat_is_known() {
    let (server, base) = FakeBotServer::spawn();
    let surface = TelegramSurface::new(base, "tok-nochat".to_string(), None);
    surface.emit(&env(
        1,
        AgentEvent::FinalChunk {
            index: 0,
            text: "chunk".into(),
        },
    ));
    surface.emit(&env(
        2,
        AgentEvent::Error {
            class: "test".into(),
            message: "boom".into(),
        },
    ));
    surface.emit(&finished(3));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        server.captured().is_empty(),
        "no outbound traffic before set_chat, got {:?}",
        server.captured()
    );
}

/// D1: a streamed reply shows as a Bot API draft (throttled, growing),
/// then arrives as the finished message.
#[tokio::test(flavor = "multi_thread")]
async fn streamed_reply_uses_drafts_then_sends_the_final_message() {
    let (server, base) = FakeBotServer::spawn();
    let surface = TelegramSurface::new(base, "tok-draft".to_string(), Some(7));
    surface.emit(&env(
        1,
        AgentEvent::TurnStarted {
            turn: String::new(),
            source: String::new(),
        },
    ));
    let chunk = "word ".repeat(12); // 60 chars per delta
    // Five deltas ~1.2 s apart: the throttle window is 1 s, and extra
    // deltas give margin when the test box is loaded (each draft send is
    // an HTTP round-trip to the fake).
    for i in 0..5 {
        surface.emit(&env(
            2 + i as u64,
            AgentEvent::Delta {
                text: chunk.clone(),
            },
        ));
        tokio::time::sleep(Duration::from_millis(1200)).await;
    }
    surface.emit(&env(
        5,
        AgentEvent::FinalChunk {
            index: 1,
            text: chunk.repeat(3),
        },
    ));
    surface.emit(&finished(6));
    let final_msg = server
        .await_capture(|c| c.method == "sendMessage", Duration::from_secs(5))
        .expect("final message");
    let drafts: Vec<_> = server
        .captured()
        .into_iter()
        .filter(|c| c.method == "sendMessageDraft")
        .collect();
    assert!(drafts.len() >= 2, "throttled drafts: {}", drafts.len());
    let id = drafts[0].body["draft_id"].as_i64().unwrap();
    assert!(
        id != 0
            && drafts
                .iter()
                .all(|d| d.body["draft_id"].as_i64() == Some(id))
    );
    let lens: Vec<usize> = drafts
        .iter()
        .map(|d| d.body["text"].as_str().unwrap().len())
        .collect();
    assert!(
        lens.windows(2).all(|w| w[0] < w[1]),
        "drafts grow: {lens:?}"
    );
    assert!(final_msg.body["text"].as_str().unwrap().contains("word"));
}

/// D1 fallback: a Bot API server without drafts — the first 404 refusal
/// turns streaming off once and the answer still arrives as a normal
/// message.
#[tokio::test(flavor = "multi_thread")]
async fn refused_drafts_fall_back_to_the_finished_message() {
    let (server, base) = FakeBotServer::spawn();
    server.set_reject_drafts(true);
    let surface = TelegramSurface::new(base, "tok-nodraft".to_string(), Some(7));
    surface.emit(&env(
        1,
        AgentEvent::TurnStarted {
            turn: String::new(),
            source: String::new(),
        },
    ));
    let chunk = "word ".repeat(12);
    for i in 0..3 {
        surface.emit(&env(
            2 + i as u64,
            AgentEvent::Delta {
                text: chunk.clone(),
            },
        ));
        tokio::time::sleep(Duration::from_millis(1100)).await;
    }
    surface.emit(&env(
        5,
        AgentEvent::FinalChunk {
            index: 1,
            text: "the whole answer".into(),
        },
    ));
    surface.emit(&finished(6));
    server
        .await_capture(
            |c| c.method == "sendMessage" && c.body["text"].as_str() == Some("the whole answer"),
            Duration::from_secs(5),
        )
        .expect("final message despite refused drafts");
    let drafts = server
        .captured()
        .into_iter()
        .filter(|c| c.method == "sendMessageDraft")
        .count();
    assert_eq!(drafts, 1, "one refusal, then streaming stays off");
}

/// M1: a 429 is flood control, not a server without drafts. The surface
/// waits `retry_after` and keeps streaming (only 400/404 disable it).
#[tokio::test(flavor = "multi_thread")]
async fn flood_control_keeps_drafts_streaming() {
    let (server, base) = FakeBotServer::spawn();
    server.fail_method("sendMessageDraft", 429, Some(1), 1);
    let surface = TelegramSurface::new(base, "tok-429".to_string(), Some(7));
    surface.emit(&env(
        1,
        AgentEvent::TurnStarted {
            turn: String::new(),
            source: String::new(),
        },
    ));
    let chunk = "word ".repeat(12); // 60 chars
    surface.emit(&env(
        2,
        AgentEvent::Delta {
            text: chunk.clone(),
        },
    ));
    server
        .await_capture(|c| c.method == "sendMessageDraft", Duration::from_secs(30))
        .expect("first draft attempted (and refused with 429)");
    // Wait out `retry_after` (1 s) plus the 1 s draft spacing, then grow
    // the text: a second draft proves streaming was never turned off.
    tokio::time::sleep(Duration::from_millis(1300)).await;
    surface.emit(&env(
        3,
        AgentEvent::Delta {
            text: chunk.repeat(2),
        },
    ));
    let retried = server
        .await_capture(
            |c| {
                c.method == "sendMessageDraft"
                    && c.body["text"].as_str().is_some_and(|t| t.len() > 60)
            },
            Duration::from_secs(30),
        )
        .expect("draft retried after retry_after; streaming stayed on");
    assert_eq!(
        retried.body["text"].as_str().unwrap().chars().count(),
        180,
        "the refused text was not shown, so it is resent whole"
    );
    // The flag is still on: the turn ends with the final message as usual.
    surface.emit(&env(
        4,
        AgentEvent::FinalChunk {
            index: 1,
            text: "done".into(),
        },
    ));
    surface.emit(&finished(5));
    server
        .await_capture(
            |c| c.method == "sendMessage" && c.body["text"].as_str() == Some("done"),
            Duration::from_secs(30),
        )
        .expect("final message");
}

/// M2: the throttle compares the truncated draft (4095 chars), so text
/// growing only past the cap never re-sends an identical draft.
#[tokio::test(flavor = "multi_thread")]
async fn draft_throttle_ignores_growth_past_the_cap() {
    let (server, base) = FakeBotServer::spawn();
    let surface = TelegramSurface::new(base, "tok-cap".to_string(), Some(7));
    surface.emit(&env(
        1,
        AgentEvent::TurnStarted {
            turn: String::new(),
            source: String::new(),
        },
    ));
    surface.emit(&env(
        2,
        AgentEvent::Delta {
            text: "a".repeat(5000),
        },
    ));
    let first = server
        .await_capture(|c| c.method == "sendMessageDraft", Duration::from_secs(30))
        .expect("first draft");
    assert_eq!(
        first.body["text"].as_str().unwrap().chars().count(),
        4095,
        "the draft carries the capped prefix"
    );
    tokio::time::sleep(Duration::from_millis(1200)).await;
    // Only content beyond the cap grows: the draft text is identical, so
    // the throttle must not fire.
    surface.emit(&env(
        3,
        AgentEvent::Delta {
            text: "b".repeat(300),
        },
    ));
    tokio::time::sleep(Duration::from_millis(500)).await;
    let drafts = server
        .captured()
        .iter()
        .filter(|c| c.method == "sendMessageDraft")
        .count();
    assert_eq!(drafts, 1, "no re-send of an identical truncated draft");
    surface.emit(&finished(4));
}

/// D3: an accepted free-text message gets a 👀 receipt; commands and
/// strangers do not.
#[tokio::test(flavor = "multi_thread")]
async fn accepted_message_gets_a_receipt_reaction() {
    let mut stack = spawn_channel(vec![42], "tok-react").await;
    stack.server.push_text_message(1, 7, 43, "stranger says hi");
    stack.server.push_text_message(2, 7, 42, "/status");
    stack
        .server
        .push_text_message(3, 7, 42, "please look into this");
    let reaction = stack
        .server
        .await_capture(
            |c| c.method == "setMessageReaction",
            Duration::from_secs(30),
        )
        .expect("receipt reaction");
    assert_eq!(reaction.body["chat_id"].as_i64(), Some(7));
    assert_eq!(reaction.body["reaction"][0]["emoji"].as_str(), Some("👀"));
    tokio::time::sleep(Duration::from_millis(300)).await;
    let count = stack
        .server
        .captured()
        .iter()
        .filter(|c| c.method == "setMessageReaction")
        .count();
    assert_eq!(count, 1, "only the operator's free text is acknowledged");
    stop(&mut stack).await;
}

/// D4: a model memory proposal reaches the phone as a card; ✅ makes it
/// stable memory (the operator still decides — one tap instead of
/// `/memory review`), and the card turns into its outcome.
#[tokio::test(flavor = "multi_thread")]
async fn memory_proposal_card_accepts_with_one_tap() {
    let mut stack = spawn_channel(vec![42], "tok-mem").await;
    // The operator's chat is known once they wrote.
    stack.server.push_text_message(1, 7, 42, "/status");
    stack
        .server
        .await_capture(|c| c.method == "sendMessage", Duration::from_secs(30))
        .expect("status reply");
    let id = ka_agent_core::ids::MemoryId::generate();
    stack
        .store
        .add_candidate(&id, "kagent", "Prefers short answers", Some("turn x"))
        .unwrap();
    assert_eq!(stack.runtime.announce_memory_proposals().unwrap(), 1);
    // Announced once: the next tick finds nothing new.
    assert_eq!(stack.runtime.announce_memory_proposals().unwrap(), 0);
    let card = stack
        .server
        .await_capture(
            |c| {
                c.method == "sendMessage"
                    && c.body["text"]
                        .as_str()
                        .unwrap_or("")
                        .contains("Remember this?")
            },
            Duration::from_secs(30),
        )
        .expect("memory card");
    let short = ka_agent_core::ids::short_id(id.as_str()).to_string();
    let data = card.body["reply_markup"]["inline_keyboard"][0][0]["callback_data"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(data, format!("m:{short}:y"));
    stack.server.push_callback(2, 7, 42, &data);
    let edited = stack
        .server
        .await_capture(|c| c.method == "editMessageText", Duration::from_secs(30))
        .expect("card settled");
    assert!(
        edited.body["text"]
            .as_str()
            .unwrap()
            .starts_with("✅ Remembered"),
        "{:?}",
        edited.body
    );
    assert!(stack.store.pending_candidates_full().unwrap().is_empty());
    let facts =
        std::fs::read_to_string(stack.db_path.parent().unwrap().join("memory/facts.md")).unwrap();
    assert!(facts.contains("Prefers short answers"), "{facts}");
    stop(&mut stack).await;
}

/// H1: from the phone the mode only gets stricter.
#[tokio::test(flavor = "multi_thread")]
async fn phone_can_tighten_but_not_loosen_the_mode() {
    let mut stack = spawn_channel(vec![42], "tok-mode").await;
    let cfg_path = stack.db_path.parent().unwrap().join("ka-agent.toml");
    std::fs::write(&cfg_path, "mode = \"guarded\"\n").unwrap();
    stack.runtime.set_config_path(cfg_path.clone());
    stack.server.push_text_message(1, 7, 42, "/mode free");
    stack
        .server
        .await_capture(
            |c| {
                c.method == "sendMessage"
                    && c.body["text"]
                        .as_str()
                        .unwrap_or("")
                        .contains("only get stricter")
            },
            Duration::from_secs(30),
        )
        .expect("loosening refused");
    stack.server.push_text_message(2, 7, 42, "/mode plan");
    stack
        .server
        .await_capture(
            |c| {
                c.method == "sendMessage"
                    && c.body["text"]
                        .as_str()
                        .unwrap_or("")
                        .contains("mode → readonly")
            },
            Duration::from_secs(30),
        )
        .expect("tightening allowed");
    assert!(
        std::fs::read_to_string(&cfg_path)
            .unwrap()
            .contains("mode = \"readonly\"")
    );
    stop(&mut stack).await;
}

/// H2: allow-for-session is not offered from the phone.
#[tokio::test(flavor = "multi_thread")]
async fn phone_cannot_allow_for_the_session() {
    let mut stack = spawn_channel(vec![42], "tok-sess").await;
    stack.server.push_text_message(1, 7, 42, "/y session");
    stack
        .server
        .await_capture(
            |c| {
                c.method == "sendMessage"
                    && c.body["text"]
                        .as_str()
                        .unwrap_or("")
                        .contains("only offered in the terminal")
            },
            Duration::from_secs(30),
        )
        .expect("refused on the phone");
    stop(&mut stack).await;
}

// ---------------------------------------------------------------------------
// Native pickers: bare `/model` `/mode` `/effort` render as inline
// keyboards instead of the router's text listing.
// ---------------------------------------------------------------------------

/// Every `(text, callback_data)` pair on a captured keyboard, row-major.
fn keyboard_buttons(req: &ka_agent_channels::fake::CapturedRequest) -> Vec<(String, String)> {
    req.body["reply_markup"]["inline_keyboard"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|r| r.as_array())
                .flatten()
                .filter_map(|b| {
                    Some((
                        b["text"].as_str()?.to_string(),
                        b["callback_data"].as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Every `callback_data` on a captured keyboard, row-major.
fn keyboard_datas(req: &ka_agent_channels::fake::CapturedRequest) -> Vec<String> {
    keyboard_buttons(req).into_iter().map(|(_, d)| d).collect()
}

/// Whether a captured edit removed the keyboard (settled card).
fn keyboard_is_gone(req: &ka_agent_channels::fake::CapturedRequest) -> bool {
    req.body["reply_markup"]["inline_keyboard"]
        .as_array()
        .is_some_and(|rows| rows.iter().all(|r| r.as_array().is_some_and(Vec::is_empty)))
}

/// A bare `/model` (the `@botname` spelling included) answers with the
/// shared header plus a provider keyboard; argument forms still answer
/// through the shared router, with no keyboard.
#[tokio::test(flavor = "multi_thread")]
async fn bare_model_sends_the_provider_keyboard() {
    let mut stack = spawn_channel(vec![42], "tok-pick0").await;
    stack.server.push_text_message(1, 7, 42, "/model");
    let sent = stack
        .server
        .await_capture(
            |c| c.method == "sendMessage" && c.body["reply_markup"].is_object(),
            Duration::from_secs(5),
        )
        .expect("picker card sent");
    let text = sent.body["text"].as_str().unwrap_or("");
    assert!(text.contains("model:"), "header first: {text}");
    assert!(text.contains("pick a provider:"), "{text}");
    let datas = keyboard_datas(&sent);
    assert!(
        datas.contains(&"mp:anthropic".to_string()),
        "a provider button: {datas:?}"
    );
    assert!(datas.contains(&"mx".to_string()), "the ✕ button: {datas:?}");
    assert!(
        datas.iter().all(|d| d.len() <= 64),
        "callback data fits 64 bytes: {datas:?}"
    );
    // `/model@bot` is the same bare command.
    stack
        .server
        .push_text_message(2, 7, 42, "/mode@ka_agent_bot");
    let mode_card = stack
        .server
        .await_capture(
            |c| {
                c.method == "sendMessage"
                    && c.body["text"].as_str().unwrap_or("").starts_with("mode:")
            },
            Duration::from_secs(5),
        )
        .expect("@botname spelling opens the /mode picker");
    assert!(mode_card.body["reply_markup"].is_object());
    // `/model <provider>` keeps routing through the shared router.
    stack.server.push_text_message(3, 7, 42, "/model anthropic");
    let routed = stack
        .server
        .await_capture(
            |c| {
                c.method == "sendMessage"
                    && c.body["text"].as_str().unwrap_or("").contains("anthropic")
                    && c.body["reply_markup"].is_null()
            },
            Duration::from_secs(5),
        )
        .expect("argument forms stay with the router");
    assert!(
        routed.body["text"]
            .as_str()
            .unwrap_or("")
            .contains("anthropic — 1 model"),
        "the router's vendor listing: {:?}",
        routed.body["text"]
    );
    stop(&mut stack).await;
}

/// `mp:<vendor>` swaps the card to that provider's models; a model
/// button switches the config model and settles the card keyboard-free
/// with the router's own switch reply.
#[tokio::test(flavor = "multi_thread")]
async fn model_picker_opens_a_vendor_and_switches_the_model() {
    let mut stack = spawn_channel(vec![42], "tok-pick1").await;
    // set_model writes the config file: give the runtime one.
    let cfg_path = stack.db_path.parent().unwrap().join("ka-agent.toml");
    std::fs::write(&cfg_path, "model = \"openai/gpt-5.1\"\n").unwrap();
    stack.runtime.set_config_path(cfg_path.clone());

    stack.server.push_text_message(1, 7, 42, "/model");
    stack
        .server
        .await_capture(
            |c| c.method == "sendMessage" && c.body["reply_markup"].is_object(),
            Duration::from_secs(5),
        )
        .expect("picker card sent");
    stack.server.push_callback(2, 7, 42, "mp:anthropic");
    let page = stack
        .server
        .await_capture(
            |c| c.method == "editMessageText" && c.body["reply_markup"].is_object(),
            Duration::from_secs(5),
        )
        .expect("vendor page edited in");
    assert!(
        page.body["text"]
            .as_str()
            .unwrap_or("")
            .contains("anthropic"),
        "vendor listing text"
    );
    let sonnet = keyboard_buttons(&page)
        .into_iter()
        .find(|(t, _)| t.contains("claude-sonnet-5"))
        .expect("claude-sonnet-5 on a button");
    assert_eq!(sonnet.1, "mm:anthropic:0");

    stack.server.push_callback(3, 7, 42, &sonnet.1);
    let answered = stack
        .server
        .await_capture(
            |c| {
                c.method == "answerCallbackQuery"
                    && c.body["text"].as_str().unwrap_or("").contains("model →")
            },
            Duration::from_secs(5),
        )
        .expect("press answered with a toast");
    assert!(
        answered.body["text"]
            .as_str()
            .unwrap_or("")
            .contains("anthropic/claude-sonnet-5")
    );
    let settled = stack
        .server
        .await_capture(
            |c| {
                c.method == "editMessageText"
                    && c.body["text"]
                        .as_str()
                        .unwrap_or("")
                        .contains("model → anthropic/claude-sonnet-5")
            },
            Duration::from_secs(5),
        )
        .expect("card settles to the switch reply");
    assert!(keyboard_is_gone(&settled), "keyboard removed on settle");
    assert!(
        std::fs::read_to_string(&cfg_path)
            .unwrap()
            .contains("model = \"anthropic/claude-sonnet-5\""),
        "config model switched"
    );
    stop(&mut stack).await;
}

/// A vendor with more than 12 models pages: `more ›` edits in the next
/// window, `‹ prev` walks back, `‹ providers` returns to the grid.
#[tokio::test(flavor = "multi_thread")]
async fn model_picker_pages_a_big_vendor() {
    let mut stack = spawn_channel(vec![42], "tok-pick2").await;
    stack.server.push_callback(1, 7, 42, "mp:openrouter");
    let page1 = stack
        .server
        .await_capture(
            |c| c.method == "editMessageText" && c.body["reply_markup"].is_object(),
            Duration::from_secs(5),
        )
        .expect("first page");
    let datas = keyboard_datas(&page1);
    assert!(
        datas.contains(&"mm:openrouter:0".to_string()),
        "models from index 0: {datas:?}"
    );
    assert!(
        datas.contains(&"mp:openrouter:12".to_string()),
        "a more button: {datas:?}"
    );
    assert!(
        datas.contains(&"mp:0".to_string()),
        "back to the providers: {datas:?}"
    );
    stack.server.push_callback(2, 7, 42, "mp:openrouter:12");
    let page2 = stack
        .server
        .await_capture(
            |c| {
                c.method == "editMessageText"
                    && keyboard_datas(c).contains(&"mm:openrouter:12".to_string())
            },
            Duration::from_secs(5),
        )
        .expect("second page");
    let datas2 = keyboard_datas(&page2);
    assert!(
        datas2.contains(&"mp:openrouter:0".to_string())
            && datas2.contains(&"mp:openrouter:24".to_string()),
        "prev and more walk the list: {datas2:?}"
    );
    stop(&mut stack).await;
}

/// The bare `/mode` keyboard tightens (`mo:readonly`) and refuses to
/// loosen from the phone (`mo:allow`) with the same text as `/mode`.
#[tokio::test(flavor = "multi_thread")]
async fn mode_picker_tightens_and_refuses_to_loosen() {
    let mut stack = spawn_channel(vec![42], "tok-pick3").await;
    let cfg_path = stack.db_path.parent().unwrap().join("ka-agent.toml");
    std::fs::write(&cfg_path, "mode = \"ask\"\n").unwrap();
    stack.runtime.set_config_path(cfg_path.clone());
    stack.server.push_text_message(1, 7, 42, "/mode");
    let sent = stack
        .server
        .await_capture(
            |c| c.method == "sendMessage" && c.body["reply_markup"].is_object(),
            Duration::from_secs(5),
        )
        .expect("mode card sent");
    assert!(
        sent.body["text"]
            .as_str()
            .unwrap_or("")
            .starts_with("mode:"),
        "current mode first"
    );
    let buttons = keyboard_buttons(&sent);
    for name in ["ask", "accept_edits", "allow", "readonly"] {
        assert!(
            buttons.iter().any(|(_, d)| *d == format!("mo:{name}")),
            "the {name} button: {buttons:?}"
        );
    }
    assert!(
        buttons
            .iter()
            .any(|(t, d)| d == "mo:ask" && t.starts_with("· ")),
        "the current mode is dotted: {buttons:?}"
    );

    stack.server.push_callback(2, 7, 42, "mo:readonly");
    let settled = stack
        .server
        .await_capture(
            |c| {
                c.method == "editMessageText"
                    && c.body["text"]
                        .as_str()
                        .unwrap_or("")
                        .contains("mode → readonly")
            },
            Duration::from_secs(5),
        )
        .expect("tightening settles the card");
    assert!(keyboard_is_gone(&settled));
    assert!(
        std::fs::read_to_string(&cfg_path)
            .unwrap()
            .contains("mode = \"readonly\"")
    );

    // The phone may only tighten: allow is refused, card shows why.
    stack.server.push_callback(3, 7, 42, "mo:allow");
    let refused = stack
        .server
        .await_capture(
            |c| {
                c.method == "answerCallbackQuery"
                    && c.body["text"].as_str().unwrap_or("").contains("stricter")
            },
            Duration::from_secs(5),
        )
        .expect("refusal toast");
    assert_eq!(refused.body["callback_query_id"].as_str(), Some("cq3"));
    let card = stack
        .server
        .await_capture(
            |c| {
                c.method == "editMessageText"
                    && c.body["text"]
                        .as_str()
                        .unwrap_or("")
                        .starts_with("! [mode]")
            },
            Duration::from_secs(5),
        )
        .expect("refusal on the card");
    assert!(
        card.body["text"]
            .as_str()
            .unwrap_or("")
            .contains("stricter")
    );
    assert!(
        std::fs::read_to_string(&cfg_path)
            .unwrap()
            .contains("mode = \"readonly\""),
        "the mode is unchanged"
    );
    stop(&mut stack).await;
}

/// The bare `/effort` keyboard: `ef:high` answers, settles the card and
/// sets the session effort.
#[tokio::test(flavor = "multi_thread")]
async fn effort_picker_sets_the_level() {
    let mut stack = spawn_channel(vec![42], "tok-pick4").await;
    stack.server.push_text_message(1, 7, 42, "/effort");
    let sent = stack
        .server
        .await_capture(
            |c| c.method == "sendMessage" && c.body["reply_markup"].is_object(),
            Duration::from_secs(5),
        )
        .expect("effort card sent");
    assert!(
        sent.body["text"]
            .as_str()
            .unwrap_or("")
            .starts_with("effort:"),
        "current effort first"
    );
    let datas = keyboard_datas(&sent);
    for level in ["off", "low", "medium", "high", "max"] {
        assert!(
            datas.contains(&format!("ef:{level}")),
            "the {level} button: {datas:?}"
        );
    }
    stack.server.push_callback(2, 7, 42, "ef:high");
    let settled = stack
        .server
        .await_capture(
            |c| {
                c.method == "editMessageText"
                    && c.body["text"]
                        .as_str()
                        .unwrap_or("")
                        .contains("effort → high (this session)")
            },
            Duration::from_secs(5),
        )
        .expect("level settles the card");
    assert!(keyboard_is_gone(&settled));
    assert_eq!(stack.runtime.effort(), Some("high".to_string()));
    stop(&mut stack).await;
}

/// Picker callback data naming something unknown (`mm:nope:1`) is
/// answered exactly once and changes nothing — same as any other
/// unknown prefix.
#[tokio::test(flavor = "multi_thread")]
async fn unknown_picker_callback_is_answered_once_and_changes_nothing() {
    let mut stack = spawn_channel(vec![42], "tok-pick5").await;
    // Learn the chat first so the callback takes the normal dispatch path.
    stack.server.push_text_message(1, 7, 42, "/status");
    stack
        .server
        .await_capture(|c| c.method == "sendMessage", Duration::from_secs(5))
        .expect("status reply");
    let before = stack.server.captured().len();
    stack.server.push_callback(2, 7, 42, "mm:nope:1");
    let answered = stack
        .server
        .await_capture(
            |c| c.method == "answerCallbackQuery",
            Duration::from_secs(5),
        )
        .expect("callback answered");
    assert_eq!(
        answered.body["text"].as_str(),
        Some(""),
        "an empty answer still closes the spinner"
    );
    tokio::time::sleep(Duration::from_millis(400)).await;
    let after: Vec<_> = stack.server.captured()[before..].to_vec();
    assert_eq!(
        after
            .iter()
            .filter(|c| c.method == "answerCallbackQuery")
            .count(),
        1,
        "answered exactly once: {after:?}"
    );
    assert!(
        after.iter().all(|c| c.method != "editMessageText"),
        "no card is touched: {after:?}"
    );
    stop(&mut stack).await;
}

/// 3.3: files leave as `sendPhoto` (jpg/jpeg/png/webp ≤ 10 MB) or
/// `sendDocument`, with the caption; a file that cannot be sent falls back
/// to a notice carrying the absolute path.
#[tokio::test(flavor = "multi_thread")]
async fn files_go_out_as_photo_or_document_with_caption() {
    let (server, base) = FakeBotServer::spawn();
    let surface = TelegramSurface::new(base, "tok-files".to_string(), Some(7));
    let dir = temp_dir();
    std::fs::create_dir_all(&dir).unwrap();

    let png_path = dir.join("pixel.png");
    std::fs::write(&png_path, b"\x89PNG\r\n\x1a\nfake image").unwrap();
    surface.emit(&env(
        1,
        AgentEvent::File {
            path: png_path.to_string_lossy().into_owned(),
            name: "pixel.png".into(),
            caption: "the chart".into(),
        },
    ));
    let photo = server
        .await_capture(|c| c.method == "sendPhoto", Duration::from_secs(30))
        .expect("image uploaded as a photo");
    let raw = photo.raw.as_deref().unwrap_or("");
    assert!(
        raw.contains("name=\"photo\"; filename=\"pixel.png\""),
        "multipart carries the photo filename: {raw}"
    );
    assert!(raw.contains("the chart"), "caption rides along: {raw}");
    assert!(raw.contains("image/png"), "photo content type: {raw}");

    let txt_path = dir.join("notes.txt");
    std::fs::write(&txt_path, b"plain words").unwrap();
    surface.emit(&env(
        2,
        AgentEvent::File {
            path: txt_path.to_string_lossy().into_owned(),
            name: "notes.txt".into(),
            caption: String::new(),
        },
    ));
    let doc = server
        .await_capture(|c| c.method == "sendDocument", Duration::from_secs(30))
        .expect("non-image uploaded as a document");
    let raw = doc.raw.as_deref().unwrap_or("");
    assert!(
        raw.contains("name=\"document\"; filename=\"notes.txt\""),
        "multipart carries the document filename: {raw}"
    );
    assert!(
        !raw.contains("name=\"caption\""),
        "no empty caption field: {raw}"
    );

    // A vanished file must not disappear silently: a notice carries the
    // absolute path.
    let ghost = dir.join("ghost.bin");
    surface.emit(&env(
        3,
        AgentEvent::File {
            path: ghost.to_string_lossy().into_owned(),
            name: "ghost.bin".into(),
            caption: String::new(),
        },
    ));
    let notice = server
        .await_capture(
            |c| {
                c.method == "sendMessage"
                    && c.body["text"].as_str().unwrap_or("").contains("! [file]")
            },
            Duration::from_secs(30),
        )
        .expect("failure notice");
    let text = notice.body["text"].as_str().unwrap_or("");
    assert!(
        text.contains(&ghost.to_string_lossy().into_owned()),
        "absolute path in the notice: {text}"
    );
}

/// M5: Telegram caps captions at 1024 characters; a longer one is
/// truncated (with `…`) before the request and the file still goes out.
#[tokio::test(flavor = "multi_thread")]
async fn over_long_caption_is_capped_and_the_file_still_goes_out() {
    let (server, base) = FakeBotServer::spawn();
    let surface = TelegramSurface::new(base, "tok-caption".to_string(), Some(7));
    let dir = temp_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let png = dir.join("chart.png");
    std::fs::write(&png, b"\x89PNG\r\n\x1a\nfake").unwrap();
    surface.emit(&env(
        1,
        AgentEvent::File {
            path: png.to_string_lossy().into_owned(),
            name: "chart.png".into(),
            caption: "c".repeat(3000),
        },
    ));
    let photo = server
        .await_capture(|c| c.method == "sendPhoto", Duration::from_secs(30))
        .expect("the file still goes out");
    let raw = photo.raw.as_deref().unwrap_or("");
    let marker = "name=\"caption\"\r\n\r\n";
    let start = raw.find(marker).expect("caption field present") + marker.len();
    let rest = &raw[start..];
    let end = rest.find("\r\n--").expect("caption field terminator");
    let caption = &rest[..end];
    assert_eq!(
        caption.chars().count(),
        1024,
        "capped at Telegram's caption limit"
    );
    assert!(caption.ends_with('…'), "ellipsis marks the truncation");
}

/// 3.3: while tools run and no answer text has streamed, the draft shows
/// `⋯ tool label`; scheduled turns (cron/briefing) never open a draft.
#[tokio::test(flavor = "multi_thread")]
async fn draft_shows_tool_progress_and_skips_scheduled_turns() {
    let (server, base) = FakeBotServer::spawn();
    let surface = TelegramSurface::new(base, "tok-activity".to_string(), Some(7));
    surface.emit(&env(
        1,
        AgentEvent::TurnStarted {
            turn: String::new(),
            source: "telegram:42".into(),
        },
    ));
    surface.emit(&env(
        2,
        AgentEvent::ToolStarted {
            id: "t1".into(),
            tool: "bash".into(),
            detail: "ls".into(),
        },
    ));
    let first = server
        .await_capture(|c| c.method == "sendMessageDraft", Duration::from_secs(30))
        .expect("activity draft");
    assert_eq!(first.body["text"].as_str(), Some("⋯ running a command"));
    let draft_id = first.body["draft_id"].as_i64().unwrap();

    // ToolFinished clears the label; the newest tool replaces it under the
    // same draft id once the throttle window elapsed.
    surface.emit(&env(
        3,
        AgentEvent::ToolFinished {
            id: "t1".into(),
            tool: "bash".into(),
            ok: true,
            ms: 3,
            error: None,
            output: None,
        },
    ));
    tokio::time::sleep(Duration::from_millis(1100)).await;
    surface.emit(&env(
        4,
        AgentEvent::ToolStarted {
            id: "t2".into(),
            tool: "web_search".into(),
            detail: "rust news".into(),
        },
    ));
    let next = server
        .await_capture(
            |c| {
                c.method == "sendMessageDraft"
                    && c.body["text"].as_str() == Some("⋯ searching the web")
            },
            Duration::from_secs(30),
        )
        .expect("newest tool label");
    assert_eq!(next.body["draft_id"].as_i64(), Some(draft_id));

    // Scheduled turns: no draft for tool progress or streamed text.
    let (cron_server, cron_base) = FakeBotServer::spawn();
    let cron = TelegramSurface::new(cron_base, "tok-cron".to_string(), Some(7));
    cron.emit(&env(
        1,
        AgentEvent::TurnStarted {
            turn: String::new(),
            source: "cron:task_1".into(),
        },
    ));
    cron.emit(&env(
        2,
        AgentEvent::ToolStarted {
            id: "t1".into(),
            tool: "bash".into(),
            detail: "ls".into(),
        },
    ));
    cron.emit(&env(
        3,
        AgentEvent::Delta {
            text: "scheduled text ".repeat(6),
        },
    ));
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        cron_server
            .captured()
            .iter()
            .all(|c| c.method != "sendMessageDraft"),
        "no drafts for scheduled turns: {:?}",
        cron_server.captured()
    );
}

/// 3.3: the skill-proposal card's Accept runs the same path as
/// `/skills accept <id>` — the SKILL.md lands under
/// `$KA_AGENT_HOME/skills/<name>/` and the store row turns accepted.
/// `std::env::set_var` is unsafe (forbidden crate-wide), so the body
/// re-executes this test with `KA_AGENT_HOME` pointed at a temp dir.
#[tokio::test(flavor = "multi_thread")]
async fn skill_proposal_card_accepts_and_installs() {
    if std::env::var_os("KA_TG_SKILL_CHILD").is_some() {
        skill_proposal_accept_body().await;
        return;
    }
    let home = temp_dir();
    std::fs::create_dir_all(&home).unwrap();
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "skill_proposal_card_accepts_and_installs",
            "--nocapture",
        ])
        .env("KA_TG_SKILL_CHILD", "1")
        .env("KA_AGENT_HOME", &home)
        .env("XDG_CONFIG_HOME", home.join("xdg"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "child failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let installed = home.join("skills/review-helper/SKILL.md");
    let body = std::fs::read_to_string(&installed)
        .unwrap_or_else(|e| panic!("{} must exist after accept: {e}", installed.display()));
    assert!(body.contains("name: review-helper"), "{body}");
    assert!(body.contains("Read the diff first."), "{body}");
}

async fn skill_proposal_accept_body() {
    let mut stack = spawn_channel(vec![42], "tok-skill").await;
    // The operator's chat is known once they wrote.
    stack.server.push_text_message(1, 7, 42, "/status");
    stack
        .server
        .await_capture(|c| c.method == "sendMessage", Duration::from_secs(30))
        .expect("status reply");

    let id = "skill_ab12cd34";
    stack
        .store
        .add_skill_candidate(
            id,
            "review-helper",
            "Helps with code review",
            "Read the diff first.",
        )
        .unwrap();
    stack.runtime.emit(AgentEvent::SkillProposal {
        id: id.into(),
        name: "review-helper".into(),
        description: "Helps with code review".into(),
    });
    let card = stack
        .server
        .await_capture(
            |c| {
                c.method == "sendMessage"
                    && c.body["text"]
                        .as_str()
                        .unwrap_or("")
                        .contains("Proposed skill: review-helper")
            },
            Duration::from_secs(30),
        )
        .expect("skill card");
    let keyboard = &card.body["reply_markup"]["inline_keyboard"][0];
    let accept = keyboard[0]["callback_data"].as_str().unwrap().to_string();
    let reject = keyboard[1]["callback_data"].as_str().unwrap().to_string();
    assert_eq!(accept, format!("sk:a:{id}"));
    assert_eq!(reject, format!("sk:r:{id}"));
    assert!(accept.len() <= 64 && reject.len() <= 64);

    stack.server.push_callback(2, 7, 42, &accept);
    let answered = stack
        .server
        .await_capture(
            |c| c.method == "answerCallbackQuery",
            Duration::from_secs(30),
        )
        .expect("callback answered");
    assert_eq!(answered.body["text"].as_str(), Some("accepted"));
    let edited = stack
        .server
        .await_capture(|c| c.method == "editMessageText", Duration::from_secs(30))
        .expect("card outcome");
    let text = edited.body["text"].as_str().unwrap_or("");
    assert!(
        text.contains("installed and enabled skill review-helper"),
        "outcome text: {text}"
    );
    // The store row is accepted (no longer pending).
    assert!(stack.store.pending_skill_candidates().unwrap().is_empty());
    let conn = rusqlite::Connection::open(&stack.db_path).unwrap();
    let status: String = conn
        .query_row(
            "SELECT status FROM skill_candidates WHERE id = ?1",
            [id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(status, "accepted");

    // M4: a double tap is a no-op — the second press answers "already
    // installed" instead of rewriting the card's installed outcome.
    stack.server.push_callback(3, 7, 42, &accept);
    let second = stack
        .server
        .await_capture(
            |c| {
                c.method == "answerCallbackQuery"
                    && c.body["text"].as_str() == Some("already installed")
            },
            Duration::from_secs(30),
        )
        .expect("second press answered as already installed");
    assert!(second.body["callback_query_id"].as_str().is_some());
    tokio::time::sleep(Duration::from_millis(400)).await;
    let edits = stack
        .server
        .captured()
        .iter()
        .filter(|c| c.method == "editMessageText")
        .count();
    assert_eq!(edits, 1, "the card keeps its installed outcome");
    let accepted: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM skill_candidates WHERE status = 'accepted'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(accepted, 1, "one install, not two");
    stop(&mut stack).await;
}

// ---------------------------------------------------------------------------
// v2 B1–B7: captions, albums, unsupported media, deep-link pairing,
// upload actions, inline video, bot profile.
// ---------------------------------------------------------------------------

/// A private photo message with an optional caption and media group, as
/// the Bot API shapes it (size-ascending variants).
fn push_photo_message(
    server: &FakeBotServer,
    update_id: i64,
    chat_id: i64,
    user_id: i64,
    file_id: &str,
    caption: Option<&str>,
    media_group_id: Option<&str>,
) {
    let mut message = json!({
        "message_id": update_id,
        "from": { "id": user_id, "is_bot": false },
        "chat": { "id": chat_id, "type": "private" },
        "date": 0,
        "photo": [
            { "file_id": format!("{file_id}-small"), "width": 90, "height": 90 },
            { "file_id": file_id, "width": 1280, "height": 720 },
        ],
    });
    if let Some(caption) = caption {
        message["caption"] = json!(caption);
    }
    if let Some(group) = media_group_id {
        message["media_group_id"] = json!(group);
    }
    server.push_update(json!({ "update_id": update_id, "message": message }));
}

/// B5: startup sets the bot profile beside the command menu — the
/// empty-chat "What can this bot do?" text and the chat-list blurb.
#[tokio::test(flavor = "multi_thread")]
async fn startup_sets_the_bot_profile_descriptions() {
    let mut stack = spawn_channel(vec![42], "tok-profile").await;
    let description = stack
        .server
        .await_capture(|c| c.method == "setMyDescription", Duration::from_secs(5))
        .expect("setMyDescription at startup");
    assert!(
        !description.body["description"]
            .as_str()
            .unwrap_or("")
            .is_empty(),
        "non-empty description: {:?}",
        description.body
    );
    let short = stack
        .server
        .await_capture(
            |c| c.method == "setMyShortDescription",
            Duration::from_secs(5),
        )
        .expect("setMyShortDescription at startup");
    let blurb = short.body["description"].as_str().unwrap_or("");
    assert!(!blurb.is_empty(), "non-empty blurb: {blurb:?}");
    assert!(blurb.chars().count() <= 120, "blurb over the 120 cap");
    stop(&mut stack).await;
}

/// B2: a video note is refused with a visible `! [media] …` reply — no
/// turn, no 👀 receipt, never a silent drop.
#[tokio::test(flavor = "multi_thread")]
async fn video_note_gets_an_unsupported_reply_and_no_turn() {
    let mut stack = spawn_channel(vec![42], "tok-vnote").await;
    stack.server.push_update(json!({
        "update_id": 1,
        "message": {
            "message_id": 1,
            "from": { "id": 42, "is_bot": false },
            "chat": { "id": 7, "type": "private" },
            "date": 0,
            "video_note": { "file_id": "vn-1", "duration": 8, "length": 240 },
        }
    }));
    let reply = stack
        .server
        .await_capture(
            |c| {
                c.method == "sendMessage"
                    && c.body["text"].as_str().unwrap_or("").contains("video note")
            },
            Duration::from_secs(5),
        )
        .expect("unsupported media refusal");
    assert_eq!(
        reply.body["text"].as_str(),
        Some("! [media] video note isn't supported yet")
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    let reacted = stack
        .server
        .captured()
        .iter()
        .any(|c| c.method == "setMessageReaction");
    assert!(!reacted, "no 👀 receipt for unsupported media");
    stop(&mut stack).await;
}

/// B1: a captioned photo is the operator's message — 👀 receipt and a
/// submitted turn — not a bare "saved" ack. (The engine turn itself is
/// asserted app-level; here the submission path is.)
#[tokio::test(flavor = "multi_thread")]
async fn captioned_photo_is_submitted_not_acked() {
    let mut stack = spawn_channel(vec![42], "tok-cap").await;
    stack
        .server
        .push_file("f-cap-1", "photos/f-cap-1.jpg", b"jpegbytes");
    push_photo_message(
        &stack.server,
        1,
        7,
        42,
        "f-cap-1",
        Some("what is this?"),
        None,
    );
    let reaction = stack
        .server
        .await_capture(|c| c.method == "setMessageReaction", Duration::from_secs(5))
        .expect("👀 on the captioned photo");
    assert_eq!(reaction.body["message_id"].as_i64(), Some(1));
    tokio::time::sleep(Duration::from_millis(400)).await;
    let acked = stack.server.captured().into_iter().any(|c| {
        c.method == "sendMessage"
            && c.body["text"]
                .as_str()
                .unwrap_or("")
                .contains("saved attachments/")
    });
    assert!(!acked, "a captioned photo is not answered with a bare ack");
    stop(&mut stack).await;
}

/// B7: a three-photo album (one `media_group_id`) is saved in full and
/// answered once, naming every `@attachments/…` ref.
#[tokio::test(flavor = "multi_thread")]
async fn album_of_three_photos_gets_one_ack_naming_every_ref() {
    let mut stack = spawn_channel(vec![42], "tok-album").await;
    for i in 1..=3 {
        stack.server.push_file(
            &format!("f-alb-{i}"),
            &format!("photos/alb{i}.jpg"),
            b"jpeg",
        );
    }
    for i in 1..=3 {
        push_photo_message(
            &stack.server,
            i,
            7,
            42,
            &format!("f-alb-{i}"),
            None,
            Some("mg-1"),
        );
    }
    let ack = stack
        .server
        .await_capture(
            |c| {
                c.method == "sendMessage"
                    && c.body["text"]
                        .as_str()
                        .unwrap_or("")
                        .starts_with("saved 3 files")
            },
            Duration::from_secs(5),
        )
        .expect("one ack for the whole album");
    let text = ack.body["text"].as_str().unwrap_or("");
    assert_eq!(
        text.matches("attachments/photo-").count(),
        3,
        "names every ref: {text}"
    );
    // The debounce must fold the whole album into that one reply.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let acks = stack
        .server
        .captured()
        .into_iter()
        .filter(|c| {
            c.method == "sendMessage" && c.body["text"].as_str().unwrap_or("").contains("saved")
        })
        .count();
    assert_eq!(acks, 1, "exactly one ack for the album");
    let dir = stack.runtime.workspace().join("attachments");
    let files = std::fs::read_dir(&dir).unwrap().flatten().count();
    assert_eq!(files, 3, "every album member lands on disk");
    stop(&mut stack).await;
}

/// B7+B1: a caption on any album member turns the single flush into one
/// caption turn (one 👀 on the captioned member) — never per-photo acks.
#[tokio::test(flavor = "multi_thread")]
async fn album_with_caption_submits_one_turn_not_per_photo_acks() {
    let mut stack = spawn_channel(vec![42], "tok-alb-cap").await;
    for i in 1..=3 {
        stack
            .server
            .push_file(&format!("f-ac-{i}"), &format!("photos/ac{i}.jpg"), b"jpeg");
    }
    push_photo_message(&stack.server, 1, 7, 42, "f-ac-1", None, Some("mg-2"));
    push_photo_message(
        &stack.server,
        2,
        7,
        42,
        "f-ac-2",
        Some("look at these three"),
        Some("mg-2"),
    );
    push_photo_message(&stack.server, 3, 7, 42, "f-ac-3", None, Some("mg-2"));
    let reaction = stack
        .server
        .await_capture(|c| c.method == "setMessageReaction", Duration::from_secs(5))
        .expect("one 👀 for the album's caption");
    // The receipt lands on the captioned member, not the first one.
    assert_eq!(reaction.body["message_id"].as_i64(), Some(2));
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let reactions = stack
        .server
        .captured()
        .iter()
        .filter(|c| c.method == "setMessageReaction")
        .count();
    assert_eq!(reactions, 1, "the album is one turn, not three");
    let acks = stack.server.captured().into_iter().any(|c| {
        c.method == "sendMessage" && c.body["text"].as_str().unwrap_or("").contains("saved")
    });
    assert!(!acks, "a captioned album is not answered with acks");
    let dir = stack.runtime.workspace().join("attachments");
    let files = std::fs::read_dir(&dir).unwrap().flatten().count();
    assert_eq!(files, 3, "every album member lands on disk");
    stop(&mut stack).await;
}

/// B3: a `t.me/<bot>?start=<code>` deep link pairs the sender — no
/// `/pair` typing needed.
#[tokio::test(flavor = "multi_thread")]
async fn start_deep_link_pairs_the_sender() {
    let mut stack = spawn_channel(vec![], "tok-deep-ok").await;
    let code = ka_agent_channels::pairing::issue(&stack.store, "telegram").unwrap();
    stack
        .server
        .push_text_message(1, 7, 55, &format!("/start {code}"));
    let welcome = stack
        .server
        .await_capture(
            |c| {
                c.method == "sendMessage"
                    && c.body["text"].as_str().unwrap_or("").contains("✓ Paired")
            },
            Duration::from_secs(5),
        )
        .expect("the deep link pairs and welcomes");
    assert_eq!(welcome.body["chat_id"].as_i64(), Some(7));
    assert!(stack.store.is_linked("telegram", "55").unwrap());
    stop(&mut stack).await;
}

/// B3: a wrong deep-link payload burns an attempt exactly like a wrong
/// `/pair` — no welcome, never the hourly owner hint — and the code
/// still redeems afterwards.
#[tokio::test(flavor = "multi_thread")]
async fn start_deep_link_wrong_payload_burns_an_attempt() {
    let mut stack = spawn_channel(vec![], "tok-deep-bad").await;
    let code = ka_agent_channels::pairing::issue(&stack.store, "telegram").unwrap();
    stack
        .server
        .push_text_message(1, 7, 56, "/start WRONGWRONG");
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        !stack.store.is_linked("telegram", "56").unwrap(),
        "a wrong payload never links"
    );
    let raw = stack
        .store
        .meta_get(ka_agent_channels::pairing::PAIRING_KEY)
        .unwrap()
        .unwrap_or_default();
    assert!(
        raw.contains("\"attempts\":1"),
        "the wrong payload burned one attempt: {raw}"
    );
    let answered = stack.server.captured().into_iter().any(|c| {
        c.method == "sendMessage"
            && c.body["text"]
                .as_str()
                .is_some_and(|t| t.contains("✓ Paired") || t.contains("private assistant"))
    });
    assert!(
        !answered,
        "neither the welcome nor the hourly START_REPLY answers a failed deep link"
    );
    // One burned attempt leaves four: the right code still pairs.
    stack
        .server
        .push_text_message(2, 7, 56, &format!("/start {code}"));
    stack
        .server
        .await_capture(
            |c| {
                c.method == "sendMessage"
                    && c.body["text"].as_str().unwrap_or("").contains("✓ Paired")
            },
            Duration::from_secs(5),
        )
        .expect("the code still redeems after a burned attempt");
    assert!(stack.store.is_linked("telegram", "56").unwrap());
    stop(&mut stack).await;
}

/// B4: outgoing files fire the matching `upload_*` chat action once,
/// before the send — the phone shows an upload instead of "typing…".
#[tokio::test(flavor = "multi_thread")]
async fn file_event_fires_the_upload_action_before_the_send() {
    let (server, base) = FakeBotServer::spawn();
    let surface = TelegramSurface::new(base, "tok-upload".to_string(), Some(7));
    let dir = temp_dir();
    std::fs::create_dir_all(&dir).unwrap();

    let png = dir.join("pixel.png");
    std::fs::write(&png, b"\x89PNG fake").unwrap();
    surface.emit(&env(
        1,
        AgentEvent::File {
            path: png.to_string_lossy().into_owned(),
            name: "pixel.png".into(),
            caption: String::new(),
        },
    ));
    server
        .await_capture(
            |c| c.method == "sendChatAction" && c.body["action"].as_str() == Some("upload_photo"),
            Duration::from_secs(30),
        )
        .expect("upload_photo action");
    server
        .await_capture(|c| c.method == "sendPhoto", Duration::from_secs(30))
        .expect("photo goes out");
    let positions = server.captured();
    let action = positions
        .iter()
        .position(|c| {
            c.method == "sendChatAction" && c.body["action"].as_str() == Some("upload_photo")
        })
        .unwrap();
    let sent = positions
        .iter()
        .position(|c| c.method == "sendPhoto")
        .unwrap();
    assert!(action < sent, "upload_photo precedes sendPhoto");

    let txt = dir.join("notes.txt");
    std::fs::write(&txt, b"words").unwrap();
    surface.emit(&env(
        2,
        AgentEvent::File {
            path: txt.to_string_lossy().into_owned(),
            name: "notes.txt".into(),
            caption: String::new(),
        },
    ));
    server
        .await_capture(
            |c| {
                c.method == "sendChatAction" && c.body["action"].as_str() == Some("upload_document")
            },
            Duration::from_secs(30),
        )
        .expect("upload_document action");
    let doc = server
        .await_capture(|c| c.method == "sendDocument", Duration::from_secs(30))
        .expect("document goes out");
    let raw = doc.raw.as_deref().unwrap_or("");
    assert!(
        raw.contains("name=\"document\"; filename=\"notes.txt\""),
        "{raw}"
    );
}

/// B6: videos and GIFs go out inline (`sendVideo` / `sendAnimation`) with
/// their captions; a refused inline send falls back to a document, so the
/// file is never lost.
#[tokio::test(flavor = "multi_thread")]
async fn video_and_gif_go_out_inline_with_document_fallback() {
    let (server, base) = FakeBotServer::spawn();
    let surface = TelegramSurface::new(base, "tok-video".to_string(), Some(7));
    let dir = temp_dir();
    std::fs::create_dir_all(&dir).unwrap();

    let mp4 = dir.join("clip.mp4");
    std::fs::write(&mp4, b"\0\0\0\x18ftypmp42fake").unwrap();
    surface.emit(&env(
        1,
        AgentEvent::File {
            path: mp4.to_string_lossy().into_owned(),
            name: "clip.mp4".into(),
            caption: "the recording".into(),
        },
    ));
    let video = server
        .await_capture(|c| c.method == "sendVideo", Duration::from_secs(30))
        .expect("mp4 goes out via sendVideo");
    let raw = video.raw.as_deref().unwrap_or("");
    assert!(
        raw.contains("name=\"video\"; filename=\"clip.mp4\""),
        "video part: {raw}"
    );
    assert!(raw.contains("video/mp4"), "video content type: {raw}");
    assert!(raw.contains("the recording"), "caption rides along: {raw}");

    let gif = dir.join("loop.gif");
    std::fs::write(&gif, b"GIF89a fake").unwrap();
    surface.emit(&env(
        2,
        AgentEvent::File {
            path: gif.to_string_lossy().into_owned(),
            name: "loop.gif".into(),
            caption: String::new(),
        },
    ));
    let animation = server
        .await_capture(|c| c.method == "sendAnimation", Duration::from_secs(30))
        .expect("gif goes out via sendAnimation");
    let raw = animation.raw.as_deref().unwrap_or("");
    assert!(
        raw.contains("name=\"animation\"; filename=\"loop.gif\""),
        "animation part: {raw}"
    );

    // A refused sendVideo must not lose the file: the document fallback.
    server.fail_method("sendVideo", 400, None, 1);
    let mp4b = dir.join("clip2.mp4");
    std::fs::write(&mp4b, b"\0\0\0\x18ftypmp42fake").unwrap();
    surface.emit(&env(
        3,
        AgentEvent::File {
            path: mp4b.to_string_lossy().into_owned(),
            name: "clip2.mp4".into(),
            caption: String::new(),
        },
    ));
    let doc = server
        .await_capture(
            |c| {
                c.method == "sendDocument"
                    && c.raw.as_deref().is_some_and(|r| r.contains("clip2.mp4"))
            },
            Duration::from_secs(30),
        )
        .expect("refused sendVideo falls back to a document");
    let raw = doc.raw.as_deref().unwrap_or("");
    assert!(
        raw.contains("name=\"document\"; filename=\"clip2.mp4\""),
        "{raw}"
    );
}
