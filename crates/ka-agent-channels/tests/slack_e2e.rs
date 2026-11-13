//! Slack e2e against a fake speaking the Socket Mode envelope/ack protocol
//! plus the Web API: DMs, the mention gate, a Block Kit approval click by
//! the acting user vs anyone else, and the workspace allowlist.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use ka_agent_channels::fake_slack::FakeSlack;
use ka_agent_channels::slack::{SlackChannel, SlackConfig, SlackParts};
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

struct Stack {
    fake: FakeSlack,
    root: PathBuf,
    store: Arc<Store>,
    runtime: Runtime,
    shutdown: watch::Sender<bool>,
    handle: tokio::task::JoinHandle<()>,
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

fn stack(tag: &str, teams: &[&str]) -> Stack {
    let dir = std::env::temp_dir().join(format!(
        "ka-slack-e2e-{tag}-{}-{}",
        std::process::id(),
        DIR_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("workspace")).unwrap();
    let (fake, base) = FakeSlack::spawn();
    // Secrets the config refers to.
    std::fs::create_dir_all(dir.join("secrets")).unwrap();
    std::fs::write(dir.join("secrets/slack_app"), "xapp-test").unwrap();
    std::fs::write(dir.join("secrets/slack_bot"), "xoxb-test").unwrap();
    // The scripted engine answers every turn.
    std::fs::create_dir_all(dir.join("ka")).unwrap();
    std::fs::write(
        dir.join("ka/scenario.json"),
        r#"{"steps": [{"text": "ok from the engine"}], "stop": "done", "cost_usd": 0.0, "tokens": 10}"#,
    )
    .unwrap();
    let store = Arc::new(Store::open(&dir.join("state.db")).unwrap());
    store
        .link_identity_to_user("slack", "T1:U1", "test", "member", "ana")
        .unwrap();
    store
        .link_identity_to_user("slack", "T1:U2", "test", "member", "bea")
        .unwrap();
    let cfg = DaemonConfig {
        workspace: dir.join("workspace"),
        sandbox: "off".into(),
        slack: ka_agent_core::SlackSettings {
            enabled: true,
            app_token_secret: Some("slack_app".into()),
            bot_token_secret: Some("slack_bot".into()),
            teams: teams.iter().map(|t| t.to_string()).collect(),
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
    let scfg = SlackConfig::from_config(&cfg, &dir.join("secrets"))
        .unwrap()
        .expect("slack enabled");
    // The connector's surface joins the daemon's fan-out, exactly like
    // `host.rs` composes it.
    let parts = SlackParts::new(&scfg);
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
    let connector = Arc::new(SlackChannel::new(
        scfg,
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
    // The first engine turn of each worker pays the fake MCP bridge's
    // connect timeout (this test binary hosts no `__mcp`), so allow it.
    let deadline = Instant::now() + Duration::from_secs(45);
    while Instant::now() < deadline {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

fn event_frame(team: &str, envelope: &str, event: serde_json::Value) -> serde_json::Value {
    json!({
        "type": "events_api",
        "envelope_id": envelope,
        "payload": {"team_id": team, "event": event}
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn dms_answer_and_echoes_are_ignored() {
    let mut stack = stack("dm", &["T1"]);
    stack.fake.wait_socket().await;
    // A DM from ana.
    stack.fake.send(event_frame(
        "T1",
        "env-1",
        json!({"type": "message", "channel_type": "im", "channel": "D1", "user": "U1",
               "text": "hello", "ts": "1.1"}),
    ));
    wait_for("the DM reply", || {
        stack
            .fake
            .calls("chat.postMessage")
            .iter()
            .any(|c| c.body["text"].as_str().unwrap_or("").contains("ok from the engine"))
    })
    .await;
    let post = stack.fake.calls("chat.postMessage")[0].clone();
    assert_eq!(post.body["channel"], "D1");
    // The envelope was acked.
    assert!(stack.fake.acks().contains(&"env-1".to_string()));
    // The bot's own echo (bot_id) never becomes a turn.
    stack.fake.send(event_frame(
        "T1",
        "env-2",
        json!({"type": "message", "channel_type": "im", "channel": "D1", "user": "U1",
               "text": "ok from the engine", "bot_id": "B1", "ts": "1.2"}),
    ));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(stack.fake.calls("chat.postMessage").len(), 1);
    let sid = stack
        .store
        .latest_session_for_user("ana")
        .unwrap()
        .expect("ana talked");
    let turn = stack
        .store
        .latest_turns_for_session(&sid.id, 1, 0)
        .unwrap()
        .pop()
        .expect("a turn");
    assert_eq!(turn.inbound_text.lines().last().unwrap_or("").trim(), "hello");
    stop(&mut stack).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn channel_needs_a_mention_and_replies_in_thread() {
    let mut stack = stack("mention", &["T1"]);
    stack.fake.wait_socket().await;
    // An unmentioned channel message is ignored.
    stack.fake.send(event_frame(
        "T1",
        "env-1",
        json!({"type": "message", "channel_type": "channel", "channel": "C1", "user": "U1",
               "text": "just chatting", "ts": "2.1"}),
    ));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(stack.fake.calls("chat.postMessage").is_empty());
    // A mention answers, in the thread, under the channel role.
    stack.fake.send(event_frame(
        "T1",
        "env-2",
        json!({"type": "app_mention", "channel": "C1", "user": "U1",
               "text": "<@UBOT> summarize the plan", "ts": "2.2"}),
    ));
    wait_for("the threaded reply", || {
        !stack.fake.calls("chat.postMessage").is_empty()
    })
    .await;
    let post = stack.fake.calls("chat.postMessage")[0].clone();
    assert_eq!(post.body["channel"], "C1");
    assert_eq!(post.body["thread_ts"], "2.2");
    // The turn ran as a per-channel surface account under the channel role:
    // ana's own session and partition stay untouched.
    assert!(
        stack.store.latest_session_for_user("ana").unwrap().is_none(),
        "a shared-channel turn must not open the person's own session"
    );
    let surface_user = ka_agent_channels::slack::surface_username("slack", "T1", "C1");
    let session = stack
        .store
        .latest_session_for_user(&surface_user)
        .unwrap()
        .expect("the channel account has its own session");
    assert_eq!(session.role, "channel");
    assert!(
        session.workspace.starts_with(stack.root.join("users")),
        "{:?}",
        session.workspace
    );
    stop(&mut stack).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_workspace_is_ignored_and_audited() {
    let mut stack = stack("team", &["T1"]);
    stack.fake.wait_socket().await;
    stack.fake.send(event_frame(
        "T9",
        "env-1",
        json!({"type": "message", "channel_type": "im", "channel": "D9", "user": "U9", "text": "hi"}),
    ));
    wait_for("the unauthorized audit row", || {
        stack
            .store
            .recent_audit(10)
            .map(|rows| rows.iter().any(|r| r.action == "slack.unauthorized_workspace"))
            .unwrap_or(false)
    })
    .await;
    assert!(stack.fake.calls("chat.postMessage").is_empty());
    stop(&mut stack).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn approval_buttons_decide_only_for_the_acting_user() {
    let mut stack = stack("approval", &["T1"]);
    stack.fake.wait_socket().await;
    // ana needs a session before an approval can reference it.
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
    // bea clicks approve: refused (ephemeral), the row stays pending.
    stack.fake.send(json!({
        "type": "interactive",
        "envelope_id": "env-1",
        "payload": {"type": "block_actions", "team": {"id": "T1"}, "user": {"id": "U2"},
                    "channel": {"id": "D2"}, "message": {"ts": "3.1"},
                    "actions": [{"action_id": format!("ka_approve:{}", approval.id)}]}
    }));
    wait_for("the refusal ephemeral", || {
        !stack.fake.calls("chat.postEphemeral").is_empty()
    })
    .await;
    let refused = stack.fake.calls("chat.postEphemeral")[0].clone();
    assert_eq!(refused.body["user"], "U2");
    assert!(
        refused.body["text"]
            .as_str()
            .unwrap_or("")
            .contains("not applied"),
        "{refused:?}"
    );
    assert_eq!(
        stack
            .store
            .get_approval(&approval.id)
            .unwrap()
            .status,
        ka_agent_core::ApprovalStatus::Pending
    );
    // ana clicks approve: the engine has no live ask behind this seeded row,
    // so it is still refused — but now for the engine's reason, not
    // authorization (that split is what the connector must preserve).
    stack.fake.send(json!({
        "type": "interactive",
        "envelope_id": "env-2",
        "payload": {"type": "block_actions", "team": {"id": "T1"}, "user": {"id": "U1"},
                    "channel": {"id": "D1"}, "message": {"ts": "3.2"},
                    "actions": [{"action_id": format!("ka_approve:{}", approval.id)}]}
    }));
    wait_for("the second refusal", || {
        stack.fake.calls("chat.postEphemeral").len() >= 2
    })
    .await;
    let second = stack.fake.calls("chat.postEphemeral")[1].clone();
    assert_eq!(second.body["user"], "U1");
    assert!(
        second.body["text"]
            .as_str()
            .unwrap_or("")
            .contains("no pending approval"),
        "the acting user reaches the engine check: {second:?}"
    );
    stop(&mut stack).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn shared_answers_with_restricted_sources_are_blocked() {
    let mut stack = stack("provenance", &["T1"]);
    stack.fake.wait_socket().await;
    // The mention's message stages a KB source that only `ops` may see.
    let source = "slack:T1:U1".to_string();
    stack.store.stage_turn_sources(
        &source,
        &[ka_agent_store::StagedSource {
            kind: "kb".into(),
            reference: "kb:secret/plan.md".into(),
            groups: vec!["ops".into()],
        }],
    );
    stack.fake.send(event_frame(
        "T1",
        "env-1",
        json!({"type": "app_mention", "channel": "C1", "user": "U1",
               "text": "<@UBOT> summarize", "ts": "4.1"}),
    ));
    // Wait for the turn to run and finish, then assert nothing was posted.
    wait_for("the blocked audit row", || {
        stack
            .store
            .recent_audit(20)
            .map(|rows| rows.iter().any(|r| r.action == "provenance.blocked"))
            .unwrap_or(false)
    })
    .await;
    assert!(
        stack.fake.calls("chat.postMessage").is_empty(),
        "the restricted answer must not reach the channel: {:?}",
        stack.fake.calls("chat.postMessage")
    );
    let row = stack
        .store
        .recent_audit(20)
        .unwrap()
        .into_iter()
        .find(|r| r.action == "provenance.blocked")
        .expect("audit row");
    assert_eq!(row.subject, "kb:secret/plan.md");
    stop(&mut stack).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dropped_socket_reconnects() {
    let mut stack = stack("reconnect", &["T1"]);
    stack.fake.wait_socket().await;
    assert_eq!(stack.fake.sessions(), 1);
    stack.fake.drop_socket();
    wait_for("a second socket", || stack.fake.sessions() >= 2).await;
    stop(&mut stack).await;
}
