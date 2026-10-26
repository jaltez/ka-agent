//! Regression tests for the review's P1 wedges (F1–F3, F12): the daemon must
//! never be *alive but mute* — a session adopted mid-turn, a budget pause, or
//! an expired ask all have to end in work happening again.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use ka_agent_core::{
    Approval, ApprovalId, ApprovalKind, ApprovalStatus, Session, Turn, TurnId, TurnStatus,
};
use ka_agent_runtime::Runtime;
use ka_agent_store::Store;
use std::sync::Arc;

fn store_at(dir: &std::path::Path) -> Arc<Store> {
    std::fs::create_dir_all(dir).unwrap();
    Arc::new(Store::open(&dir.join("state.db")).unwrap())
}

/// Minimal runtime on a temp data root; the worker port never spawns (no
/// turn is submitted in these tests) and the surface is silent.
fn runtime_with(
    root: &std::path::Path,
    store: Arc<Store>,
    cfg: ka_agent_core::DaemonConfig,
) -> Runtime {
    let mut port = ka_agent_port::WorkerPort::with_current_exe();
    // No turn is submitted in these tests; point the binary nowhere so a
    // stray submit can never spawn a real worker.
    port.binary = std::path::PathBuf::from("/nonexistent/ka-agent");
    Runtime::new(
        cfg,
        ka_agent_runtime::Paths::new(root.to_path_buf()),
        store,
        port,
        Arc::new(SilentSurface),
    )
    .unwrap()
}

struct SilentSurface;
impl ka_agent_runtime::surface::Surface for SilentSurface {
    fn emit(&self, _env: &ka_agent_runtime::Envelope) {}
}

fn config(root: &std::path::Path) -> ka_agent_core::DaemonConfig {
    ka_agent_core::DaemonConfig {
        workspace: root.join("ws"),
        ..ka_agent_core::DaemonConfig::default()
    }
}

