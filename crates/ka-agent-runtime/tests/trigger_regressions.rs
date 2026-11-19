//! C2/B14 trigger regressions (ADR-0005): lineage sessions, per-trigger
//! identity, error-after-N, the in-flight cap, and the preflight gate.
//!
//! The worker is this test binary on a scripted scenario (scout.rs style):
//! `stop = "error"` scenarios make every fire fail deterministically, so
//! the streak and history contracts are observable without a real engine.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ka_agent_core::{DaemonConfig, TriggerKind, TriggerSpec};
use ka_agent_port::WorkerPort;
use ka_agent_runtime::event::AgentEvent;
use ka_agent_runtime::surface::Surface;
use ka_agent_runtime::{Envelope, Paths, Runtime};
use ka_agent_store::Store;
use ka_agent_store::UserRow;

/// Not a test in the ordinary sense: when the port spawns this binary as
/// its worker, libtest's filter `__worker` runs the real worker loop on
/// the scenario file the parent wrote (see `tests/scout.rs`).
#[test]
#[allow(non_snake_case)]
fn mock__worker() {
    if !std::env::args().any(|a| a == "__worker") {
        return;
    }
    // The gate worker's KA_DATA_DIR is a scratch home, so the scenario
    // must come from the `--scenario` flag the port passes (the real
    // worker reads the same flag); fall back for direct spawns.
    let scenario = std::env::args()
        .position(|a| a == "--scenario")
        .and_then(|i| std::env::args().nth(i + 1))
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("KA_DATA_DIR").map(|dir| PathBuf::from(dir).join("scenario.json"))
        });
    let Some(scenario) = scenario.filter(|p| p.exists()) else {
        return;
    };
    let _ = ka_agent_port::run_worker(ka_agent_port::WorkerRun {
        scenario: Some(scenario),
    });
}

/// Everything the runtime told the surface, in order.
#[derive(Default)]
struct Recorder(Mutex<Vec<String>>);

