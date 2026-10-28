//! End-to-end worker tests: the real `PortSession` ↔ `ka-port-test-worker`
//! pair over NDJSON stdio, each with a fresh isolated environment (own
//! temp root for `HOME`/`KA_DATA_DIR`/`XDG_*` and the workspace).
//!
//! The scripted speaker (`--scenario`) stands in for a model provider, so
//! these run the real ka engine — turn machine, permission gate, bash
//! hand, strand persistence — with zero network.

#![allow(clippy::unwrap_used, clippy::expect_used)]
use std::path::{Path, PathBuf};
use std::time::Duration;

use ka_agent_port::port::{KaPort, OpenRequest, PortEvent, StrandSelector, WorkerEnv, WorkerPort};

/// Per-event timeout: generous enough for CI, loud enough to fail a
/// wedged worker instead of hanging the suite.
const EVENT_TIMEOUT: Duration = Duration::from_secs(30);

struct Env {
    root: PathBuf,
}

impl Env {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!("ka-port-e2e-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for dir in ["home", "ka", "state", "cache", "ws"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        Env { root }
    }

    fn worker_env(&self) -> WorkerEnv {
        WorkerEnv {
            home: self.root.join("home"),
            data_dir: self.root.join("ka"),
            state_dir: self.root.join("state"),
            cache_dir: self.root.join("cache"),
            agent_data_root: self.root.clone(),
            guard_write: Vec::new(),
            guard_read: Vec::new(),
        }
    }

    fn workspace(&self) -> PathBuf {
        self.root.join("ws")
    }

    fn scenario(&self, tag: &str, json: &str) -> PathBuf {
        let path = self.root.join(format!("{tag}.json"));
        std::fs::write(&path, json).unwrap();
        path
    }

