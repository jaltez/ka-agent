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

#[tokio::test(flavor = "multi_thread")]
async fn surface_noops_until_chat_is_known() {
    let (server, base) = FakeBotServer::spawn();
    let surface = TelegramSurface::new(base, "tok-nochat".to_string(), None);
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

/// D1: a streamed reply shows as a Bot API draft (throttled, growing),
/// then arrives as the finished message.
#[tokio::test(flavor = "multi_thread")]
async fn streamed_reply_uses_drafts_then_sends_the_final_message() {
    let (server, base) = FakeBotServer::spawn();
    let surface = TelegramSurface::new(base, "tok-draft".to_string(), Some(7));
    surface.on_turn_started();
    let chunk = "word ".repeat(12); // 60 chars per delta
    for _ in 0..3 {
        surface.on_delta_text(&chunk);
        tokio::time::sleep(Duration::from_millis(1100)).await;
    }
    surface.on_final_chunk(1, &chunk.repeat(3));
    surface.on_turn_finished("done", 0.0, 0);
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

/// D1 fallback: a Bot API server without drafts — the first refusal turns
/// streaming off and the answer still arrives as a normal message.
#[tokio::test(flavor = "multi_thread")]
async fn refused_drafts_fall_back_to_the_finished_message() {
    let (server, base) = FakeBotServer::spawn();
    server.set_reject_drafts(true);
    let surface = TelegramSurface::new(base, "tok-nodraft".to_string(), Some(7));
    surface.on_turn_started();
    let chunk = "word ".repeat(12);
    for _ in 0..3 {
        surface.on_delta_text(&chunk);
        tokio::time::sleep(Duration::from_millis(1100)).await;
    }
    surface.on_final_chunk(1, "the whole answer");
    surface.on_turn_finished("done", 0.0, 0);
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
        .await_capture(|c| c.method == "setMessageReaction", Duration::from_secs(5))
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
        .await_capture(|c| c.method == "sendMessage", Duration::from_secs(5))
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
            Duration::from_secs(5),
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
        .await_capture(|c| c.method == "editMessageText", Duration::from_secs(5))
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
            Duration::from_secs(5),
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
            Duration::from_secs(5),
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
            Duration::from_secs(5),
        )
        .expect("refused on the phone");
    stop(&mut stack).await;
}