impl Recorder {
    fn lines(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

impl Surface for Recorder {
    fn emit(&self, env: &Envelope) {
        match &env.ev {
            AgentEvent::Inbound { source, text } => {
                self.0
                    .lock()
                    .unwrap()
                    .push(format!("inbound {source}: {text}"));
            }
            AgentEvent::Notice { text } => self.0.lock().unwrap().push(format!("notice: {text}")),
            AgentEvent::Error { class, message, .. } => {
                self.0
                    .lock()
                    .unwrap()
                    .push(format!("error[{class}]: {message}"));
            }
            _ => {}
        }
    }
}

fn fresh_root(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("ka-trigger-{tag}-{}", std::process::id()));
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

/// A port that spawns this test binary as the worker on a scripted
/// scenario whose final stop word is configurable (`"error"` = every
/// fire fails).
#[cfg(unix)]
fn scenario_port(root: &Path, steps: &str, stop: &str) -> WorkerPort {
    let paths = Paths::new(root.to_path_buf());
    let scenario = paths.ka_data().join("scenario.json");
    std::fs::create_dir_all(scenario.parent().unwrap()).unwrap();
    std::fs::write(
        &scenario,
        format!(r#"{{"steps": [{steps}], "stop": "{stop}", "cost_usd": 0.01, "tokens": 25}}"#),
    )
    .unwrap();
    let wrapper = root.join("worker.sh");
    write_executable(
        &wrapper,
        &format!(
            // The wrapper drops the port's `--scenario` flag on purpose:
            // libtest rejects unknown options, so the child's argv must be
            // just the `__worker` filter; the mock reads the scenario from
            // `KA_DATA_DIR` (scout.rs does the same).
            "#!/bin/sh\nexec '{}' __worker\n",
            std::env::current_exe().unwrap().display()
        ),
    );
    WorkerPort {
        binary: wrapper,
        scenario_override: Some(scenario),
    }
}

fn user(username: &str, role: &str) -> UserRow {
    UserRow {
        username: username.to_string(),
        display_name: username.to_string(),
        pw_hash: None,
        role: role.to_string(),
        quota_daily_usd: None,
        disabled: false,
        created_ms: ka_agent_core::now_ms(),
        created_by: None,
    }
}

fn op() -> ka_agent_core::Principal {
    ka_agent_core::Principal::operator()
}

/// A runtime on a temp root with the scripted worker and the two accounts
/// the identity tests need. `stop` decides what every fire's engine says.
#[cfg(unix)]
fn trigger_runtime(
    tag: &str,
    stop: &str,
    roles: &[ka_agent_core::config::RoleSpec],
) -> (
    Runtime,
    Arc<Store>,
    Arc<Recorder>,
    PathBuf,
    ka_agent_core::TaskId,
) {
    let root = fresh_root(tag);
    let port = scenario_port(&root, r#"{"text": "triaged"}"#, stop);
    let store = Arc::new(Store::open(&root.join("state.db")).unwrap());
    store
        .ensure_user(&user(ka_agent_core::OPERATOR_USER, "admin"))
        .unwrap();
    store.ensure_user(&user("member", "member")).unwrap();
    let rec = Arc::new(Recorder::default());
    let cfg = DaemonConfig {
        workspace: root.join("ws"),
        sandbox: "off".into(),
        roles: roles.to_vec(),
        ..DaemonConfig::default()
    };
    let rt = Runtime::new(
        cfg,
        Paths::new(root.clone()),
        Arc::clone(&store),
        port,
        Arc::clone(&rec) as Arc<dyn Surface>,
    )
    .unwrap();
    (rt, store, rec, root, ka_agent_core::TaskId::generate())
}

fn member_role(name: &str, allow: &[&str]) -> ka_agent_core::config::RoleSpec {
    ka_agent_core::config::RoleSpec {
        name: name.to_string(),
        mode: "readonly".into(),
        allow: allow.iter().map(|t| t.to_string()).collect(),
        ..ka_agent_core::config::RoleSpec::default()
    }
}

fn webhook_spec(hook: &str, preflight: bool) -> TriggerSpec {
    TriggerSpec {
        kind: TriggerKind::Webhook,
        text: "triage the CI event".into(),
        source: "operator".into(),
        session_id: String::new(),
        role: None,
        username: None,
        tools: Vec::new(),
        error_streak: 0,
        recurrence_ms: None,
        at_local: None,
        weekdays: Vec::new(),
        month_day: None,
        gate: None,
        hook_path: Some(hook.into()),
        secret: None,
        filter: None,
        max_body_bytes: None,
        preflight,
    }
}

/// Advance an armed/parked trigger to due now (the scheduler sweep only
/// fires what is due).
fn make_due(store: &Store, id: &ka_agent_core::TaskId) {
    store
        .update_task_schedule(id, Some(ka_agent_core::now_ms() - 1), "queued")
        .unwrap();
}

async fn wait_until(tag: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {tag}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn trigger_runs(store: &Store, id: &str) -> Vec<ka_agent_store::RunRow> {
    store.runs_for(id, 50).unwrap()
}

/// A webhook fire submits into the trigger's own lineage session — the
/// creator's live conversation shows no inbound and no turn.
#[cfg(unix)]
#[tokio::test]
async fn trigger_fires_into_its_own_lineage_session() {
    let (rt, store, rec, _root, _seed) =
        trigger_runtime("lineage", "done", &[member_role("member", &["read"])]);
    // The creator's LIVE conversation, captured before the trigger's
    // lineage session exists (afterward `latest_session_for_user` returns
    // the newer lineage row).
    let live_sid = store
        .latest_session_for_user(ka_agent_core::OPERATOR_USER)
        .unwrap()
        .unwrap()
        .id;
    let task_id = rt
        .add_trigger(webhook_spec("ci/ping", false), None, &op())
        .unwrap();
    let task = store.get_task(&task_id).unwrap().unwrap();
    let spec = TriggerSpec::from_payload(task.payload.as_deref().unwrap()).unwrap();
    let lineage = ka_agent_core::SessionId(spec.session_id.clone());

    rt.arm_trigger(&task, &spec, r#"{"status":"passed"}"#)
        .unwrap();
    rt.fire_due_tasks().await.unwrap();
    wait_until("lineage turn", || {
        store.count_turns_for_session(&lineage).unwrap_or(0) == 1
    })
    .await;

    // The lineage session holds exactly one turn carrying the fenced
    // payload; the creator's live session holds none, and no Inbound
    // event ever reached a surface.
    let turns = store.latest_turns_for_session(&lineage, 5, 0).unwrap();
    assert_eq!(turns.len(), 1);
    assert!(turns[0].inbound_text.contains("<event-payload>"));
    assert!(turns[0].inbound_text.contains("status"));
    assert_ne!(live_sid, lineage, "lineage is never the live chat");
    assert_eq!(
        store.count_turns_for_session(&live_sid).unwrap(),
        0,
        "no inbound/turn on the creator's live conversation"
    );
    assert!(
        !rec.lines().iter().any(|l| l.starts_with("inbound")),
        "no inbound on human surfaces: {:?}",
        rec.lines()
    );
    // The fire is recorded, and the webhook trigger parked for the next
    // delivery.
    wait_until("run row", || {
        !trigger_runs(&store, task_id.as_str()).is_empty()
    })
    .await;
    assert_eq!(trigger_runs(&store, task_id.as_str())[0].kind, "trigger");
    let parked = store.get_task(&task_id).unwrap().unwrap();
    assert_eq!(parked.status, ka_agent_core::TaskStatus::Queued);
    assert!(parked.scheduled_at_ms.is_none());
}

/// The member principal: the turn records the member (via the lineage
/// session), and demoting the member re-clamps at fire — the resolved
/// role comes from the user row, never the stale snapshot.
#[cfg(unix)]
#[tokio::test]
async fn webhook_trigger_runs_as_creator_role_not_admin() {
    let (rt, store, _rec, _root, _seed) = trigger_runtime(
        "creator-role",
        "done",
        &[
            member_role("member", &["read", "grep"]),
            member_role("viewer", &["read"]),
        ],
    );
    let member = ka_agent_core::Principal::new("member", "member");
    let task_id = rt
        .add_trigger(webhook_spec("ci/role", false), None, &member)
        .unwrap();
    let task = store.get_task(&task_id).unwrap().unwrap();
    let spec = TriggerSpec::from_payload(task.payload.as_deref().unwrap()).unwrap();
    let lineage = ka_agent_core::SessionId(spec.session_id.clone());

    rt.arm_trigger(&task, &spec, "{}").unwrap();
    rt.fire_due_tasks().await.unwrap();
    wait_until("member turn", || {
        store.count_turns_for_session(&lineage).unwrap_or(0) == 1
    })
    .await;
    let turn = store
        .latest_turns_for_session(&lineage, 1, 0)
        .unwrap()
        .remove(0);
    // The recorded principal is the member's, not the operator/admin's.
    assert_eq!(
        store.turn_username(&turn.id).unwrap().as_deref(),
        Some("member")
    );
    assert_eq!(
        store.turn_role(&turn.id).unwrap().as_deref(),
        Some("member")
    );

    // Demotion: the fire-time role re-resolves from the user row and the
    // clamp narrows with it (the exact two calls the runner makes).
    store.set_user_role("member", "viewer").unwrap();
    let fire_principal = rt.trigger_fire_principal(&spec);
    assert_eq!(fire_principal.role, "viewer");
    let narrowed = rt.clamp_cron_tools(
        &fire_principal.role,
        &["read".to_string(), "grep".to_string(), "bash".to_string()],
    );
    assert_eq!(narrowed, vec!["read".to_string()]);
}

/// Three consecutive failed fires pause the trigger (`interrupted`) with
/// exactly one Error notice; run history keeps every fire; the cron
/// resume path re-queues it.
#[cfg(unix)]
#[tokio::test]
async fn trigger_errors_after_n_consecutive_failures_and_keeps_history() {
    let (rt, store, rec, _root, _seed) =
        trigger_runtime("streak", "error", &[member_role("member", &["read"])]);
    let mut recurring = webhook_spec("ci/streak", false);
    recurring.kind = TriggerKind::Schedule;
    recurring.recurrence_ms = Some(60_000);
    recurring.hook_path = None;
    let task_id = rt
        .add_trigger(recurring, Some(ka_agent_core::now_ms() - 1), &op())
        .unwrap();

    for round in 1..=3 {
        make_due(&store, &task_id);
        rt.fire_due_tasks().await.unwrap();
        wait_until(&format!("failed fire {round}"), || {
            trigger_runs(&store, task_id.as_str()).len() >= round
        })
        .await;
    }
    // The pause lands right after the third run row (the runner records
    // the fire, then settles) — wait for it explicitly.
    wait_until("third strike pauses", || {
        store.get_task(&task_id).unwrap().unwrap().status == ka_agent_core::TaskStatus::Interrupted
    })
    .await;
    let paused = store.get_task(&task_id).unwrap().unwrap();
    assert_eq!(paused.status, ka_agent_core::TaskStatus::Interrupted);
    let strikes = trigger_runs(&store, task_id.as_str()).len();
    assert_eq!(strikes, 3, "one run row per fire");
    let lines = rec.lines();
    let errors: Vec<_> = lines
        .iter()
        .filter(|l| l.contains("paused after 3 consecutive failed fires"))
        .collect();
    assert_eq!(
        errors.len(),
        1,
        "exactly one Error notice: {:?}",
        rec.lines()
    );

    // Resume (the cron resume path — the payload falls to its
    // reschedule-at-now branch) and fire again: it works, and history
    // keeps growing.
    let task = store.get_task(&task_id).unwrap().unwrap();
    let now = jiff::Timestamp::now().to_zoned(jiff::tz::TimeZone::UTC);
    ka_agent_runtime::cron_ops::resume(&store, &task, &now).unwrap();
    let resumed = store.get_task(&task_id).unwrap().unwrap();
    assert_eq!(resumed.status, ka_agent_core::TaskStatus::Queued);
    make_due(&store, &task_id);
    rt.fire_due_tasks().await.unwrap();
    wait_until("fourth fire", || {
        trigger_runs(&store, task_id.as_str()).len() >= 4
    })
    .await;
    assert!(
        trigger_runs(&store, task_id.as_str()).len() >= 4,
        "history kept"
    );
}

/// Per-trigger in-flight cap 1: a fire while the lineage session holds an
/// open turn records `skipped`, does not stack, and leaves the queued
/// event durable for the next arm.
#[cfg(unix)]
#[tokio::test]
async fn trigger_skips_when_previous_run_still_in_flight() {
    let (rt, store, _rec, _root, _seed) =
        trigger_runtime("in-flight", "done", &[member_role("member", &["read"])]);
    let task_id = rt
        .add_trigger(webhook_spec("ci/busy", false), None, &op())
        .unwrap();
    let task = store.get_task(&task_id).unwrap().unwrap();
    let spec = TriggerSpec::from_payload(task.payload.as_deref().unwrap()).unwrap();
    let lineage = ka_agent_core::SessionId(spec.session_id.clone());

    // A turn still open on the lineage session (a wedged previous fire).
    let mut open = ka_agent_core::Turn::new(lineage.clone(), "previous fire".into());
    open.transition(ka_agent_core::TurnStatus::Submitted)
        .unwrap();
    open.transition(ka_agent_core::TurnStatus::Running).unwrap();
    store.insert_turn(&open).unwrap();

    rt.arm_trigger(&task, &spec, r#"{"status":"started"}"#)
        .unwrap();
    let fired = rt.fire_due_tasks().await.unwrap();
    assert!(fired.is_empty(), "nothing fired past the cap: {fired:?}");
    wait_until("skip row", || {
        trigger_runs(&store, task_id.as_str())
            .iter()
            .any(|r| r.status == "skipped")
    })
    .await;
    let skipped = trigger_runs(&store, task_id.as_str());
    assert!(
        skipped[0]
            .error_message
            .as_deref()
            .unwrap_or_default()
            .contains("previous run still in flight")
    );
    // The event was not stacked away: it stays queued for the next arm.
    assert_eq!(store.queue_len(&lineage).unwrap(), 1);
    assert_eq!(store.count_turns_for_session(&lineage).unwrap(), 1);
}

/// The preflight gate answers [SKIP]: no lineage turn, no tokens on the
/// trigger's run row, and the gate's own spend lands on its row.
#[cfg(unix)]
#[tokio::test]
async fn preflight_skip_does_not_spend_an_agent_turn() {
    let root = fresh_root("preflight-skip");
    let port = scenario_port(&root, r#"{"text": "[SKIP] marketing spam"}"#, "done");
    let store = Arc::new(Store::open(&root.join("state.db")).unwrap());
    store
        .ensure_user(&user(ka_agent_core::OPERATOR_USER, "admin"))
        .unwrap();
    let rec = Arc::new(Recorder::default());
    let cfg = DaemonConfig {
        workspace: root.join("ws"),
        sandbox: "off".into(),
        ..DaemonConfig::default()
    };
    let rt = Runtime::new(
        cfg,
        Paths::new(root.clone()),
        Arc::clone(&store),
        port,
        Arc::clone(&rec) as Arc<dyn Surface>,
    )
    .unwrap();

    let mut spec = webhook_spec("ci/gated", true);
    let task_id = rt.add_trigger(spec.clone(), None, &op()).unwrap();
    let task = store.get_task(&task_id).unwrap().unwrap();
    spec = TriggerSpec::from_payload(task.payload.as_deref().unwrap()).unwrap();
    let lineage = ka_agent_core::SessionId(spec.session_id.clone());

    rt.arm_trigger(&task, &spec, r#"{"subject":"buy now"}"#)
        .unwrap();
    rt.fire_due_tasks().await.unwrap();
    wait_until("preflight skip row", || {
        trigger_runs(&store, task_id.as_str()).iter().any(|r| {
            r.status == "skipped"
                && r.error_message
                    .as_deref()
                    .unwrap_or_default()
                    .contains("preflight")
        })
    })
    .await;

    // Zero agent spend: no lineage turn at all, and the trigger's own run
    // row records no tokens. The gate's spend is visible on its row.
    assert_eq!(store.count_turns_for_session(&lineage).unwrap(), 0);
    let rows = trigger_runs(&store, task_id.as_str());
    let skip = rows
        .iter()
        .find(|r| r.kind == "trigger")
        .expect("trigger run row");
    assert!(skip.tokens.is_none(), "no tokens on the trigger row");
    let gate = rows
        .iter()
        .find(|r| r.kind == "preflight")
        .expect("preflight run row");
    // The gate's own spend is recorded (the scripted speaker's usage).
    assert!(gate.tokens.is_some_and(|t| t > 0), "gate tokens: {gate:?}");
    // The scratch engine-home is gone and the pool slot released.
    assert!(!root.join("tmp").read_dir().unwrap().next().is_some());
    wait_until("pool released", || rt.live_worker_count() == 0).await;
}