    fn open(
        &self,
        strand: StrandSelector,
        model: Option<&str>,
        mode: Option<&str>,
        scenario: Option<PathBuf>,
    ) -> OpenRequest {
        OpenRequest {
            workspace: self.workspace(),
            strand,
            model: model.map(str::to_string),
            mode: mode.map(str::to_string),
            engine_layer_toml: None,
            env: self.worker_env(),
            binary: PathBuf::from(env!("CARGO_BIN_EXE_ka-port-test-worker")),
            worker_args: Vec::new(),
            scenario,
            auto_allow: Vec::new(),
        }
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

async fn open_session(
    req: OpenRequest,
) -> (
    ka_agent_port::PortSession,
    tokio::sync::mpsc::Receiver<PortEvent>,
) {
    let port = WorkerPort {
        binary: PathBuf::new(),
        scenario_override: None,
    };
    let session = port.open(req).await.expect("open session");
    let events = session.take_events();
    (session, events)
}

/// Collect events until `done` (inclusive) or the stream ends. Panics on
/// timeout with everything seen so far.
async fn collect_until(
    rx: &mut tokio::sync::mpsc::Receiver<PortEvent>,
    mut done: impl FnMut(&PortEvent) -> bool,
) -> Vec<PortEvent> {
    let mut out = Vec::new();
    loop {
        match tokio::time::timeout(EVENT_TIMEOUT, rx.recv()).await {
            Ok(Some(ev)) => {
                if std::env::var_os("KA_PORT_TRACE").is_some() {
                    eprintln!("[trace] {ev:?}");
                }
                let stop = done(&ev);
                out.push(ev);
                if stop {
                    return out;
                }
            }
            Ok(None) => return out,
            Err(_) => panic!("timed out waiting for event; saw so far: {out:?}"),
        }
    }
}

async fn collect_until_idle(rx: &mut tokio::sync::mpsc::Receiver<PortEvent>) -> Vec<PortEvent> {
    // An open with an explicit model/mode re-asserts them after `replay`
    // (validation F-1) and each re-assert settles with its own `idle` —
    // skip those bootstrap idles; the delivery boundary is the idle that
    // follows the turn this side submitted.
    let mut turn_started = false;
    collect_until(rx, |ev| {
        if matches!(ev, PortEvent::TurnStarted) {
            turn_started = true;
        }
        matches!(ev, PortEvent::EngineClosed) || (turn_started && matches!(ev, PortEvent::Idle))
    })
    .await
}

fn bootstrap(events: &[PortEvent]) -> &PortEvent {
    events
        .iter()
        .find(|e| matches!(e, PortEvent::Bootstrap { .. }))
        .unwrap_or_else(|| panic!("no Bootstrap in {events:?}"))
}

fn find_strand_file(data_dir: &Path) -> Option<PathBuf> {
    let strands = data_dir.join("strands");
    let mut found = None;
    for entry in std::fs::read_dir(&strands).ok()?.flatten() {
        for file in std::fs::read_dir(entry.path()).ok()?.flatten() {
            if file.path().extension().is_some_and(|e| e == "jsonl") {
                found = Some(file.path());
            }
        }
    }
    found
}

/// 1. Fresh session: Open → Bootstrap (replay 0) → Submit → DeltaText +
///    TurnFinished(done) + Idle; strand id known.
#[tokio::test]
async fn fresh_session_full_turn_lifecycle() {
    let env = Env::new("fresh");
    let scenario = env.scenario(
        "scen",
        r#"{"steps":[{"text":"Hello from scenario"}],"stop":"done","tokens":42}"#,
    );
    let (session, mut rx) =
        open_session(env.open(StrandSelector::New, None, None, Some(scenario.clone()))).await;

    let boot = collect_until(&mut rx, |e| matches!(e, PortEvent::Bootstrap { .. })).await;
    let PortEvent::Bootstrap {
        strand_id,
        strand_path,
        replay_messages,
    } = bootstrap(&boot)
    else {
        unreachable!()
    };
    assert_eq!(
        *replay_messages, 0,
        "fresh strand replays nothing: {boot:?}"
    );
    let strand_id = strand_id.clone().expect("strand id known at bootstrap");
    assert!(strand_id.starts_with('s'), "{strand_id}");
    // a fresh strand materializes only on first append — no file yet
    assert!(strand_path.is_none());

    session
        .submit("hello".to_string(), Vec::new(), Vec::new())
        .await
        .expect("submit");
    let turn = collect_until_idle(&mut rx).await;
    assert!(
        turn.iter()
            .any(|e| matches!(e, PortEvent::DeltaText(t) if t == "Hello from scenario")),
        "{turn:?}"
    );
    let PortEvent::TurnFinished {
        stop,
        cost_usd,
        tokens,
    } = turn
        .iter()
        .find(|e| matches!(e, PortEvent::TurnFinished { .. }))
        .expect("turn finished")
    else {
        unreachable!()
    };
    assert_eq!(stop, "done");
    // 42 scripted input tokens plus the engine's chars-per-token
    // estimate for the 20-char reply (5 at ratio 4.0)
    assert!(*tokens >= 42, "input+output+cache tokens: {tokens}");
    // scenario model priced at $1/Mtok input: 42 input tokens = 42e-6 USD
    assert!((cost_usd - 42.0 / 1_000_000.0).abs() < 1e-12, "{cost_usd}");

    assert_eq!(session.strand_id().as_deref(), Some(strand_id.as_str()));
    session.close().await.expect("close");
    assert!(session.is_closed());
}

/// 2. Resume: close, re-open with `StrandSelector::Path` on the same
///    `KA_DATA_DIR` → Bootstrap with replay ≥ 1 and the strand path.
#[tokio::test]
async fn resume_replays_prior_history() {
    let env = Env::new("resume");
    let scenario = env.scenario(
        "scen",
        r#"{"steps":[{"text":"first reply"}],"stop":"done","tokens":5}"#,
    );
    let (first, mut rx) =
        open_session(env.open(StrandSelector::New, None, None, Some(scenario.clone()))).await;
    collect_until(&mut rx, |e| matches!(e, PortEvent::Bootstrap { .. })).await;
    first
        .submit("first prompt".to_string(), Vec::new(), Vec::new())
        .await
        .unwrap();
    collect_until_idle(&mut rx).await;
    first.close().await.unwrap();

    let strand = find_strand_file(&env.root.join("ka")).expect("strand file on disk after a turn");
    let (second, mut rx2) = open_session(env.open(
        StrandSelector::Path(strand.clone()),
        None,
        None,
        Some(scenario),
    ))
    .await;
    let boot = collect_until(&mut rx2, |e| matches!(e, PortEvent::Bootstrap { .. })).await;
    let PortEvent::Bootstrap {
        strand_id,
        strand_path,
        replay_messages,
    } = bootstrap(&boot)
    else {
        unreachable!()
    };
    assert!(
        *replay_messages >= 1,
        "resumed strand replays the prior exchange: {boot:?}"
    );
    assert_eq!(strand_id.as_deref(), first.strand_id().as_deref());
    assert_eq!(
        strand_path.as_deref(),
        Some(strand.to_string_lossy().into_owned().as_str()),
        "resolved strand path is announced"
    );
    second.close().await.unwrap();
}

/// 3. Ask flow: a non-read-only bash command in ask mode asks; deny →
///    the turn continues and finishes. Opened with the old `guarded`
///    spelling on purpose — aliases still parse at the port.
#[tokio::test]
async fn guarded_bash_asks_and_deny_completes_turn() {
    let env = Env::new("ask");
    let scenario = env.scenario(
        "scen",
        r#"{"steps":[{"call":{"tool":"bash","args":{"command":"touch x"}}}]}"#,
    );
    let (session, mut rx) =
        open_session(env.open(StrandSelector::New, None, Some("guarded"), Some(scenario))).await;
    collect_until(&mut rx, |e| matches!(e, PortEvent::Bootstrap { .. })).await;

    session
        .submit("run it".to_string(), Vec::new(), Vec::new())
        .await
        .unwrap();
    let mut events = Vec::new();
    let mut asks = 0usize;
    let mut turn_started = false; // skip the re-assert bootstrap idles
    loop {
        let Some(ev) = tokio::time::timeout(EVENT_TIMEOUT, rx.recv())
            .await
            .expect("event within timeout")
        else {
            break;
        };
        match &ev {
            PortEvent::Ask {
                ask_id, options, ..
            } => {
                asks += 1;
                let deny = options
                    .iter()
                    .position(|o| o == "deny")
                    .unwrap_or_else(|| panic!("no deny option in {options:?}"));
                session.answer(ask_id.clone(), deny).await.unwrap();
            }
            PortEvent::TurnStarted => turn_started = true,
            PortEvent::EngineClosed => {
                events.push(ev);
                break;
            }
            PortEvent::Idle if turn_started => {
                events.push(ev);
                break;
            }
            _ => {}
        }
        events.push(ev);
    }
    assert_eq!(asks, 1, "exactly one permission ask: {events:?}");
    let PortEvent::TurnFinished { stop, .. } = events
        .iter()
        .find(|e| matches!(e, PortEvent::TurnFinished { .. }))
        .expect("turn finished after deny")
    else {
        unreachable!()
    };
    assert_eq!(
        stop, "done",
        "denied call feeds back and the turn completes: {events:?}"
    );
    assert!(
        !env.workspace().join("x").exists(),
        "the denied command never ran"
    );
    session.close().await.unwrap();
}

/// 4. Abort: two scripted calls; abort after the first `CallStarted` →
///    `TurnFinished(aborted)` + `Idle` promptly.
#[tokio::test]
async fn abort_mid_turn_finishes_aborted() {
    let env = Env::new("abort");
    let scenario = env.scenario(
        "scen",
        r#"{"steps":[
             {"call":{"tool":"bash","args":{"command":"sleep 1 && echo one"}}},
             {"call":{"tool":"bash","args":{"command":"echo two"}}},
             {"text":"after"}
           ]}"#,
    );
    // free mode: the bash call runs ungated, and `sleep 1` gives the
    // abort a deterministic window to land while the tool is live
    let (session, mut rx) =
        open_session(env.open(StrandSelector::New, None, Some("free"), Some(scenario))).await;
    collect_until(&mut rx, |e| matches!(e, PortEvent::Bootstrap { .. })).await;

