//! Checkpoint e2e (B1): the real `ka-agent` binary with its scripted
//! engine speaker — ask-gated bash writes leave a shadow-git checkpoint
//! at the single Allow-settle site, `/rewind` lists and restores them,
//! denies leave none, and an open cycle refuses the rewind.
//!
//! Limits carried by design (plan v2): the checkpoint fires only on
//! Allow-settled asks; a failed snapshot would be a listed hole, never
//! a blocked approval (unit-covered in `runtime/checkpoint.rs`).

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

/// Ask-mode chat over `root/ws`, mirroring `chat_e2e`'s harness.
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
        let mut stdin = self.child.stdin.take().unwrap();
        writeln!(stdin, "{line}").unwrap();
        stdin.flush().unwrap();
        self.child.stdin.replace(stdin);
    }

    fn transcript(&self) -> String {
        self.seen.borrow().clone()
    }

    fn expect(&mut self, needle: &str, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            while let Ok(line) = self.rx.try_recv() {
                self.seen.borrow_mut().push_str(&line);
                self.seen.borrow_mut().push('\n');
            }
            if self.transcript().contains(needle) {
                return;
            }
            if Instant::now() >= deadline {
                panic!(
                    "timed out waiting for {needle:?}; transcript:\n{}",
                    self.transcript()
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Position in the transcript: workers replay scenarios, so plain
    /// `contains` can match stale output (chat_e2e's `mark` pattern).
    fn mark(&self) -> usize {
        self.transcript().len()
    }

    fn expect_from(&mut self, needle: &str, from: usize, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            while let Ok(line) = self.rx.try_recv() {
                self.seen.borrow_mut().push_str(&line);
                self.seen.borrow_mut().push('\n');
            }
            if self.transcript()[from..].contains(needle) {
                return;
            }
            if Instant::now() >= deadline {
                panic!(
                    "timed out waiting for {needle:?}; transcript:\n{}",
                    self.transcript()
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn scenario_file(root: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
    let p = root.join(name);
    std::fs::write(&p, body).unwrap();

    p
}

fn temp_root(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "ka-checkpoint-e2e-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Two Allow-settled asks, a workspace the test mutates between them
/// (the scripted engine simulates bash calls — it raises the ask but
/// does not execute the command), then `/rewind 2` restores the FIRST
/// checkpoint's tree, leaves post-checkpoint files alone, and replies
/// with the stored question plus the checkpointed turn's inbound text.
#[test]
fn approved_write_checkpoints_and_rewind_restores() {
    let root = temp_root("restore");
    let sc = scenario_file(
        &root,
        "two-writes.json",
        r#"{
            "steps": [
                {"call": {"tool": "bash", "args": {"command": "echo broken > fix.rs"}}},
                {"call": {"tool": "bash", "args": {"command": "echo worse > fix.rs"}}},
                {"text": "both writes ran"}
            ],
            "stop": "done", "cost_usd": 0.0, "tokens": 10
        }"#,
    );
    let mut chat = spawn_chat(&root, &sc);
    let ws = root.join("ws");
    chat.send("fix the file");
    chat.expect("approve:", Duration::from_secs(60));
    // The first write lands before its allow releases it.
    std::fs::write(ws.join("fix.rs"), "broken\n").unwrap();
    chat.send("/y");
    // The second ask follows the first allow (the first card still
    // lingers in the transcript, so wait from a mark).
    let m = chat.mark();
    chat.expect_from("approve:", m, Duration::from_secs(60));
    std::fs::write(ws.join("fix.rs"), "worse\n").unwrap();
    chat.send("/y");
    chat.expect("both writes ran", Duration::from_secs(60));
    chat.expect("turn done", Duration::from_secs(30));
    assert_eq!(
        std::fs::read_to_string(ws.join("fix.rs")).unwrap(),
        "worse\n"
    );
    // A file created after the last checkpoint: restore must leave it.
    std::fs::write(ws.join("new.txt"), "after\n").unwrap();

    // Newest-first list shows both snapshots.
    chat.send("/rewind");
    chat.expect("checkpoints (newest first", Duration::from_secs(30));
    chat.expect("2. ", Duration::from_secs(30));

    // /rewind 2 = the first checkpoint's tree: the second write is
    // gone, the post-checkpoint file survives (checkout-only, no clean).
    chat.send("/rewind 2");
    chat.expect("rewound to", Duration::from_secs(30));
    chat.expect("re-propose: fix the file", Duration::from_secs(30));
    assert_eq!(
        std::fs::read_to_string(ws.join("fix.rs")).unwrap(),
        "broken\n",
        "the second write must be gone; the first checkpoint's tree restored"
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("new.txt")).unwrap(),
        "after\n"
    );
    chat.send("/quit");
}

/// A denied write settles outside the Allow site: no row, no snapshot.
#[test]
fn denied_writes_leave_no_checkpoint() {
    let root = temp_root("deny");
    let sc = scenario_file(
        &root,
        "one-write.json",
        r#"{
            "steps": [
                {"call": {"tool": "bash", "args": {"command": "echo nope > fix.rs"}}},
                {"text": "unreachable"}
            ],
            "stop": "done", "cost_usd": 0.0, "tokens": 10
        }"#,
    );
    let mut chat = spawn_chat(&root, &sc);
    chat.send("try the write");
    chat.expect("approve:", Duration::from_secs(60));
    chat.send("/n");
    chat.expect("→ Denied", Duration::from_secs(30));
    chat.send("/rewind");
    chat.expect("no checkpoints", Duration::from_secs(30));
    chat.send("/quit");
}

/// A session-allow settle checkpoints too: the ✓ auto-answer goes
/// through the same Allow site, so its write leaves a row.
#[test]
fn session_allowed_write_checkpoints_too() {
    let root = temp_root("session");
    // The two calls are identical: the second raise hits the
    // session-allow key and settles without a card.
    let sc = scenario_file(
        &root,
        "repeat.json",
        r#"{
            "steps": [
                {"call": {"tool": "bash", "args": {"command": "echo hi > f.txt"}}},
                {"text": "first done"},
                {"call": {"tool": "bash", "args": {"command": "echo hi > f.txt"}}},
                {"text": "second done"}
            ],
            "stop": "done", "cost_usd": 0.0, "tokens": 10
        }"#,
    );
    let mut chat = spawn_chat(&root, &sc);
    chat.send("write it once");
    chat.expect("approve:", Duration::from_secs(60));
    chat.send("/y session");
    chat.expect("first done", Duration::from_secs(60));
    chat.send("write it twice");
    chat.expect("✓ allowed for this session", Duration::from_secs(60));
    chat.expect("second done", Duration::from_secs(60));
    // Both settles checkpointed: the list numbers two rows.
    chat.send("/rewind");
    chat.expect("checkpoints (newest first", Duration::from_secs(30));
    chat.expect("2. ", Duration::from_secs(30));
    chat.send("/quit");
}

/// `/rewind` refuses while the session has an open cycle: the pending
/// ask keeps the workspace mid-write, and a restore would race it.
#[test]
fn rewind_refuses_while_cycle_open() {
    let root = temp_root("open");
    let sc = scenario_file(
        &root,
        "pending.json",
        r#"{
            "steps": [
                {"call": {"tool": "bash", "args": {"command": "echo one > f.rs"}}},
                {"text": "first done"},
                {"call": {"tool": "bash", "args": {"command": "echo two > f.rs"}}},
                {"text": "second done"}
            ],
            "stop": "done", "cost_usd": 0.0, "tokens": 10
        }"#,
    );
    let mut chat = spawn_chat(&root, &sc);
    let ws = root.join("ws");
    chat.send("first turn");
    chat.expect("approve:", Duration::from_secs(60));
    std::fs::write(ws.join("f.rs"), "one\n").unwrap();
    chat.send("/y");
    chat.expect("first done", Duration::from_secs(60));
    // Second turn: its ask is pending, the cycle is open.
    chat.send("second turn");
    let m = chat.mark();
    chat.expect_from("approve:", m, Duration::from_secs(60));
    chat.send("/rewind 1");
    chat.expect("open cycle", Duration::from_secs(30));
    // The workspace is untouched by the refused rewind.
    assert_eq!(std::fs::read_to_string(ws.join("f.rs")).unwrap(), "one\n");
    // Clean up: deny the pending ask so the cycle closes.
    chat.send("/n");
    chat.expect("→ Denied", Duration::from_secs(30));
    chat.expect("turn done", Duration::from_secs(30));
    // Now the restore works.
    chat.send("/rewind 1");
    chat.expect("rewound to", Duration::from_secs(30));
    chat.send("/quit");
}
