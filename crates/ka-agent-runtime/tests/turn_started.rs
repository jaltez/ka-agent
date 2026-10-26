//! Live-path regressions: exactly one `AgentEvent::TurnStarted` per runtime
//! turn, carrying the recorded turn id and the channel source.
//!
//! Drives a real worker (the scripted-speaker harness) through the runtime,
//! so both emission paths are live: the engine's `turn_started` port event
//! and the submit handshake. Before the fix the handshake emitted a second
//! `TurnStarted` for the same turn (after `submit` resolved), resetting every
//! surface's per-turn stream mid-turn.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ka_agent_port::WorkerPort;
use ka_agent_runtime::event::AgentEvent;
use ka_agent_runtime::surface::Surface;
use ka_agent_runtime::{Envelope, Paths, Runtime};
use ka_agent_store::Store;

/// Everything the runtime told the surface, in order.
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

/// Not a test in the ordinary sense: when the port spawns this binary as its
/// worker, libtest's filter `__worker` lands here and runs the real worker
/// loop on the scenario the parent wrote to `KA_DATA_DIR/scenario.json`.
/// Without the `__worker` argument it is a no-op in a normal test run.
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

/// A port that spawns this test binary as the worker. The port adds
/// `__worker --scenario <path>`; the wrapper forwards only the filter, and
/// the scenario reaches the worker through `KA_DATA_DIR` (as in the channel
/// e2e suites).
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

/// A runtime on a fresh temp root with the scripted worker and a recording
/// surface. The caller removes the root at the end.
#[cfg(unix)]
fn runtime_with(tag: &str, steps: &str) -> (Runtime, Arc<Store>, Arc<Recorder>, PathBuf) {
    let root = std::env::temp_dir().join(format!("ka-turn-started-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let store = Arc::new(Store::open(&root.join("state.db")).unwrap());
    let port = scripted_port(&root, steps);
    let rec = Arc::new(Recorder::default());
    let cfg = ka_agent_core::DaemonConfig {
        workspace: root.join("ws"),
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

async fn wait_until(tag: &str, rec: &Recorder, mut done: impl FnMut(&[AgentEvent]) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if done(&rec.events()) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{tag}: timed out; events so far: {:#?}",
            rec.events()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn finished(events: &[AgentEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, AgentEvent::TurnFinished { .. }))
        .count()
}

fn starts(events: &[AgentEvent]) -> Vec<(String, String)> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::TurnStarted { turn, source } => Some((turn.clone(), source.clone())),
            _ => None,
        })
        .collect()
}

/// Two live turns: one `TurnStarted` each, with the recorded turn id and the
/// submitting channel as source, and the announcement before the turn's first
/// delta.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_turn_started_per_live_turn() {
    let (rt, store, rec, root) = runtime_with("live", r#"{"text": "scripted answer"}"#);

    rt.handle_message("first", "console").await.unwrap();
    wait_until("first turn", &rec, |evs| finished(evs) == 1).await;
    wait_until("first turn idle", &rec, |_| {
        rt.session_status() == ka_agent_core::SessionStatus::Idle
    })
    .await;

    rt.handle_message("second", "matrix:u1").await.unwrap();
    wait_until("second turn", &rec, |evs| finished(evs) == 2).await;

    let events = rec.events();
    let turns = store
        .latest_turns_for_session(&rt.session_id(), 10)
        .unwrap();
    assert_eq!(turns.len(), 2, "two runtime turns were recorded");
    assert_eq!(
        starts(&events),
        vec![
            (turns[0].id.to_string(), "console".to_string()),
            (turns[1].id.to_string(), "matrix:u1".to_string()),
        ],
        "exactly one announcement per turn, with the recorded id and the channel source; \
         events: {events:#?}"
    );

    // The announcement precedes the turn's first delta, so a surface resets
    // an empty stream, never one that is already streaming.
    let start_idx = events
        .iter()
        .position(|e| matches!(e, AgentEvent::TurnStarted { .. }))
        .unwrap();
    let delta_idx = events
        .iter()
        .position(|e| matches!(e, AgentEvent::Delta { .. }))
        .expect("the scripted turn streamed a delta");
    assert!(
        start_idx < delta_idx,
        "TurnStarted must come before the first delta; events: {events:#?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// The engine runs several turns inside one runtime turn (auto-poke): the
/// scripted model leaves a todo pending, so ka starts a second turn in the
/// same `submit → … → idle` cycle. The surface is still told once, so its
/// stream is not reset under the running answer.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn engine_turns_inside_one_runtime_turn_are_announced_once() {
    let steps = r#"{"call": {"tool": "todo", "args": {"items": [{"text": "still pending", "state": "pending"}]}}}, {"text": "working"}"#;
    let (rt, store, rec, root) = runtime_with("auto-poke", steps);

    rt.handle_message("do the work", "console").await.unwrap();
    // The poke note proves a second engine turn ran inside this runtime
    // turn; the two finished turns count both engine turns.
    wait_until("auto-poke", &rec, |evs| {
        evs.iter()
            .any(|e| matches!(e, AgentEvent::Note { text } if text.contains("auto-poke")))
            && finished(evs) == 2
    })
    .await;

    let events = rec.events();
    let turns = store
        .latest_turns_for_session(&rt.session_id(), 10)
        .unwrap();
    assert_eq!(turns.len(), 1, "one runtime turn was recorded");
    assert_eq!(
        starts(&events),
        vec![(turns[0].id.to_string(), "console".to_string())],
        "the runtime announces the cycle once, not once per engine turn; \
         events: {events:#?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}
