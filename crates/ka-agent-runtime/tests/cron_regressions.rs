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
        at_local: None,
        weekdays: Vec::new(),
        month_day: None,
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

/// A reminder scheduled while the maintenance loop sleeps must fire at its
/// due time, not at the next interval: `add_reminder` notifies the loop,
/// which recomputes its sleep from the next due time.
#[tokio::test]
async fn reminder_added_while_sleeping_fires_on_time() {
    let root = root("reminder-prompt");
    let store = store_at(&root.join("data"));
    let notes = Arc::new(Notes::default());
    let rt = runtime_with(&root, Arc::clone(&store), Arc::clone(&notes));

    let driver = rt.clone();
    tokio::spawn(async move {
        driver
            .run_maintenance(std::time::Duration::from_secs(15))
            .await;
    });
    // Let the loop finish its boot pass (which would consume any immediate
    // wake) and settle into its idle sleep.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    rt.add_reminder_at(ka_agent_core::now_ms() + 1_000, "prompt-check")
        .unwrap();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let seen = notes.0.lock().unwrap().join("\n");
        if seen.contains("prompt-check") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "reminder did not fire within 5s; notes: {seen}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
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

/// Review F1: a cancel that lands mid-flight (while the row is `running`)
/// must win — the fire loop's settle/re-arm are conditional on `running`.
#[tokio::test]
async fn concurrent_cancel_wins_over_rearm() {
    let root = root("cancel-race");
    let store = store_at(&root.join("data"));
    let notes = Arc::new(Notes::default());
    let rt = runtime_with(&root, Arc::clone(&store), notes);
    rt.set_no_agent_handler("echo", Arc::new(|arg: &str| Ok(format!("echo[{arg}]"))));

    let now = ka_agent_core::now_ms();
    let id = rt.add_cron(spec("tick", Some(60_000)), now - 1).unwrap();

    // Simulate the fire loop having claimed the row (queued -> running)…
    assert!(store.claim_task(&id).unwrap(), "first claim takes the row");
    assert!(!store.claim_task(&id).unwrap(), "double claim is refused");
    // …the operator cancelling mid-flight…
    assert!(matches!(
        rt.cancel_scheduled(&id.as_str()[..8]).unwrap(),
        ka_agent_runtime::CancelOutcome::Cancelled(_)
    ));
    // …and the loop finishing: settle and re-arm must both be no-ops.
    assert!(!store.settle_task(&id, "succeeded").unwrap());
    assert!(!store.rearm_task(&id, now + 60_000).unwrap());
    let row = store.get_task(&id).unwrap().unwrap();
    assert_eq!(row.status, ka_agent_core::TaskStatus::Cancelled);
    let _ = std::fs::remove_dir_all(&root);
}

/// Review F3: a crash mid-fire leaves the row `running`; the next boot
/// reconciles it to `interrupted` (visible as paused, resumable), not stuck
/// invisible forever.
#[test]
fn startup_reconciles_tasks_left_running() {
    let root = root("reconcile-tasks");
    let store = store_at(&root.join("data"));

    // A previous process died between claim and settle.
    let mut task = ka_agent_core::Task::new("tick".into(), "cron", false);
    task.scheduled_at_ms = Some(ka_agent_core::now_ms() - 60_000);
    task.payload = Some(spec("tick", Some(60_000)).to_payload());
    task.status = ka_agent_core::TaskStatus::Running;
    store.insert_task(&task).unwrap();

    let rt = runtime_with(&root, Arc::clone(&store), Arc::new(Notes::default()));
    let crons = rt.list_crons().unwrap();
    assert_eq!(crons.len(), 1);
    assert_eq!(crons[0].status, ka_agent_core::TaskStatus::Interrupted);
    // And it no longer pretends to be claimable by the scheduler.
    assert!(!store.claim_task(&crons[0].id).unwrap());
    let _ = std::fs::remove_dir_all(&root);
}

/// Review F2: absurd recurrence values (hand-crafted payloads — creation is
/// capped by `parse_duration`) must fail visibly, never wrap or spin.
#[tokio::test]
async fn overflowing_recurrence_fails_visibly() {
    let root = root("overflow");
    let store = store_at(&root.join("data"));
    let notes = Arc::new(Notes::default());
    let rt = runtime_with(&root, Arc::clone(&store), Arc::clone(&notes));
    rt.set_no_agent_handler("echo", Arc::new(|arg: &str| Ok(format!("echo[{arg}]"))));

    let now = ka_agent_core::now_ms();
    let mut s = spec("boom", Some(i64::MAX));
    s.mode = CronMode::NoAgent("echo".into());
    let mut task = ka_agent_core::Task::new(s.text.clone(), "cron", false);
    task.scheduled_at_ms = Some(now - 1);
    task.payload = Some(s.to_payload());
    store.insert_task(&task).unwrap();

    rt.fire_due_tasks().await.unwrap();
    let row = store.get_task(&task.id).unwrap().unwrap();
    assert_eq!(row.status, ka_agent_core::TaskStatus::Failed);
    let seen = notes.0.lock().unwrap().join("\n");
    assert!(seen.contains("recurrence overflow"), "surface saw: {seen}");
    let _ = std::fs::remove_dir_all(&root);
}

/// A reminder overdue beyond the grace hour is recorded `missed` and
/// reported — never fired silently hours later (SEED.md §7.2).
#[tokio::test]
async fn reminder_beyond_grace_is_missed() {
    let root = root("reminder-missed");
    let store = store_at(&root.join("data"));
    let notes = Arc::new(Notes::default());
    let rt = runtime_with(&root, Arc::clone(&store), Arc::clone(&notes));

    let mut task = ka_agent_core::Task::new("water the plants".into(), "reminder", false);
    task.scheduled_at_ms = Some(ka_agent_core::now_ms() - 2 * 3_600_000);
    store.insert_task(&task).unwrap();

    rt.fire_due_tasks().await.unwrap();
    let row = store.get_task(&task.id).unwrap().unwrap();
    assert_eq!(row.status, ka_agent_core::TaskStatus::Missed);
    let seen = notes.0.lock().unwrap().join("\n");
    assert!(
        seen.contains("⏰ missed: water the plants"),
        "surface saw: {seen}"
    );
    assert!(seen.contains("overdue by 2h"), "surface saw: {seen}");
    let _ = std::fs::remove_dir_all(&root);
}

/// A reminder within the grace window still fires — with the lateness on
/// the notification instead of hidden.
#[tokio::test]
async fn reminder_within_grace_fires_with_late_marker() {
    let root = root("reminder-late");
    let store = store_at(&root.join("data"));
    let notes = Arc::new(Notes::default());
    let rt = runtime_with(&root, Arc::clone(&store), Arc::clone(&notes));

    let mut task = ka_agent_core::Task::new("stretch".into(), "reminder", false);
    task.scheduled_at_ms = Some(ka_agent_core::now_ms() - 5 * 60_000);
    store.insert_task(&task).unwrap();

    rt.fire_due_tasks().await.unwrap();
    let row = store.get_task(&task.id).unwrap().unwrap();
    assert_eq!(row.status, ka_agent_core::TaskStatus::Succeeded);
    let seen = notes.0.lock().unwrap().join("\n");
    assert!(
        seen.contains("⏰ stretch (late by 5m)"),
        "surface saw: {seen}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A recurring cron whose due time passed while the machine slept skips
/// the missed occupation and re-arms; a one-shot beyond grace is missed.
#[tokio::test]
async fn recurring_beyond_grace_skips_and_rearms_oneshot_is_missed() {
    let root = root("cron-overdue");
    let store = store_at(&root.join("data"));
    let notes = Arc::new(Notes::default());
    let rt = runtime_with(&root, Arc::clone(&store), Arc::clone(&notes));
    rt.set_no_agent_handler("echo", Arc::new(|arg: &str| Ok(format!("echo[{arg}]"))));

    let now = ka_agent_core::now_ms();
    // Recurring, overdue by 2h: no fire, re-armed strictly after now.
    let tick_id = rt
        .add_cron(spec("tick", Some(60_000)), now - 2 * 3_600_000)
        .unwrap();
    // One-shot, overdue by 2h: recorded missed (terminal — off the live set).
    let once_id = rt
        .add_cron(spec("once", None), now - 2 * 3_600_000)
        .unwrap();

    rt.fire_due_tasks().await.unwrap();

    let tick = store.get_task(&tick_id).unwrap().unwrap();
    assert_eq!(tick.status, ka_agent_core::TaskStatus::Queued);
    let next = tick.scheduled_at_ms.unwrap();
    assert!(
        next > now && next <= now + 60_000 + 1_000,
        "re-armed after now: {next}"
    );
    let once = store.get_task(&once_id).unwrap().unwrap();
    assert_eq!(once.status, ka_agent_core::TaskStatus::Missed);
    assert!(
        !store.cron_tasks().unwrap().iter().any(|t| t.id == once_id),
        "a missed one-shot is terminal: off the live set"
    );

    let seen = notes.0.lock().unwrap().join("\n");
    assert!(
        seen.contains("skipped a missed occurrence (overdue by 2h); re-armed"),
        "surface saw: {seen}"
    );
    assert!(seen.contains("'once' missed (overdue by 2h)"), "{seen}");
    assert!(!seen.contains("echo[tick]"), "must not fire: {seen}");
    let _ = std::fs::remove_dir_all(&root);
}
