//! F12 plans e2e: the editable plan artifact and its daemon-side
//! verify/repair cycle, through the real `ka-agent` binary with a
//! scripted engine (chat_e2e style). Pre-seeded plans ride
//! `Store::open` (migrations) before the daemon boots; boot adopts the
//! newest operator session (or creates one), and plan ids resolve per
//! user, so seeded rows are reachable from `/plan` directly.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use ka_agent_core::{OPERATOR_USER, SessionId};
use ka_agent_store::{PlanRow, PlanStatus, Store};

struct Chat {
    child: Child,
    rx: Receiver<String>,
    seen: std::cell::RefCell<String>,
}

impl Drop for Chat {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_chat(root: &std::path::Path, scenario: &std::path::Path) -> Chat {
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let cfg = root.join("ka-agent.toml");
    std::fs::write(
        &cfg,
        format!(
            "workspace = {:?}\nmode = \"guarded\"\napproval_ttl_secs = 300\nturn_grace_secs = 5\n",
            ws.display().to_string()
        ),
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_ka-agent"))
        .arg("chat")
        .arg("--config")
        .arg(&cfg)
        .arg("--scenario")
        .arg(scenario)
        .env("KA_AGENT_DATA_DIR", root.join("data"))
        .env_remove("KA_MODEL")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let rx = {
        let out = child.stdout.take().unwrap();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        rx
    };
    Chat {
        child,
        rx,
        seen: std::cell::RefCell::new(String::new()),
    }
}

impl Chat {
    fn send(&mut self, line: &str) {
        let stdin = self.child.stdin.as_mut().unwrap();
        stdin.write_all(format!("{line}\n").as_bytes()).unwrap();
        stdin.flush().unwrap();
    }

    fn transcript(&self) -> String {
        self.seen.borrow().clone()
    }

    /// Wait until the output so far contains `needle`.
    fn expect(&mut self, needle: &str, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            while let Ok(line) = self.rx.try_recv() {
                self.seen.borrow_mut().push_str(&line);
                self.seen.borrow_mut().push('\n');
            }
            if self.seen.borrow().contains(needle) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!(
            "timed out waiting for {needle:?}; output so far:\n{}",
            self.seen.borrow()
        );
    }

    /// Wait until output appended after `from` contains `needle`
    /// (scenario steps replay, so plain `contains` can match stale
    /// turn-1 output).
    fn expect_from(&mut self, needle: &str, from: usize, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            while let Ok(line) = self.rx.try_recv() {
                self.seen.borrow_mut().push_str(&line);
                self.seen.borrow_mut().push('\n');
            }
            if self.seen.borrow().len() > from && self.seen.borrow()[from..].contains(needle) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!(
            "timed out waiting for fresh {needle:?}; output so far:\n{}",
            self.seen.borrow()
        );
    }