/// F1: a session adopted in `running`/`awaiting_approval`/`stopping` must come
/// back as `idle`, or every message queues behind a worker that never starts.
#[test]
fn adopted_busy_session_returns_to_idle_on_startup() {
    let root = std::env::temp_dir().join(format!("ka-adopt-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let store = store_at(&root.join("data"));

    // A previous process died mid-turn: the row says `running`, the turn says
    // `running` too.
    let mut session = Session::new(root.join("ws"));
    session.status = ka_agent_core::SessionStatus::Running;
    store.insert_session(&session).unwrap();
    let mut turn = Turn::new(session.id.clone(), "in flight when we died".into());
    store.insert_turn(&turn).unwrap();
    turn.transition(TurnStatus::Submitted).unwrap();
    store.update_turn(&turn).unwrap();
    turn.transition(TurnStatus::Running).unwrap();
    store.update_turn(&turn).unwrap();

    // Restart the runtime over the same store.
    let rt = runtime_with(&root, Arc::clone(&store), config(&root));
    assert_eq!(
        rt.session_status(),
        ka_agent_core::SessionStatus::Idle,
        "an adopted busy session must not stay busy"
    );
    // The durable row agrees (a persisted `running` would come back on the
    // next start and wedge again).
    let persisted = store.get_session(&session.id).unwrap();
    assert_eq!(persisted.status, ka_agent_core::SessionStatus::Idle);
    // The interrupted turn is marked, not silently kept.
    let t = store.get_turn(&turn.id).unwrap().unwrap();
    assert!(t.status.is_terminal(), "turn status {:?}", t.status);

    let _ = std::fs::remove_dir_all(&root);
}

/// F3: an expired ask must leave the in-memory map, or `ask_pending` blocks
/// the idle transition and the queue drain forever.
#[tokio::test]
async fn expired_ask_is_dropped_from_the_pending_map() {
    let root = std::env::temp_dir().join(format!("ka-expire-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let store = store_at(&root.join("data"));
    let rt = runtime_with(&root, Arc::clone(&store), config(&root));

    // An approval that expired before this sweep, registered as if the engine
    // had asked and nobody answered.
    let mut session = Session::new(root.join("ws"));
    session.id = rt.session_id();
    if store.get_session(&session.id).is_err() {
        store.insert_session(&session).unwrap();
    }
    let turn = TurnId::generate();
    let mut approval = Approval::new(
        session.id.clone(),
        turn,
        ApprovalKind::KaAsk {
            ask_id: "ask-1".into(),
            question: "run it?".into(),
            detail: None,
            options: vec!["allow".into(), "deny".into()],
        },
        -10, // TTL in the past: already expired
    );
    approval.status = ApprovalStatus::Pending;
    store.insert_approval(&approval).unwrap();

    let expired = rt.expire_approvals().await.unwrap();
    assert_eq!(expired, vec![ApprovalId(approval.id.0.clone())]);
    let row = store.get_approval(&approval.id).unwrap();
    assert_eq!(row.status, ApprovalStatus::Expired);

    // The invariant that matters: nothing is left claiming a live ask, so the
    // next message is submitted (or queued for drain), never wedged.
    let status = rt.session_status();
    assert_ne!(
        status,
        ka_agent_core::SessionStatus::AwaitingApproval,
        "an expired ask must not keep the session waiting"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// F12: a decision arriving after the TTL but before the sweep must not be
/// applied — deny-on-expiry is an invariant, not a schedule.
#[tokio::test]
async fn decide_after_ttl_is_refused() {
    let root = std::env::temp_dir().join(format!("ka-late-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let store = store_at(&root.join("data"));
    let rt = runtime_with(&root, Arc::clone(&store), config(&root));
    let mut session = Session::new(root.join("ws"));
    session.id = rt.session_id();
    if store.get_session(&session.id).is_err() {
        store.insert_session(&session).unwrap();
    }
    let mut approval = Approval::new(
        session.id.clone(),
        TurnId::generate(),
        ApprovalKind::KaAsk {
            ask_id: "ask-late".into(),
            question: "too late?".into(),
            detail: None,
            options: vec!["allow".into(), "deny".into()],
        },
        -5, // expired five seconds ago; the sweep has not run yet
    );
    approval.status = ApprovalStatus::Pending;
    store.insert_approval(&approval).unwrap();

    let err = rt
        .decide(approval.id.as_str(), ka_agent_core::ApprovalDecision::Allow)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("no pending approval"), "{err}");
    assert_eq!(
        store.get_approval(&approval.id).unwrap().status,
        ApprovalStatus::Pending,
        "a refused decision must not settle the row"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// F2: a paused session has an exit — resuming is refused while the budget is
/// still exceeded, and works once the window rolls.
#[test]
fn resume_lifts_a_pause_and_refuses_on_budget() {
    let root = std::env::temp_dir().join(format!("ka-resume-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let store = store_at(&root.join("data"));
    let mut cfg = config(&root);
    cfg.budgets.daily_usd = Some(0.01);

    // Persisted budget for *today*, already over the cap — exactly the state
    // the budget guard leaves behind when it pauses the session.
    let spent = ka_agent_core::BudgetState {
        day: ka_agent_core::day_key(ka_agent_core::now_ms()),
        spent_usd: 0.05,
        spent_tokens: 0,
        daily_usd_cap: Some(0.01),
        daily_token_cap: None,
    };
    store
        .meta_set("budget", &serde_json::to_string(&spent).unwrap())
        .unwrap();
    let rt = runtime_with(&root, Arc::clone(&store), cfg.clone());

    // Not paused → nothing to resume.
    assert!(!rt.resume().unwrap());

    // Pause it as the guard would and try to lift it: refused, with the
    // reason, because the budget for this window is still exceeded.
    let mut session = Session::new(root.join("ws"));
    session.id = rt.session_id();
    if store.get_session(&session.id).is_err() {
        store.insert_session(&session).unwrap();
    }
    store
        .update_session_status(&session.id, ka_agent_core::SessionStatus::Paused)
        .ok();
    let paused_runtime = runtime_with(&root, Arc::clone(&store), cfg.clone());
    let err = paused_runtime.resume().unwrap_err().to_string();
    assert!(err.contains("budget still exceeded"), "{err}");

    // A new day window (fresh meta) makes resume work.
    let fresh = ka_agent_core::BudgetState {
        day: ka_agent_core::day_key(ka_agent_core::now_ms()),
        spent_usd: 0.0,
        spent_tokens: 0,
        daily_usd_cap: Some(0.01),
        daily_token_cap: None,
    };
    store
        .meta_set("budget", &serde_json::to_string(&fresh).unwrap())
        .unwrap();
    let after_roll = runtime_with(&root, Arc::clone(&store), cfg);
    assert!(after_roll.resume().unwrap());
    assert_eq!(
        after_roll.session_status(),
        ka_agent_core::SessionStatus::Idle
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// F5 (no-op half): `/stop` with no turn in flight says so and touches
/// nothing. The escalation half needs a real worker and lives in the
/// app-level `chat_e2e` (`stop_does_not_kill_a_healthy_worker`).
#[test]
fn stop_escalates_on_the_turn_not_the_process() {
    let root = std::env::temp_dir().join(format!("ka-stop-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let store = store_at(&root.join("data"));
    let rt = runtime_with(&root, Arc::clone(&store), config(&root));

    // No live worker: a no-op, not an error.
    futures_lite_block(rt.stop_turn()).unwrap();
    assert_eq!(rt.session_status(), ka_agent_core::SessionStatus::Idle);

    let _ = std::fs::remove_dir_all(&root);
}

fn futures_lite_block<F: std::future::Future>(fut: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(fut)
}

/// Restart with a durable queue: the message that wakes the daemon must not
/// overtake what was already queued (the worker spawns lazily, so nothing
/// drains the queue until a message arrives).
#[tokio::test]
async fn waking_message_does_not_overtake_the_durable_queue() {
    let root = std::env::temp_dir().join(format!("ka-fifo-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let store = store_at(&root.join("data"));

    // A previous process left one message durably queued.
    let session = Session::new(root.join("ws"));
    store.insert_session(&session).unwrap();
    store
        .enqueue(&ka_agent_core::QueueItem::new(
            session.id.clone(),
            "from before the restart".into(),
            "telegram",
        ))
        .unwrap();

    // The worker cannot spawn (no binary); what matters is which message
    // the runtime tries and where the new one lands.
    let rt = runtime_with(&root, Arc::clone(&store), config(&root));
    let _ = rt.handle_message("sent after the restart", "console").await;

    // The new message went behind the old one, not in front of it.
    let queued = store.queued_items(&rt.session_id()).unwrap();
    let texts: Vec<&str> = queued.iter().map(|q| q.text.as_str()).collect();
    assert_eq!(
        texts,
        vec!["from before the restart", "sent after the restart"],
        "queue order after the waking message"
    );
    // And the session is not wedged in `running` by the failed spawn.
    assert_eq!(rt.session_status(), ka_agent_core::SessionStatus::Idle);
    let _ = std::fs::remove_dir_all(&root);
}
