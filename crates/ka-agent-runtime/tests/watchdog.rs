//! The staged turn watchdog (turn_watchdog config): a worker that
//! accepts a prompt and then goes silent (a long `sleep_ms` scenario
//! step) must produce the "still working" notice once, then the abort
//! notice, leaving the session `idle` and the turn terminal.
//!
//! The engine worker is the real scripted-speaker binary
//! (`ka-port-test-worker`), spawned through `WorkerPort` with
//! `scenario_override` — the same wire path production takes.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ka_agent_core::TurnStatus;
use ka_agent_runtime::event::AgentEvent;
use ka_agent_runtime::surface::Surface;
use ka_agent_runtime::{Envelope, Paths, Runtime};
use ka_agent_store::Store;

/// Collects every emitted event for assertions.
#[derive(Clone, Default)]
struct Collect(Arc<Mutex<Vec<AgentEvent>>>);

impl Surface for Collect {
    fn emit(&self, env: &Envelope) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(env.ev.clone());
    }
}

impl Collect {
    fn wait_for_notice(&self, needle: &str, timeout: Duration) -> String {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let hit = self
                .0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .find_map(|ev| match ev {
                    AgentEvent::Notice { text } if text.contains(needle) => Some(text.clone()),
                    _ => None,
                });
            if let Some(text) = hit {
                return text;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "no notice containing {needle:?} within {timeout:?}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn count_notice(&self, needle: &str) -> usize {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|ev| matches!(ev, AgentEvent::Notice { text } if text.contains(needle)))
            .count()
    }
}

/// The scripted-speaker worker binary. Built for these tests by the
/// `test-support` dev-dependency on ka-agent-port; `CARGO_BIN_EXE_*` is
/// only exported to a package's own tests, so resolve it from the target
/// dir (honoring `CARGO_TARGET_DIR`, debug before release).
fn test_worker_binary() -> PathBuf {
    if let Ok(exe) = std::env::var("CARGO_BIN_EXE_ka-port-test-worker") {
        return PathBuf::from(exe);
    }
    let exe = if cfg!(windows) {
        "ka-port-test-worker.exe"
    } else {
        "ka-port-test-worker"
    };
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .join("target")
        });
    for profile in ["debug", "release"] {
        let p = target.join(profile).join(exe);
        if p.exists() {
            return p;
        }
    }
    // A dev-dependency does not build the dependency's bin targets, so
    // a clean target dir has no worker yet — build it (the test-support
    // feature exists precisely for this).
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let _ = std::process::Command::new(cargo)
        .args(["build", "-p", "ka-agent-port", "--features", "test-support"])
        .status();
    for profile in ["debug", "release"] {
        let p = target.join(profile).join(exe);
        if p.exists() {
            return p;
        }
    }
    panic!(
        "ka-port-test-worker not built under {} and `cargo build -p ka-agent-port \
         --features test-support` did not produce it",
        target.display()
    );
}

/// One watchdog run: submit a prompt against a silent scripted worker,
/// wait for both notices, then for the session to return `idle` with the
/// turn terminal.
async fn watchdog_run(silent_secs: u32, abort_secs: u32) -> (Collect, TurnStatus) {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(&root.path().join("state.db")).unwrap());
    let scenario = root.path().join("stall.json");
    // A single long sleep: the worker accepts the prompt and then emits
    // nothing — the wedge shape the watchdog exists for.
    std::fs::write(
        &scenario,
        r#"{ "steps": [ {"sleep_ms": 120000} ], "stop": "done", "cost_usd": 0.0, "tokens": 0 }"#,
    )
    .unwrap();

    let cfg = ka_agent_core::DaemonConfig {
        workspace: root.path().join("ws"),
        turn_grace_secs: 1,
        turn_watchdog: ka_agent_core::TurnWatchdog {
            silent_secs,
            abort_secs,
        },
        ..ka_agent_core::DaemonConfig::default()
    };
    let mut port = ka_agent_port::WorkerPort::with_current_exe();
    port.binary = test_worker_binary();
    port.scenario_override = Some(scenario);
    let collect = Collect::default();
    let rt = Runtime::new(
        cfg,
        Paths::new(root.path().to_path_buf()),
        Arc::clone(&store),
        port,
        Arc::new(collect.clone()),
    )
    .unwrap();

    // Every source resolves to a user: link this identity (an unlinked
    // sender is refused silently, as on the channels).
    store
        .link_identity_as("telegram", "1", "test", "admin")
        .unwrap();
    rt.handle_message("stall please", "telegram:1")
        .await
        .unwrap();

    let maintenance = tokio::spawn({
        let rt = rt.clone();
        async move { rt.run_maintenance(Duration::from_millis(200)).await }
    });

    // Notify first, then the abort — in that order.
    let notify = collect.wait_for_notice("still working", Duration::from_secs(60));
    assert!(
        notify.contains("/stop"),
        "the notice names the manual exit: {notify:?}"
    );
    collect.wait_for_notice("aborting", Duration::from_secs(60));

    // The abort lands like /stop: the turn goes terminal and the session
    // returns to idle.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let turn_status = loop {
        let status = rt.status_report_for(&op()).unwrap().status;
        let latest = store
            .latest_turns_for_session(&rt.session_id_for(&op()).unwrap(), 1, 0)
            .unwrap()
            .into_iter()
            .next()
            .map(|t| t.status);
        if status == "Idle" && latest.is_some_and(|s| s.is_terminal()) {
            break latest.unwrap();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "session never returned to idle (status {status:?}, latest turn {latest:?})"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    maintenance.abort();
    (collect, turn_status)
}

/// The operator: the principal every local surface acts as.
fn op() -> ka_agent_core::Principal {
    ka_agent_core::Principal::operator()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn silent_worker_is_notified_then_aborted() {
    let (collect, turn_status) = watchdog_run(1, 4).await;
    // Exactly one "still working" notice for the silent stretch.
    assert_eq!(
        collect.count_notice("still working"),
        1,
        "the notice fires once per silent stretch"
    );
    // The abort is /stop's path: the turn is terminal (aborted or, when
    // the deliberate kill won the race, interrupted) — never left open.
    assert!(
        matches!(turn_status, TurnStatus::Aborted | TurnStatus::Interrupted),
        "turn status was {turn_status:?}"
    );
}
