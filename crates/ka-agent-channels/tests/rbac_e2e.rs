//! RBAC e2e: one daemon, one bot, two linked identities with different
//! roles. A real scripted worker (the `mock__worker` pattern) exercises the
//! full wire: per-role sessions, toolsets, commands, modes, cron role and
//! tools, memory/notes isolation, approvals and per-identity delivery.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use ka_agent_channels::fake::FakeBotServer;
use ka_agent_channels::{TelegramChannel, TelegramConfig, TelegramSurface};
use ka_agent_core::config::{RoleSpec, Telegram as TelegramSettings};
use ka_agent_core::{ApprovalDecision, DaemonConfig};
use ka_agent_port::WorkerPort;
use ka_agent_runtime::surface::Surface;
use ka_agent_runtime::{Paths, Runtime};
use ka_agent_store::Store;
use serde_json::json;
use tokio::sync::watch;

static DIR_SEQ: AtomicU32 = AtomicU32::new(0);

/// When the port spawns this binary as its worker, run the real worker
/// loop (libtest's name filter runs this test because `__worker` matches).
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

fn temp_dir() -> PathBuf {
    std::env::temp_dir().join(format!(
        "ka-rbac-e2e-{}-{}",
        std::process::id(),
        DIR_SEQ.fetch_add(1, Ordering::Relaxed)
    ))
}

/// The scenario JSON the next worker open will read (the role switch kills
/// the live worker, so rewriting this between turns is how each turn gets
/// its own script).
fn scenario_path(root: &Path) -> PathBuf {
    Paths::new(root.to_path_buf())
        .ka_data()
        .join("scenario.json")
}

fn write_scenario(root: &Path, json: &str) {
    let path = scenario_path(root);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, json).unwrap();
}

#[cfg(unix)]
fn scripted_port(root: &Path) -> WorkerPort {
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
        scenario_override: Some(scenario_path(root)),
    }
}

#[cfg(not(unix))]
fn scripted_port(_root: &Path) -> WorkerPort {
    WorkerPort::default()
}

struct Stack {
    server: FakeBotServer,
    root: PathBuf,
    store: Arc<Store>,
    runtime: Runtime,
    shutdown: watch::Sender<bool>,
    handle: tokio::task::JoinHandle<()>,
}

/// Admin uid 42 (chat 900) and guest uid 77 (chat 700), both linked.
fn spawn_stack(tag: &str, role: RoleSpec) -> Stack {
    let (server, base) = FakeBotServer::spawn();
    let root = temp_dir().join(tag);
    std::fs::create_dir_all(&root).unwrap();
    write_scenario(
        &root,
        r#"{"steps": [{"text": "ok"}], "stop": "done", "cost_usd": 0.0, "tokens": 10}"#,
    );
    let store = Arc::new(Store::open(&root.join("state.db")).unwrap());
    store.link_identity("telegram", "42", "test").unwrap();
    store
        .link_identity_as("telegram", "77", "test", &role.name)
        .unwrap();
    let surface = Arc::new(TelegramSurface::new(
        base.clone(),
        format!("tok-{tag}"),
        None,
        Some(Arc::clone(&store)),
    ));
    let cfg = DaemonConfig {
        workspace: root.join("workspace"),
        sandbox: "off".into(),
        telegram: TelegramSettings {
            enabled: true,
            token_secret: None,
            allow_user_ids: vec![42, 77],
            api_base: Some(base),
            ..TelegramSettings::default()
        },
        roles: vec![role],
        ..DaemonConfig::default()
    };
    let runtime = Runtime::new(
        cfg.clone(),
        Paths::new(root.clone()),
        Arc::clone(&store),
        scripted_port(&root),
        Arc::clone(&surface) as Arc<dyn Surface>,
    )
    .unwrap();
    let channel = TelegramChannel::new(
        TelegramConfig::from_config(&cfg, format!("tok-{tag}")),
        runtime.clone(),
        Arc::clone(&store),
    );
    let (shutdown, rx) = watch::channel(false);
    let handle = tokio::spawn(channel.run(rx));
    Stack {
        server,
        root,
        store,
        runtime,
        shutdown,
        handle,
    }
}

