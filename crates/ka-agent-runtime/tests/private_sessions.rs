//! Private (incognito) sessions: a live scripted turn through a private
//! conversation records usage but no text and no episodes; memory and
//! scheduling are refused; the session's strand, queued text and ephemeral
//! attachments are destroyed when it ends — at `/new` and at boot.

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

/// When the port spawns this binary as its worker, run the real worker
/// loop. The scenario rides the wire in the `open` command (the real
/// worker's path); the KA_DATA_DIR file is only the fallback for spawns
/// without one — a private session redirects the worker's KA_DATA_DIR to
/// its scratch home, so the wire form must win.
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
fn runtime_with(tag: &str, steps: &str) -> (Runtime, Arc<Store>, Arc<Recorder>, PathBuf) {
    let root = std::env::temp_dir().join(format!("ka-private-{tag}-{}", std::process::id()));
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

/// Wait until the turn finished and the session returned to idle.
#[cfg(unix)]
async fn wait_idle(tag: &str, rt: &Runtime, rec: &Recorder) {
    let done = |rec: &Recorder, rt: &Runtime| {
        rec.events()
            .iter()
            .any(|e| matches!(e, AgentEvent::TurnFinished { .. }))
            && rt.session_status() == ka_agent_core::SessionStatus::Idle
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
}

/// A private turn records usage (model, tokens) but no text and never
/// enters the episodic index — `/history` cannot find it.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn private_turn_records_usage_without_text_or_episodes() {
    let (rt, store, rec, root) = runtime_with("turn", r#"{"text": "secret scripted answer"}"#);
    rt.new_session(true).await.unwrap();
    assert!(rt.session_private());

    rt.handle_message("my sensitive question", "console")
        .await
        .unwrap();
    wait_idle("private turn", &rt, &rec).await;

    let turns = store
        .latest_turns_for_session(&rt.session_id(), 10)
        .unwrap();
    assert_eq!(turns.len(), 1, "one usage row for the private turn");
    assert_eq!(
        turns[0].inbound_text, "",
        "the private turn row carries no text"
    );
    assert!(turns[0].tokens > 0, "usage accounting survives");
    assert!(
        store.search_episodes("sensitive", 10).unwrap().is_empty(),
        "no episode for the inbound text"
    );
    assert!(
        store
            .search_episodes("secret scripted", 10)
            .unwrap()
            .is_empty(),
        "no episode for the answer"
    );
    // The outbox is the durable crash-recovery copy of the answer — a
    // private answer must never land in it (delivery rides the
    // FinalChunk events only).
    assert!(
        store.chunks_for_turn(&turns[0].id).unwrap().is_empty(),
        "no outbox chunk carries the private answer"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// The control: a normal session records text and episodes exactly as
/// before the private mode existed.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn normal_turn_still_records_text_and_episodes() {
    let (rt, store, rec, root) = runtime_with("normal", r#"{"text": "public answer"}"#);
    assert!(!rt.session_private());

    rt.handle_message("ordinary question", "console")
        .await
        .unwrap();
    wait_idle("normal turn", &rt, &rec).await;

    let turns = store
        .latest_turns_for_session(&rt.session_id(), 10)
        .unwrap();
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].inbound_text, "ordinary question");
    // Episodes are indexed at the engine's idle delivery boundary, after
    // the TurnFinished event — poll the store until they land.
    let deadline = Instant::now() + Duration::from_secs(10);
    while store.search_episodes("ordinary", 10).unwrap().len() < 1
        || store.search_episodes("public answer", 10).unwrap().len() < 1
    {
        assert!(
            Instant::now() < deadline,
            "episodes never landed: {:#?}",
            rec.events()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let _ = std::fs::remove_dir_all(&root);
}

/// A private conversation refuses memory writes and anything that would
/// outlive it (reminders, crons).
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn private_refuses_memory_and_scheduling() {
    let (rt, _store, _rec, root) = runtime_with("refuse", r#"{"text": "ok"}"#);
    rt.new_session(true).await.unwrap();

    let err = rt
        .add_reminder_at(ka_agent_core::now_ms() + 60_000, "call the bank")
        .unwrap_err()
        .to_string();
    assert!(err.contains("private"), "reminder refused: {err}");

    let err = rt
        .remember("a private fact", "console")
        .unwrap_err()
        .to_string();
    assert!(err.contains("private"), "memory refused: {err}");

    let err = rt.retry_last().await.unwrap_err().to_string();
    assert!(err.contains("private"), "retry refused: {err}");

    let _ = std::fs::remove_dir_all(&root);
}

/// A message steered into a running private reply never lands in the
/// durable turn row (the steer append is the one writer that used to
/// bypass the empty-text rule).
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn steered_text_never_reaches_the_private_turn_row() {
    let (rt, store, rec, root) = runtime_with(
        "steer",
        r#"{ "sleep_ms": 1500 }, { "text": "steered done" }"#,
    );
    rt.new_session(true).await.unwrap();
    rt.handle_message("first private question", "console")
        .await
        .unwrap();
    // Wait until the turn is running, then steer a second message into
    // it while the scripted speaker sleeps.
    let deadline = Instant::now() + Duration::from_secs(10);
    while rt.session_status() != ka_agent_core::SessionStatus::Running {
        assert!(Instant::now() < deadline, "turn never started running");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    rt.handle_message("second private addendum", "console")
        .await
        .unwrap();
    wait_idle("steered turn", &rt, &rec).await;

    let turns = store
        .latest_turns_for_session(&rt.session_id(), 10)
        .unwrap();
    assert!(!turns.is_empty(), "the steered turn was recorded");
    for t in &turns {
        assert!(
            t.inbound_text.is_empty(),
            "no private text in the durable row: {:?}",
            t.inbound_text
        );
    }

    let _ = std::fs::remove_dir_all(&root);
}

/// Ending a private session (any `/new`) destroys its strand file, its
/// queued text and its ephemeral attachment directory.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ending_private_session_destroys_remnants() {
    let (rt, store, rec, root) = runtime_with("end", r#"{"text": "ok"}"#);
    rt.new_session(true).await.unwrap();
    rt.handle_message("hi", "console").await.unwrap();
    wait_idle("warm", &rt, &rec).await;

    // The private conversation's engine state lives in its scratch
    // directory — the real strand lands there, not in the shared home.
    let warm_token = rt.private_session_token().unwrap();
    let scratch = root
        .join("tmp")
        .join(format!("private-engine-{warm_token}"));
    assert!(
        scratch.exists(),
        "the scratch engine home exists during the session"
    );

    // Remnants a crash could leave: a strand file bound in the store, a
    // queued (not yet drained) message and an ephemeral attachment
    // directory.
    let sid = rt.session_id();
    let strand = root.join("strands").join("private.jsonl");
    std::fs::create_dir_all(strand.parent().unwrap()).unwrap();
    std::fs::write(&strand, "{}").unwrap();
    store.set_strand(&sid, "str-private", &strand).unwrap();
    store
        .enqueue(&ka_agent_core::QueueItem::new(
            sid.clone(),
            "queued private text".into(),
            "telegram",
        ))
        .unwrap();
    let token = rt.private_session_token().unwrap();
    let media = root
        .join("ws")
        .join("attachments")
        .join(format!(".private-{token}"));
    std::fs::create_dir_all(&media).unwrap();
    std::fs::write(media.join("photo-x.jpg"), b"x").unwrap();

    rt.new_session(false).await.unwrap();

    assert!(!strand.exists(), "the private strand file is destroyed");
    assert!(
        !scratch.exists(),
        "the scratch engine home (strands, waypoints) is destroyed"
    );
    assert!(!media.exists(), "the ephemeral attachments are destroyed");
    assert!(
        store.next_queued(&sid).unwrap().is_none(),
        "queued private text is purged"
    );
    assert!(!rt.session_private(), "the new session is normal");

    let _ = std::fs::remove_dir_all(&root);
}

/// Boot sweeps remnants of private conversations that ended before the
/// daemon stopped (crash, kill): their strands, queued text and ephemeral
/// attachments cannot outlive the process that knew them. The private
/// session the daemon resumes keeps its context while it stays live.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boot_sweep_destroys_ended_private_remnants() {
    let root = std::env::temp_dir().join(format!("ka-private-boot-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("ws")).unwrap();
    let store = Arc::new(Store::open(&root.join("state.db")).unwrap());

    // A private session that died with the previous process.
    let mut private = ka_agent_core::Session::new(root.join("ws"));
    private.private = true;
    store.insert_session(&private).unwrap();
    let strand = root.join("strands").join("dead.jsonl");
    std::fs::create_dir_all(strand.parent().unwrap()).unwrap();
    std::fs::write(&strand, "{}").unwrap();
    // `set_strand` bumps `updated_ms`, so seed it (and the queue row)
    // before the normal session, which must stay the most recently
    // updated row for the daemon to adopt.
    store.set_strand(&private.id, "str-dead", &strand).unwrap();
    // `ORDER BY updated_ms DESC` has no tiebreaker: guarantee the normal
    // row is strictly newer than `set_strand`'s same-millisecond bump.
    while ka_agent_core::now_ms() <= store.get_session(&private.id).unwrap().updated_ms {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    let normal = ka_agent_core::Session::new(root.join("ws"));
    store.insert_session(&normal).unwrap();
    store
        .enqueue(&ka_agent_core::QueueItem::new(
            private.id.clone(),
            "queued private text".into(),
            "telegram",
        ))
        .unwrap();

    let rec = Arc::new(Recorder::default());
    let cfg = ka_agent_core::DaemonConfig {
        workspace: root.join("ws"),
        ..ka_agent_core::DaemonConfig::default()
    };
    let port = scripted_port(&root, r#"{"text": "ok"}"#);
    let rt = Runtime::new(
        cfg,
        Paths::new(root.clone()),
        Arc::clone(&store),
        port,
        Arc::clone(&rec) as Arc<dyn Surface>,
    )
    .unwrap();

    assert!(!strand.exists(), "the dead private strand is swept at boot");
    assert!(
        store.next_queued(&private.id).unwrap().is_none(),
        "the dead private queued text is swept at boot"
    );
    assert_eq!(rt.session_id(), normal.id, "the normal session resumed");

    let _ = std::fs::remove_dir_all(&root);
}

/// `/status` and the Settings event tell every surface that the live
/// conversation is private (the TUI shows 🕶, the web shows the line).
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_and_settings_report_private() {
    let (rt, _store, rec, root) = runtime_with("status", r#"{"text": "ok"}"#);

    rt.new_session(true).await.unwrap();
    assert!(rt.status_report().unwrap().private);
    assert!(
        rec.events().iter().any(|e| matches!(
            e,
            AgentEvent::Settings {
                private: Some(true),
                ..
            }
        )),
        "surfaces were told the session went private: {:#?}",
        rec.events()
    );

    rt.new_session(false).await.unwrap();
    assert!(!rt.status_report().unwrap().private);
    assert!(
        rec.events().iter().any(|e| matches!(
            e,
            AgentEvent::Settings {
                private: Some(false),
                ..
            }
        )),
        "surfaces were told the session went back to normal"
    );

    let _ = std::fs::remove_dir_all(&root);
}
