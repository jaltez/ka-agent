//! A2 + A3 e2e over the real `ka-agent` binary driven through the console
//! chat surface: the queue manager (`/queue top|edit|now` — reorder, edit,
//! abort-and-jump) and the bare interrupt keywords (`stop`, … abort the
//! live cycle and become the next turn; `interrupts = "off"` restores the
//! plain steer).

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

/// Spawn the chat binary with `extra` config lines (feature tests append
/// `busy_messages` / `interrupts` here).
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

fn scenario_file(root: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
    let p = root.join(name);
    std::fs::write(&p, body).unwrap();
    p
}

fn temp_root(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("ka-agent-queue-e2e-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A scripted turn that stays live until aborted: one answer, then a long
/// pause the test never waits out.
fn long_turn(root: &std::path::Path, name: &str) -> std::path::PathBuf {
    scenario_file(
        root,
        name,
        r#"{
            "steps": [
                {"text": "working"},
                {"sleep_ms": 30000},
                {"text": "finished"}
            ],
            "stop": "done", "cost_usd": 0.0, "tokens": 1
        }"#,
    )
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

/// Status of the newest turn whose stored inbound text is exactly `text`
/// (`None` while no such turn exists). Targets one message, so a newer
/// cycle (the promoted item) can never shadow it.
fn turn_status_for(data: &std::path::Path, text: &str) -> Option<String> {
    use rusqlite::OpenFlags;
    rusqlite::Connection::open_with_flags(data.join("state.db"), OpenFlags::SQLITE_OPEN_READ_ONLY)
        .ok()
        .and_then(|c| {
            c.query_row(
                "SELECT status FROM turns WHERE inbound_text = ?1
             ORDER BY created_ms DESC, id DESC LIMIT 1",
                [text],
                |r| r.get::<_, String>(0),
            )
            .ok()
        })
}

/// Wait until `f` holds (bounded).
fn wait_until(f: impl Fn() -> bool, timeout: Duration, what: &str, transcript: &str) {
    let deadline = Instant::now() + timeout;
    while !f() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}; output so far:\n{transcript}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A2: with a turn running and three messages queued, `/queue top 3` and
/// `/queue edit` reorder and rewrite the list; `/queue now 2` aborts the
/// running turn (store: `aborted`) and the promoted, edited item is the
/// cycle submitted next; the final `/queue` render shows the new head.
#[test]
fn queue_edit_reorder_and_jump() {
    let root = temp_root("reorder");
    let sc = long_turn(&root, "long.json");
    let data = root.join("data");
    let mut chat = spawn_chat_cfg(&root, &sc, "busy_messages = \"queue\"\n");
    chat.expect("local session", Duration::from_secs(60));
    chat.send("alpha");
    if !wait_turn_running(&data, Duration::from_secs(30)) {
        panic!("turn never reached running:\n{}", chat.transcript());
    }
    chat.send("beta");
    chat.expect("queued (#1)", Duration::from_secs(20));
    chat.send("gamma");
    chat.expect("queued (#2)", Duration::from_secs(20));
    chat.send("delta");
    chat.expect("queued (#3)", Duration::from_secs(20));

    // Reorder: delta from #3 to the head.
    chat.send("/queue top 3");
    chat.expect("· delta", Duration::from_secs(20));

    // Edit the item that will be promoted (now #2).
    chat.send("/queue edit 2 edited-beta");
    chat.expect("edited", Duration::from_secs(20));

    // Jump: `/queue now 2` promotes the edited item and aborts the live
    // turn — the engine never sees a mid-cycle prompt.
    chat.send("/queue now 2");
    chat.expect(
        "promoted — starting now: edited-beta",
        Duration::from_secs(20),
    );
    wait_until(
        || turn_status_for(&data, "alpha").as_deref() == Some("aborted"),
        Duration::from_secs(60),
        "the running turn to abort",
        &chat.transcript(),
    );
    // The promoted, edited item is the cycle submitted next.
    wait_until(
        || turn_status_for(&data, "edited-beta").is_some_and(|s| s == "running" || s == "done"),
        Duration::from_secs(60),
        "the edited item to become the next cycle",
        &chat.transcript(),
    );
    // The final render: the head was consumed by the drain, the rest shows.
    chat.send("/queue");
    chat.expect("2 queued", Duration::from_secs(20));
    chat.expect("· delta", Duration::from_secs(20));
    chat.expect("· gamma", Duration::from_secs(20));
    chat.send("/quit");
}

/// A3 (default `busy_messages = "steer"`): a bare `stop` aborts the live
/// cycle, toasts the ⏹ notice (never the steer note), and the word itself
/// becomes the next submitted cycle.
#[test]
fn plain_stop_aborts_and_becomes_next_turn() {
    let root = temp_root("stop-keyword");
    let sc = long_turn(&root, "long.json");
    let data = root.join("data");
    let mut chat = spawn_chat_cfg(&root, &sc, "");
    chat.expect("local session", Duration::from_secs(60));
    chat.send("long question");
    if !wait_turn_running(&data, Duration::from_secs(30)) {
        panic!("turn never reached running:\n{}", chat.transcript());
    }
    chat.send("stop");
    chat.expect("⏹ stopped — your message is next", Duration::from_secs(20));
    assert!(
        !chat.transcript().contains("steered into the running reply"),
        "stop must not steer:\n{}",
        chat.transcript()
    );
    wait_until(
        || turn_status_for(&data, "long question").as_deref() == Some("aborted"),
        Duration::from_secs(60),
        "the running turn to abort",
        &chat.transcript(),
    );
    // The next cycle's inbound text is the interrupt word itself.
    wait_until(
        || turn_status_for(&data, "stop").is_some_and(|s| s == "running" || s == "done"),
        Duration::from_secs(60),
        "stop to become the next cycle",
        &chat.transcript(),
    );
    chat.send("/quit");
}

/// A3: medium words and longer messages are ordinary traffic — `no` and
/// `wait for the green build` steer exactly as before, the turn survives.
#[test]
fn medium_and_long_messages_do_not_interrupt() {
    let root = temp_root("no-interrupt");
    let sc = long_turn(&root, "long.json");
    let data = root.join("data");
    let mut chat = spawn_chat_cfg(&root, &sc, "");
    chat.expect("local session", Duration::from_secs(60));
    chat.send("long question");
    if !wait_turn_running(&data, Duration::from_secs(30)) {
        panic!("turn never reached running:\n{}", chat.transcript());
    }
    chat.send("no");
    chat.expect("steered into the running reply", Duration::from_secs(20));
    chat.send("wait for the green build");
    chat.expect("steered into the running reply", Duration::from_secs(20));
    assert!(
        !chat.transcript().contains("⏹ stopped"),
        "a medium word must not interrupt:\n{}",
        chat.transcript()
    );
    assert!(
        !chat.transcript().contains("queued (#"),
        "steered messages must not queue:\n{}",
        chat.transcript()
    );
    let conn = rusqlite::Connection::open(data.join("state.db")).unwrap();
    let turns: i64 = conn
        .query_row("SELECT COUNT(*) FROM turns", [], |r| r.get(0))
        .unwrap();
    assert_eq!(turns, 1, "the steers must not become their own turns");
    chat.send("/quit");
}

/// A3: `interrupts = "off"` restores today's behavior — `stop` steers.
#[test]
fn interrupts_off_restores_steer() {
    let root = temp_root("interrupts-off");
    let sc = long_turn(&root, "long.json");
    let data = root.join("data");
    let mut chat = spawn_chat_cfg(&root, &sc, "interrupts = \"off\"\n");
    chat.expect("local session", Duration::from_secs(60));
    chat.send("long question");
    if !wait_turn_running(&data, Duration::from_secs(30)) {
        panic!("turn never reached running:\n{}", chat.transcript());
    }
    chat.send("stop");
    chat.expect("steered into the running reply", Duration::from_secs(20));
    assert!(
        !chat.transcript().contains("⏹ stopped"),
        "interrupts = off must not abort:\n{}",
        chat.transcript()
    );
    chat.send("/quit");
}
