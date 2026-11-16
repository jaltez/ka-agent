//! The empty-final regression: a model reply that carries only reasoning
//! (content empty, no tool calls) used to end as a done turn with zero
//! delivered text — no outbox row, no error, no fallback, no notice. The
//! operator saw "typing…" and then silence.
//!
//! The runtime cannot force the model to answer, but it must never let
//! that pass silently: the turn is recorded with the stop word `empty`,
//! the model is asked once more automatically (a queued follow-on turn),
//! and a second empty reply produces an explicit channel notice instead
//! of a dead "typing…".

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
    let root = std::env::temp_dir().join(format!("ka-empty-final-{tag}-{}", std::process::id()));
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
        assert!(Instant::now() < deadline, "timed out waiting for {tag}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn finished(events: &[AgentEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, AgentEvent::TurnFinished { .. }))
        .count()
}

/// A reasoning-only reply (thought, no text) is never silent: the turn is
/// recorded `empty` (not done), the runtime asks the model once more, and
/// when the re-ask also comes back empty the channel gets an explicit
/// notice. No FinalChunk ever fires — there is nothing to deliver — and
/// the nudge does not loop.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reasoning_only_reply_nudges_once_then_says_so() {
    let steps = r#"{"thought": "the whole answer, but only as reasoning"}"#;
    let (rt, store, rec, root) = runtime_with("give-up", steps);

    rt.handle_message("what is the state of the disk?", "console")
        .await
        .unwrap();
    wait_until("empty-answer notice", &rec, |evs| {
        evs.iter()
            .any(|e| matches!(e, AgentEvent::Notice { text } if text.contains("empty answer")))
    })
    .await;

    let events = rec.events();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Note { text } if text.contains("asking again"))),
        "the automatic re-ask is announced; events: {events:#?}"
    );
    assert_eq!(
        finished(&events),
        2,
        "the prompt turn plus the one nudge turn; events: {events:#?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::FinalChunk { .. })),
        "an empty answer delivers nothing; events: {events:#?}"
    );

    // Both turns are done in the store but distinguishable: stop `empty`.
    let sid = rt.session_id_for(&op()).unwrap();
    let turns = store.latest_turns_for_session(&sid, 10, 0).unwrap();
    assert_eq!(turns.len(), 2, "one row for the prompt, one for the nudge");
    assert!(
        turns.iter().all(|t| t.stop.as_deref() == Some("empty")),
        "the silent family is distinguishable in telemetry; turns: {turns:?}"
    );
    assert!(
        turns.iter().all(|t| t.status == ka_agent_core::TurnStatus::Done),
        "usage accounting still lands; turns: {turns:?}"
    );
    assert!(
        store
            .chunks_for_turn(&turns[0].id)
            .unwrap()
            .is_empty()
            && store.chunks_for_turn(&turns[1].id).unwrap().is_empty(),
        "no outbox row: there was never any text"
    );

    // The nudge is spent: no third turn appears after the dust settles.
    tokio::time::sleep(Duration::from_millis(750)).await;
    assert_eq!(
        finished(&rec.events()),
        2,
        "the re-ask is bounded to one per worker"
    );
    assert_eq!(store.queue_len(&sid).unwrap(), 0);

    let _ = std::fs::remove_dir_all(&root);
}