    session
        .submit("do things".to_string(), Vec::new(), Vec::new())
        .await
        .unwrap();
    let mut events = Vec::new();
    let mut aborted = false;
    let mut turn_started = false; // skip the re-assert bootstrap idles
    loop {
        let Some(ev) = tokio::time::timeout(EVENT_TIMEOUT, rx.recv())
            .await
            .expect("event within timeout")
        else {
            break;
        };
        if matches!(&ev, PortEvent::CallStarted { tool, .. } if tool == "bash" && !aborted) {
            session.abort().await.unwrap();
            aborted = true;
        }
        if matches!(ev, PortEvent::TurnStarted) {
            turn_started = true;
        }
        let stop = matches!(ev, PortEvent::EngineClosed)
            || (turn_started && matches!(ev, PortEvent::Idle));
        events.push(ev);
        if stop {
            break;
        }
    }
    assert!(aborted, "a bash call started before abort: {events:?}");
    let PortEvent::TurnFinished { stop, .. } = events
        .iter()
        .find(|e| matches!(e, PortEvent::TurnFinished { .. }))
        .expect("turn finished after abort")
    else {
        unreachable!()
    };
    assert_eq!(stop, "aborted", "{events:?}");
    assert!(
        events.iter().any(|e| matches!(e, PortEvent::Idle)),
        "{events:?}"
    );
    session.close().await.unwrap();
}

