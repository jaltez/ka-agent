//! The scout child (v4 F13): one read-only research worker per call.
//!
//! These are scripted-worker e2e tests: the port spawns this test binary as
//! its worker, the scenario file decides what the engine says, and the
//! assertions are about the child's *contract* — fresh strand, readonly
//! permissions, question-only prompt, strict toolset, non-authoritative
//! header, a `kind = scout` run row, fail-closed asks, deadline, and the
//! worker-pool cap.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ka_agent_core::DaemonConfig;
use ka_agent_port::WorkerPort;
use ka_agent_runtime::event::AgentEvent;
use ka_agent_runtime::surface::Surface;
use ka_agent_runtime::{Envelope, Paths, Runtime};
use ka_agent_store::Store;

/// Not a test in the ordinary sense: when the port spawns this binary as its
/// worker, libtest's filter `__worker` lands here and runs the real worker
/// loop on the scenario the parent wrote to `KA_DATA_DIR/scenario.json`.
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

fn fresh_root(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("ka-scout-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn write_executable(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// A port that spawns this test binary as the worker on a scripted scenario.
#[cfg(unix)]
fn scenario_port(root: &Path, steps: &str) -> WorkerPort {
    let paths = Paths::new(root.to_path_buf());
    let scenario = paths.ka_data().join("scenario.json");
    std::fs::create_dir_all(scenario.parent().unwrap()).unwrap();
    std::fs::write(
        &scenario,
        format!(r#"{{"steps": [{steps}], "stop": "done", "cost_usd": 0.0, "tokens": 10}}"#),
    )
    .unwrap();
    let wrapper = root.join("worker.sh");
    write_executable(
        &wrapper,
        &format!(
            "#!/bin/sh\nexec '{}' __worker\n",
            std::env::current_exe().unwrap().display()
        ),
    );
    WorkerPort {
        binary: wrapper,
        scenario_override: Some(scenario),
    }
}

/// A port whose worker tees its stdin (every protocol line, including the
/// prompt) into `<root>/stdin.<pid>`, so a test can read what the child was
/// actually given.
#[cfg(unix)]
fn capture_port(root: &Path, steps: &str) -> WorkerPort {
    let mut port = scenario_port(root, steps);
    let wrapper = root.join("capture.sh");
    write_executable(
        &wrapper,
        &format!(
            "#!/bin/sh\ntee -a '{}'/stdin.$$ | exec '{}' __worker\n",
            root.display(),
            std::env::current_exe().unwrap().display()
        ),
    );
    port.binary = wrapper;
    port
}

/// A fake worker that emits one permission ask and then finishes: the child
/// ask-deny path has no seam in the scripted speaker.
#[cfg(unix)]
fn ask_port(root: &Path, answer_log: &Path) -> WorkerPort {
    let wrapper = root.join("ask.sh");
    write_executable(
        &wrapper,
        &format!(
            r#"#!/bin/sh
printf '%s\n' '{{"ev":"ready"}}'
while IFS= read -r line; do
  case "$line" in
    *'"op":"open"'*) printf '%s\n' '{{"ev":"strand","id":"strand_test","path":null}}' ;;
    *'"op":"submit"'*)
      printf '%s\n' '{{"ev":"ka","event":{{"type":"ask","id":"ask_1","questions":[{{"text":"run bash?","options":["allow","always","deny"]}}]}}}}'
      IFS= read -r answer
      printf '%s\n' "$answer" >> '{}'
      printf '%s\n' '{{"ev":"ka","event":{{"type":"delta","kind":{{"text":"answered after deny"}}}}}}'
      printf '%s\n' '{{"ev":"ka","event":{{"type":"turn_finished","stop":"done","usage":{{"input":1,"output":1,"cache_read":0,"cache_write":0,"cost":0.001}}}}}}'
      printf '%s\n' '{{"ev":"ka","event":{{"type":"idle"}}}}'
      ;;
    *'"op":"close"'*) exit 0 ;;
  esac
done
"#,
            answer_log.display()
        ),
    );
    WorkerPort {
        binary: wrapper,
        scenario_override: None,
    }
}

#[cfg(unix)]
fn scout_runtime(
    tag: &str,
    port_fn: impl FnOnce(&Path) -> WorkerPort,
    tune: impl FnOnce(&mut DaemonConfig),
) -> (Runtime, Arc<Store>, Arc<Recorder>, PathBuf) {
    let root = fresh_root(tag);
    let port = port_fn(&root);
    let store = Arc::new(Store::open(&root.join("state.db")).unwrap());
    let rec = Arc::new(Recorder::default());
    let mut cfg = DaemonConfig {
        workspace: root.join("ws"),
        sandbox: "off".into(),
        ..DaemonConfig::default()
    };
    cfg.scout.enabled = true;
    tune(&mut cfg);
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

fn scout_runs(store: &Store) -> Vec<ka_agent_store::RunRow> {
    store
        .recent_runs(20)
        .unwrap()
        .into_iter()
        .filter(|r| r.kind == "scout")
        .collect()
}

/// The child answers; the parent gets the header, the summary, and a
/// `kind = scout` run row with the spend.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn scout_runs_a_read_only_child_and_returns_the_summary() {
    let (rt, store, _rec, _root) = scout_runtime(
        "summary",
        |root| scenario_port(root, r#"{"text": "the deploy key rotates on friday"}"#),
        |cfg| cfg.scout.timeout_secs = 30,
    );
    let reply = rt
        .run_scout(&op(), "when does the deploy key rotate?")
        .await
        .unwrap();
    assert!(
        reply.starts_with(
            "--- scout summary (read-only child, non-authoritative — verify before acting) ---"
        ),
        "{reply}"
    );
    assert!(reply.contains("rotates on friday"), "{reply}");
    let runs = scout_runs(&store);
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0].status, "succeeded");
    assert_eq!(runs[0].stop.as_deref(), Some("done"));
    assert!(
        runs[0].tokens.is_some_and(|tokens| tokens > 0),
        "no token spend booked: {runs:?}"
    );
    assert!(runs[0].cost_usd.is_some(), "no cost booked: {runs:?}");
    // Nothing of the child outlives the call.
    assert_eq!(rt.live_worker_count(), 0);
}

