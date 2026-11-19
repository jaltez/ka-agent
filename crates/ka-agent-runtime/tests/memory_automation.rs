//! B15b read-side bracketing: a scheduled turn compiles without personal
//! memory files, and the full compile is restored at every exit of the
//! bracket — the normal cycle end and a turn that ends in error.

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

#[test]
#[allow(non_snake_case)]
fn mock__worker() {
    if !std::env::args().any(|a| a == "__worker") {
        return;
    }
    let fallback = std::env::var_os("KA_DATA_DIR")
        .map(|d| PathBuf::from(d).join("scenario.json"))
        .filter(|p| p.exists());
    let _ = ka_agent_port::run_worker(ka_agent_port::WorkerRun { scenario: fallback });
}

fn op() -> ka_agent_core::Principal {
    ka_agent_core::Principal::operator()
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
fn runtime_with(tag: &str, steps: &str) -> (Runtime, Arc<Recorder>, PathBuf) {
    let root = std::env::temp_dir().join(format!("ka-mem-auto-{tag}-{}", std::process::id()));
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
    (rt, rec, root)
}

#[cfg(unix)]
fn engine_memory(root: &Path) -> PathBuf {
    Paths::new(root.to_path_buf())
        .user_engine_home("operator")
        .join(".config/ka/MEMORY.md")
}

/// Seed one machine fact and one personal fact, both present in the full
/// compile, and return the engine-home MEMORY.md bytes.
#[cfg(unix)]
fn seed_full_memory(rt: &Runtime, root: &Path) -> Vec<u8> {
    let memory = rt.memory_store(&op().username).unwrap();
    memory
        .add(
            ka_agent_memory::MemoryFile::Environment,
            "laptop has 32 GB",
            "operator",
            None,
        )
        .unwrap();
    memory.commit("memory: seed environment").unwrap();
    rt.remember("likes tea personally", "operator", &op())
        .unwrap();
    let path = engine_memory(root);
    let bytes = std::fs::read(&path).unwrap();
    let text = String::from_utf8_lossy(&bytes).to_string();
    assert!(text.contains("laptop has 32 GB"), "{text}");
    assert!(text.contains("likes tea personally"), "{text}");
    bytes
}

#[cfg(unix)]
async fn wait_idle(tag: &str, rt: &Runtime, rec: &Recorder) {
    let done = |rec: &Recorder, rt: &Runtime| {
        rec.events()
            .iter()
            .any(|e| matches!(e, AgentEvent::TurnFinished { .. }))
            && rt.session_status_for(&op()) == ka_agent_core::SessionStatus::Idle
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    while !done(rec, rt) {
        assert!(
            Instant::now() < deadline,
            "{tag}: timed out; events so far: {:#?}",
            rec.events()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // The idle boundary does the final delivery; give the restore a beat.
    tokio::time::sleep(Duration::from_millis(200)).await;
}

/// During the cron cycle the engine-home MEMORY.md is the automation
/// compile (environment bullet, no personal bullet); after the cycle
/// settles it is byte-identical to the pre-cycle full compile.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scheduled_turn_compiles_without_personal_files_and_restores() {
    // A slow first step keeps the turn open long enough to observe the
    // lean compile mid-cycle.
    let (rt, rec, root) = runtime_with("cycle", r#"{"sleep_ms": 800}, {"text": "cron answer"}"#);
    let before = seed_full_memory(&rt, &root);

    rt.handle_cron_message("status check", "cron:test", &op(), None)
        .await
        .unwrap();

    // Mid-cycle: the automation compile is installed — machine facts in,
    // personal facts out.
    let deadline = Instant::now() + Duration::from_secs(10);
    let lean_seen = loop {
        let text = std::fs::read_to_string(engine_memory(&root)).unwrap_or_default();
        if !text.contains("likes tea personally") && text.contains("laptop has 32 GB") {
            break true;
        }
        assert!(
            Instant::now() < deadline,
            "automation compile never installed; MEMORY.md: {text}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert!(lean_seen);

    wait_idle("scheduled cycle", &rt, &rec).await;
    let after = std::fs::read(engine_memory(&root)).unwrap();
    assert_eq!(
        after, before,
        "the full compile is restored byte-for-byte after the cycle"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// A scheduled turn that ends in error still restores the full compile
/// (the bracket's terminal-point restore, not just the happy path).
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scheduled_turn_failure_restores_full_memory() {
    let (rt, rec, root) = runtime_with(
        "fail",
        r#"{"fail": {"class": "protocol", "retryable": false, "message": "boom"}}"#,
    );
    let before = seed_full_memory(&rt, &root);

    rt.handle_cron_message("status check", "cron", &op(), None)
        .await
        .unwrap();
    wait_idle("scheduled failure", &rt, &rec).await;

    let after = std::fs::read(engine_memory(&root)).unwrap();
    assert_eq!(
        after, before,
        "the full compile is restored even when the turn fails"
    );

    let _ = std::fs::remove_dir_all(&root);
}
