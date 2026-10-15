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
    chat.expect("tokens · recent worker crashes:", Duration::from_secs(10));
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
