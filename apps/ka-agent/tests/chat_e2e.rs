//! Full-stack e2e: the real `ka-agent` binary, its runtime, store, port,
//! worker subprocess and scripted engine speaker, driven through the console
//! chat surface (SEED.md §13 tasks 4–8 in miniature).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

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
    spawn_chat_cfg(root, scenario, "")
}

/// Same, with extra `ka-agent.toml` lines appended (feature tests).
fn spawn_chat_cfg(root: &std::path::Path, scenario: &std::path::Path, extra: &str) -> Chat {
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let cfg = root.join("ka-agent.toml");
    std::fs::write(
        &cfg,
        format!(
            "workspace = {:?}\nmode = \"guarded\"\napproval_ttl_secs = 300\nturn_grace_secs = 5\n{}",
            ws.display().to_string(),
            extra
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

    /// Everything the surface has printed so far (for failure messages).
    fn transcript(&self) -> String {
        self.seen.borrow().clone()
    }

    /// Wait until the output so far contains any of `needles`; returns the
    /// one that matched. Used where the engine can legitimately take either
    /// path (a live ask vs. an ask that arrives after the cycle reported
    /// idle), so the test asserts each path instead of racing.
    fn expect_any(&mut self, needles: &[&'static str], timeout: Duration) -> &'static str {
        let deadline = Instant::now() + timeout;
        loop {
            while let Ok(line) = self.rx.try_recv() {
                self.seen.borrow_mut().push_str(&line);
                self.seen.borrow_mut().push('\n');
            }
            let hit = {
                let seen = self.seen.borrow();
                needles.iter().find(|n| seen.contains(**n)).copied()
            };
            if let Some(n) = hit {
                return n;
            }
            if Instant::now() >= deadline {
                panic!(
                    "timed out waiting for any of {needles:?}; output so far:\n{}",
                    self.seen.borrow()
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Wait until the whole output so far contains `needle`.
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
}

fn scenario_file(root: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
    let p = root.join(name);
    std::fs::write(&p, body).unwrap();
    p
}

fn temp_root(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("ka-agent-e2e-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn simple_turn_and_status() {
    let root = temp_root("simple");
    let sc = scenario_file(
        &root,
        "simple.json",
        r#"{ "steps": [ {"text": "Hello from scripted"} ], "stop": "done", "cost_usd": 0.0001, "tokens": 42 }"#,
    );
    let mut chat = spawn_chat(&root, &sc);
    chat.send("hi there");
    chat.expect("Hello from scripted", Duration::from_secs(30));
    chat.expect("turn done", Duration::from_secs(10));
    chat.send("/status");
    chat.expect("budget: $", Duration::from_secs(10));
    chat.expect("tokens · crashes:", Duration::from_secs(10));
    chat.send("/quit");
    std::fs::remove_dir_all(&root).ok();
}

/// Idle reap (minimal-footprint contract): with `worker_idle_timeout_secs`
/// set low, a settled session loses its worker after the timeout — noted
/// as a deliberate stop, not a crash — and the next message restarts it
/// (strand resumed by path) with zero crash bookkeeping.
#[test]
fn idle_worker_is_stopped_then_restarted_on_demand() {
    let root = temp_root("idle-reap");
    let sc = scenario_file(
        &root,
        "reap.json",
        r#"{ "steps": [ {"text": "first-alive"}, {"text": "second-alive"} ], "stop": "done", "cost_usd": 0.0, "tokens": 1 }"#,
    );
    let mut chat = spawn_chat_cfg(&root, &sc, "worker_idle_timeout_secs = 2\n");
    chat.expect("commands:", Duration::from_secs(10));
    chat.send("first message");
    chat.expect("first-alive", Duration::from_secs(30));
    chat.expect("turn done", Duration::from_secs(10));
    // Settled and idle: the maintenance loop reaps the worker within its
    // 2 s timeout plus one loop interval, and books it as deliberate.
    chat.expect("worker stopped", Duration::from_secs(20));
    chat.expect("session Idle", Duration::from_secs(5));

    // The next message works exactly as before: a fresh worker on the
    // resumed strand, with zero crash bookkeeping.
    chat.send("second message");
    chat.expect("second-alive", Duration::from_secs(30));
    chat.expect("turn done", Duration::from_secs(10));
    chat.send("/status");
    chat.expect("tokens · crashes: 0", Duration::from_secs(10));
    chat.send("/quit");
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn approval_flow_and_queue() {
    let root = temp_root("approval");
    let sc = scenario_file(
        &root,
        "ask.json",
        r#"{
            "steps": [
                {"call": {"tool": "bash", "args": {"command": "touch /tmp/ka-agent-e2e-should-not-exist"}}},
                {"text": "done anyway"}
            ],
            "stop": "done", "cost_usd": 0.0, "tokens": 10
        }"#,
    );
    let mut chat = spawn_chat(&root, &sc);
    chat.send("please run the thing");
    // The ask surfaces as a card while its turn is live. ka can also report
    // the cycle idle before its permission gate opens (the scripted speaker
    // finishes its response first), and then the runtime denies the ask
    // fail-closed rather than leave an approval no surface can settle.
    // Assert whichever happened: both are contract-correct.
    match chat.expect_any(&["approve:", "ask auto-denied"], Duration::from_secs(60)) {
        "approve:" => {
            // While the approval is pending, a second message must queue (gap G4).
            chat.send("and then summarize");
            chat.expect("queued (#1)", Duration::from_secs(30));
            chat.send("/y");
            chat.expect("done anyway", Duration::from_secs(60));
            // The queued message runs next and gets its own scripted answer.
            chat.expect("turn done", Duration::from_secs(60));
        }
        _ => {
            chat.expect("already finished", Duration::from_secs(10));
        }
    }
    let _ = std::fs::remove_file("/tmp/ka-agent-e2e-should-not-exist");
    chat.send("/quit");
}

#[test]
fn message_queues_while_a_turn_is_running() {
    let root = temp_root("queue-busy");
    // A slow response keeps the turn in flight long enough to observe it, so
    // the queue path (gap G4) is exercised without racing the idle signal.
    // The test quits as soon as the queue note lands, so the length of the
    // scripted pause costs nothing when the assertion passes.
    let sc = scenario_file(
        &root,
        "slow.json",
        r#"{
            "steps": [
                {"text": "thinking"},
                {"sleep_ms": 30000},
                {"text": "finished"}
            ],
            "stop": "done", "cost_usd": 0.0, "tokens": 5
        }"#,
    );
    let data = root.join("data");
    let mut chat = spawn_chat(&root, &sc);
    chat.send("first");
    // Wait for the durable state instead of guessing at wall-clock: the turn
    // is persisted `running` once the engine has accepted the prompt, which is
    // also when the chat loop can read the next line.
    if !wait_turn_running(&data, Duration::from_secs(30)) {
        panic!(
            "turn never reached `running`; output so far:\n{}",
            chat.transcript()
        );
    }
    chat.send("second");
    // The invariant is the queue, not the turn's ending: assert it while the
    // turn is still running and quit (the pending sleep is abandoned).
    chat.expect("queued (#1)", Duration::from_secs(20));
    chat.send("/quit");
}

/// True once the newest turn is persisted as `running` (the engine accepted
/// the prompt, so the chat loop can read the next line). Polls with a fresh
/// read-only connection: the instance owns the database, and a read-write
/// handle from the test would block its start-up.
fn wait_turn_running(data: &std::path::Path, timeout: Duration) -> bool {
    use rusqlite::OpenFlags;
    let path = data.join("state.db");
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let running =
            rusqlite::Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
                .ok()
                .and_then(|c| {
                    c.query_row(
                        "SELECT status FROM turns ORDER BY created_ms DESC, id DESC LIMIT 1",
                        [],
                        |r| r.get::<_, String>(0),
                    )
                    .ok()
                })
                .is_some_and(|status| status == "running");
        if running {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

/// `YYYYMMDDTHHMMSSZ` for `t` (UTC), for ICS fixtures.
fn ics_stamp(t: i64) -> String {
    let secs = t.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    format!("{year:04}{month:02}{d:02}T{h:02}{m:02}{s:02}Z")
}

/// One `inbound_text` row of the newest matching turn, read-only (the
/// instance owns the database).
fn newest_inbound_containing(data: &std::path::Path, needle: &str, timeout: Duration) -> bool {
    use rusqlite::OpenFlags;
    let path = data.join("state.db");
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let hit = rusqlite::Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .ok()
            .and_then(|c| {
                c.query_row(
                    "SELECT COUNT(*) FROM turns WHERE inbound_text LIKE ?1",
                    [format!("%{needle}%")],
                    |r| r.get::<_, i64>(0),
                )
                .ok()
            })
            .is_some_and(|n| n > 0);
        if hit {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// M11 cron e2e: a one-shot job fires through the real scheduler and the
/// engine answers; `/cron list` shows the recurring job; `/cron rm` removes
/// it. Also covers the AGENTS.md seed (created once, never overwritten).
#[test]
fn cron_end_to_end_and_agents_md_seeded_once() {
    let root = temp_root("cron-e2e");
    let sc = scenario_file(
        &root,
        "cron.json",
        r#"{ "steps": [ {"text": "pong"} ], "stop": "done", "cost_usd": 0.0, "tokens": 1 }"#,
    );
    let mut chat = spawn_chat(&root, &sc);

    // AGENTS.md is seeded during Runtime::new; wait for boot to finish.
    let agents_md = root.join("ws/AGENTS.md");
    let deadline = Instant::now() + Duration::from_secs(15);
    while !agents_md.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    let seeded = std::fs::read_to_string(&agents_md).unwrap_or_default();
    assert!(seeded.contains("# Ka Agent"), "{seeded}");
    assert!(seeded.contains("/cron"), "{seeded}");
    // The operator takes ownership of the file.
    std::fs::write(&agents_md, "OPERATOR-MARKER-XYZ\n").unwrap();

    chat.send("/cron add every 1h tick-tick");
    chat.expect("cron ", Duration::from_secs(10)); // "cron <id> scheduled"
    chat.send("/cron add in 3s say ping");
    chat.expect("cron ", Duration::from_secs(10));
    // The scheduler ticks every 15 s: pong lands well within 30 s.
    chat.expect("pong", Duration::from_secs(30));
    chat.expect("turn done", Duration::from_secs(30));

    chat.send("/cron list");
    chat.expect("every 1h", Duration::from_secs(10));
    // The one-shot is done; only the recurring job stays live.
    let data = root.join("data");
    let recurring_id = cron_id_from_store(&data);
    chat.send(&format!("/cron rm {recurring_id}"));
    chat.expect("cron removed", Duration::from_secs(10));
    chat.send("/cron list");
    chat.expect("no crons", Duration::from_secs(10));
    chat.send("/quit");

    // Second boot over the same root: the operator's AGENTS.md survives.
    let sc2 = scenario_file(
        &root,
        "cron2.json",
        r#"{ "steps": [ {"text": "x"} ], "stop": "done", "cost_usd": 0.0, "tokens": 1 }"#,
    );
    let mut chat2 = spawn_chat(&root, &sc2);
    chat2.expect("ka-agent chat", Duration::from_secs(15));
    assert_eq!(
        std::fs::read_to_string(&agents_md).unwrap_or_default(),
        "OPERATOR-MARKER-XYZ\n",
        "a second boot must not overwrite the operator's AGENTS.md"
    );
    chat2.send("/quit");
    std::fs::remove_dir_all(&root).ok();
}

/// First live cron id prefix from the store (read-only).
fn cron_id_from_store(data: &std::path::Path) -> String {
    use rusqlite::OpenFlags;
    rusqlite::Connection::open_with_flags(
        data.join("state.db"),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap()
    .query_row(
        "SELECT substr(id,1,8) FROM tasks WHERE kind='cron' AND status IN ('queued','running','interrupted') LIMIT 1",
        [],
        |r| r.get::<_, String>(0),
    )
    .unwrap()
}

/// M11 @refs e2e: `@nota.md` injects the file content into the persisted
/// turn; a missing reference is visible, not dropped.
#[test]
fn refs_inject_workspace_file_into_turn() {
    let root = temp_root("refs-e2e");
    let sc = scenario_file(
        &root,
        "refs.json",
        r#"{ "steps": [ {"text": "ack-1"}, {"text": "ack-2"} ], "stop": "done", "cost_usd": 0.0, "tokens": 1 }"#,
    );
    std::fs::create_dir_all(root.join("ws")).unwrap();
    std::fs::write(root.join("ws/nota.md"), "CONTENIDO-UNICO-XYZ").unwrap();
    let mut chat = spawn_chat(&root, &sc);
    chat.send("resume @nota.md");
    chat.expect("ack-1", Duration::from_secs(30));
    chat.expect("turn done", Duration::from_secs(30));
    chat.send("check @noexiste.md");
    chat.expect("ack-2", Duration::from_secs(30));

    let data = root.join("data");
    assert!(
        newest_inbound_containing(&data, "CONTENIDO-UNICO-XYZ", Duration::from_secs(10)),
        "expansion must be persisted in turns.inbound_text"
    );
    assert!(
        newest_inbound_containing(&data, "--- @nota.md ---", Duration::from_secs(10)),
        "the ref block marker must be persisted"
    );
    assert!(
        newest_inbound_containing(
            &data,
            "[ref @noexiste.md: not found]",
            Duration::from_secs(10)
        ),
        "a missing ref must be visibly recorded"
    );
    chat.send("/quit");
    std::fs::remove_dir_all(&root).ok();
}

/// M11 calendar e2e: `/calendar add` registers the daily no-agent digest;
/// `/calendar today` renders the fixture on demand, including the
/// RRULE-not-expanded note.
#[test]
fn calendar_digest_on_demand() {
    let root = temp_root("cal-e2e");
    let sc = scenario_file(
        &root,
        "cal.json",
        r#"{ "steps": [ {"text": "unused"} ], "stop": "done", "cost_usd": 0.0, "tokens": 1 }"#,
    );
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    std::fs::create_dir_all(root.join("ws")).unwrap();
    let ics_path = root.join("ws/cal.ics");
    std::fs::write(
        &ics_path,
        format!(
            "BEGIN:VCALENDAR\nBEGIN:VEVENT\nSUMMARY:CTest-XYZ\nDTSTART:{}\nEND:VEVENT\nEND:VCALENDAR\n",
            ics_stamp(now_ms + 2 * 3_600_000)
        ),
    )
    .unwrap();
    let mut chat = spawn_chat(&root, &sc);
    chat.send(&format!("/calendar add {}", ics_path.display()));
    chat.expect(
        "calendar digest daily at 08:00 UTC",
        Duration::from_secs(10),
    );
    chat.send("/calendar today");
    chat.expect("CTest-XYZ", Duration::from_secs(30));

    // Same source, now with a recurring event: counted, not expanded.
    std::fs::write(
        &ics_path,
        format!(
            "BEGIN:VCALENDAR\nBEGIN:VEVENT\nSUMMARY:CTest-RRULE\nDTSTART:{}\nRRULE:FREQ=DAILY\nEND:VEVENT\nEND:VCALENDAR\n",
            ics_stamp(now_ms + 3 * 3_600_000)
        ),
    )
    .unwrap();
    chat.send("/calendar today");
    chat.expect("CTest-RRULE", Duration::from_secs(30));
    chat.expect("1 recurring event(s) not expanded", Duration::from_secs(10));
    chat.send("/quit");
    std::fs::remove_dir_all(&root).ok();
}

/// Skills (SEED.md §9.1): enable provisions into the engine home, `/skills`
/// lists it, `/skill <name> <text>` submits a turn through the worker
/// protocol with that skill attached, and an unknown skill is refused
/// before any engine work.
#[test]
fn skills_enable_list_invoke() {
    let root = temp_root("skills");
    let sc = scenario_file(
        &root,
        "skills.json",
        r#"{ "steps": [ {"text": "skill-ack"} ], "stop": "done", "cost_usd": 0.0, "tokens": 1 }"#,
    );

    // The operator's skill library lives in the config dir (XDG-scoped).
    let cfg_home = root.join("cfg-home");
    let skill_dir = cfg_home.join("ka-agent/skills/review");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname = \"review\"\ndescription = \"review a diff carefully\"\n---\n\nRead the diff twice.\n",
    )
    .unwrap();

    // CLI: enable against the same data root.
    let out = Command::new(env!("CARGO_BIN_EXE_ka-agent"))
        .args(["skills", "enable", "review"])
        .env("KA_AGENT_DATA_DIR", root.join("data"))
        .env("XDG_CONFIG_HOME", &cfg_home)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let provisioned = root.join("data/engine-home/.config/ka/skills/review/SKILL.md");
    assert!(provisioned.is_file(), "skill provisioned into engine home");

    // Chat: list, invoke, and a fail-closed unknown-skill case.
    let mut chat = spawn_chat(&root, &sc);
    chat.expect("commands:", Duration::from_secs(10));
    chat.send("/skills");
    chat.expect("review — review a diff carefully", Duration::from_secs(10));
    chat.send("/skill ghost do something");
    chat.expect(
        "! [skill] skill \"ghost\" is not enabled",
        Duration::from_secs(10),
    );
    chat.send("/skill review check the attached diff");
    chat.expect("skill-ack", Duration::from_secs(30));
    chat.expect("turn done", Duration::from_secs(10));
    chat.send("/quit");
    std::fs::remove_dir_all(&root).ok();
}
