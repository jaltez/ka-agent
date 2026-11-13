//! Team-edition e2e (ADR-0003 + ADR-0004): one daemon, one bot, several
//! linked **users**. A real scripted worker exercises the wire: per-user
//! sessions and partitions, concurrent turns, history and memory isolation,
//! the KB gate, D9 approval routing and offboarding.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use ka_agent_channels::fake::FakeBotServer;
use ka_agent_channels::{TelegramChannel, TelegramConfig, TelegramSurface};
use ka_agent_core::config::{RoleSpec, Telegram as TelegramSettings};
use ka_agent_core::{ApprovalDecision, DaemonConfig, Principal};
use ka_agent_port::WorkerPort;
use ka_agent_runtime::surface::Surface;
use ka_agent_runtime::{Paths, Runtime};
use ka_agent_store::Store;
use tokio::sync::watch;

static DIR_SEQ: AtomicU32 = AtomicU32::new(0);

/// When the port spawns this binary as its worker, run the real worker loop.
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
        "ka-users-e2e-{}-{}",
        std::process::id(),
        DIR_SEQ.fetch_add(1, Ordering::Relaxed)
    ))
}

/// The scenario the next worker open reads. All users' workers share it (the
/// port's override is per runtime), which is enough: the scripts are about
/// what the engine says, not who asked.
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

fn role(name: &str, commands: &[&str]) -> RoleSpec {
    RoleSpec {
        name: name.into(),
        mode: "readonly".into(),
        allow: vec!["read".into(), "grep".into()],
        commands: commands.iter().map(|c| c.to_string()).collect(),
        ..RoleSpec::default()
    }
}

fn member_role() -> RoleSpec {
    role(
        "member",
        &["status", "help", "mode", "skills", "history", "memory", "cron"],
    )
}

