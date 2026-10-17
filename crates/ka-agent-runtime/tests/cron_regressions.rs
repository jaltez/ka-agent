//! M11 cron regressions: one-shot vs recurring semantics, missed-occupation
//! skipping, the no-agent handler registry, and the AGENTS.md seed.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use ka_agent_core::{CronMode, CronSpec};
use ka_agent_runtime::Runtime;
use ka_agent_store::Store;

fn store_at(dir: &std::path::Path) -> Arc<Store> {
    std::fs::create_dir_all(dir).unwrap();
    Arc::new(Store::open(&dir.join("state.db")).unwrap())
}

fn config(root: &std::path::Path) -> ka_agent_core::DaemonConfig {
    ka_agent_core::DaemonConfig {
        workspace: root.join("ws"),
        ..ka_agent_core::DaemonConfig::default()
    }
}

/// Minimal runtime on a temp data root; the worker port never spawns (no
/// turn is submitted in these tests) and the surface records everything.
fn runtime_with(root: &std::path::Path, store: Arc<Store>, notes: Arc<Notes>) -> Runtime {
    let mut port = ka_agent_port::WorkerPort::with_current_exe();
    port.binary = std::path::PathBuf::from("/nonexistent/ka-agent");
    Runtime::new(
        config(root),
        ka_agent_runtime::Paths::new(root.to_path_buf()),
        store,
        port,
        notes,
    )
    .unwrap()
}

#[derive(Default)]
struct Notes(Mutex<Vec<String>>);

impl ka_agent_runtime::surface::Surface for Notes {
    fn on_note(&self, text: &str) {
        self.0.lock().unwrap().push(format!("note: {text}"));
    }
    fn on_approval(&self, _card: &ka_agent_runtime::surface::ApprovalCard) {}
    fn on_approval_settled(&self, _id: &str, _status: ka_agent_core::ApprovalStatus) {}
    fn on_turn_finished(&self, _stop: &str, _cost_usd: f64, _tokens: u64) {}
    fn on_session_status(&self, _status: ka_agent_core::SessionStatus) {}
    fn on_error(&self, class: &str, message: &str) {
        self.0
            .lock()
            .unwrap()
            .push(format!("error[{class}]: {message}"));
    }
}

