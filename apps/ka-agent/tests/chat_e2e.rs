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
    chat.expect("approve:", Duration::from_secs(60));
    // While the approval is pending, a second message must queue (gap G4).
    chat.send("and then summarize");
    chat.expect("queued (#1)", Duration::from_secs(30));
    chat.send("/y");
    chat.expect("done anyway", Duration::from_secs(60));
    // The queued message runs next and gets its own scripted answer.
    chat.expect("turn done", Duration::from_secs(60));
    let _ = std::fs::remove_file("/tmp/ka-agent-e2e-should-not-exist");
    chat.send("/quit");
}