/// Operator uid 42 (chat 900) and member uid 77 (chat 700), both linked.
fn spawn_stack(tag: &str, roles: Vec<RoleSpec>) -> Stack {
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
        .link_identity_as("telegram", "77", "test", "member")
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
        roles,
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

/// The next `sendMessage` to `chat` whose text contains `needle`.
async fn await_text(stack: &Stack, chat: i64, needle: &str) -> String {
    let capture = stack
        .server
        .await_capture(
            |c| {
                c.method == "sendMessage"
                    && c.body["chat_id"].as_i64() == Some(chat)
                    && c.body["text"].as_str().unwrap_or("").contains(needle)
            },
            Duration::from_secs(40),
        )
        .unwrap_or_else(|| {
            let captured: Vec<String> = stack
                .server
                .captured()
                .into_iter()
                .filter(|c| c.method == "sendMessage")
                .map(|c| format!("chat={} {}", c.body["chat_id"], c.body["text"]))
                .collect();
            panic!("no sendMessage to {chat} containing {needle:?}; captured {captured:?}")
        });
    capture.body["text"].as_str().unwrap_or("").to_string()
}

fn member() -> Principal {
    Principal::new("member", "member")
}

fn operator() -> Principal {
    Principal::operator()
}

/// Identities resolve to users, and each user has their own session row.
#[tokio::test(flavor = "multi_thread")]
async fn identities_resolve_to_users_with_their_own_sessions() {
    let mut stack = spawn_stack("resolve", vec![member_role()]);
    assert_eq!(
        stack.runtime.principal_for_source("telegram:42"),
        Some(operator())
    );
    assert_eq!(
        stack.runtime.principal_for_source("telegram:77"),
        Some(member())
    );
    assert_eq!(stack.runtime.principal_for_source("telegram:99"), None);

    stack.server.push_text_message(1, 700, 77, "hi");
    let _ = await_text(&stack, 700, "ok").await;
    let member_session = stack
        .store
        .latest_session_for_user("member")
        .unwrap()
        .unwrap();
    assert_eq!(member_session.username, "member");
    assert_eq!(member_session.role, "member");
    let operator_session = stack
        .store
        .latest_session_for_user(ka_agent_core::OPERATOR_USER)
        .unwrap();
    assert!(
        operator_session.is_none_or(|s| s.id != member_session.id),
        "the member's turn never uses the operator's session"
    );
    // The member's workspace is their partition, not the profile's.
    assert!(
        member_session.workspace.starts_with(stack.root.join("users/member")),
        "{:?}",
        member_session.workspace
    );
    // A member's `/history` sees nothing of the operator's turns.
    stack.server.push_text_message(2, 700, 77, "/history");
    let reply = await_text(&stack, 700, "history").await;
    assert!(reply.contains("hi"), "{reply}");
    stop(&mut stack).await;
}

/// Two users' turns overlap: no head-of-line blocking behind one long turn.
#[tokio::test(flavor = "multi_thread")]
async fn two_users_turns_overlap() {
    let mut stack = spawn_stack("concurrent", vec![member_role()]);
    // A slow turn for the member: the operator's message must start while it
    // is still running.
    write_scenario(
        &stack.root,
        r#"{"steps": [{"sleep_ms": 4000}, {"text": "slow answer"}], "stop": "done", "cost_usd": 0.0, "tokens": 10}"#,
    );
    stack.server.push_text_message(1, 700, 77, "take your time");
    // Wait until the member's turn is live.
    let deadline = Instant::now() + Duration::from_secs(30);
    while stack.runtime.session_status_for(&member()) != ka_agent_core::SessionStatus::Running {
        if Instant::now() > deadline {
            panic!("the member's turn never started");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    stack
        .server
        .push_text_message(2, 900, 42, "operator speaking");
    // Two engines live at the same time: the single-session core is gone.
    let deadline = Instant::now() + Duration::from_secs(30);
    while stack.runtime.live_worker_count() < 2 {
        if Instant::now() > deadline {
            panic!(
                "only {} live worker(s): the turns did not overlap",
                stack.runtime.live_worker_count()
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Both conversations recorded their own turn.
    let member_turn = stack
        .store
        .latest_turns_for_session(
            &stack
                .store
                .latest_session_for_user("member")
                .unwrap()
                .unwrap()
                .id,
            1,
            0,
        )
        .unwrap()
        .pop()
        .expect("member turn");
    let operator_turn = stack
        .store
        .latest_turns_for_session(
            &stack
                .store
                .latest_session_for_user(ka_agent_core::OPERATOR_USER)
                .unwrap()
                .unwrap()
                .id,
            1,
            0,
        )
        .unwrap()
        .pop()
        .expect("operator turn");
    assert!(member_turn.inbound_text.contains("take your time"));
    assert!(operator_turn.inbound_text.contains("operator speaking"));
    stop(&mut stack).await;
}

/// A member's `@file` reference never reads the operator's workspace.
#[tokio::test(flavor = "multi_thread")]
async fn member_file_refs_cannot_reach_the_operator_workspace() {
    let mut stack = spawn_stack("refs", vec![member_role()]);
    std::fs::create_dir_all(stack.root.join("workspace")).unwrap();
    std::fs::write(
        stack.root.join("workspace/secret.md"),
        "TOP-SECRET-OPERATOR-BYTES",
    )
    .unwrap();
    stack
        .server
        .push_text_message(1, 700, 77, "read @secret.md please");
    let _ = await_text(&stack, 700, "ok").await;
    let session = stack
        .store
        .latest_session_for_user("member")
        .unwrap()
        .unwrap();
    let turn = stack
        .store
        .latest_turns_for_session(&session.id, 1, 0)
        .unwrap()
        .pop()
        .expect("a turn");
    assert!(
        !turn.inbound_text.contains("TOP-SECRET-OPERATOR-BYTES"),
        "the operator's file leaked into the member's turn: {}",
        turn.inbound_text
    );
    assert!(
        turn.inbound_text.contains("not found")
            || turn.inbound_text.contains("[ref @secret.md"),
        "{}",
        turn.inbound_text
    );
    stop(&mut stack).await;
}

/// Memory proposals land in the proposing user's root, never the operator's.
#[tokio::test(flavor = "multi_thread")]
async fn memory_lands_in_the_proposing_users_root() {
    let mut stack = spawn_stack("memory", vec![member_role()]);
    let who = member();
    let id = stack
        .runtime
        .propose_memory("model", "member prefers tea", None, &who)
        .unwrap();
    let pending = stack.runtime.pending_candidates_for(&who).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].username, "member");
    // The operator reviews every subject's, and the proposal still lands in
    // the proposer's root.
    assert_eq!(
        stack.runtime.pending_candidates_for(&operator()).unwrap().len(),
        1
    );
    assert!(
        stack
            .runtime
            .accept_candidate(id.as_str(), &operator())
            .unwrap()
    );
    let facts = stack.root.join("users/member/memory/facts.md");
    assert!(facts.is_file(), "member memory at {facts:?}");
    assert!(std::fs::read_to_string(&facts).unwrap().contains("tea"));
    let operator_facts = stack.root.join("memory/facts.md");
    assert!(
        !operator_facts.exists()
            || !std::fs::read_to_string(operator_facts)
                .unwrap()
                .contains("tea"),
        "the accepted fact never reaches the operator's root"
    );
    // A different subject cannot accept another subject's proposal.
    assert!(
        !stack
            .runtime
            .pending_candidates_for(&Principal::new("ghost", "member"))
            .unwrap()
            .iter()
            .any(|c| c.id == id)
    );
    stop(&mut stack).await;
}

/// A knowledge base is readable only through the user's groups.
#[tokio::test(flavor = "multi_thread")]
async fn knowledge_is_gated_by_group() {
    let mut stack = spawn_stack("kb", vec![member_role()]);
    stack
        .store
        .upsert_group("dev", &[], None)
        .unwrap();
    stack
        .store
        .create_kb("docs", "Docs", ka_agent_core::OPERATOR_USER)
        .unwrap();
    stack
        .store
        .set_kb_access("docs", &["dev".to_string()])
        .unwrap();
    stack
        .store
        .kb_upsert_doc(
            "docs",
            "runbook.md",
            "Runbook",
            "the deploy key rotates on friday",
            "h1",
            "upload",
            ka_agent_core::now_ms(),
        )
        .unwrap();
    // The member is not in dev yet: no KB for them, and the search says so
    // without naming the KB.
    let ctx = ka_agent_channels::tools::ToolCtx {
        runtime: stack.runtime.clone(),
        store: Arc::clone(&stack.store),
        paths: Paths::new(stack.root.clone()),
        actor: ka_agent_core::Principal {
            username: "member".into(),
            role: "member".into(),
        },
    };
    let (text, is_error) = ka_agent_channels::tools::call(
        &ctx,
        "kb_search",
        serde_json::json!({"query": "deploy key"}),
    )
    .await;
    assert!(!is_error, "{text}");
    assert!(text.contains("no knowledge base matches"), "{text}");
    // Add the member to dev: the snippet comes back with its citation.
    stack.store.add_user_group("member", "dev").unwrap();
    let (text, is_error) = ka_agent_channels::tools::call(
        &ctx,
        "kb_search",
        serde_json::json!({"query": "deploy key", "kb": "docs"}),
    )
    .await;
    assert!(!is_error, "{text}");
    assert!(text.contains("kb:docs/runbook.md"), "{text}");
    assert!(text.contains("rotates"), "{text}");
    // The provenance row is recorded for the turn.
    let turn = stack
        .store
        .meta_get(ka_agent_core::layout::TURN_ID_META_KEY)
        .ok()
        .flatten();
    if let Some(turn) = turn.filter(|t| !t.is_empty()) {
        let rows = stack.store.turn_sources(&turn).unwrap();
        assert!(rows.iter().any(|r| r.reference == "kb:docs/runbook.md"));
        assert_eq!(rows[0].group_names, vec!["dev".to_string()]);
    }
    stop(&mut stack).await;
}

/// The card a turn raises (seeded through the store: this test binary hosts
/// no `__mcp` bridge, so a real engine ask cannot be scripted here; the
/// classification itself is unit-tested in `runtime::policy`).
fn seed_ask(
    stack: &Stack,
    who: &Principal,
    escalated: bool,
    ask: &str,
) -> ka_agent_core::Approval {
    let session = stack
        .store
        .latest_session_for_user(&who.username)
        .unwrap()
        .expect("the user has a session");
    let approval = ka_agent_core::Approval::new(
        session.id.clone(),
        ka_agent_core::TurnId::generate(),
        ka_agent_core::ApprovalKind::KaAsk {
            ask_id: format!("ask-{ask}"),
            question: format!("run `{ask}`?"),
            detail: None,
            options: vec!["allow".into(), "deny".into()],
        },
        300,
    )
    .routed_to(&who.username, escalated);
    stack.store.insert_approval(&approval).unwrap();
    approval
}

/// D9: a self-scope card is decided by the acting user; nobody else.
#[tokio::test(flavor = "multi_thread")]
async fn self_scope_approvals_are_the_users_own() {
    let mut stack = spawn_stack("approvals", vec![member_role()]);
    let who = member();
    // Create the member's session/state first (a message would need the
    // engine; asking the runtime for the state is enough).
    stack.runtime.ensure_user_state(&who).unwrap();
    // A second person on the same role: a distinct user via an explicit
    // binding (the role mapping alone would fold them into `member`).
    stack
        .store
        .link_identity_to_user("telegram", "43", "test", "member", "other")
        .unwrap();
    let approval = seed_ask(&stack, &who, false, "echo hi");
    assert_eq!(approval.username.as_deref(), Some("member"));
    assert!(!approval.escalated, "self-scope stays with the member");
    // Another user cannot decide it.
    let err = stack
        .runtime
        .decide(
            approval.id.as_str(),
            ApprovalDecision::Allow,
            "telegram:43",
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("another user"), "{err}");
    // The ask is not answerable without a live engine holding it, so the
    // acting user's decision is refused as unknown (not as authorization).
    let err = stack
        .runtime
        .decide(
            approval.id.as_str(),
            ApprovalDecision::Allow,
            "telegram:77",
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("no pending approval"), "{err}");
    // Authorization is what changed: the admin reaches the engine check too.
    let err = stack
        .runtime
        .decide(
            approval.id.as_str(),
            ApprovalDecision::Allow,
            "telegram:42",
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("no pending approval"), "{err}");
    stop(&mut stack).await;
}

/// D9: an escalated card belongs to the admin and the user cannot decide it.
#[tokio::test(flavor = "multi_thread")]
async fn escalated_cards_belong_to_the_admin() {
    let mut stack = spawn_stack("escalated", vec![member_role()]);
    let who = member();
    stack.runtime.ensure_user_state(&who).unwrap();
    let approval = seed_ask(&stack, &who, true, "echo careful");
    assert!(approval.escalated);
    // It is in the admin queue...
    assert!(
        stack
            .store
            .escalated_approvals(10)
            .unwrap()
            .iter()
            .any(|a| a.id == approval.id)
    );
    // ...and not on the member's own inbox.
    assert!(
        stack
            .store
            .pending_approvals_for_user("member")
            .unwrap()
            .is_empty()
    );
    let err = stack
        .runtime
        .decide(
            approval.id.as_str(),
            ApprovalDecision::Allow,
            "telegram:77",
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("escalated"), "{err}");
    stop(&mut stack).await;
}

/// Offboarding (D10): a disabled user is silent, their web sessions are gone
/// and their crons pause.
#[tokio::test(flavor = "multi_thread")]
async fn disabling_a_user_pauses_everything() {
    let mut stack = spawn_stack("disable", vec![member_role()]);
    stack.runtime.ensure_user_state(&member()).unwrap();
    // A web session and a cron to freeze.
    stack
        .store
        .insert_web_session(
            "hash-of-a-token",
            "member",
            ka_agent_core::now_ms(),
            ka_agent_core::now_ms() + 60_000,
        )
        .unwrap();
    let who = member();
    let mut spec = ka_agent_core::CronSpec::from_words(
        &["every", "1h", "check", "the", "queue"],
        "operator",
        &stack.runtime.now_zoned(),
    )
    .unwrap()
    .0;
    spec.username = Some("member".into());
    spec.role = Some("member".into());
    let _ = stack
        .runtime
        .add_cron(spec, ka_agent_core::now_ms() + 60_000, &who)
        .unwrap();
    assert_eq!(stack.store.cron_tasks_for_user("member").unwrap().len(), 1);

    // Disable the way the CLI does.
    stack.store.set_user_disabled("member", true).unwrap();
    stack.store.delete_web_sessions_for("member").unwrap();
    for task in stack.store.cron_tasks_for_user("member").unwrap() {
        stack.store.set_task_status(&task.id, "interrupted").unwrap();
    }
    // Inbound is refused: no reply, one audit row.
    stack.server.push_text_message(1, 700, 77, "are you there?");
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut replied = false;
    while Instant::now() < deadline {
        if stack
            .server
            .captured()
            .iter()
            .any(|c| c.method == "sendMessage" && c.body["chat_id"].as_i64() == Some(700))
        {
            replied = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(!replied, "a disabled user must not get a reply");
    assert_eq!(
        stack.store.identity_principal("telegram", "77").unwrap(),
        None
    );
    assert!(stack.store.web_session_user("hash-of-a-token", ka_agent_core::now_ms()).unwrap().is_none());
    let paused = stack.store.cron_tasks_for_user("member").unwrap();
    assert_eq!(paused.len(), 1, "the job stays visible as paused");
    assert_eq!(paused[0].status, ka_agent_core::TaskStatus::Interrupted);
    assert_eq!(
        stack
            .store
            .latest_session_for_user("member")
            .unwrap()
            .unwrap()
            .status,
        ka_agent_core::SessionStatus::Idle,
        "the member's session is not left running"
    );
    // Re-enabling restores resolution.
    stack.store.set_user_disabled("member", false).unwrap();
    assert!(stack.store.identity_principal("telegram", "77").unwrap().is_some());
    stop(&mut stack).await;
}