fn root(tag: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("ka-cron-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn spec(text: &str, recurrence_ms: Option<i64>) -> CronSpec {
    CronSpec {
        text: text.into(),
        source: "operator".into(),
        recurrence_ms,
        mode: CronMode::NoAgent("echo".into()),
    }
}

/// A one-shot cron fires once and settles `succeeded`.
#[tokio::test]
async fn one_shot_cron_fires_once_and_succeeds() {
    let root = root("one-shot");
    let store = store_at(&root.join("data"));
    let notes = Arc::new(Notes::default());
    let rt = runtime_with(&root, Arc::clone(&store), Arc::clone(&notes));
    rt.set_no_agent_handler("echo", Arc::new(|arg: &str| Ok(format!("echo[{arg}]"))));

    let now = ka_agent_core::now_ms();
    let id = rt.add_cron(spec("ping", None), now - 1).unwrap();
    rt.fire_due_tasks().await.unwrap();

    // Terminal state is `succeeded`, off the live set and off the due list.
    let crons = store.cron_tasks().unwrap();
    assert!(
        crons.is_empty(),
        "one-shot must leave the live set: {crons:?}"
    );
    assert!(store.due_tasks(now + 60_000).unwrap().is_empty());
    assert_eq!(
        store.get_task(&id).unwrap().map(|t| t.status),
        Some(ka_agent_core::TaskStatus::Succeeded)
    );
    // The handler output reached the surface (NoAgent notes the result).
    let seen = notes.0.lock().unwrap().join("\n");
    assert!(seen.contains("echo[ping]"), "surface saw: {seen}");
    assert!(!seen.contains("error"), "surface saw: {seen}");
    let _ = std::fs::remove_dir_all(&root);
}

/// A recurring cron skips missed occupations: armed strictly after now,
/// back to `queued`, exactly one fire per sweep.
#[tokio::test]
async fn recurring_cron_skips_missed_occupations() {
    let root = root("recurring");
    let store = store_at(&root.join("data"));
    let notes = Arc::new(Notes::default());
    let rt = runtime_with(&root, Arc::clone(&store), notes);
    rt.set_no_agent_handler("echo", Arc::new(|arg: &str| Ok(format!("echo[{arg}]"))));

    let now = ka_agent_core::now_ms();
    rt.add_cron(spec("tick", Some(1_000)), now - 5_000).unwrap();
    rt.fire_due_tasks().await.unwrap();

    let after = ka_agent_core::now_ms();
    let crons = store.cron_tasks().unwrap();
    assert_eq!(crons.len(), 1, "recurring stays live: {crons:?}");
    assert_eq!(crons[0].status, ka_agent_core::TaskStatus::Queued);
    let next = crons[0].scheduled_at_ms.unwrap();
    assert!(
        next > after && next <= after + 1_000,
        "next fire {next} must sit in ({after}, {}]",
        after + 1_000
    );
    // The missed occupations were skipped, not replayed: the surface saw
    // exactly one fire.
    let fires = rt.fire_due_tasks().await.unwrap().len();
    assert_eq!(fires, 0, "nothing is due until the next tick");
    let _ = std::fs::remove_dir_all(&root);
}

/// A failing no-agent cron is marked `failed` and NOT re-armed.
#[tokio::test]
async fn failing_no_agent_cron_fails_without_rearm() {
    let root = root("failing");
    let store = store_at(&root.join("data"));
    let notes = Arc::new(Notes::default());
    let rt = runtime_with(&root, Arc::clone(&store), Arc::clone(&notes));
    rt.set_no_agent_handler("echo", Arc::new(|_: &str| Err("boom".to_string())));

    let now = ka_agent_core::now_ms();
    rt.add_cron(spec("bad", Some(1_000)), now - 1).unwrap();
    rt.fire_due_tasks().await.unwrap();

    assert!(
        store.cron_tasks().unwrap().is_empty(),
        "failed cron leaves the live set"
    );
    let seen = notes.0.lock().unwrap().join("\n");
    assert!(seen.contains("error[cron]"), "failure surfaced: {seen}");
    let _ = std::fs::remove_dir_all(&root);
}

/// An unregistered kind is refused, visibly, without crashing the sweep.
#[tokio::test]
async fn unknown_no_agent_kind_is_refused() {
    let root = root("unknown-kind");
    let store = store_at(&root.join("data"));
    let notes = Arc::new(Notes::default());
    let rt = runtime_with(&root, Arc::clone(&store), Arc::clone(&notes));

    let mut s = spec("x", Some(60_000));
    s.mode = CronMode::NoAgent("nosuch".into());
    rt.add_cron(s, ka_agent_core::now_ms() - 1).unwrap();
    rt.fire_due_tasks().await.unwrap();

    let seen = notes.0.lock().unwrap().join("\n");
    assert!(seen.contains("no handler for kind"), "surface saw: {seen}");
    assert!(store.cron_tasks().unwrap().is_empty());
    let _ = std::fs::remove_dir_all(&root);
}

/// M11: the workspace gets an AGENTS.md on first boot, and an operator's
/// own file is never overwritten on later boots.
#[test]
fn agents_md_seeded_once_never_overwritten() {
    let root = root("agents-md");
    let store = store_at(&root.join("data"));
    let rt = runtime_with(&root, Arc::clone(&store), Arc::new(Notes::default()));
    let path = rt.workspace().join("AGENTS.md");
    let first = std::fs::read_to_string(&path).unwrap();
    assert!(first.contains("# Ka Agent"), "{first}");
    assert!(first.contains("/cron"), "{first}");

    // The operator owns the file now; a second boot must not touch it.
    std::fs::write(&path, "OPERATOR-MARKER-XYZ\n").unwrap();
    let _ = runtime_with(&root, store, Arc::new(Notes::default()));
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "OPERATOR-MARKER-XYZ\n",
        "an existing AGENTS.md must never be overwritten"
    );
    let _ = std::fs::remove_dir_all(&root);
}
