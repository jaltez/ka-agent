//! M3 e2e: memory + history through the real binary (scripted engine).

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

    fn drain(&mut self) {
        while let Ok(line) = self.rx.try_recv() {
            self.seen.borrow_mut().push_str(&line);
            self.seen.borrow_mut().push('\n');
        }
    }

    fn expect(&mut self, needle: &str, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            self.drain();
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

    fn seen(&mut self) -> String {
        self.drain();
        self.seen.borrow().clone()
    }
}

fn scenario_file(root: &std::path::Path, body: &str) -> std::path::PathBuf {
    let p = root.join("s.json");
    std::fs::write(&p, body).unwrap();
    p
}

fn temp_root(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("ka-agent-m3-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn history_remember_forget_flow() {
    let root = temp_root("flow");
    let sc = scenario_file(
        &root,
        r#"{ "steps": [ {"text": "The Eiffel tower is in Paris"} ], "stop": "done", "cost_usd": 0.0, "tokens": 5 }"#,
    );
    let mut chat = spawn_chat(&root, &sc);
    chat.send("what about the eiffel");
    chat.expect("turn done", Duration::from_secs(30));
    chat.send("/status");
    chat.expect("● idle", Duration::from_secs(10));
    let out = chat.seen();
    assert!(
        out.contains("Eiffel"),
        "history should show the snippet:\n{out}"
    );
    // Remember → report shows the file; forget removes it.
    chat.send("/remember I prefer concise answers");
    chat.expect("remembered mem_", Duration::from_secs(10));
    chat.send("/memory");
    chat.expect("concise answers", Duration::from_secs(10));
    let mem_id = {
        let out = chat.seen();
        out.lines()
            .find_map(|l| l.split("remembered ").nth(1).map(|s| s.trim().to_string()))
            .unwrap()
    };
    chat.send(&format!("/forget {mem_id}"));
    chat.expect("forgotten", Duration::from_secs(10));
    // The compiled engine memory no longer carries the fact.
    let compiled =
        std::fs::read_to_string(root.join("data/engine-home/.config/ka/MEMORY.md")).unwrap();
    assert!(
        !compiled.contains("concise answers"),
        "compiled memory still has it:\n{compiled}"
    );
    // The memory git repo recorded both operations.
    let log = std::process::Command::new("git")
        .arg("-C")
        .arg(root.join("data/memory"))
        .arg("log")
        .arg("--format=%s")
        .output()
        .unwrap();
    let log = String::from_utf8_lossy(&log.stdout);
    assert!(
        log.contains(&format!("memory: accept {mem_id}")),
        "git log:\n{log}"
    );
    assert!(
        log.contains(&format!("memory: forget {mem_id}")),
        "git log:\n{log}"
    );

    chat.send("/quit");
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn candidate_review_accept_flow() {
    let root = temp_root("candidates");
    let sc = scenario_file(
        &root,
        r#"{ "steps": [ {"text": "ok"} ], "stop": "done", "cost_usd": 0.0, "tokens": 1 }"#,
    );

    // Seed one pending candidate directly into the store (the model-side
    // producer arrives with the connector bridge; SEED.md §4.4 path 2).
    {
        use ka_agent_store::Store;
        let store = Store::open(&root.join("data/state.db")).unwrap();
        store
            .add_candidate(
                &ka_agent_core::MemoryId("mem_cand-test-0001".into()),
                "model",
                "Operator lives in Madrid",
                Some("session/test"),
            )
            .unwrap();
    }

    let mut chat = spawn_chat(&root, &sc);
    chat.send("/memory review");
    chat.expect("mem_cand-test-0001", Duration::from_secs(10));
    let out = chat.seen();
    assert!(out.contains("Madrid"), "review shows text:\n{out}");

    chat.send("/memory accept mem_cand-test-0001");
    chat.expect("accepted into stable memory", Duration::from_secs(10));
    // Now it is stable memory: facts.md + compiled MEMORY.md + git commit.
    let facts = std::fs::read_to_string(root.join("data/memory/facts.md")).unwrap();
    assert!(facts.contains("Madrid"), "facts.md:\n{facts}");
    let compiled =
        std::fs::read_to_string(root.join("data/engine-home/.config/ka/MEMORY.md")).unwrap();
    assert!(compiled.contains("Madrid"), "compiled:\n{compiled}");
    // Review is empty again.
    chat.send("/memory review");
    chat.expect("no pending candidates", Duration::from_secs(10));

    chat.send("/quit");
    std::fs::remove_dir_all(&root).ok();
}