async fn stop(stack: &mut Stack) {
    stack.shutdown.send(true).unwrap();
    let handle = std::mem::replace(&mut stack.handle, tokio::spawn(async {}));
    let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
}

fn guest_role() -> RoleSpec {
    RoleSpec {
        name: "guest".into(),
        mode: "readonly".into(),
        allow: vec!["read".into(), "grep".into()],
        commands: vec![
            "status".into(),
            "help".into(),
            "mode".into(),
            "skills".into(),
            "history".into(),
            "memory".into(),
            "cron".into(),
        ],
        budget_daily_usd: Some(0.01),
        ..RoleSpec::default()
    }
}

/// A role whose mode allows everything but whose toolset is only `read`:
/// proves the per-turn allowlist, not the mode, blocks the tool.
fn reader_role() -> RoleSpec {
    RoleSpec {
        name: "reader".into(),
        mode: "allow".into(),
        allow: vec!["read".into()],
        ..RoleSpec::default()
    }
}

/// The next `sendMessage` whose text contains `needle`: `(text, chat_id)`.
async fn await_text(stack: &Stack, needle: &str) -> (String, Option<i64>) {
    let capture = stack
        .server
        .await_capture(
            |c| c.method == "sendMessage" && c.body["text"].as_str().unwrap_or("").contains(needle),
            // The first turn pays the engine's MCP-bridge connect timeout
            // (the test binary's `__mcp` is a no-op harness), so allow it.
            Duration::from_secs(40),
        )
        .unwrap_or_else(|| {
            let status = stack.runtime.session_status();
            let captured: Vec<String> = stack
                .server
                .captured()
                .into_iter()
                .filter(|c| c.method == "sendMessage")
                .map(|c| format!("chat={} {}", c.body["chat_id"], c.body["text"]))
                .collect();
            panic!("no sendMessage containing {needle:?}; status {status:?}, captured {captured:?}")
        });
    (
        capture.body["text"].as_str().unwrap_or("").to_string(),
        capture.body["chat_id"].as_i64(),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn guest_commands_and_mode_ceiling() {
    let mut stack = spawn_stack("cmds", guest_role());
    // A subject's `/mode` can only get stricter than its ceiling.
    stack.server.push_text_message(1, 700, 77, "/mode allow");
    let (reply, _) = await_text(&stack, "ceiling").await;
    assert!(reply.contains("readonly"), "{reply}");
    // Commands outside the role's list are refused.
    stack
        .server
        .push_text_message(2, 700, 77, "/remember I like tea");
    let _ = await_text(&stack, "/remember is not available for role guest").await;
    // Admin-only commands stay refused whatever the list says.
    stack.server.push_text_message(3, 700, 77, "/model");
    let _ = await_text(&stack, "/model is not available for role guest").await;
    // The floor always works.
    stack.server.push_text_message(4, 700, 77, "/status");
    let (reply, _) = await_text(&stack, "session").await;
    assert!(!reply.is_empty());
    // The guest's session is the guest's, not the admin's.
    assert_eq!(stack.runtime.active_role(), "guest");
    stop(&mut stack).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn guest_turn_cannot_run_bash_even_in_allow_mode() {
    let mut stack = spawn_stack("bash", reader_role());
    let marker = stack.root.join("pwned");
    write_scenario(
        &stack.root,
        &format!(
            r#"{{"steps": [{{"call": {{"tool": "bash", "args": {{"command": "touch {}"}}}}}}, {{"text": "finished"}}], "stop": "done", "cost_usd": 0.0, "tokens": 10}}"#,
            marker.display()
        ),
    );
    stack
        .server
        .push_text_message(1, 700, 77, "please touch that file");
    let (reply, _) = await_text(&stack, "finished").await;
    assert!(!reply.is_empty());
    assert!(
        !marker.exists(),
        "the per-turn allowlist must reject bash at admit"
    );
    stop(&mut stack).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn cron_carries_its_creator_role_and_narrows_tools() {
    let mut stack = spawn_stack("cron", guest_role());
    write_scenario(
        &stack.root,
        r#"{"steps": [{"text": "cron ran"}], "stop": "done", "cost_usd": 0.0, "tokens": 10}"#,
    );
    stack.server.push_text_message(
        1,
        700,
        77,
        "/cron add every 1h --tools read check the report",
    );
    let (reply, _) = await_text(&stack, "scheduled").await;
    assert!(reply.contains("cron"), "{reply}");
    let tasks = stack.runtime.list_crons().unwrap();
    assert_eq!(tasks.len(), 1, "{tasks:?}");
    let spec = ka_agent_core::CronSpec::from_payload(tasks[0].payload.as_deref().unwrap()).unwrap();
    assert_eq!(spec.role.as_deref(), Some("guest"));
    assert_eq!(spec.tools, vec!["read"]);
    // `cron list` shows the role and the toolset.
    stack.server.push_text_message(2, 700, 77, "/cron list");
    let (list, _) = await_text(&stack, "role=guest").await;
    assert!(list.contains("tools=[read]"), "{list}");

    // Force the fire and prove the turn lands in the guest session only.
    let admin_session = stack
        .store
        .latest_session_for_role("admin")
        .unwrap()
        .unwrap()
        .id;
    stack
        .store
        .update_task_schedule(
            &tasks[0].id,
            Some(ka_agent_core::now_ms() - 1_000),
            "queued",
        )
        .unwrap();
    let fired = stack.runtime.fire_due_tasks().await.unwrap();
    assert_eq!(fired.len(), 1, "{fired:?}");
    let guest_session = stack
        .store
        .latest_session_for_role("guest")
        .unwrap()
        .unwrap()
        .id;
    let guest_turns = stack
        .store
        .latest_turns_for_session(&guest_session, 10, 0)
        .unwrap();
    assert!(
        guest_turns
            .iter()
            .any(|t| t.inbound_text.contains("check the report")),
        "the cron fired into the guest session: {guest_turns:?}"
    );
    let admin_turns = stack
        .store
        .latest_turns_for_session(&admin_session, 10, 0)
        .unwrap();
    assert!(
        !admin_turns
            .iter()
            .any(|t| t.inbound_text.contains("check the report")),
        "never the admin session"
    );
    stop(&mut stack).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn memory_notes_and_approvals_are_role_scoped() {
    let mut stack = spawn_stack("memory", guest_role());
    // Activate the guest without a turn (the same switch `/status` does).
    assert!(stack.runtime.activate_role("telegram:77").await);
    assert_eq!(stack.runtime.active_role(), "guest");
    let id = stack
        .runtime
        .propose_memory("model", "guest prefers tea", None)
        .unwrap();
    let candidates = stack.runtime.pending_candidates().unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].role, "guest");
    // The candidate lands in the guest's memory root when accepted.
    assert!(stack.runtime.accept_candidate(id.as_str()).unwrap());
    let facts = stack.root.join("roles/guest/memory/facts.md");
    assert!(facts.is_file(), "guest memory at {facts:?}");
    assert!(
        std::fs::read_to_string(&facts).unwrap().contains("tea"),
        "the accepted fact is in the guest root"
    );
    let admin_facts = stack.root.join("memory/facts.md");
    assert!(
        !admin_facts.exists()
            || !std::fs::read_to_string(admin_facts)
                .unwrap()
                .contains("tea"),
        "the accepted fact never reaches the admin root"
    );

    // Notes resolve the turn's role from the store key the runtime writes.
    stack
        .store
        .meta_set(ka_agent_core::layout::TURN_ROLE_META_KEY, "guest")
        .unwrap();
    let ctx = ka_agent_channels::tools::ToolCtx {
        runtime: stack.runtime.clone(),
        store: Arc::clone(&stack.store),
        paths: Paths::new(stack.root.clone()),
    };
    let (text, is_error) = ka_agent_channels::tools::call(
        &ctx,
        "notes_append",
        json!({"file": "guest-note", "text": "hello"}),
    )
    .await;
    assert!(!is_error, "{text}");
    assert!(
        stack.root.join("roles/guest/notes/guest-note.md").is_file(),
        "note under the guest root"
    );
    assert!(
        !stack.root.join("notes/guest-note.md").exists(),
        "never the admin notes dir"
    );

    // Approvals are the admin's: a subject's decision is refused.
    let approval = ka_agent_core::Approval::new(
        stack.runtime.session_id(),
        ka_agent_core::TurnId::generate(),
        ka_agent_core::ApprovalKind::KaAsk {
            ask_id: "ask-rbac".into(),
            question: "run something?".into(),
            detail: None,
            options: vec!["allow".into(), "deny".into()],
        },
        300,
    );
    stack.store.insert_approval(&approval).unwrap();
    let err = stack
        .runtime
        .decide(approval.id.as_str(), ApprovalDecision::Allow, "telegram:77")
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("admin role"), "{err}");
    stop(&mut stack).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn replies_go_back_to_the_sender_not_the_last_chat() {
    let mut stack = spawn_stack("delivery", guest_role());
    write_scenario(
        &stack.root,
        r#"{"steps": [{"text": "guest answer"}], "stop": "done", "cost_usd": 0.0, "tokens": 10}"#,
    );
    stack.server.push_text_message(1, 700, 77, "guest question");
    let (reply, chat) = await_text(&stack, "guest answer").await;
    assert!(reply.contains("guest answer"));
    assert_eq!(chat, Some(700));
    // The next turn (admin) gets a fresh script; the role switch respawns
    // the worker, which reads it at open.
    write_scenario(
        &stack.root,
        r#"{"steps": [{"text": "admin answer"}], "stop": "done", "cost_usd": 0.0, "tokens": 10}"#,
    );
    stack.server.push_text_message(2, 900, 42, "admin question");
    let (reply, chat) = await_text(&stack, "admin answer").await;
    assert!(reply.contains("admin answer"));
    assert_eq!(chat, Some(900));
    // The guest never receives the admin's answer.
    let leaked = stack.server.captured().into_iter().any(|c| {
        c.method == "sendMessage"
            && c.body["chat_id"].as_i64() == Some(700)
            && c.body["text"]
                .as_str()
                .unwrap_or("")
                .contains("admin answer")
    });
    assert!(!leaked, "guest chat must not receive the admin's reply");
    stop(&mut stack).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn budget_pause_is_per_role() {
    let mut stack = spawn_stack(
        "budget",
        RoleSpec {
            name: "guest".into(),
            mode: "readonly".into(),
            allow: vec!["read".into()],
            budget_daily_usd: Some(0.01),
            ..RoleSpec::default()
        },
    );
    // The scripted catalog prices input at $1/Mtok; 40k tokens is $0.04,
    // well past the role's $0.01 cap.
    write_scenario(
        &stack.root,
        r#"{"steps": [{"text": "expensive answer"}], "stop": "done", "cost_usd": 0.0, "tokens": 40000}"#,
    );
    stack.server.push_text_message(1, 700, 77, "spend it");
    // The role's $0.01 cap reached the engine's spend guard: it asks
    // before continuing. The card belongs to the admin; with no admin chat
    // known it is withheld (never delivered to the subject's chat).
    let _ = await_text(&stack, "waiting for the operator").await;
    assert_eq!(
        stack.runtime.session_status(),
        ka_agent_core::SessionStatus::AwaitingApproval
    );
    let guest_session = stack.runtime.session_id();
    let pending = stack.store.pending_approvals(&guest_session).unwrap();
    assert_eq!(pending.len(), 1, "the engine's spend ask is pending");
    match &pending[0].kind {
        ka_agent_core::ApprovalKind::KaAsk { question, .. } => {
            assert!(question.contains("spend cap $0.01"), "{question}");
        }
        other => panic!("unexpected approval kind: {other:?}"),
    }
    let leaked_card = stack.server.captured().into_iter().any(|c| {
        c.method == "sendMessage"
            && c.body["chat_id"].as_i64() == Some(700)
            && c.body["text"].as_str().unwrap_or("").contains("continue?")
    });
    assert!(
        !leaked_card,
        "an approval card never reaches a subject chat"
    );
    // Another role never disturbs the subject's live turn (the message
    // would queue on the admin session instead).
    assert!(!stack.runtime.activate_role("telegram:42").await);
    assert_eq!(stack.runtime.active_role(), "guest");
    stop(&mut stack).await;
}
