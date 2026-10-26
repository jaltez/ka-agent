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
                        .contains("mode → plan")
            },
            Duration::from_secs(30),
        )
        .expect("tightening allowed");
    assert!(
        std::fs::read_to_string(&cfg_path)
            .unwrap()
            .contains("mode = \"plan\"")
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