/// The prompt the child receives: a context line and the question — its own
/// fresh strand, readonly permissions, the strict toolset, and nothing of
/// the parent's conversation.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn scout_child_gets_the_question_not_the_parent_transcript() {
    let (rt, _store, rec, root) = scout_runtime(
        "prompt",
        |root| capture_port(root, r#"{"text": "found it"}"#),
        |cfg| cfg.scout.timeout_secs = 30,
    );
    rt.handle_message(
        "PARENT-SENTINEL: what did we decide about the deploy?",
        "console",
    )
    .await
    .unwrap();
    wait_until("parent turn", &rec, |events| {
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::TurnFinished { .. }))
    })
    .await;
    let reply = rt
        .run_scout(&op(), "where do the deploy docs live?")
        .await
        .unwrap();
    assert!(reply.contains("found it"), "{reply}");

    // The child's own protocol lines are the one file that carries the
    // scout question.
    let child = std::fs::read_dir(&root)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("stdin."))
        .find_map(|entry| {
            let text = std::fs::read_to_string(entry.path()).ok()?;
            text.contains("<scout-question>").then_some(text)
        })
        .expect("no captured scout stdin");
    assert!(
        child.contains("where do the deploy docs live?"),
        "the child never saw the question: {child}"
    );
    assert!(
        !child.contains("PARENT-SENTINEL"),
        "the child inherited the parent transcript: {child}"
    );
    assert!(
        child.contains(r#""mode":"readonly""#),
        "the child did not open readonly: {child}"
    );
    assert!(
        child.contains(r#""strand":"new""#),
        "the child resumed the parent strand: {child}"
    );
    // Admin has no allowlist: the base read-only set is submitted as an
    // explicit allowlist (never `null` = every tool).
    assert!(
        child.contains(r#""allowed_tools":["read","glob","grep","web_search","web_fetch"]"#),
        "unexpected child toolset: {child}"
    );
}

/// A wedged child hits its deadline and errors; the run row says timeout.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn scout_wedge_hits_its_deadline_and_errors() {
    let (rt, store, _rec, _root) = scout_runtime(
        "wedge",
        |root| scenario_port(root, r#"{"sleep_ms": 5000}"#),
        |cfg| cfg.scout.timeout_secs = 1,
    );
    let err = rt
        .run_scout(&op(), "why is the moon made of cheese?")
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("did not finish within 1 s"),
        "{err}"
    );
    let runs = scout_runs(&store);
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0].status, "failed");
    assert_eq!(runs[0].stop.as_deref(), Some("timeout"));
    assert_eq!(rt.live_worker_count(), 0);
}

/// A child ask is denied immediately (fail closed) and audited; the child
/// continues with what it already had and never widens its permissions.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn scout_child_ask_is_denied_fail_closed() {
    let (rt, store, _rec, root) = scout_runtime(
        "ask",
        |root| ask_port(root, &root.join("answer.jsonl")),
        |cfg| cfg.scout.timeout_secs = 30,
    );
    let answer_log = root.join("answer.jsonl");
    let reply = rt.run_scout(&op(), "does the build pass?").await.unwrap();
    assert!(reply.contains("answered after deny"), "{reply}");
    let answer = std::fs::read_to_string(&answer_log).unwrap();
    assert!(answer.contains(r#""choice":2"#), "not a deny: {answer}");
    let audit = store.recent_audit(10).unwrap();
    assert!(
        audit.iter().any(|row| {
            row.action == "approval.decide"
                && row
                    .detail
                    .as_deref()
                    .is_some_and(|d| d.contains("scout child may not ask"))
        }),
        "no scout ask audit row: {audit:?}"
    );
}

/// Scouts are counted in the worker pool: with the only slot taken, a second
/// scout answers busy instead of spawning past `runtime.max_workers`.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn scout_respects_the_worker_cap() {
    let (rt, _store, _rec, _root) = scout_runtime(
        "cap",
        |root| scenario_port(root, r#"{"sleep_ms": 800}, {"text": "slow done"}"#),
        |cfg| {
            cfg.runtime.max_workers = 1;
            cfg.scout.max_concurrent = 2;
            cfg.scout.timeout_secs = 30;
        },
    );
    let first = tokio::spawn({
        let rt = rt.clone();
        async move { rt.run_scout(&op(), "slow question").await }
    });
    // Let the first claim the pool slot before the second asks for one.
    tokio::time::sleep(Duration::from_millis(250)).await;
    let err = rt.run_scout(&op(), "second question").await.unwrap_err();
    assert!(
        err.to_string().contains("all 1 engine workers are busy"),
        "{err}"
    );
    let reply = first.await.unwrap().unwrap();
    assert!(reply.contains("slow done"), "{reply}");
}