/// 5. Engine death: `fail {class: auth}` → `EngineError(auth)` +
///    `TurnFinished(error)` + `Idle`.
#[tokio::test]
async fn auth_failure_surfaces_error_then_error_stop() {
    let env = Env::new("authfail");
    let scenario = env.scenario(
        "scen",
        r#"{"steps":[{"fail":{"class":"auth","retryable":false,"message":"invalid api key"}}]}"#,
    );
    let (session, mut rx) =
        open_session(env.open(StrandSelector::New, None, None, Some(scenario.clone()))).await;
    collect_until(&mut rx, |e| matches!(e, PortEvent::Bootstrap { .. })).await;

    session
        .submit("hello".to_string(), Vec::new(), Vec::new())
        .await
        .unwrap();
    let events = collect_until_idle(&mut rx).await;
    assert!(
        events.iter().any(|e| matches!(
            e,
            PortEvent::EngineError { class, retryable: false, message }
                if class == "auth" && message.contains("invalid api key")
        )),
        "{events:?}"
    );
    let PortEvent::TurnFinished { stop, .. } = events
        .iter()
        .find(|e| matches!(e, PortEvent::TurnFinished { .. }))
        .expect("turn finished")
    else {
        unreachable!()
    };
    assert_eq!(stop, "error", "{events:?}");
    session.close().await.unwrap();
}

/// 6. Worker kill: `kill()` → the event stream yields `EngineClosed`;
///    further submits error.
#[tokio::test]
async fn kill_closes_session_and_rejects_sends() {
    let env = Env::new("kill");
    let scenario = env.scenario("scen", r#"{"steps":[{"text":"hi"}]}"#);
    let (session, mut rx) =
        open_session(env.open(StrandSelector::New, None, None, Some(scenario.clone()))).await;
    collect_until(&mut rx, |e| matches!(e, PortEvent::Bootstrap { .. })).await;

    session.kill().await;
    let events = collect_until(&mut rx, |e| matches!(e, PortEvent::EngineClosed)).await;
    assert!(
        events.iter().any(|e| matches!(e, PortEvent::EngineClosed)),
        "{events:?}"
    );
    let err = session
        .submit("too late".to_string(), Vec::new(), Vec::new())
        .await
        .expect_err("submit after kill must fail");
    assert!(
        matches!(err, ka_agent_port::PortError::EngineClosed),
        "{err:?}"
    );
    assert!(session.is_closed());
}

/// 7. Policy re-assert: open with model+mode set; the worker sends
///    `set_model`/`set_mode` after `replay` (validation F-1) — observed
///    as the two resulting `idle` events at bootstrap and the worker's
///    stderr notes.
#[tokio::test]
async fn policy_reassert_after_replay() {
    let env = Env::new("reassert");
    let scenario = env.scenario("scen", r#"{"steps":[{"text":"ok"}]}"#);
    let (session, mut rx) = open_session(env.open(
        StrandSelector::New,
        Some("scen/m"),
        Some("plan"),
        Some(scenario.clone()),
    ))
    .await;

    // bootstrap: Bootstrap first, then the two Idles produced by the
    // re-asserted set_model/set_mode commands (nothing else idles before
    // a submit)
    let mut idles_after_bootstrap = 0usize;
    let mut saw_bootstrap = false;
    let events = collect_until(&mut rx, |e| match e {
        PortEvent::Bootstrap { .. } => {
            saw_bootstrap = true;
            false
        }
        PortEvent::Idle => {
            if saw_bootstrap {
                idles_after_bootstrap += 1;
            }
            saw_bootstrap && idles_after_bootstrap >= 2
        }
        _ => false,
    })
    .await;
    assert!(saw_bootstrap, "{events:?}");
    assert_eq!(
        idles_after_bootstrap, 2,
        "set_model and set_mode each settle with an idle: {events:?}"
    );

    // the worker logged the re-assertion on stderr (human diagnostics)
    let stderr = session.stderr_tail().join("\n");
    assert!(
        stderr.contains("set_model \"scen/m\" re-asserted after replay"),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("set_mode plan re-asserted after replay"),
        "stderr: {stderr}"
    );

    // and the Change records persisted: ka materializes the strand file
    // on first append, and `set_model`/`set_mode` append Change records
    // (F-1) — read the strand itself for the ground truth, then reopen
    // by path to prove it resumes cleanly
    session.close().await.unwrap();
    let strand = find_strand_file(&env.root.join("ka")).expect("strand persisted");
    let strand_text = std::fs::read_to_string(&strand).unwrap();
    assert!(
        strand_text.contains("scen/m"),
        "Change record carries the model: {strand_text}"
    );
    assert!(
        strand_text.contains("plan"),
        "Change record carries the mode: {strand_text}"
    );
    let (second, mut rx2) = open_session(env.open(
        StrandSelector::Path(strand.clone()),
        Some("scen/m"),
        Some("plan"),
        Some(scenario),
    ))
    .await;
    let boot = collect_until(&mut rx2, |e| matches!(e, PortEvent::Bootstrap { .. })).await;
    let PortEvent::Bootstrap { strand_path, .. } = bootstrap(&boot) else {
        unreachable!()
    };
    assert_eq!(
        strand_path.as_deref(),
        Some(strand.to_string_lossy().into_owned().as_str()),
        "reopened by path: {boot:?}"
    );
    second.close().await.unwrap();
}
