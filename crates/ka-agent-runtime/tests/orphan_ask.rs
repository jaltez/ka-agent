//! The orphan-ask regression (live finding, 2026-11-17): a `/stop` under an
//! open approval card used to leave the session parked in `awaiting_approval`
//! with an ask nobody would ever answer — its turn was dead. Until the card's
//! TTL lapsed, every inbound message was handed to `steer`, and an idle
//! engine silently dropped it: the next prompt after an aborted gated turn
//! simply vanished. The finish handler now denies the orphans, the boundary
//! closes, and the message that follows is answered.
//!
//! The ask is driven by the engine's spend guard (a `budgets.daily_usd` of a
//! micro-dollar trips it on the first request) — deterministic and
//! config-driven, unlike the bash analyzer.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ka_agent_port::WorkerPort;
use ka_agent_runtime::event::AgentEvent;
use ka_agent_runtime::surface::Surface;
use ka_agent_runtime::{Envelope, Paths, Runtime};
use ka_agent_store::Store;

#[derive(Default)]
struct Recorder(Mutex<Vec<AgentEvent>>);

impl Recorder {
    fn events(&self) -> Vec<AgentEvent> {
        self.0.lock().unwrap().clone()
    }
}

impl Surface for Recorder {
    fn emit(&self, env: &Envelope) {
        self.0.lock().unwrap().push(env.ev.clone());
    }
}

/// When the port spawns this binary as its worker, libtest's `__worker`
/// filter lands here and runs the real worker loop on the scenario the
/// parent wrote to `KA_DATA_DIR/scenario.json`.
#[test]
#[allow(non_snake_case)]
fn mock__worker() {
    if !std::env::args().any(|a| a == "__worker") {
        return;
    }
    let Some(dir) = std::env::var_os("KA_DATA_DIR") else {
        return;
    };
    let scenario = PathBuf::from(dir).join("scenario.json");
    if !scenario.exists() {
        return;
    }
    let _ = ka_agent_port::run_worker(ka_agent_port::WorkerRun {
        scenario: Some(scenario),
    });
}

#[cfg(unix)]
fn scripted_port(root: &Path, steps: &str) -> WorkerPort {
    let paths = Paths::new(root.to_path_buf());
    let scenario = paths.ka_data().join("scenario.json");
    std::fs::create_dir_all(scenario.parent().unwrap()).unwrap();
    std::fs::write(
        &scenario,
        format!(r#"{{"steps": [{steps}], "stop": "done", "cost_usd": 0.0, "tokens": 10}}"#),
    )
    .unwrap();
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
        scenario_override: Some(scenario),
    }
}

#[cfg(unix)]
fn runtime_with(tag: &str, steps: &str) -> (Runtime, Arc<Store>, Arc<Recorder>, PathBuf) {
    let root = std::env::temp_dir().join(format!("ka-orphan-ask-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let store = Arc::new(Store::open(&root.join("state.db")).unwrap());
    let port = scripted_port(&root, steps);
    let rec = Arc::new(Recorder::default());
    // A micro-dollar daily cap: the engine's spend guard raises a
    // continue-ask on the very first request, which is the open card
    // this test needs under its abort.
    let cfg = ka_agent_core::DaemonConfig {
        workspace: root.join("ws"),
        budgets: ka_agent_core::config::Budgets {
            daily_usd: Some(0.000001),
            ..ka_agent_core::config::Budgets::default()
        },
        ..ka_agent_core::DaemonConfig::default()
    };
    let rt = Runtime::new(
        cfg,
        Paths::new(root.clone()),
        Arc::clone(&store),
        port,
        Arc::clone(&rec) as Arc<dyn Surface>,
    )
    .unwrap();
    (rt, store, rec, root)
}

/// The operator: the principal every local surface acts as.
fn op() -> ka_agent_core::Principal {
    ka_agent_core::Principal::operator()
}

async fn wait_until(tag: &str, rec: &Recorder, mut done: impl FnMut(&[AgentEvent]) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if done(&rec.events()) {
            return;
        }
        if Instant::now() >= deadline {
            panic!("timed out waiting for {tag}; events: {:#?}", rec.events());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// `/stop` under an open card: the orphan ask is denied at the turn's
/// finish, the session leaves `awaiting_approval`, and the message that
/// arrives right after runs — not steered into a dead reply.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_under_an_open_ask_does_not_swallow_the_next_message() {
    let steps = r#"{"text": "the answer"}"#;
    let (rt, store, rec, root) = runtime_with("abort", steps);

    rt.handle_message("run the gated thing", "console").await.unwrap();
    // The spend guard raises a card and parks the session in
    // awaiting_approval before the model gets to answer.
    wait_until("approval card", &rec, |evs| {
        evs.iter().any(|e| matches!(e, AgentEvent::Approval { .. }))
    })
    .await;
    assert_eq!(
        rt.session_status_for(&op()),
        ka_agent_core::SessionStatus::AwaitingApproval
    );

    // The abort kills the turn while the card is open.
    rt.stop_turn(&op(), "console").await.unwrap();
    // Whichever way the race resolves — the sweep denies a registered
    // orphan at the turn's finish, or the ask lands while the session is
    // already stopping and the ask arm itself denies it — the invariant
    // is the same: the turn aborts and the boundary closes.
    wait_until("turn aborted and boundary closed", &rec, |evs| {
        evs.iter()
            .any(|e| matches!(e, AgentEvent::TurnFinished { stop, .. } if stop == "aborted"))
            && evs
                .iter()
                .any(|e| matches!(e, AgentEvent::Session { status } if status == "Idle"))
    })
    .await;

    // The session is not parked: the boundary closed with the turn.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_ne!(
        rt.session_status_for(&op()),
        ka_agent_core::SessionStatus::AwaitingApproval,
        "an orphan ask must not park the session until its TTL"
    );
    // No pending approval row is left actionable.
    let pending = store
        .pending_approvals(&rt.session_id_for(&op()).unwrap())
        .unwrap()
        .len();
    assert_eq!(pending, 0, "the orphan card is settled, not pending");

    // The message that used to vanish into the dead reply now runs.
    rt.handle_message("still there?", "console").await.unwrap();
    wait_until("next message becomes a turn", &rec, |evs| {
        evs.iter()
            .any(|e| matches!(e, AgentEvent::Inbound { text, .. } if text == "still there?"))
    })
    .await;
    let sid = rt.session_id_for(&op()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let turns = store.latest_turns_for_session(&sid, 5, 0).unwrap();
        if turns.iter().any(|t| t.inbound_text.contains("still there")) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the post-abort prompt must become a turn; turns: {turns:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let _ = std::fs::remove_dir_all(&root);
}