    fn mark(&self) -> usize {
        self.seen.borrow().len()
    }
}

fn temp_root(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("ka-agent-plans-e2e-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn scenario_file(root: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
    let p = root.join(name);
    std::fs::write(&p, body).unwrap();
    p
}

/// Seed the store (runs all migrations) with one plan row, then drop
/// the handle: a read-write connection held by the test would block the
/// daemon's start-up.
fn seed_plan(root: &std::path::Path, row: &PlanRow) {
    let store = Store::open(&root.join("data/state.db")).unwrap();
    store.insert_plan(row).unwrap();
}

fn plan_row(id: &str, status: PlanStatus, verify: Option<&str>, max_iterations: i64) -> PlanRow {
    PlanRow {
        id: id.to_string(),
        session: SessionId("sess_e2e".into()),
        username: OPERATOR_USER.into(),
        title: "e2e plan".into(),
        body: "1. do the thing".into(),
        verify: verify.map(str::to_string),
        max_iterations,
        iterations: 0,
        status,
        draft_turn: None,
        created_ms: ka_agent_core::now_ms(),
        updated_ms: ka_agent_core::now_ms(),
    }
}

/// One scalar read from the instance's database, read-only (a
/// read-write handle from the test would fight the daemon's lock).
fn db_scalar(root: &std::path::Path, sql: &str) -> i64 {
    use rusqlite::OpenFlags;
    let conn = rusqlite::Connection::open_with_flags(
        root.join("data/state.db"),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    conn.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap()
}

/// One text column read (None when the row is missing).
fn db_text(root: &std::path::Path, sql: &str) -> Option<String> {
    use rusqlite::OpenFlags;
    let conn = rusqlite::Connection::open_with_flags(
        root.join("data/state.db"),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    conn.query_row(sql, [], |r| r.get::<_, Option<String>>(0))
        .ok()
        .flatten()
}

fn plan_status(root: &std::path::Path, id: &str) -> Option<String> {
    db_text(root, &format!("SELECT status FROM plans WHERE id = '{id}'"))
}

fn quit(chat: &mut Chat) {
    chat.send("/quit");
}

/// 1: `/plan draft` captures the cycle answer as the body, lifts the
/// `Verify:` line, and flips `drafting -> draft`.
#[test]
fn plan_draft_captures_the_answer_as_a_draft_artifact() {
    let root = temp_root("draft");
    let sc = scenario_file(
        &root,
        "draft.sc",
        r#"{ "steps": [
            {"text": "Plan:\n1. migrate posts\n2. redirect links\nVerify: `cargo test -q`"}
        ], "stop": "done", "cost_usd": 0.0, "tokens": 1 }"#,
    );
    let mut chat = spawn_chat(&root, &sc);
    chat.expect("local session", Duration::from_secs(60));
    let mark = chat.mark();
    chat.send("/plan draft migrate the blog");
    chat.expect(
        "drafting — its reply below becomes the draft",
        Duration::from_secs(30),
    );
    chat.expect("captured as a draft", Duration::from_secs(30));
    let id = db_text(
        &root,
        "SELECT id FROM plans WHERE username = 'operator' \
         ORDER BY created_ms DESC, id DESC LIMIT 1",
    )
    .unwrap();
    chat.send(&format!("/plan show {id}"));
    chat.expect_from("unarmed until /plan approve", mark, Duration::from_secs(10));
    chat.expect_from("1. migrate posts", mark, Duration::from_secs(10));
    let status = plan_status(&root, &id).unwrap();
    assert_eq!(status, "draft");
    let verify = db_text(
        &root,
        &format!("SELECT verify FROM plans WHERE id = '{id}'"),
    )
    .unwrap();
    assert_eq!(verify, "cargo test -q");
    let body = db_text(&root, &format!("SELECT body FROM plans WHERE id = '{id}'")).unwrap();
    assert!(body.contains("1. migrate posts"), "{body}");
    assert!(
        !body.contains("Verify:"),
        "the verify line must lift out: {body}"
    );
    quit(&mut chat);
    std::fs::remove_dir_all(&root).ok();
}

/// 2a: the capture appends when the model prefilled the body mid-turn
/// (`plan_update` splits the engine requests; the cycle text then
/// appends instead of overwriting), and `Verify: none` records no gate.
#[test]
fn plan_draft_capture_appends_to_a_prefilled_body() {
    let root = temp_root("append");
    let sc = scenario_file(
        &root,
        "append.sc",
        r#"{ "steps": [
            {"call": {"tool": "kagent__plan_update", "args": {"body": "preface from the tool"}}},
            {"text": "1. write it down\nVerify: none"}
        ], "stop": "done", "cost_usd": 0.0, "tokens": 1 }"#,
    );
    let mut chat = spawn_chat(&root, &sc);
    chat.expect("local session", Duration::from_secs(60));
    chat.send("/plan draft something");
    chat.expect("captured as a draft", Duration::from_secs(30));
    let id = db_text(
        &root,
        "SELECT id FROM plans WHERE username = 'operator' \
         ORDER BY created_ms DESC, id DESC LIMIT 1",
    )
    .unwrap();
    assert_eq!(plan_status(&root, &id).as_deref(), Some("draft"));
    assert_eq!(
        db_text(&root, &format!("SELECT body FROM plans WHERE id = '{id}'")).as_deref(),
        Some("preface from the tool\n\n1. write it down"),
        "the cycle text appends, it never overwrites a prefilled body"
    );
    assert_eq!(
        db_text(
            &root,
            &format!("SELECT verify FROM plans WHERE id = '{id}'")
        ),
        None,
        "Verify: none records no gate"
    );
    quit(&mut chat);
    std::fs::remove_dir_all(&root).ok();
}

/// Poll a read-only scalar until it is non-zero (events the chat
/// surface does not render — e.g. give-up outbox rows — are asserted
/// through their durable rows).
fn wait_db_scalar(root: &std::path::Path, sql: &str, timeout: Duration) -> i64 {
    let deadline = Instant::now() + timeout;
    loop {
        let v = db_scalar(root, sql);
        if v != 0 || Instant::now() >= deadline {
            return v;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// 2b: an empty draft answer nudges once; the still-empty re-ask gives
/// up (its notice lands in the durable outbox — the chat surface does
/// not render FinalChunks) and the plan stays `drafting` — nothing was
/// ever captured.
#[test]
fn plan_draft_nudge_reask_stays_drafting_when_still_empty() {
    let root = temp_root("nudge");
    let sc = scenario_file(
        &root,
        "nudge.sc",
        r#"{ "steps": [], "stop": "done", "cost_usd": 0.0, "tokens": 1 }"#,
    );
    let mut chat = spawn_chat(&root, &sc);
    chat.expect("local session", Duration::from_secs(60));
    chat.send("/plan draft something");
    chat.expect(
        "empty reply from the model — asking again",
        Duration::from_secs(30),
    );
    let giveups = wait_db_scalar(
        &root,
        "SELECT COUNT(*) FROM outbox WHERE text LIKE '%returned an empty answer twice%'",
        Duration::from_secs(30),
    );
    assert!(giveups >= 1, "the re-ask give-up must reach the outbox");
    let id = db_text(
        &root,
        "SELECT id FROM plans WHERE username = 'operator' \
         ORDER BY created_ms DESC, id DESC LIMIT 1",
    )
    .unwrap();
    assert_eq!(plan_status(&root, &id).as_deref(), Some("drafting"));
    assert_eq!(
        db_text(&root, &format!("SELECT body FROM plans WHERE id = '{id}'")).as_deref(),
        Some(""),
        "an empty cycle never captures"
    );
    quit(&mut chat);
    std::fs::remove_dir_all(&root).ok();
}

/// 3: an approved plan with a passing gate settles `done`, and every
/// gate execution leaves a `plan.verify` audit row.
#[test]
fn verify_gate_pass_marks_the_plan_done() {
    let root = temp_root("pass");
    let sc = scenario_file(
        &root,
        "pass.sc",
        r#"{ "steps": [ {"text": "did the thing"} ], "stop": "done", "cost_usd": 0.0, "tokens": 1 }"#,
    );
    seed_plan(
        &root,
        &plan_row("plan_e2e_pass", PlanStatus::Approved, Some("true"), 3),
    );
    let mut chat = spawn_chat(&root, &sc);
    chat.expect("local session", Duration::from_secs(60));
    chat.send("/plan run plan_e2e_pass");
    chat.expect("executing — repairs 0/3", Duration::from_secs(30));
    chat.expect("done — verify passed", Duration::from_secs(30));
    assert_eq!(plan_status(&root, "plan_e2e_pass").as_deref(), Some("done"));
    let audits = db_scalar(
        &root,
        "SELECT COUNT(*) FROM audit WHERE action = 'plan.verify'",
    );
    assert!(audits >= 1, "gate execution must be audited");
    assert_eq!(
        db_scalar(
            &root,
            "SELECT iterations FROM plans WHERE id = 'plan_e2e_pass'"
        ),
        0
    );
    quit(&mut chat);
    std::fs::remove_dir_all(&root).ok();
}

/// 4: a failing gate repairs up to the cap (2), then settles `failed`;
/// the queue is empty and each gate run is audited with its output
/// re-fed inside the `<gate-output>` envelope.
#[test]
fn verify_gate_failure_repairs_then_caps() {
    let root = temp_root("cap");
    let sc = scenario_file(
        &root,
        "cap.sc",
        r#"{ "steps": [
            {"text": "try one"}
        ], "stop": "done", "cost_usd": 0.0, "tokens": 1 }"#,
    );
    seed_plan(
        &root,
        &plan_row("plan_e2e_cap", PlanStatus::Approved, Some("false"), 2),
    );
    let mut chat = spawn_chat(&root, &sc);
    chat.expect("local session", Duration::from_secs(60));
    chat.send("/plan run plan_e2e_cap");
    chat.expect("repair 1/2 queued", Duration::from_secs(60));
    chat.expect("repair 2/2 queued", Duration::from_secs(60));
    chat.expect("failed after 2 repair", Duration::from_secs(60));
    assert_eq!(
        plan_status(&root, "plan_e2e_cap").as_deref(),
        Some("failed")
    );
    assert_eq!(
        db_scalar(
            &root,
            "SELECT iterations FROM plans WHERE id = 'plan_e2e_cap'"
        ),
        2
    );
    // Every repair turn consumed its queued item; nothing is left.
    let pending = db_scalar(&root, "SELECT COUNT(*) FROM queue WHERE turned IS NULL");
    assert_eq!(pending, 0, "no repair may stay queued at the cap");
    // Three gate executions: the run plus two repairs, each audited.
    let audits = db_scalar(
        &root,
        "SELECT COUNT(*) FROM audit WHERE action = 'plan.verify'",
    );
    assert_eq!(audits, 3);
    // Each repair actually re-fed the gate output to the model.
    let repairs = db_scalar(
        &root,
        "SELECT COUNT(*) FROM turns WHERE inbound_text LIKE '%<gate-output>%'",
    );
    assert_eq!(repairs, 2, "repairs carry the <gate-output> envelope");
    quit(&mut chat);
    std::fs::remove_dir_all(&root).ok();
}

/// 5: `/plan stop` while a repair is queued behind an ordinary message
/// removes the queued repair — it never becomes a turn (the repair
/// notices never reach `repair 2/3`, and nothing is left queued).
#[test]
fn plan_stop_removes_the_queued_repair() {
    let root = temp_root("stop");
    let sc = scenario_file(
        &root,
        "stop.sc",
        r#"{ "steps": [
            {"sleep_ms": 600},
            {"text": "ran the plan"},
            {"sleep_ms": 4000},
            {"text": "hello answer"}
        ], "stop": "done", "cost_usd": 0.0, "tokens": 1 }"#,
    );
    seed_plan(
        &root,
        &plan_row("plan_e2e_stop", PlanStatus::Approved, Some("false"), 3),
    );
    let mut chat = spawn_chat(&root, &sc);
    chat.expect("local session", Duration::from_secs(60));
    chat.send("/plan run plan_e2e_stop");
    chat.expect("executing — repairs 0/3", Duration::from_secs(30));
    // Arrives mid-run-turn: queues ahead of the repair (FIFO).
    chat.send("hello there");
    chat.expect("repair 1/3 queued", Duration::from_secs(60));
    // The drain started the ordinary message's turn; the repair waits
    // behind it. Drop the plan (and its queued repair) mid-turn.
    chat.send("/plan stop plan_e2e_stop");
    chat.expect("abandoned — queued repair removed", Duration::from_secs(30));
    chat.expect("hello answer", Duration::from_secs(30));
    // Give the post-hello drain a beat, then assert the repair never
    // ran: no second gate, no second repair, nothing left queued.
    std::thread::sleep(Duration::from_secs(2));
    assert!(
        !chat.transcript().contains("repair 2/3"),
        "the queued repair must not become a turn:\n{}",
        chat.transcript()
    );
    assert_eq!(
        db_scalar(&root, "SELECT COUNT(*) FROM queue WHERE turned IS NULL"),
        0,
        "the queued repair must be gone"
    );
    assert_eq!(
        plan_status(&root, "plan_e2e_stop").as_deref(),
        Some("abandoned")
    );
    quit(&mut chat);
    std::fs::remove_dir_all(&root).ok();
}

