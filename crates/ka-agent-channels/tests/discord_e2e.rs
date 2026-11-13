//! Discord e2e against a fake gateway + REST: DMs, the mention gate, a
//! button interaction by the acting user vs anyone else, the guild
//! allowlist, and resume-after-drop (the client must send op 6 with the
//! cached session, never Identify again).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use ka_agent_channels::discord::{DiscordChannel, DiscordConfig, DiscordParts};
use ka_agent_channels::fake_discord::FakeDiscord;
use ka_agent_core::config::RoleSpec;
use ka_agent_core::{DaemonConfig, Principal};
use ka_agent_port::WorkerPort;
use ka_agent_runtime::surface::Surface;
use ka_agent_runtime::{Paths, Runtime};
use ka_agent_store::Store;
use serde_json::json;
use tokio::sync::watch;

static DIR_SEQ: AtomicU32 = AtomicU32::new(0);

#[test]
#[allow(non_snake_case)]
fn mock__worker() {
    if !std::env::args().any(|a| a == "__worker") {
        return;
    }
    let fallback = std::env::var_os("KA_DATA_DIR")
        .map(PathBuf::from)
        .map(|d| d.join("scenario.json"))
        .filter(|p| p.exists());
    let _ = ka_agent_port::run_worker(ka_agent_port::WorkerRun { scenario: fallback });
}

#[cfg(unix)]
fn scripted_port(root: &std::path::Path) -> WorkerPort {
    let wrapper = root.join("worker.sh");
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
        scenario_override: Some(root.join("ka/scenario.json")),
    }
}

#[cfg(not(unix))]
fn scripted_port(_root: &std::path::Path) -> WorkerPort {
    WorkerPort::default()
}

struct Stack {
    fake: FakeDiscord,
    root: PathBuf,
    store: Arc<Store>,
    runtime: Runtime,
    shutdown: watch::Sender<bool>,
    handle: tokio::task::JoinHandle<()>,
}

