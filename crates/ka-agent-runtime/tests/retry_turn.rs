//! One failed turn, then `/retry` re-sends it on the same scripted worker.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::{Arc, Mutex};
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
    let text = rt.retry_last().await.unwrap();
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
