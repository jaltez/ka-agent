//! One failed turn, then `/retry` re-sends it on the same scripted worker;
//! and the throttle stamp a terminal rate-limit report sets (A6).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use ka_agent_port::WorkerPort;
use ka_agent_runtime::event::AgentEvent;
use ka_agent_runtime::surface::Surface;
use ka_agent_runtime::{Envelope, Paths, Runtime};
use ka_agent_store::Store;

#[derive(Default)]
struct Recorder(Mutex<Vec<AgentEvent>>);
impl Surface for Recorder {
    fn emit(&self, env: &Envelope) {
        self.0.lock().unwrap().push(env.ev.clone());
    }
}

impl Recorder {
    fn events(&self) -> Vec<AgentEvent> {
        self.0.lock().unwrap().clone()
    }
}

#[test]
#[allow(non_snake_case)]
fn mock__worker() {
    if !std::env::args().any(|a| a == "__worker") {
        return;
    }
    let _ = ka_agent_port::run_worker(ka_agent_port::WorkerRun { scenario: None });
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

#[cfg(not(unix))]
fn scripted_port(_root: &Path, _steps: &str) -> WorkerPort {
    WorkerPort::default()
}

#[cfg(unix)]
/// The operator: the principal every local surface acts as.
fn op() -> ka_agent_core::Principal {
    ka_agent_core::Principal::operator()
}

#[tokio::test(flavor = "multi_thread")]
async fn retry_resends_after_a_protocol_error() {
    let root = std::env::temp_dir().join(format!("ka-retry-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let store = Arc::new(Store::open(&root.join("state.db")).unwrap());
    let rec = Arc::new(Recorder::default());
    let cfg = ka_agent_core::DaemonConfig {
        workspace: root.join("ws"),
        sandbox: "off".into(),
        ..ka_agent_core::DaemonConfig::default()
    };
    let rt = Runtime::new(
        cfg,
        Paths::new(root.clone()),
        Arc::clone(&store),
        scripted_port(
            &root,
            r#"{"fail": {"class": "protocol", "retryable": false, "message": "glitch"}}, {"text": "recovered answer"}"#,
        ),
        Arc::clone(&rec) as Arc<dyn Surface>,
    )
    .unwrap();
    rt.handle_message("please answer", "console").await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(40);
    while !rec
        .events()
        .iter()
        .any(|e| matches!(e, AgentEvent::TurnFinished { .. }))
    {
        assert!(
            Instant::now() < deadline,
            "first turn never finished: {:?}",
            rec.events()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let text = rt.retry_last(&op()).await.unwrap();
    assert_eq!(text.as_deref(), Some("please answer"));
    let deadline = Instant::now() + Duration::from_secs(40);
    loop {
        let events = rec.events();
        if events.iter().any(|e| matches!(e, AgentEvent::FinalChunk { text, .. } if text.contains("recovered answer"))) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "retry never answered: {events:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// A6: records, for every engine `rate_limit` report that reaches the
/// surface, whether `/status` showed the throttle at that moment — the
/// window between the report and the turn's finish, which clears the stamp
/// again.
#[derive(Default)]
struct ThrottleWatch {
    runtime: OnceLock<Runtime>,
    stamped: Mutex<Vec<bool>>,
    finished: AtomicBool,
}

impl Surface for ThrottleWatch {
    fn emit(&self, env: &Envelope) {
        match &env.ev {
            AgentEvent::Error { class, .. } if class == "rate_limit" => {
                let rt = self.runtime.get().unwrap();
                let stamped = rt.status_report_for(&op()).unwrap().throttled_since_ms;
                self.stamped.lock().unwrap().push(stamped.is_some());
            }
            AgentEvent::TurnFinished { .. } => self.finished.store(true, Ordering::SeqCst),
            _ => {}
        }
    }
}

/// The engine's terminal report always carries `retryable: false` — its own
/// backoff and fallback hops are spent by then — so the class alone stamps
/// the throttle: `/status` shows the cooldown from the report until the
/// turn's finish clears it. Keying on the flag never stamped anything.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_terminal_rate_limit_report_stamps_the_throttle() {
    let root = std::env::temp_dir().join(format!("ka-throttle-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let store = Arc::new(Store::open(&root.join("state.db")).unwrap());
    let watch = Arc::new(ThrottleWatch::default());
    let cfg = ka_agent_core::DaemonConfig {
        workspace: root.join("ws"),
        sandbox: "off".into(),
        ..ka_agent_core::DaemonConfig::default()
    };
    let rt = Runtime::new(
        cfg,
        Paths::new(root.clone()),
        Arc::clone(&store),
        scripted_port(
            &root,
            r#"{"fail": {"class": "rate_limit", "retryable": false, "message": "429 from the provider"}}"#,
        ),
        Arc::clone(&watch) as Arc<dyn Surface>,
    )
    .unwrap();
    watch.runtime.set(rt.clone()).ok();
    rt.handle_message("please answer", "console").await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(40);
    while !watch.finished.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "the turn never finished");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let stamped = watch.stamped.lock().unwrap().clone();
    assert_eq!(
        stamped,
        [true],
        "the rate-limit report did not find the throttle stamped"
    );
    assert!(
        rt.status_report_for(&op())
            .unwrap()
            .throttled_since_ms
            .is_none(),
        "the settled turn must clear the stamp"
    );
    let _ = std::fs::remove_dir_all(&root);
}