fn stack(tag: &str, guilds: &[&str]) -> Stack {
    let dir = std::env::temp_dir().join(format!(
        "ka-discord-e2e-{tag}-{}-{}",
        std::process::id(),
        DIR_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("workspace")).unwrap();
    let (fake, base) = FakeDiscord::spawn();
    std::fs::create_dir_all(dir.join("secrets")).unwrap();
    std::fs::write(dir.join("secrets/discord_bot"), "discord-token").unwrap();
    std::fs::create_dir_all(dir.join("ka")).unwrap();
    std::fs::write(
        dir.join("ka/scenario.json"),
        r#"{"steps": [{"text": "ok from the engine"}], "stop": "done", "cost_usd": 0.0, "tokens": 10}"#,
    )
    .unwrap();
    let store = Arc::new(Store::open(&dir.join("state.db")).unwrap());
    store
        .link_identity_to_user("discord", "G1:U1", "test", "member", "ana")
        .unwrap();
    store
        .link_identity_to_user("discord", "G1:U2", "test", "member", "bea")
        .unwrap();
    // A DM identity has no guild prefix.
    store
        .link_identity_to_user("discord", "dm:U1", "test", "member", "ana")
        .unwrap();
    let cfg = DaemonConfig {
        workspace: dir.join("workspace"),
        sandbox: "off".into(),
        discord: ka_agent_core::DiscordSettings {
            enabled: true,
            bot_token_secret: Some("discord_bot".into()),
            guilds: guilds.iter().map(|g| g.to_string()).collect(),
            auto_provision: false,
            api_base: Some(base),
        },
        roles: vec![
            RoleSpec {
                name: "member".into(),
                mode: "readonly".into(),
                allow: vec!["read".into()],
                commands: vec!["status".into(), "help".into()],
                ..RoleSpec::default()
            },
            RoleSpec {
                name: "channel".into(),
                mode: "readonly".into(),
                allow: vec!["read".into()],
                commands: vec!["help".into()],
                ..RoleSpec::default()
            },
        ],
        ..DaemonConfig::default()
    };
    let dcfg = DiscordConfig::from_config(&cfg, &dir.join("secrets"))
        .unwrap()
        .expect("discord enabled");
    let parts = DiscordParts::new(&dcfg);
    let surface: Arc<dyn Surface> = Arc::new(ka_agent_channels::MultiSurface::new(vec![
        Arc::new(NullSurface),
        parts.surface(),
    ]));
    let runtime = Runtime::new(
        cfg.clone(),
        Paths::new(dir.clone()),
        Arc::clone(&store),
        scripted_port(&dir),
        Arc::clone(&surface),
    )
    .unwrap();
    parts.attach_runtime(&runtime);
    let connector = Arc::new(DiscordChannel::new(
        dcfg,
        runtime.clone(),
        Arc::clone(&store),
        parts,
    ));
    let (shutdown, rx) = watch::channel(false);
    let handle = tokio::spawn(connector.run(rx));
    Stack {
        fake,
        root: dir,
        store,
        runtime,
        shutdown,
        handle,
    }
}

struct NullSurface;
impl Surface for NullSurface {
    fn emit(&self, _env: &ka_agent_runtime::Envelope) {}
}

async fn stop(stack: &mut Stack) {
    let _ = stack.shutdown.send(true);
    let handle = std::mem::replace(&mut stack.handle, tokio::spawn(async {}));
    let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    let _ = std::fs::remove_dir_all(&stack.root);
}

async fn wait_for<F: Fn() -> bool>(what: &str, f: F) {
    let deadline = Instant::now() + Duration::from_secs(45);
    while Instant::now() < deadline {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

fn dm_message(user: &str, text: &str) -> serde_json::Value {
    json!({
        "id": "m1", "channel_id": "D1", "content": text,
        "author": {"id": user, "bot": false}
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn dms_answer_and_bots_are_ignored() {
    let mut stack = stack("dm", &["G1"]);
    stack.fake.wait_socket().await;
    stack.fake.send_message(dm_message("U1", "hello"));
    wait_for("the DM reply", || {
        stack
            .fake
            .calls("POST")
            .iter()
            .any(|c| c.path.contains("/channels/D1/messages")
                && c.body["content"].as_str().unwrap_or("").contains("ok from the engine"))
    })
    .await;
    let post = stack.fake.calls("POST")[0].clone();
    assert!(post.path.contains("/channels/D1/messages"), "{post:?}");
    // A bot's own message is never a turn.
    stack.fake.send_message(json!({
        "id": "m2", "channel_id": "D1", "content": "ok from the engine",
        "author": {"id": "UBOT", "bot": true}
    }));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(stack.fake.calls("POST").len(), 1);
    stop(&mut stack).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn guild_needs_a_mention_and_uses_the_channel_account() {
    let mut stack = stack("mention", &["G1"]);
    stack.fake.wait_socket().await;
    // Not mentioned: ignored.
    stack.fake.send_message(json!({
        "id": "m1", "channel_id": "C1", "guild_id": "G1", "content": "chatting",
        "author": {"id": "U1", "bot": false}
    }));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(stack.fake.calls("POST").is_empty());
    // Mentioned: answers, as the channel account.
    stack.fake.send_message(json!({
        "id": "m2", "channel_id": "C1", "guild_id": "G1",
        "content": "<@UBOT> summarize the plan",
        "author": {"id": "U1", "bot": false}
    }));
    wait_for("the channel reply", || !stack.fake.calls("POST").is_empty()).await;
    let surface_user = ka_agent_channels::slack::surface_username("discord", "G1", "C1");
    let session = stack
        .store
        .latest_session_for_user(&surface_user)
        .unwrap()
        .expect("the channel account has its own session");
    assert_eq!(session.role, "channel");
    assert!(stack.store.latest_session_for_user("ana").unwrap().is_none());
    stop(&mut stack).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_guild_is_ignored_and_audited() {
    let mut stack = stack("guild", &["G1"]);
    stack.fake.wait_socket().await;
    stack.fake.send_message(json!({
        "id": "m1", "channel_id": "C9", "guild_id": "G9", "content": "<@UBOT> hi",
        "author": {"id": "U9", "bot": false}
    }));
    wait_for("the unauthorized audit row", || {
        stack
            .store
            .recent_audit(10)
            .map(|rows| rows.iter().any(|r| r.action == "discord.unauthorized_guild"))
            .unwrap_or(false)
    })
    .await;
    assert!(stack.fake.calls("POST").is_empty());
    stop(&mut stack).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn interaction_is_refused_for_another_user() {
    let mut stack = stack("interaction", &["G1"]);
    stack.fake.wait_socket().await;
    let ana = Principal::new("ana", "member");
    stack.runtime.ensure_user_state(&ana).unwrap();
    let sid = stack.store.latest_session_for_user("ana").unwrap().unwrap().id;
    let approval = ka_agent_core::Approval::new(
        sid,
        ka_agent_core::TurnId::generate(),
        ka_agent_core::ApprovalKind::KaAsk {
            ask_id: "ask-1".into(),
            question: "run `echo hi`?".into(),
            detail: None,
            options: vec!["allow".into(), "deny".into()],
        },
        300,
    )
    .routed_to("ana", false);
    stack.store.insert_approval(&approval).unwrap();
    // bea clicks: refused with an ephemeral (flags 64), row stays pending.
    stack.fake.send_interaction(json!({
        "id": "i1", "token": "tok1", "type": 3,
        "guild_id": "G1", "member": {"user": {"id": "U2"}},
        "data": {"custom_id": format!("ka_approve:{}", approval.id)}
    }));
    wait_for("the refusal callback", || {
        stack
            .fake
            .calls("POST")
            .iter()
            .any(|c| c.path.contains("/interactions/i1/tok1/callback"))
    })
    .await;
    let refused = stack.fake.calls("POST")[0].clone();
    assert_eq!(refused.body["type"], 4);
    assert_eq!(refused.body["data"]["flags"], 64);
    assert_eq!(
        stack.store.get_approval(&approval.id).unwrap().status,
        ka_agent_core::ApprovalStatus::Pending
    );
    // ana clicks: reaches the engine check (still refuses, but not on
    // authorization) and gets an update rather than an ephemeral.
    stack.fake.send_interaction(json!({
        "id": "i2", "token": "tok2", "type": 3,
        "guild_id": "G1", "member": {"user": {"id": "U1"}},
        "data": {"custom_id": format!("ka_approve:{}", approval.id)}
    }));
    wait_for("the second callback", || {
        stack.fake.calls("POST").len() >= 2
    })
    .await;
    let second = stack.fake.calls("POST")[1].clone();
    assert_eq!(second.body["type"], 4, "no live ask to update: {second:?}");
    assert!(
        second.body["data"]["content"]
            .as_str()
            .unwrap_or("")
            .contains("no pending approval"),
        "{second:?}"
    );
    stop(&mut stack).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dropped_gateway_resumes_with_the_session() {
    let mut stack = stack("resume", &["G1"]);
    stack.fake.wait_socket().await;
    assert_eq!(stack.fake.sessions(), 1);
    stack.fake.drop_socket();
    wait_for("a resume", || !stack.fake.resumes().is_empty()).await;
    let resumes = stack.fake.resumes();
    assert_eq!(resumes[0].0, 6, "op 6 (resume)");
    assert_eq!(resumes[0].1, "sess-1");
    // Still exactly one Identify: the drop resumed instead of re-identifying
    // (the 1000/24 h cap).
    assert_eq!(stack.fake.sessions(), 2);
    stop(&mut stack).await;
}