/// 6: the model's `plan_update` is refused on an approved plan — the
/// body stays frozen even when the model names the id explicitly.
#[test]
fn plan_update_tool_refuses_approved_plans() {
    let root = temp_root("frozen");
    let sc = scenario_file(
        &root,
        "frozen.sc",
        r#"{ "steps": [
            {"call": {"tool": "kagent__plan_update", "args": {"id": "plan_e2e_frozen", "body": "hacked"}}},
            {"text": "the plan is frozen"}
        ], "stop": "done", "cost_usd": 0.0, "tokens": 1 }"#,
    );
    seed_plan(
        &root,
        &plan_row("plan_e2e_frozen", PlanStatus::Approved, None, 3),
    );
    let mut chat = spawn_chat(&root, &sc);
    chat.expect("local session", Duration::from_secs(60));
    chat.send("try to rewrite the plan");
    chat.expect("the plan is frozen", Duration::from_secs(60));
    let body = db_text(&root, "SELECT body FROM plans WHERE id = 'plan_e2e_frozen'").unwrap();
    assert_eq!(body, "1. do the thing", "an approved body must not change");
    assert_eq!(
        plan_status(&root, "plan_e2e_frozen").as_deref(),
        Some("approved")
    );
    quit(&mut chat);
    std::fs::remove_dir_all(&root).ok();
}

/// 7: boot finding `executing` with an empty queue resumes nothing.
#[test]
fn executing_plan_survives_restart_without_auto_run() {
    let root = temp_root("restart");
    let sc = scenario_file(
        &root,
        "restart.sc",
        r#"{ "steps": [ {"text": "SHOULD NEVER RUN"} ], "stop": "done", "cost_usd": 0.0, "tokens": 1 }"#,
    );
    seed_plan(
        &root,
        &plan_row("plan_e2e_boot", PlanStatus::Executing, Some("false"), 3),
    );
    let mut chat = spawn_chat(&root, &sc);
    chat.expect("local session", Duration::from_secs(60));
    // No turn is submitted at boot; the scripted answer never appears.
    std::thread::sleep(Duration::from_secs(3));
    assert!(
        !chat.transcript().contains("SHOULD NEVER RUN"),
        "boot must not auto-run an executing plan:\n{}",
        chat.transcript()
    );
    chat.send("/plan show plan_e2e_boot");
    chat.expect("[executing]", Duration::from_secs(10));
    chat.expect("repairs: 0/3", Duration::from_secs(10));
    quit(&mut chat);
    std::fs::remove_dir_all(&root).ok();
}
